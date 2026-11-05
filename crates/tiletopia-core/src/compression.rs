//! Mesh compression and optimization using meshopt.
//!
//! Optimizes vertex/index buffers for GPU rendering efficiency.

use crate::glb_writer::GlbMesh;

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
    mesh_longest_side: f32,
) -> Vec<u32> {
    let target_count = ((indices.len() / 3) as f32 * target_ratio) as usize * 3;
    let vertex_adapter = meshopt::VertexDataAdapter::new(
        bytemuck::cast_slice(vertices),
        std::mem::size_of::<[f32; 3]>(),
        0,
    )
    .expect("valid vertex layout");
    // meshopt measures a sparse simplify's relative error against the indexed vertices only
    meshopt::simplify(
        indices,
        &vertex_adapter,
        target_count,
        1e-2 * mesh_longest_side,
        meshopt::SimplifyOptions::Sparse | meshopt::SimplifyOptions::ErrorAbsolute,
        None,
    )
}

fn in_first_use_order<T: Copy>(values: &[T], first_use_order: &[usize]) -> Vec<T> {
    first_use_order.iter().map(|&old| values[old]).collect()
}

fn reorder_indices_by_first_use(indices: &mut Vec<u32>, vertex_count: usize) -> Vec<usize> {
    *indices = meshopt::optimize_vertex_cache(indices, vertex_count);

    // meshopt::optimize_vertex_fetch_remap returns a table shorter than the vertex count when vertices go unused
    let mut new_indices = vec![None; vertex_count];
    let mut first_use_order = Vec::new();
    for index in indices.iter_mut() {
        let new_index = new_indices[*index as usize].get_or_insert_with(|| {
            first_use_order.push(*index as usize);
            first_use_order.len() as u32 - 1
        });
        *index = *new_index;
    }
    first_use_order
}

pub fn reorder_vertices_for_meshopt(mesh: &mut GlbMesh) {
    if let Some(indices) = &mut mesh.indices {
        let first_use_order = reorder_indices_by_first_use(indices, mesh.positions.len());
        mesh.positions = in_first_use_order(&mesh.positions, &first_use_order);
        mesh.normals = mesh
            .normals
            .as_ref()
            .map(|values| in_first_use_order(values, &first_use_order));
        mesh.colors = mesh
            .colors
            .as_ref()
            .map(|values| in_first_use_order(values, &first_use_order));
        mesh.texcoords = mesh
            .texcoords
            .as_ref()
            .map(|values| in_first_use_order(values, &first_use_order));
        mesh.feature_ids = mesh
            .feature_ids
            .as_ref()
            .map(|values| in_first_use_order(values, &first_use_order));
    }
    for instanced in &mut mesh.instanced_meshes {
        let first_use_order =
            reorder_indices_by_first_use(&mut instanced.indices, instanced.positions.len());
        instanced.positions = in_first_use_order(&instanced.positions, &first_use_order);
        instanced.normals = instanced
            .normals
            .as_ref()
            .map(|values| in_first_use_order(values, &first_use_order));
    }
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
    fn sparse_simplify_of_a_corner_keeps_the_dense_triangle_count() {
        let side = 64u32;
        let stride = side + 1;
        let positions: Vec<[f32; 3]> = (0..stride)
            .flat_map(|y| {
                (0..stride).map(move |x| {
                    let (x, y) = (x as f32, y as f32);
                    [x, y, 2.0 * (x * 0.8).sin() * (y * 0.8).sin()]
                })
            })
            .collect();
        let corner_indices: Vec<u32> = (0..side / 4)
            .flat_map(|y| {
                (0..side / 4).flat_map(move |x| {
                    let near = y * stride + x;
                    let far = near + stride;
                    [near, near + 1, far, near + 1, far + 1, far]
                })
            })
            .collect();
        let ratio = 0.05;

        let sparse = simplify_mesh(&positions, &corner_indices, ratio, side as f32);

        let vertex_adapter = meshopt::VertexDataAdapter::new(
            bytemuck::cast_slice(&positions),
            std::mem::size_of::<[f32; 3]>(),
            0,
        )
        .unwrap();
        let target_count = ((corner_indices.len() / 3) as f32 * ratio) as usize * 3;
        let dense = meshopt::simplify(
            &corner_indices,
            &vertex_adapter,
            target_count,
            1e-2,
            meshopt::SimplifyOptions::None,
            None,
        );
        assert!(
            dense.len() > target_count,
            "the error limit should stop the dense simplify before its target"
        );
        assert!(
            sparse.len().abs_diff(dense.len()) * 20 <= dense.len(),
            "sparse kept {} indices, dense kept {}",
            sparse.len(),
            dense.len()
        );
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
