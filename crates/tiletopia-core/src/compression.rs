//! Mesh compression and optimization using meshopt.
//!
//! Optimizes vertex/index buffers for GPU rendering efficiency.

/// Optimize a mesh's index and vertex buffers for GPU performance.
///
/// Applies: vertex cache optimization, overdraw optimization,
/// and vertex fetch optimization — in that order, as recommended by meshoptimizer.
pub fn optimize_mesh(vertices: &mut Vec<[f32; 3]>, indices: &mut Vec<u32>) {
    if indices.is_empty() || vertices.is_empty() {
        return;
    }

    // Vertex cache optimization — reorders triangles for better post-transform cache hit rate.
    meshopt::optimize_vertex_cache_in_place(indices, vertices.len());

    // Overdraw optimization — reorders triangles to reduce pixel overdraw.
    let vertex_adapter = meshopt::VertexDataAdapter::new(
        bytemuck::cast_slice(vertices),
        std::mem::size_of::<[f32; 3]>(),
        0,
    )
    .expect("valid vertex layout");
    meshopt::optimize_overdraw_in_place(indices, &vertex_adapter, 1.05);

    // Vertex fetch optimization — reorders vertices for better memory access patterns.
    let remap = meshopt::optimize_vertex_fetch_remap(indices, vertices.len());
    *indices = meshopt::remap_index_buffer(Some(indices), vertices.len(), &remap);
    *vertices = meshopt::remap_vertex_buffer(vertices, vertices.len(), &remap);
}

/// Simplify a mesh by reducing triangle count while preserving shape.
///
/// `target_ratio` is the fraction of triangles to keep (0.0–1.0).
pub fn simplify_mesh(
    vertices: &[[f32; 3]],
    indices: &[u32],
    target_ratio: f32,
) -> Vec<u32> {
    let target_count = ((indices.len() / 3) as f32 * target_ratio) as usize * 3;
    let vertex_adapter = meshopt::VertexDataAdapter::new(
        bytemuck::cast_slice(vertices),
        std::mem::size_of::<[f32; 3]>(),
        0,
    )
    .expect("valid vertex layout");
    meshopt::simplify(indices, &vertex_adapter, target_count, 1e-2, meshopt::SimplifyOptions::None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimize_empty() {
        let mut verts: Vec<[f32; 3]> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        optimize_mesh(&mut verts, &mut indices);
        assert!(verts.is_empty());
    }

    #[test]
    fn test_optimize_single_triangle() {
        let mut verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let mut indices = vec![0, 1, 2];
        optimize_mesh(&mut verts, &mut indices);
        assert_eq!(indices.len(), 3);
        assert_eq!(verts.len(), 3);
    }
}
