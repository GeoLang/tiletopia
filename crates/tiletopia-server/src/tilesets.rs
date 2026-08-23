//! Vector tilesets: an uploaded vector file, a tippecanoe build of it into one
//! PMTiles archive, and the martin source that serves the archive.
//!
//! A build runs for minutes, so the upload answers 202 and a worker picks the
//! row up. The archive is an explicit snapshot: nothing rebuilds on its own,
//! and re-uploading makes a new one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::db::{Database, TilesetRecord, TilesetStatus};
use crate::map_tiles::martin_backend::MartinTileBackend;
use crate::{AppState, auth, users};

/// Directory the built archives live in. Unset puts them under
/// `<data-dir>/tilesets`. Kept apart from `TILETOPIA_PMTILES_DIR`, which is
/// scanned wholesale at startup: these archives re-register from the registry.
pub const TILESET_DIR_ENV: &str = "TILETOPIA_TILESET_DIR";

/// How long one tippecanoe run may take before it is killed.
pub const BUILD_TIMEOUT_ENV: &str = "TILETOPIA_TILESET_TIMEOUT_SECS";

/// Address space the build may map.
pub const MEMORY_LIMIT_ENV: &str = "TILETOPIA_TILESET_MEMORY_MB";

/// Largest single file the build may write, the archive included.
pub const DISK_LIMIT_ENV: &str = "TILETOPIA_TILESET_DISK_MB";

const DEFAULT_TIMEOUT_SECS: u64 = 3600;
const DEFAULT_MEMORY_MB: u64 = 4096;
const DEFAULT_DISK_MB: u64 = 20_480;

/// tippecanoe reports progress and refusals on stderr and nowhere else, so a
/// failed build keeps this much of it.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

const TIPPECANOE: &str = "tippecanoe";

/// Extensions tippecanoe reads, longest first so `.geojson.gz` is not read as
/// `.geojson`.
const ACCEPTED_EXTENSIONS: [&str; 4] = [".geojson.gz", ".geojson", ".fgb", ".csv"];

/// How often the worker looks for a queued build.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The 202 answer to an upload: the job that will build the archive, and the
/// registry row it is building. One build per archive, so the job id is the
/// tileset id and `GET /api/v1/tilesets/{id}` is what a client polls.
#[derive(Debug, Serialize)]
pub struct UploadResponse {
    pub job_id: Uuid,
    pub tileset: TilesetRecord,
}

type RouteError = (StatusCode, String);

fn bad_request(message: impl Into<String>) -> RouteError {
    (StatusCode::BAD_REQUEST, message.into())
}

fn server_error(message: impl Into<String>) -> RouteError {
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

/// Where the built archives sit.
pub fn tileset_dir(data_dir: &Path) -> PathBuf {
    std::env::var(TILESET_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("tilesets"))
}

/// The uploaded file and tippecanoe's scratch space for one build, removed once
/// the build reaches a terminal state.
fn build_dir(data_dir: &Path, id: Uuid) -> PathBuf {
    data_dir.join("tileset_builds").join(id.to_string())
}

/// What tippecanoe is told to spill into, which is where the file-size limit
/// bites rather than the volume filling up.
fn temporary_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("tmp")
}

/// Which of the accepted extensions this filename ends in.
fn accepted_extension(filename: &str) -> Option<&'static str> {
    let lower = filename.to_lowercase();
    ACCEPTED_EXTENSIONS
        .into_iter()
        .find(|extension| lower.ends_with(extension))
}

/// The layer name inside the archive: the filename with its extension taken
/// off, cut down to what a layer name may hold. tippecanoe would derive its own
/// from the input path, which is this server's scratch filename rather than
/// anything the uploader would recognise.
fn layer_name(filename: &str, extension: &str) -> String {
    let stem = &filename[..filename.len().saturating_sub(extension.len())];
    let stem = stem.rsplit(['/', '\\']).next().unwrap_or(stem);
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "layer".to_string()
    } else {
        cleaned
    }
}

/// The exact tippecanoe run for this build, recorded in the registry as it is.
/// `-zg` picks a maxzoom from the spacing of the features and
/// `--drop-densest-as-needed` thins a tile that comes out too large, which
/// together keep an unseen upload from producing tiles no client can load.
fn tippecanoe_argv(input: &Path, output: &Path, layer: &str, temporary_dir: &Path) -> Vec<String> {
    // tippecanoe refuses a work directory that does not start with a slash, and
    // the data directory the server was started with may well be relative
    let path = |p: &Path| {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned()
    };
    vec![
        TIPPECANOE.to_string(),
        "-o".to_string(),
        path(output),
        "-l".to_string(),
        layer.to_string(),
        "-zg".to_string(),
        "--drop-densest-as-needed".to_string(),
        "--temporary-directory".to_string(),
        path(temporary_dir),
        path(input),
    ]
}

/// The tippecanoe version string, or `None` when the binary is not on `PATH`.
/// Tests that need a real build skip on `None`.
pub fn tippecanoe_version() -> Option<String> {
    // tippecanoe prints its version on stderr and exits non-zero for -v
    let output = std::process::Command::new(TIPPECANOE)
        .arg("-v")
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&output.stdout);
    let version = if version.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    } else {
        version.trim().to_string()
    };
    Some(version)
}

/// What one build may spend.
#[derive(Debug, Clone, Copy)]
pub struct BuildLimits {
    pub timeout: Duration,
    pub memory_bytes: u64,
    pub file_bytes: u64,
}

impl BuildLimits {
    /// The limits the environment asks for, each falling back to its default
    /// when unset or unreadable.
    pub fn from_env() -> Self {
        let number = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(default)
        };
        Self {
            timeout: Duration::from_secs(number(BUILD_TIMEOUT_ENV, DEFAULT_TIMEOUT_SECS)),
            memory_bytes: number(MEMORY_LIMIT_ENV, DEFAULT_MEMORY_MB) * 1024 * 1024,
            file_bytes: number(DISK_LIMIT_ENV, DEFAULT_DISK_MB) * 1024 * 1024,
        }
    }
}

/// The command for one build. The limits are set between fork and exec, so
/// tippecanoe inherits them and this process keeps its own.
fn build_command(argv: &[String], limits: &BuildLimits) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        let limits = *limits;
        // safety: the hook does nothing but two setrlimit syscalls on stack
        // values, which is all a forked child may do before exec
        unsafe {
            command.pre_exec(move || {
                set_limits(&limits);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = limits;

    command
}

/// tippecanoe is OOM-prone on a large input, so the address space is capped.
/// `RLIMIT_FSIZE` is the quota a work directory on a shared volume can be
/// given: it caps any single file the build writes, the archive included.
#[cfg(unix)]
fn set_limits(limits: &BuildLimits) {
    let apply = |resource, bytes: u64| {
        let limit = libc::rlimit {
            rlim_cur: bytes as libc::rlim_t,
            rlim_max: bytes as libc::rlim_t,
        };
        // safety: a bare syscall on a stack value, in the forked child
        unsafe {
            libc::setrlimit(resource, &limit);
        }
    };
    apply(libc::RLIMIT_AS, limits.memory_bytes);
    apply(libc::RLIMIT_FSIZE, limits.file_bytes);
}

/// Everything tippecanoe wrote on stderr, cut to its last [`STDERR_TAIL_BYTES`].
async fn stderr_tail(mut stderr: tokio::process::ChildStderr) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(read) = stderr.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 2 * STDERR_TAIL_BYTES {
            buffer.drain(..buffer.len() - STDERR_TAIL_BYTES);
        }
    }
    if buffer.len() > STDERR_TAIL_BYTES {
        buffer.drain(..buffer.len() - STDERR_TAIL_BYTES);
    }
    String::from_utf8_lossy(&buffer).trim().to_string()
}

/// Run one tippecanoe build. `Ok` means the archive is written.
async fn run_tippecanoe(argv: &[String], limits: &BuildLimits) -> Result<(), String> {
    let mut child = build_command(argv, limits)
        .spawn()
        .map_err(|e| format!("could not run {TIPPECANOE}: {e}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{TIPPECANOE} gave no stderr to read"))?;
    let collect = tokio::spawn(stderr_tail(stderr));

    // the kill goes before the stderr read is awaited: a live child holds the
    // write end of the pipe open, so reading to end would wait for it
    let waited = tokio::time::timeout(limits.timeout, child.wait()).await;
    if waited.is_err() {
        let _ = child.kill().await;
    }
    let tail = collect.await.unwrap_or_default();

    let Ok(status) = waited else {
        let seconds = limits.timeout.as_secs();
        return Err(format!(
            "{TIPPECANOE} was killed after {seconds} seconds: {tail}"
        ));
    };
    let status = status.map_err(|e| format!("{TIPPECANOE} could not be waited on: {e}"))?;

    if !status.success() {
        return Err(format!("{TIPPECANOE} {status}: {tail}"));
    }
    Ok(())
}

/// Builds queued archives one at a time and registers each finished one with
/// the martin backend.
pub struct TilesetBuilder {
    db: Arc<Database>,
    data_dir: PathBuf,
    tileset_dir: PathBuf,
    backend: MartinTileBackend,
    limits: BuildLimits,
}

impl TilesetBuilder {
    pub fn new(
        db: Arc<Database>,
        data_dir: PathBuf,
        tileset_dir: PathBuf,
        backend: MartinTileBackend,
    ) -> Self {
        Self {
            db,
            data_dir,
            tileset_dir,
            backend,
            limits: BuildLimits::from_env(),
        }
    }

    /// Start the background worker loop.
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.db.claim_tileset_build().await {
                    Ok(Some(tileset)) => self.build(tileset).await,
                    Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
                    Err(e) => {
                        tracing::error!("could not poll the tileset build queue: {e}");
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                }
            }
        })
    }

    /// Build one archive and write the outcome back to the registry.
    pub async fn build(&self, mut tileset: TilesetRecord) {
        let scratch = build_dir(&self.data_dir, tileset.id);
        let archive = self.tileset_dir.join(&tileset.object_key);
        let outcome = self.build_archive(&tileset, &scratch, &archive).await;
        let _ = tokio::fs::remove_dir_all(&scratch).await;

        tileset.built_at = Some(chrono::Utc::now());
        match outcome {
            Ok(size_bytes) => {
                tileset.status = TilesetStatus::Ready;
                tileset.size_bytes = size_bytes;
                tracing::info!(
                    "tileset {} serves at /martin/{} ({size_bytes} bytes)",
                    tileset.id,
                    tileset.source_id
                );
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&archive).await;
                tileset.status = TilesetStatus::Failed;
                tracing::error!("tileset {} failed: {error}", tileset.id);
                tileset.error = Some(error);
            }
        }
        if let Err(e) = self.db.finish_tileset(&tileset).await {
            tracing::error!("could not record tileset {}: {e}", tileset.id);
        }
    }

    /// Run the build and register the archive, reporting its size.
    async fn build_archive(
        &self,
        tileset: &TilesetRecord,
        scratch: &Path,
        archive: &Path,
    ) -> Result<u64, String> {
        tokio::fs::create_dir_all(temporary_dir(scratch))
            .await
            .map_err(|e| format!("could not make the build's work directory: {e}"))?;
        tokio::fs::create_dir_all(&self.tileset_dir)
            .await
            .map_err(|e| format!("could not make the tileset directory: {e}"))?;

        run_tippecanoe(&tileset.argv, &self.limits).await?;

        let size_bytes = tokio::fs::metadata(archive)
            .await
            .map_err(|e| format!("{TIPPECANOE} wrote no archive: {e}"))?
            .len();

        self.backend
            .add_pmtiles(&tileset.source_id, archive)
            .await?;
        Ok(size_bytes)
    }
}

/// Register every archive the registry says is ready, so a restart serves what
/// the last run built. An archive that cannot be opened is logged and skipped,
/// the same as a hand-dropped one in the PMTiles directory.
pub async fn register_ready_tilesets(
    db: &Database,
    backend: &MartinTileBackend,
    tileset_dir: &Path,
) -> Result<(), String> {
    let ready = db
        .list_ready_tilesets()
        .await
        .map_err(|e| format!("could not read the tileset registry: {e}"))?;

    for tileset in ready {
        let archive = tileset_dir.join(&tileset.object_key);
        match backend.add_pmtiles(&tileset.source_id, &archive).await {
            Ok(()) => tracing::info!(
                "serving tileset '{}' from {}",
                tileset.source_id,
                archive.display()
            ),
            Err(e) => tracing::warn!("skipping tileset '{}': {e}", tileset.source_id),
        }
    }

    Ok(())
}

/// Whether these claims may modify this tileset. Every row has an owner, so
/// there is no legacy case to wave through.
fn may_modify(claims: &auth::Claims, tileset: &TilesetRecord) -> bool {
    claims.can_admin() || claims.sub == tileset.owner_id
}

/// Take a vector file, write it aside, and queue the build that turns it into
/// a PMTiles archive.
pub async fn upload_tileset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), RouteError> {
    // the route sits behind require_editor, so a valid token is always present
    let owner_id = users::claims_from_headers(&headers)
        .map_err(|status| (status, String::new()))?
        .sub;

    let mut name = None;
    let mut filename = None;
    let mut data = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart body"))?
    {
        match field.name().unwrap_or("") {
            "name" => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("name is not text"))?,
                );
            }
            "file" => {
                filename = Some(field.file_name().unwrap_or("upload").to_string());
                data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|_| bad_request("could not read the uploaded file"))?,
                );
            }
            _ => {}
        }
    }

    let file_data = data.ok_or_else(|| bad_request("no file field in the upload"))?;
    let original_filename = filename.unwrap_or_else(|| "upload".to_string());
    let extension = accepted_extension(&original_filename).ok_or_else(|| {
        bad_request(format!(
            "{original_filename}: a tileset is built from {}",
            ACCEPTED_EXTENSIONS.join(", ")
        ))
    })?;

    let id = Uuid::new_v4();
    let scratch = build_dir(&state.data_dir, id);
    tokio::fs::create_dir_all(&scratch)
        .await
        .map_err(|_| server_error("could not create the build directory"))?;

    // the uploader's filename never becomes a path here, only the extension
    // tippecanoe reads the format from
    let input = scratch.join(format!("source{extension}"));
    tokio::fs::write(&input, &file_data)
        .await
        .map_err(|_| server_error("could not write the uploaded file"))?;

    let layer_name = layer_name(&original_filename, extension);
    let object_key = format!("{id}.pmtiles");
    let archive = state.tileset_dir.join(&object_key);

    let tileset = TilesetRecord {
        id,
        name: name.unwrap_or_else(|| original_filename.clone()),
        status: TilesetStatus::Building,
        source_id: id.to_string(),
        object_key,
        original_filename,
        argv: tippecanoe_argv(&input, &archive, &layer_name, &temporary_dir(&scratch)),
        layer_name,
        size_bytes: 0,
        created_at: chrono::Utc::now(),
        started_at: None,
        built_at: None,
        error: None,
        owner_id,
    };

    state
        .db
        .create_tileset(&tileset)
        .await
        .map_err(|_| server_error("could not store the tileset"))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(UploadResponse {
            job_id: id,
            tileset,
        }),
    ))
}

/// The caller's tilesets, newest first. Admins see every row.
pub async fn list_tilesets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<TilesetRecord>>, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let tilesets = state
        .db
        .list_tilesets()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .filter(|tileset| may_modify(&claims, tileset))
        .collect();
    Ok(Json(tilesets))
}

pub async fn get_tileset(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<TilesetRecord>, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let tileset = state
        .db
        .get_tileset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !may_modify(&claims, &tileset) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(tileset))
}

/// Drop the archive, its registry row and its martin source together, so no
/// half of the pair outlives the other.
pub async fn delete_tileset(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let tileset = state
        .db
        .get_tileset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if !may_modify(&claims, &tileset) {
        return Err(StatusCode::FORBIDDEN);
    }

    state.martin_backend.remove_source(&tileset.source_id).await;
    let _ = tokio::fs::remove_file(state.tileset_dir.join(&tileset.object_key)).await;
    let _ = tokio::fs::remove_dir_all(build_dir(&state.data_dir, id)).await;
    state
        .db
        .delete_tileset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_accepted_extension_is_recognised_case_insensitively() {
        assert_eq!(accepted_extension("roads.geojson"), Some(".geojson"));
        assert_eq!(accepted_extension("roads.GeoJSON"), Some(".geojson"));
        assert_eq!(accepted_extension("roads.geojson.gz"), Some(".geojson.gz"));
        assert_eq!(accepted_extension("roads.fgb"), Some(".fgb"));
        assert_eq!(accepted_extension("roads.csv"), Some(".csv"));
    }

    #[test]
    fn an_unaccepted_extension_is_none() {
        assert_eq!(accepted_extension("roads.shp"), None);
        assert_eq!(accepted_extension("roads.gpkg"), None);
        assert_eq!(accepted_extension("roads"), None);
    }

    #[test]
    fn the_layer_name_is_the_filename_stem_without_its_path_or_punctuation() {
        assert_eq!(layer_name("roads.geojson", ".geojson"), "roads");
        assert_eq!(
            layer_name("city roads.geojson.gz", ".geojson.gz"),
            "city_roads"
        );
        assert_eq!(layer_name("/tmp/sub dir/roads.fgb", ".fgb"), "roads");
        assert_eq!(layer_name("...csv", ".csv"), "layer");
    }

    #[test]
    fn the_argv_names_the_output_the_layer_the_work_directory_and_the_input() {
        let argv = tippecanoe_argv(
            Path::new("/data/tileset_builds/abc/source.geojson"),
            Path::new("/data/tilesets/abc.pmtiles"),
            "roads",
            Path::new("/data/tileset_builds/abc/tmp"),
        );
        assert_eq!(
            argv,
            [
                "tippecanoe",
                "-o",
                "/data/tilesets/abc.pmtiles",
                "-l",
                "roads",
                "-zg",
                "--drop-densest-as-needed",
                "--temporary-directory",
                "/data/tileset_builds/abc/tmp",
                "/data/tileset_builds/abc/source.geojson",
            ]
        );
    }

    #[test]
    fn the_tileset_directory_falls_back_to_one_under_the_data_directory() {
        // the variable is read per call, so this only checks the unset default
        if std::env::var(TILESET_DIR_ENV).is_ok() {
            return;
        }
        assert_eq!(
            tileset_dir(Path::new("/data")),
            PathBuf::from("/data/tilesets")
        );
    }

    #[tokio::test]
    async fn a_build_that_outlives_its_timeout_is_killed_and_says_so() {
        let limits = BuildLimits {
            timeout: Duration::from_millis(200),
            memory_bytes: DEFAULT_MEMORY_MB * 1024 * 1024,
            file_bytes: DEFAULT_DISK_MB * 1024 * 1024,
        };
        let argv = vec!["sleep".to_string(), "30".to_string()];

        let error = run_tippecanoe(&argv, &limits).await.unwrap_err();
        assert!(error.contains("killed after"), "{error}");
    }

    #[tokio::test]
    async fn a_child_that_exits_non_zero_reports_its_stderr() {
        let limits = BuildLimits {
            timeout: Duration::from_secs(30),
            memory_bytes: DEFAULT_MEMORY_MB * 1024 * 1024,
            file_bytes: DEFAULT_DISK_MB * 1024 * 1024,
        };
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo refused on stderr >&2; exit 3".to_string(),
        ];

        let error = run_tippecanoe(&argv, &limits).await.unwrap_err();
        assert!(error.contains("refused on stderr"), "{error}");
    }
}
