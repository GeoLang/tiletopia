use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::upload::MAX_UPLOAD_BYTES;

// zip holds every central directory record in memory, a few hundred bytes each
const CENTRAL_DIRECTORY_READ_BYTES: u64 = 8 * 1024 * 1024;
// deflate reaches about 1032 to 1 on a bomb, and a 51 MB GeoJSON measured 3.3 to 1
const MOST_UNPACKED_SOURCE_BYTES_PER_ZIP_BYTE: u64 = 100;
pub(crate) const MOST_ZIP_ENTRIES: usize = 10_000;
// macos archive utility writes resource forks under this directory
const MACOS_RESOURCE_FORK_DIR: &str = "__MACOSX";

struct ReadAllowance<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for ReadAllowance<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 && !buffer.is_empty() {
            return Err(std::io::Error::other(format!(
                "the zip's central directory reads past {CENTRAL_DIRECTORY_READ_BYTES} bytes"
            )));
        }
        let most = buffer
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buffer[..most])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for ReadAllowance<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

fn open(zip_path: &Path) -> Result<File, String> {
    File::open(zip_path).map_err(|e| format!("could not open {}: {e}", zip_path.display()))
}

fn refuse_large_central_directory(zip_path: &Path) -> Result<(), String> {
    let capped = ReadAllowance {
        inner: open(zip_path)?,
        remaining: CENTRAL_DIRECTORY_READ_BYTES,
    };
    zip::ZipArchive::new(capped)
        .map(drop)
        .map_err(|e| format!("not a readable zip: {e}"))
}

pub(crate) struct OpenedZip {
    pub(crate) archive: zip::ZipArchive<BufReader<File>>,
    pub(crate) zip_bytes: u64,
}

pub(crate) fn open_zip(zip_path: &Path) -> Result<OpenedZip, String> {
    refuse_large_central_directory(zip_path)?;
    let file = open(zip_path)?;
    let zip_bytes = file
        .metadata()
        .map_err(|e| format!("could not read {}: {e}", zip_path.display()))?
        .len();
    let archive = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("not a readable zip: {e}"))?;
    Ok(OpenedZip { archive, zip_bytes })
}

pub(crate) fn enclosed_entry_path(
    entry: &zip::read::ZipFile<'_, BufReader<File>>,
) -> Result<PathBuf, String> {
    let entry_name = entry.name();
    if entry.is_symlink() {
        return Err(format!(
            "{entry_name}: a symbolic link, which a zip upload may not hold"
        ));
    }
    let leaves = || format!("{entry_name}: a path that leaves the zip's own directory");
    // zip's enclosed_name drops a leading slash or drive letter and resolves a/../b
    let name = entry_name.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') || name.split('/').any(|step| step == "..") {
        return Err(leaves());
    }
    entry.enclosed_name().ok_or_else(leaves)
}

pub(crate) fn is_macos_resource_fork(relative_path: &Path) -> bool {
    relative_path
        .components()
        .any(|component| component.as_os_str() == MACOS_RESOURCE_FORK_DIR)
}

pub(crate) struct UnpackedZip {
    pub(crate) relative_paths: Vec<PathBuf>,
    pub(crate) total_bytes: u64,
}

pub(crate) fn unpack_zip(zip_path: &Path, destination: &Path) -> Result<UnpackedZip, String> {
    let OpenedZip {
        mut archive,
        zip_bytes,
    } = open_zip(zip_path)?;
    let entry_count = archive.len();
    if entry_count > MOST_ZIP_ENTRIES {
        return Err(format!(
            "the zip holds {entry_count} entries, more than the {MOST_ZIP_ENTRIES} allowed"
        ));
    }
    let most_unpacked_bytes = zip_bytes
        .saturating_mul(MOST_UNPACKED_SOURCE_BYTES_PER_ZIP_BYTE)
        .min(MAX_UPLOAD_BYTES as u64);

    let mut relative_paths = Vec::new();
    let mut total_bytes = 0u64;
    for index in 0..entry_count {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("zip entry {index}: {e}"))?;
        let relative_path = enclosed_entry_path(&entry)?;
        if entry.is_dir() || is_macos_resource_fork(&relative_path) {
            continue;
        }

        let entry_name = entry.name().to_string();
        let path = destination.join(&relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{entry_name}: {e}"))?;
        }
        // create_new refuses a second entry that normalizes to the same path
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{entry_name}: {e}"))?;
        let room_left = most_unpacked_bytes - total_bytes;
        total_bytes += std::io::copy(&mut (&mut entry).take(room_left + 1), &mut file)
            .map_err(|e| format!("{entry_name}: {e}"))?;
        if total_bytes > most_unpacked_bytes {
            return Err(format!(
                "the zip unpacks to more than {most_unpacked_bytes} bytes, the limit for a zip of {zip_bytes} bytes"
            ));
        }
        relative_paths.push(relative_path);
    }
    Ok(UnpackedZip {
        relative_paths,
        total_bytes,
    })
}
