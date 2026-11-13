use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use axum::http::StatusCode;

type TileFailure = (StatusCode, String);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn setting_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

const DEFAULT_CACHE_MEGABYTES: u64 = 1024;

pub(crate) fn cache_bytes_setting(variable: &str) -> Result<u64, String> {
    let megabytes = match setting_text(variable) {
        None => DEFAULT_CACHE_MEGABYTES,
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|&megabytes| megabytes > 0)
            .ok_or_else(|| {
                format!("{variable} is {raw:?}, expected a whole number of megabytes above 0")
            })?,
    };
    Ok(megabytes * 1024 * 1024)
}

pub(crate) struct TileCache {
    directory: PathBuf,
    capacity_bytes: u64,
    extension: &'static str,
    index: Mutex<Option<CacheIndex>>,
}

#[derive(Clone, Copy)]
pub(crate) struct TileAddress {
    pub level: u32,
    pub x: u32,
    pub y: u32,
}

fn cache_failure(path: &Path, error: std::io::Error) -> TileFailure {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("tile cache {}: {error}", path.display()),
    )
}

impl TileCache {
    pub(crate) fn new(directory: PathBuf, capacity_bytes: u64, extension: &'static str) -> Self {
        TileCache {
            directory,
            capacity_bytes,
            extension,
            index: Mutex::new(None),
        }
    }

    fn tile_path(&self, tile: TileAddress) -> PathBuf {
        self.directory
            .join(tile.level.to_string())
            .join(tile.x.to_string())
            .join(format!("{}.{}", tile.y, self.extension))
    }

    pub(crate) async fn read(&self, tile: TileAddress) -> Result<Option<Vec<u8>>, TileFailure> {
        let path = self.tile_path(tile);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(cache_failure(&path, error)),
        }
    }

    pub(crate) fn store(&self, tile: TileAddress, bytes: &[u8]) -> Result<(), TileFailure> {
        let path = self.tile_path(tile);
        let mut loaded = lock(&self.index);
        if loaded.is_none() {
            let scanned = CacheIndex::scan(&self.directory)
                .map_err(|error| cache_failure(&self.directory, error))?;
            *loaded = Some(scanned);
        }
        let index = loaded.as_mut().expect("the index was just loaded");
        if path.exists() {
            return Ok(());
        }
        let level_directory = path
            .parent()
            .expect("a tile path sits in a column directory");
        std::fs::create_dir_all(level_directory)
            .map_err(|error| cache_failure(level_directory, error))?;
        let staging = path.with_extension("partial");
        std::fs::write(&staging, bytes).map_err(|error| cache_failure(&staging, error))?;
        std::fs::rename(&staging, &path).map_err(|error| cache_failure(&path, error))?;
        index.oldest_first.push_back((path, bytes.len() as u64));
        index.total_bytes += bytes.len() as u64;
        while index.total_bytes > self.capacity_bytes {
            let Some((oldest, size)) = index.oldest_first.pop_front() else {
                break;
            };
            match std::fs::remove_file(&oldest) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(cache_failure(&oldest, error)),
            }
            index.total_bytes -= size;
        }
        Ok(())
    }
}

struct CacheIndex {
    oldest_first: VecDeque<(PathBuf, u64)>,
    total_bytes: u64,
}

fn collect_files(
    directory: &Path,
    files: &mut Vec<(SystemTime, PathBuf, u64)>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_files(&entry.path(), files)?;
        } else {
            files.push((metadata.modified()?, entry.path(), metadata.len()));
        }
    }
    Ok(())
}

impl CacheIndex {
    fn scan(directory: &Path) -> std::io::Result<CacheIndex> {
        let mut files = Vec::new();
        collect_files(directory, &mut files)?;
        files.sort_by_key(|(modified, _, _)| *modified);
        let total_bytes: u64 = files.iter().map(|(_, _, size)| size).sum();
        Ok(CacheIndex {
            oldest_first: files
                .into_iter()
                .map(|(_, path, size)| (path, size))
                .collect(),
            total_bytes,
        })
    }
}
