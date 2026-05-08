//! GPU-accelerated tiling via wgpu compute shaders.
//!
//! Provides GPU-based point cloud decimation and spatial hashing.
//! Falls back to CPU (Rayon) when no GPU is available.
//!
//! Enable with `--features gpu`.

#[cfg(feature = "gpu")]
use wgpu;

/// GPU device context for compute operations.
#[cfg(feature = "gpu")]
pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

#[cfg(feature = "gpu")]
impl GpuContext {
    /// Initialize GPU context. Returns None if no suitable GPU is found.
    pub fn new() -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("TileTopia GPU"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;
        Some(Self { device, queue })
    }

    /// Run a compute shader that computes Morton codes for spatial hashing.
    ///
    /// Input: N points as [f32; 3] (flattened to 3N f32s).
    /// Output: N u32 Morton codes.
    pub fn compute_morton_codes(&self, positions: &[[f32; 3]], cell_size: f32) -> Vec<u32> {
        let n = positions.len() as u32;
        let input_data: Vec<f32> = positions.iter().flat_map(|p| p.iter().copied()).collect();
        let input_bytes = bytemuck_cast_slice(&input_data);

        // Create buffers
        let input_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("input_positions"),
            size: input_bytes.len() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("output_morton"),
            size: (n as u64) * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: (n as u64) * 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let params_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 8,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Upload data
        self.queue.write_buffer(&input_buffer, 0, input_bytes);
        let params = [cell_size, n as f32];
        self.queue.write_buffer(&params_buffer, 0, bytemuck_cast_slice(&params));

        // Create shader module
        let shader = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("morton_shader"),
            source: wgpu::ShaderSource::Wgsl(MORTON_SHADER.into()),
        });

        // Pipeline
        let bind_group_layout = self.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("morton_pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: input_buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: output_buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: params_buffer.as_entire_binding() },
            ],
        });

        // Dispatch
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups((n + 63) / 64, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output_buffer, 0, &staging_buffer, 0, (n as u64) * 4);
        self.queue.submit([encoder.finish()]);

        // Read back
        let slice = staging_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| { tx.send(result).unwrap(); });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range();
        let result: Vec<u32> = data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        drop(data);
        staging_buffer.unmap();
        result
    }
}

/// WGSL compute shader for Morton code computation.
#[cfg(feature = "gpu")]
const MORTON_SHADER: &str = r#"
struct Params {
    cell_size: f32,
    count: f32,
}

@group(0) @binding(0) var<storage, read> positions: array<f32>;
@group(0) @binding(1) var<storage, read_write> output: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

fn expand_bits(v_in: u32) -> u32 {
    var v = v_in & 0x000003ffu;
    v = (v | (v << 16u)) & 0x030000ffu;
    v = (v | (v << 8u)) & 0x0300f00fu;
    v = (v | (v << 4u)) & 0x030c30c3u;
    v = (v | (v << 2u)) & 0x09249249u;
    return v;
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let idx = id.x;
    if (idx >= u32(params.count)) {
        return;
    }
    let base = idx * 3u;
    let x = positions[base];
    let y = positions[base + 1u];
    let z = positions[base + 2u];
    let cx = u32(floor(x / params.cell_size));
    let cy = u32(floor(y / params.cell_size));
    let cz = u32(floor(z / params.cell_size));
    output[idx] = expand_bits(cx) | (expand_bits(cy) << 1u) | (expand_bits(cz) << 2u);
}
"#;

#[cfg(feature = "gpu")]
fn bytemuck_cast_slice(data: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4) }
}

// ─── CPU fallback (always available) ───────────────────────────────────────────

/// Whether GPU acceleration is available at runtime.
pub fn gpu_available() -> bool {
    #[cfg(feature = "gpu")]
    { GpuContext::new().is_some() }
    #[cfg(not(feature = "gpu"))]
    { false }
}

/// GPU-accelerated point decimation.
///
/// Strategy: compute Morton codes (GPU), sort points by spatial locality,
/// then take uniformly-spaced samples. This gives spatially-uniform decimation
/// unlike naive stride sampling on unsorted data.
pub fn decimate_points_gpu(
    positions: &[[f32; 3]],
    target_count: usize,
) -> Vec<[f32; 3]> {
    if positions.len() <= target_count {
        return positions.to_vec();
    }

    // Compute spatial hash (uses GPU when available)
    let cell_size = estimate_cell_size(positions, target_count);
    let morton_codes = spatial_hash_gpu(positions, cell_size);

    // Sort indices by Morton code for spatial locality
    let mut indices: Vec<usize> = (0..positions.len()).collect();
    indices.sort_unstable_by_key(|&i| morton_codes[i]);

    // Stride sample from spatially-sorted points → uniform spatial distribution
    let stride = std::cmp::max(1, positions.len() / target_count);
    indices.iter().step_by(stride).map(|&i| positions[i]).collect()
}

/// Estimate a good cell size for the given decimation ratio.
fn estimate_cell_size(positions: &[[f32; 3]], target_count: usize) -> f32 {
    if positions.is_empty() { return 1.0; }
    // Compute bounding box extent
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for p in positions {
        for i in 0..3 {
            min[i] = min[i].min(p[i]);
            max[i] = max[i].max(p[i]);
        }
    }
    let extent = ((max[0] - min[0]) * (max[1] - min[1]) * (max[2] - min[2])).cbrt();
    // Cell size such that ~target_count cells cover the volume
    let cells_per_dim = (target_count as f32).cbrt();
    (extent / cells_per_dim).max(0.001)
}

/// Compute spatial hash (Morton codes) for point cloud.
///
/// When GPU feature is enabled and hardware is available, runs on GPU.
/// Otherwise uses CPU.
pub fn spatial_hash_gpu(
    positions: &[[f32; 3]],
    cell_size: f32,
) -> Vec<u32> {
    #[cfg(feature = "gpu")]
    {
        if let Some(ctx) = GpuContext::new() {
            return ctx.compute_morton_codes(positions, cell_size);
        }
    }
    // CPU fallback
    positions.iter().map(|p| {
        let cx = (p[0] / cell_size).floor() as u32;
        let cy = (p[1] / cell_size).floor() as u32;
        let cz = (p[2] / cell_size).floor() as u32;
        morton_encode(cx, cy, cz)
    }).collect()
}

/// Encode 3D coordinates to Morton code (Z-order curve).
pub fn morton_encode(x: u32, y: u32, z: u32) -> u32 {
    fn expand_bits(v: u32) -> u32 {
        let mut v = v & 0x000003ff;
        v = (v | (v << 16)) & 0x030000ff;
        v = (v | (v << 8)) & 0x0300f00f;
        v = (v | (v << 4)) & 0x030c30c3;
        v = (v | (v << 2)) & 0x09249249;
        v
    }
    expand_bits(x) | (expand_bits(y) << 1) | (expand_bits(z) << 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decimate_cpu_fallback() {
        let points: Vec<[f32; 3]> = (0..1000).map(|i| [i as f32, 0.0, 0.0]).collect();
        let result = decimate_points_gpu(&points, 100);
        assert!(result.len() <= 110);
        assert!(result.len() >= 90);
    }

    #[test]
    fn test_spatial_hash() {
        let points = vec![[0.0f32, 0.0, 0.0], [1.0, 1.0, 1.0], [5.0, 5.0, 5.0]];
        let hashes = spatial_hash_gpu(&points, 2.0);
        assert_eq!(hashes.len(), 3);
        assert_eq!(hashes[0], morton_encode(0, 0, 0));
    }

    #[test]
    fn test_morton_encode() {
        assert_eq!(morton_encode(0, 0, 0), 0);
        assert_eq!(morton_encode(1, 0, 0), 1);
        assert_eq!(morton_encode(0, 1, 0), 2);
        assert_eq!(morton_encode(0, 0, 1), 4);
    }

    #[test]
    fn test_gpu_available() {
        let _ = gpu_available();
    }
}
