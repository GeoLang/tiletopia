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
pub fn simplify_mesh(vertices: &[[f32; 3]], indices: &[u32], target_ratio: f32) -> Vec<u32> {
    let target_count = ((indices.len() / 3) as f32 * target_ratio) as usize * 3;
    let vertex_adapter = meshopt::VertexDataAdapter::new(
        bytemuck::cast_slice(vertices),
        std::mem::size_of::<[f32; 3]>(),
        0,
    )
    .expect("valid vertex layout");
    meshopt::simplify(
        indices,
        &vertex_adapter,
        target_count,
        1e-2,
        meshopt::SimplifyOptions::None,
        None,
    )
}

/// Encode mesh geometry (vertices + indices) to Draco compressed bytes.
///
/// Uses Draco's point cloud compression for vertex positions and stores
/// indices alongside the compressed data.
#[cfg(feature = "draco")]
pub fn draco_encode_mesh(vertices: &[[f32; 3]], indices: &[u32]) -> Result<Vec<u8>, String> {
    use draco_rs::pointcloud::PointCloudBuilder;
    use draco_rs::prelude::*;

    let mut builder = PointCloudBuilder::new(vertices.len() as u32);
    let attr_id = builder.add_attribute(
        ffi::draco::GeometryAttribute_Type::POSITION,
        3,
        ffi::draco::DataType::DT_FLOAT32,
    );

    for (i, v) in vertices.iter().enumerate() {
        builder.add_point(attr_id, i, v.as_slice());
    }

    let pc = builder.build(false);
    let mut encoder = Encoder::new()
        .set_speed_options(5, 5)
        .set_attribute_quantization(ffi::draco::GeometryAttribute_Type::POSITION, 14);

    let buffer = pc
        .to_buffer(&mut encoder)
        .map_err(|e| format!("Draco encode error: {e:?}"))?;

    let draco_bytes = buffer.as_slice();
    let mut out = Vec::with_capacity(8 + indices.len() * 4 + draco_bytes.len());
    out.extend_from_slice(&(vertices.len() as u32).to_le_bytes());
    out.extend_from_slice(&(indices.len() as u32).to_le_bytes());
    for idx in indices {
        out.extend_from_slice(&idx.to_le_bytes());
    }
    out.extend_from_slice(draco_bytes);
    Ok(out)
}

/// Decode Draco compressed bytes back to mesh geometry (vertices + indices).
#[cfg(feature = "draco")]
pub fn draco_decode_mesh(data: &[u8]) -> Result<(Vec<[f32; 3]>, Vec<u32>), String> {
    use draco_rs::pointcloud::PointCloud;
    use draco_rs::prelude::*;

    if data.len() < 8 {
        return Err("Draco data too short".to_string());
    }

    let _num_vertices = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let num_indices = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;

    let indices_end = 8 + num_indices * 4;
    if data.len() < indices_end {
        return Err("Draco data truncated (indices)".to_string());
    }

    let mut indices = Vec::with_capacity(num_indices);
    for i in 0..num_indices {
        let offset = 8 + i * 4;
        indices.push(u32::from_le_bytes(
            data[offset..offset + 4].try_into().unwrap(),
        ));
    }

    let draco_data = &data[indices_end..];
    let mut buf = DecoderBuffer::from_buffer(draco_data);
    let mut decoder = Decoder::new();
    let mut pc = PointCloud::from_buffer(&mut decoder, &mut buf)
        .map_err(|e| format!("Draco decode error: {e:?}"))?;

    let n = pc.num_points() as usize;
    let mut vertices = Vec::with_capacity(n);

    // Position is the first (and only) attribute, so its ID matches what was used during encoding.
    let attr_id = draco_rs::prelude::AttrId(0);
    for i in 0..n {
        let mut pos = [0.0f32; 3];
        pc.get_point(attr_id, i as u32, &mut pos);
        vertices.push(pos);
    }

    Ok((vertices, indices))
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

    #[cfg(feature = "draco")]
    #[test]
    fn test_draco_roundtrip() {
        let vertices = vec![
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
        ];
        let indices = vec![0u32, 1, 2, 1, 3, 2];

        let encoded = draco_encode_mesh(&vertices, &indices).expect("encode should succeed");
        assert!(!encoded.is_empty());

        let (dec_verts, dec_indices) = draco_decode_mesh(&encoded).expect("decode should succeed");
        assert_eq!(dec_verts.len(), vertices.len());
        assert_eq!(dec_indices.len(), indices.len());
    }
}
