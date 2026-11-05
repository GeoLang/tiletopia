mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, token};
use terrano_core::{CogParams, CogReader, SampleFormat, write_cog_tiles};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;

struct PeakCountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for PeakCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

// cargo builds each tests file as its own binary, so only this file's one test reaches the peak
#[global_allocator]
static ALLOCATOR: PeakCountingAllocator = PeakCountingAllocator;

const SIDE: usize = 6000;
const TILE_SIDE: usize = 256;

fn sample(col: usize, row: usize) -> u8 {
    ((col / 61 * 3 + row / 67 * 5) % 256) as u8
}

fn tiled_gray_tiff() -> Vec<u8> {
    let params = CogParams {
        tile_width: TILE_SIDE as u32,
        tile_height: TILE_SIDE as u32,
        overview_levels: 0,
        epsg: 4326,
        origin_x: 7.0,
        origin_y: 47.0,
        pixel_width: 0.0001,
        pixel_height: 0.0001,
        deflate: true,
        nodata: None,
        format: SampleFormat::U8,
    };
    let mut file = Cursor::new(Vec::new());
    write_cog_tiles(
        SIDE,
        SIDE,
        1,
        &params,
        |_, tile_col, tile_row| {
            let mut tile = Vec::with_capacity(TILE_SIDE * TILE_SIDE);
            for row in tile_row * TILE_SIDE..(tile_row + 1) * TILE_SIDE {
                for col in tile_col * TILE_SIDE..(tile_col + 1) * TILE_SIDE {
                    tile.push(sample(col, row));
                }
            }
            Ok(tile)
        },
        &mut file,
    )
    .unwrap();
    file.into_inner()
}

async fn upload_as_imagery(state: &Arc<AppState>, file: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let boundary = "tiletopiamemoryboundary";
    let mut body: Vec<u8> = Vec::new();
    for (field, filename, value) in [
        ("asset_type", None, b"imagery".to_vec()),
        ("file", Some("large.tif"), file),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"").as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(&value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/assets")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(
            "authorization",
            format!("Bearer {}", token("imagery-owner", "editor")),
        )
        .body(Body::from(body))
        .unwrap();
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn a_tiled_tiff_far_past_the_old_decode_limit_decodes_in_a_fraction_of_its_size() {
    let state = test_state().await;
    let file = tiled_gray_tiff();
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    let (status, body) = upload_as_imagery(&state, file).await;
    let peak_growth = PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let decoded_bytes = SIDE * SIDE;
    assert!(
        peak_growth < decoded_bytes / 4,
        "the upload held {peak_growth} bytes at its peak for a {decoded_bytes} byte image"
    );

    let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(asset["asset_type"], "imagery");
    assert_eq!(asset["status"], "ready");
    let imagery = state
        .data_dir
        .join(asset["id"].as_str().unwrap())
        .join("imagery");
    let scratch_files: Vec<String> = std::fs::read_dir(&imagery)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".raw"))
        .collect();
    assert!(scratch_files.is_empty(), "{scratch_files:?}");

    let cog = std::fs::File::open(imagery.join("source.tif")).unwrap();
    let reader = CogReader::open(cog).unwrap();
    // 6000 px down to at most 512 px a side in four halvings
    assert_eq!(reader.levels().len(), 5);
    for (col, row) in [(0, 0), (255, 256), (4097, 1234), (SIDE - 1, SIDE - 1)] {
        let pixel = reader.read_window(0, col, row, 1, 1).unwrap();
        assert_eq!(pixel.data()[0], f64::from(sample(col, row)), "{col},{row}");
    }
}
