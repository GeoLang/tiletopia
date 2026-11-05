use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use tiletopia_core::mesh_tiler::{MeshData, MeshTilingConfig, tile_meshes};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

fn record_growth(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_LIVE_BYTES.fetch_max(live, Ordering::Relaxed);
}

struct PeakTrackingAllocator;

unsafe impl GlobalAlloc for PeakTrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_growth(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_growth(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            record_growth(new_size);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: PeakTrackingAllocator = PeakTrackingAllocator;

fn grid_mesh(quads_per_side: u32) -> MeshData {
    let stride = quads_per_side + 1;
    let positions: Vec<[f64; 3]> = (0..stride)
        .flat_map(|y| (0..stride).map(move |x| [f64::from(x), f64::from(y), 0.0]))
        .collect();
    let mut indices = Vec::new();
    for y in 0..quads_per_side {
        for x in 0..quads_per_side {
            let near = y * stride + x;
            let far = near + stride;
            indices.extend_from_slice(&[near, near + 1, far, near + 1, far + 1, far]);
        }
    }
    MeshData {
        normals: vec![[0.0, 0.0, 1.0]; positions.len()],
        positions,
        texcoords: Vec::new(),
        indices,
        name: "grid".into(),
        asset_id: None,
        class: None,
        storey: None,
        base_color_factor: None,
        texture: None,
    }
}

#[test]
fn tiling_holds_one_root_to_leaf_path_of_tile_meshes_at_a_time() {
    let mesh = grid_mesh(128);
    let input_bytes = std::mem::size_of_val(mesh.positions.as_slice())
        + std::mem::size_of_val(mesh.normals.as_slice())
        + std::mem::size_of_val(mesh.indices.as_slice());
    let dir = std::env::temp_dir().join(format!("tiletopia_peak_memory_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = MeshTilingConfig {
        max_triangles_per_tile: 16,
        ..Default::default()
    };

    let live_before = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_LIVE_BYTES.store(live_before, Ordering::Relaxed);
    let stats =
        tile_meshes(std::slice::from_ref(&mesh), &dir, &config).expect("tiling should succeed");
    let peak_growth = PEAK_LIVE_BYTES.load(Ordering::Relaxed) - live_before;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        stats.tile_count > 1000,
        "the grid should split ten levels deep, got {} tiles",
        stats.tile_count
    );
    assert!(
        peak_growth < 6 * input_bytes,
        "tiling held {peak_growth} bytes at its peak for {input_bytes} bytes of input"
    );
}
