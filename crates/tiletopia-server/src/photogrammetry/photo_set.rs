use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use crate::zip_unpack::{OpenedZip, enclosed_entry_path, is_macos_resource_fork, open_zip};

const MAX_PHOTO_BYTES: u64 = 128 * 1024 * 1024;
// jpeg data is already compressed, so deflate shrinks a photo by a few percent at most
const MOST_UNPACKED_BYTES_PER_ZIP_BYTE: u64 = 2;

fn require_gps(name: &str, photo: &[u8]) -> Result<(), String> {
    let exif_block = exif::get_exif_attr_from_jpeg(&mut &photo[..])
        .map_err(|e| format!("{name}: no EXIF block in a JPEG: {e}"))?;
    let exif = exif::Reader::new()
        .read_raw(exif_block)
        .map_err(|e| format!("{name}: unreadable EXIF: {e}"))?;
    for tag in [exif::Tag::GPSLatitude, exif::Tag::GPSLongitude] {
        let degrees_minutes_seconds = exif.get_field(tag, exif::In::PRIMARY).map(|f| &f.value);
        if !matches!(degrees_minutes_seconds, Some(exif::Value::Rational(parts)) if parts.len() == 3)
        {
            return Err(format!(
                "{name}: no EXIF {tag}, and ODM places the model by its photos' GPS"
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_photo_set(zip_path: &Path) -> Result<(), String> {
    for_each_photo(zip_path, |_, _, _| Ok(()))
}

pub(super) fn for_each_photo(
    zip_path: &Path,
    mut visit: impl FnMut(&str, Vec<u8>, f64) -> Result<(), String>,
) -> Result<(), String> {
    let OpenedZip {
        mut archive,
        zip_bytes,
    } = open_zip(zip_path)?;
    let most_unpacked_bytes = zip_bytes.saturating_mul(MOST_UNPACKED_BYTES_PER_ZIP_BYTE);

    let entry_count = archive.len();
    let mut names = HashSet::new();
    let mut unpacked_bytes = 0u64;
    let mut photos = 0;
    for index in 0..entry_count {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("zip entry {index}: {e}"))?;
        let entry_name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        let path = enclosed_entry_path(&entry)?;
        if is_macos_resource_fork(&path) {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{entry_name}: no file name"))?
            .to_string();
        let is_jpeg = Path::new(&name).extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        });
        if !is_jpeg {
            return Err(format!(
                "{entry_name}: not a .jpg or .jpeg, and the zip may hold only JPEG photos"
            ));
        }
        if !names.insert(name.clone()) {
            return Err(format!(
                "{entry_name}: a second photo named {name}, and ODM needs every photo name once"
            ));
        }

        let mut photo = Vec::new();
        (&mut entry)
            .take(MAX_PHOTO_BYTES + 1)
            .read_to_end(&mut photo)
            .map_err(|e| format!("{entry_name}: {e}"))?;
        if photo.len() as u64 > MAX_PHOTO_BYTES {
            return Err(format!("{entry_name}: larger than {MAX_PHOTO_BYTES} bytes"));
        }
        unpacked_bytes += photo.len() as u64;
        if unpacked_bytes > most_unpacked_bytes {
            return Err(format!(
                "the photos unpack to more than {MOST_UNPACKED_BYTES_PER_ZIP_BYTE} times the \
                 zip's {zip_bytes} bytes, which JPEG photos do not"
            ));
        }
        require_gps(&entry_name, &photo)?;
        visit(&name, photo, (index + 1) as f64 / entry_count as f64)?;
        photos += 1;
    }
    if photos == 0 {
        return Err("the zip holds no photos".to_string());
    }
    Ok(())
}
