//! Cloud Optimized GeoTIFF serving.
//!
//! `TILETOPIA_COG_SOURCES` names what to serve, one href per comma-separated
//! entry, each a local path or an http(s) URL. Every entry is opened once at
//! startup for the layout the file declares, and reads go through terrano's
//! windowed `CogReader`: local files by seek, remote files by HTTP `Range`, so a
//! window costs the internal tiles it touches rather than the whole file.
//!
//! Unset serves nothing. A reader is not kept open between requests, so a window
//! pays for a few small header reads before its tiles.

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use terrano_core::{CogReader, Error as TerranoError, RangeRead};

/// Hrefs to serve, comma separated, each a local path or an http(s) URL.
pub const SOURCES_ENV: &str = "TILETOPIA_COG_SOURCES";

/// Most pixels one window may ask for. A 512-square window of f64 samples is
/// already megabytes of JSON per band.
pub const MAX_WINDOW_PIXELS: usize = 512 * 512;

/// How long one range request waits. A window is several of these in sequence,
/// and each holds a blocking thread while it waits.
const RANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A registered COG, described by what its own header says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CogDataset {
    /// Filename stem of the href, the way a PMTiles source is keyed.
    pub id: String,
    pub href: String,
    pub file_size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub band_count: usize,
    pub crs: String,
    /// `[west, south, east, north]` in `crs` units, which are metres for a
    /// projected file and degrees only for a geographic one.
    pub bounds: [f64; 4],
    pub pixel_size: [f64; 2],
    pub tile_size: [u32; 2],
    /// Full resolution first, then every overview the file carries.
    pub levels: Vec<CogLevelInfo>,
}

/// One resolution level of a registered COG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CogLevelInfo {
    pub level: usize,
    pub width: u32,
    pub height: u32,
    pub pixel_size: [f64; 2],
}

/// A pixel window of one level, in that level's own pixel coordinates.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct WindowRequest {
    #[serde(default)]
    pub level: usize,
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
}

/// The samples a window read returned.
#[derive(Debug, Clone, Serialize)]
pub struct WindowResponse {
    pub id: String,
    pub level: usize,
    pub col: usize,
    pub row: usize,
    pub cols: usize,
    pub rows: usize,
    /// One row-major plane per band. Nodata and pixels past the edge of the
    /// image come back null.
    pub bands: Vec<Vec<f64>>,
}

/// Why a COG could not be registered or read.
#[derive(Debug, thiserror::Error)]
pub enum CogError {
    #[error("{href} could not be opened: {reason}")]
    Open { href: String, reason: String },
    #[error("no COG dataset {id:?} is registered")]
    UnknownDataset { id: String },
    #[error("a window of {cols}x{rows} pixels is past the {MAX_WINDOW_PIXELS} allowed")]
    WindowTooLarge { cols: usize, rows: usize },
    #[error("a window needs at least one pixel")]
    EmptyWindow,
    #[error("{href} could not be read: {reason}")]
    Read { href: String, reason: String },
}

impl CogError {
    pub fn status(&self) -> StatusCode {
        match self {
            CogError::UnknownDataset { .. } => StatusCode::NOT_FOUND,
            CogError::WindowTooLarge { .. } | CogError::EmptyWindow => StatusCode::BAD_REQUEST,
            CogError::Open { .. } | CogError::Read { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// The COGs this server serves.
#[derive(Debug, Default)]
pub struct CogEngine {
    datasets: Vec<CogDataset>,
}

impl CogEngine {
    /// An engine serving nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open every href in `TILETOPIA_COG_SOURCES` and register what it declares
    /// about itself. Unset registers nothing and is not an error. A local path
    /// that cannot be opened is one, the way an unreadable PMTiles directory is:
    /// it is a typo, and an empty dataset list hides it. A remote href that
    /// cannot be opened is logged and skipped, so one unreachable host does not
    /// keep the server from starting.
    ///
    /// This opens files and makes range requests, so it belongs on a blocking
    /// thread.
    pub fn from_env() -> Result<Self, String> {
        let Some(sources) = std::env::var(SOURCES_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            return Ok(Self::new());
        };
        Self::from_sources(sources.split(','))
    }

    /// [`from_env`](Self::from_env) over the hrefs it read, so it is testable
    /// without touching process-global environment variables.
    pub fn from_sources<'a>(hrefs: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut engine = Self::new();
        for href in hrefs {
            let href = href.trim();
            if href.is_empty() {
                continue;
            }
            let Some(id) = source_id(href) else {
                return Err(format!("{SOURCES_ENV} entry {href:?} names no file"));
            };
            if engine.get_dataset(&id).is_some() {
                return Err(format!(
                    "{SOURCES_ENV} names two sources called {id:?}, and a dataset id has to be unique"
                ));
            }
            match open_dataset(&id, href) {
                Ok(dataset) => {
                    tracing::info!("serving COG {id:?} from {href}");
                    engine.datasets.push(dataset);
                }
                Err(e) if is_remote(href) => tracing::warn!("skipping COG source {href}: {e}"),
                Err(e) => return Err(format!("{SOURCES_ENV}: {e}")),
            }
        }
        Ok(engine)
    }

    pub fn list_datasets(&self) -> &[CogDataset] {
        &self.datasets
    }

    pub fn get_dataset(&self, id: &str) -> Option<&CogDataset> {
        self.datasets.iter().find(|d| d.id == id)
    }

    /// Read a pixel window from one level of a registered dataset.
    ///
    /// This reopens the source and makes range requests, so it belongs on a
    /// blocking thread.
    pub fn read_window(
        &self,
        id: &str,
        request: &WindowRequest,
    ) -> Result<WindowResponse, CogError> {
        let dataset = self
            .get_dataset(id)
            .ok_or_else(|| CogError::UnknownDataset { id: id.to_string() })?;
        let pixels = request
            .cols
            .checked_mul(request.rows)
            .ok_or(CogError::WindowTooLarge {
                cols: request.cols,
                rows: request.rows,
            })?;
        if pixels == 0 {
            return Err(CogError::EmptyWindow);
        }
        if pixels > MAX_WINDOW_PIXELS {
            return Err(CogError::WindowTooLarge {
                cols: request.cols,
                rows: request.rows,
            });
        }

        let read_failed = |e: TerranoError| CogError::Read {
            href: dataset.href.clone(),
            reason: e.to_string(),
        };
        let mut source = RangeSource::open(&dataset.href)?;
        let mut reader = CogReader::open(&mut source).map_err(read_failed)?;
        let bands = reader
            .read_window_bands(
                request.level,
                request.col,
                request.row,
                request.cols,
                request.rows,
            )
            .map_err(read_failed)?;

        Ok(WindowResponse {
            id: dataset.id.clone(),
            level: request.level,
            col: request.col,
            row: request.row,
            cols: request.cols,
            rows: request.rows,
            bands: bands
                .bands()
                .iter()
                .map(|band| band.data().to_vec())
                .collect(),
        })
    }
}

/// The dataset id an href is served under: its filename without the extension.
fn source_id(href: &str) -> Option<String> {
    let path = href.split(['?', '#']).next().unwrap_or(href);
    let name = path.rsplit(['/', '\\']).next()?;
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    (!stem.is_empty()).then(|| stem.to_string())
}

fn is_remote(href: &str) -> bool {
    href.starts_with("http://") || href.starts_with("https://")
}

/// Open an href and read the layout its header declares.
fn open_dataset(id: &str, href: &str) -> Result<CogDataset, CogError> {
    let open_failed = |reason: String| CogError::Open {
        href: href.to_string(),
        reason,
    };
    let mut source = RangeSource::open(href)?;
    // the reader borrows the source rather than taking it, so the size the range
    // answers reported is still readable once the reader is done
    let (levels, meta) = {
        let reader = CogReader::open(&mut source).map_err(|e| open_failed(e.to_string()))?;
        (reader.levels().to_vec(), reader.meta().clone())
    };
    let file_size_bytes = source.size().map_err(open_failed)?;

    let full = &levels[0];
    Ok(CogDataset {
        id: id.to_string(),
        href: href.to_string(),
        file_size_bytes,
        width: full.width as u32,
        height: full.height as u32,
        band_count: full.samples,
        crs: format!("EPSG:{}", meta.epsg),
        bounds: [
            meta.origin_x,
            meta.origin_y - full.height as f64 * meta.pixel_height,
            meta.origin_x + full.width as f64 * meta.pixel_width,
            meta.origin_y,
        ],
        pixel_size: [meta.pixel_width, meta.pixel_height],
        tile_size: [full.tile_width as u32, full.tile_height as u32],
        levels: levels
            .iter()
            .enumerate()
            .map(|(level, l)| CogLevelInfo {
                level,
                width: l.width as u32,
                height: l.height as u32,
                pixel_size: [l.pixel_width, l.pixel_height],
            })
            .collect(),
    })
}

/// Byte-range access to a registered source: a local file by seek, a remote one
/// by HTTP `Range` request.
enum RangeSource {
    File(std::fs::File),
    Http {
        url: String,
        /// Total size, as the `Content-Range` of the last answer reported it.
        total_bytes: Option<u64>,
    },
}

impl RangeSource {
    fn open(href: &str) -> Result<Self, CogError> {
        if is_remote(href) {
            return Ok(RangeSource::Http {
                url: href.to_string(),
                total_bytes: None,
            });
        }
        std::fs::File::open(href)
            .map(RangeSource::File)
            .map_err(|e| CogError::Open {
                href: href.to_string(),
                reason: e.to_string(),
            })
    }

    /// Size of the whole file. A remote source only knows it once it has read
    /// something.
    fn size(&self) -> Result<u64, String> {
        match self {
            RangeSource::File(file) => file.metadata().map(|m| m.len()).map_err(|e| e.to_string()),
            RangeSource::Http { total_bytes, .. } => {
                total_bytes.ok_or_else(|| "no Content-Range was seen".to_string())
            }
        }
    }

    fn fetch(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, TerranoError> {
        match self {
            RangeSource::File(file) => {
                use std::io::{Read, Seek, SeekFrom};
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; len as usize];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
            RangeSource::Http { url, total_bytes } => {
                let (bytes, total) = fetch_range(url, offset, len)?;
                *total_bytes = total.or(*total_bytes);
                Ok(bytes)
            }
        }
    }
}

// the reader takes its source by value, so the impl is on the reference and
// registration can still ask the source what it learned
impl RangeRead for &mut RangeSource {
    fn read_range(&mut self, offset: u64, len: u64) -> Result<Vec<u8>, TerranoError> {
        self.fetch(offset, len)
    }
}

/// Shared blocking client. Every call sits on a blocking thread, which is what
/// `reqwest::blocking` requires, and one client keeps one connection pool.
fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(concat!("tiletopia/", env!("CARGO_PKG_VERSION")))
            .timeout(RANGE_TIMEOUT)
            .build()
            .expect("rustls http client")
    })
}

/// One HTTP range request, answering the bytes and the total file size the
/// `Content-Range` reported.
fn fetch_range(url: &str, offset: u64, len: u64) -> Result<(Vec<u8>, Option<u64>), TerranoError> {
    let last = offset + len - 1;
    let format_error = |detail: String| TerranoError::Format(format!("{url}: {detail}"));
    let response = http_client()
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
        .send()
        .map_err(|e| format_error(e.to_string()))?;

    // a 200 here means the whole file, which is the answer of a host that does
    // not do ranges, and reading a cog off one would pull every byte per tile
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format_error(format!(
            "answered {} to a range request, serving a COG needs a host that honours Range",
            response.status()
        )));
    }
    let total_bytes = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(content_range_total);
    let bytes = response
        .bytes()
        .map_err(|e| format_error(e.to_string()))?
        .to_vec();
    if bytes.len() as u64 != len {
        return Err(format_error(format!(
            "answered {} bytes for a range of {len}",
            bytes.len()
        )));
    }
    Ok((bytes, total_bytes))
}

/// The total size out of a `Content-Range: bytes 0-7/12345`.
fn content_range_total(header: &str) -> Option<u64> {
    header.rsplit_once('/')?.1.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use terrano_core::{CogParams, Raster, SampleFormat, write_cog};

    /// A 96-square ramp written as a real deflate COG with 32-pixel internal
    /// tiles and two overviews, so a window read has tiles to pick between.
    fn sample_cog() -> (Vec<u8>, Raster) {
        let (width, height) = (96usize, 96usize);
        let data: Vec<f64> = (0..width * height).map(|i| i as f64).collect();
        let raster = Raster::from_vec(width, height, data, 1.0, f64::NAN).unwrap();
        let params = CogParams {
            tile_width: 32,
            tile_height: 32,
            overview_levels: 2,
            epsg: 32610,
            origin_x: 500_000.0,
            origin_y: 4_180_000.0,
            pixel_width: 10.0,
            pixel_height: 10.0,
            deflate: true,
            format: SampleFormat::F64,
            nodata: None,
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        write_cog(&raster, &params, &mut bytes).unwrap();
        (bytes.into_inner(), raster)
    }

    fn write_sample_cog(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, Raster) {
        let (bytes, raster) = sample_cog();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        (path, raster)
    }

    /// The `(offset, length)` of every range the served file was asked for.
    type AskedRanges = std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>;

    /// Serve one COG over loopback with real `Range` support, recording the
    /// ranges asked for so a windowed read can be told from a whole-file read.
    async fn serve_cog(bytes: Vec<u8>) -> (String, AskedRanges) {
        use axum::http::{HeaderMap, StatusCode, header};
        use std::sync::{Arc, Mutex};

        let ranges: AskedRanges = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&ranges);
        let app = axum::Router::new().route(
            "/sample.tif",
            axum::routing::get(move |headers: HeaderMap| {
                let bytes = bytes.clone();
                let recorder = Arc::clone(&recorder);
                async move {
                    let Some((first, last)) = headers
                        .get(header::RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_range_header)
                    else {
                        return (StatusCode::OK, HeaderMap::new(), bytes);
                    };
                    recorder.lock().unwrap().push((first, last - first + 1));
                    let mut out = HeaderMap::new();
                    out.insert(
                        header::CONTENT_RANGE,
                        format!("bytes {first}-{last}/{}", bytes.len())
                            .parse()
                            .unwrap(),
                    );
                    (
                        StatusCode::PARTIAL_CONTENT,
                        out,
                        bytes[first as usize..=last as usize].to_vec(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/sample.tif"), ranges)
    }

    fn parse_range_header(header: &str) -> Option<(u64, u64)> {
        let (first, last) = header.strip_prefix("bytes=")?.split_once('-')?;
        Some((first.parse().ok()?, last.parse().ok()?))
    }

    #[test]
    fn an_empty_registry_serves_nothing() {
        let engine = CogEngine::new();
        assert!(engine.list_datasets().is_empty());
        assert!(engine.get_dataset("anything").is_none());
    }

    #[test]
    fn no_sources_configured_registers_nothing() {
        let engine = CogEngine::from_sources(["", "  "]).unwrap();
        assert!(engine.list_datasets().is_empty());
    }

    #[test]
    fn a_window_of_an_unregistered_dataset_is_a_404() {
        let engine = CogEngine::new();
        let request = WindowRequest {
            level: 0,
            col: 0,
            row: 0,
            cols: 4,
            rows: 4,
        };
        let err = engine.read_window("missing", &request).unwrap_err();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_local_cog_registers_the_layout_its_header_declares() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_sample_cog(dir.path(), "ramp.tif");
        let href = path.to_str().unwrap();

        let engine = CogEngine::from_sources([href]).unwrap();
        let dataset = engine.get_dataset("ramp").expect("keyed by filename stem");
        assert_eq!(dataset.width, 96);
        assert_eq!(dataset.height, 96);
        assert_eq!(dataset.band_count, 1);
        assert_eq!(dataset.tile_size, [32, 32]);
        assert_eq!(dataset.crs, "EPSG:32610");
        assert_eq!(dataset.pixel_size, [10.0, 10.0]);
        assert_eq!(
            dataset.bounds,
            [500_000.0, 4_180_000.0 - 960.0, 500_960.0, 4_180_000.0]
        );
        assert_eq!(
            dataset.file_size_bytes,
            std::fs::metadata(&path).unwrap().len()
        );
        // full resolution plus the two overviews the writer generated
        assert_eq!(dataset.levels.len(), 3);
        assert_eq!(dataset.levels[1].width, 48);
        assert_eq!(dataset.levels[2].pixel_size, [40.0, 40.0]);
    }

    #[test]
    fn a_local_window_read_answers_the_pixels_written() {
        let dir = tempfile::tempdir().unwrap();
        let (path, raster) = write_sample_cog(dir.path(), "ramp.tif");
        let engine = CogEngine::from_sources([path.to_str().unwrap()]).unwrap();

        let request = WindowRequest {
            level: 0,
            col: 40,
            row: 70,
            cols: 5,
            rows: 3,
        };
        let window = engine.read_window("ramp", &request).unwrap();
        assert_eq!(window.bands.len(), 1);
        let pixels = raster.data();
        let expected: Vec<f64> = (0..3)
            .flat_map(|r| (0..5).map(move |c| pixels[(70 + r) * 96 + 40 + c]))
            .collect();
        assert_eq!(window.bands[0], expected);
    }

    #[test]
    fn a_missing_local_source_stops_the_server() {
        let err = CogEngine::from_sources(["/nonexistent/ramp.tif"]).unwrap_err();
        assert!(err.contains(SOURCES_ENV), "{err}");
        assert!(err.contains("could not be opened"), "{err}");
    }

    #[test]
    fn a_local_file_that_is_not_a_cog_stops_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ramp.tif");
        std::fs::write(&path, b"not a tiff at all").unwrap();
        let err = CogEngine::from_sources([path.to_str().unwrap()]).unwrap_err();
        assert!(err.contains("could not be opened"), "{err}");
    }

    #[test]
    fn two_sources_with_one_id_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_sample_cog(dir.path(), "ramp.tif");
        let href = path.to_str().unwrap();
        let err = CogEngine::from_sources([href, href]).unwrap_err();
        assert!(err.contains("unique"), "{err}");
    }

    #[test]
    fn a_window_past_the_cap_is_refused_before_any_read() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_sample_cog(dir.path(), "ramp.tif");
        let engine = CogEngine::from_sources([path.to_str().unwrap()]).unwrap();

        let too_big = WindowRequest {
            level: 0,
            col: 0,
            row: 0,
            cols: 1024,
            rows: 1024,
        };
        assert_eq!(
            engine.read_window("ramp", &too_big).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        let empty = WindowRequest {
            level: 0,
            col: 0,
            row: 0,
            cols: 0,
            rows: 8,
        };
        assert_eq!(
            engine.read_window("ramp", &empty).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_level_the_file_does_not_have_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = write_sample_cog(dir.path(), "ramp.tif");
        let engine = CogEngine::from_sources([path.to_str().unwrap()]).unwrap();
        let request = WindowRequest {
            level: 9,
            col: 0,
            row: 0,
            cols: 2,
            rows: 2,
        };
        let err = engine.read_window("ramp", &request).unwrap_err();
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_content_range_total_is_read_off_the_header() {
        assert_eq!(content_range_total("bytes 0-7/12345"), Some(12345));
        assert_eq!(content_range_total("bytes */12345"), Some(12345));
        assert_eq!(content_range_total("bytes 0-7/*"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_cog_registers_and_reads_over_range_requests() {
        let (bytes, raster) = sample_cog();
        let total = bytes.len() as u64;
        let (url, ranges) = serve_cog(bytes).await;

        let engine = tokio::task::spawn_blocking({
            let url = url.clone();
            move || CogEngine::from_sources([url.as_str()])
        })
        .await
        .unwrap()
        .unwrap();

        let dataset = engine.get_dataset("sample").expect("keyed by url filename");
        assert_eq!(dataset.href, url);
        assert_eq!(dataset.width, 96);
        // the size came off Content-Range, no separate HEAD
        assert_eq!(dataset.file_size_bytes, total);

        let request = WindowRequest {
            level: 0,
            col: 33,
            row: 33,
            cols: 4,
            rows: 4,
        };
        let window = tokio::task::spawn_blocking(move || engine.read_window("sample", &request))
            .await
            .unwrap()
            .unwrap();
        let pixels = raster.data();
        let expected: Vec<f64> = (0..4)
            .flat_map(|r| (0..4).map(move |c| pixels[(33 + r) * 96 + 33 + c]))
            .collect();
        assert_eq!(window.bands[0], expected);

        // one internal tile covers that window, so opening and reading it cost
        // less than the file: header reads plus that tile, never the whole thing
        let asked = ranges.lock().unwrap().clone();
        assert!(!asked.is_empty(), "the reader made no range request");
        let fetched: u64 = asked.iter().map(|&(_, len)| len).sum();
        assert!(
            fetched < total,
            "fetched {fetched} bytes of a {total} byte file for a 4x4 window"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_remote_source_is_skipped_rather_than_fatal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/sample.tif");

        let engine = tokio::task::spawn_blocking(move || CogEngine::from_sources([url.as_str()]))
            .await
            .unwrap()
            .unwrap();
        assert!(engine.list_datasets().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_that_ignores_range_is_refused() {
        // this handler answers 200 and the whole file whatever Range it is sent,
        // which is what a plain object host does, and reading tiles off one would
        // pull the file per tile
        let (bytes, _) = sample_cog();
        let app = axum::Router::new().route(
            "/sample.tif",
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{addr}/sample.tif");

        let engine = tokio::task::spawn_blocking(move || CogEngine::from_sources([url.as_str()]))
            .await
            .unwrap()
            .unwrap();
        assert!(engine.list_datasets().is_empty());
    }
}
