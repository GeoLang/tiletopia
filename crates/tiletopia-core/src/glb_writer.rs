//! GLB (binary glTF 2.0) tile writer for mesh data.
//!
//! Produces spec-compliant GLB files suitable for 3D Tiles mesh content.
//! See: <https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#glb-file-format-specification>

use std::io::{self, Write};
use std::path::Path;

const GLB_MAGIC: u32 = 0x46546C67; // "glTF"
const GLB_VERSION: u32 = 2;
const CHUNK_TYPE_JSON: u32 = 0x4E4F534A; // "JSON"
const CHUNK_TYPE_BIN: u32 = 0x004E4942; // "BIN\0"

const COMPONENT_FLOAT: u32 = 5126;
const COMPONENT_UNSIGNED_INT: u32 = 5125;
const COMPONENT_UNSIGNED_BYTE: u32 = 5121;

/// Mesh data for GLB output.
pub struct GlbMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub indices: Option<Vec<u32>>,
    pub colors: Option<Vec<[u8; 4]>>,
    pub texcoords: Option<Vec<[f32; 2]>>,
    pub metadata: Option<TileMetadata>,
    pub feature_ids: Option<Vec<u32>>,
    pub texture: Option<TextureData>,
    /// glTF `baseColorFactor`, RGBA. Written as a material of its own when the
    /// mesh has no texture.
    pub base_color_factor: Option<[f32; 4]>,
}

/// Texture image data for photogrammetry meshes.
#[derive(Debug, Clone)]
pub struct TextureData {
    pub image_data: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

/// Per-feature metadata for 3D Tiles Next (EXT_structural_metadata).
pub struct TileMetadata {
    pub class_name: String,
    pub properties: Vec<MetadataProperty>,
}

pub struct MetadataProperty {
    pub name: String,
    pub values: MetadataValues,
}

pub enum MetadataValues {
    String(Vec<String>),
    Float32(Vec<f32>),
    Int32(Vec<i32>),
    Uint8(Vec<u8>),
}

struct BufferLayout {
    positions_offset: usize,
    positions_len: usize,
    normals_offset: usize,
    normals_len: usize,
    indices_offset: usize,
    indices_len: usize,
    colors_offset: usize,
    colors_len: usize,
    texcoords_offset: usize,
    texcoords_len: usize,
    feature_ids_offset: usize,
    feature_ids_len: usize,
    texture_offset: usize,
    texture_len: usize,
    metadata_segments: Vec<MetadataBufferSegment>,
    total_len: usize,
}

/// A segment of the binary buffer used for metadata property values.
struct MetadataBufferSegment {
    offset: usize,
    len: usize,
    /// For string properties: offset/length of the string-offsets array.
    string_offsets_offset: usize,
    string_offsets_len: usize,
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn compute_layout(mesh: &GlbMesh) -> BufferLayout {
    let mut offset = 0usize;

    let positions_offset = offset;
    let positions_len = mesh.positions.len() * 12;
    offset += align4(positions_len);

    let normals_offset = offset;
    let normals_len = mesh.normals.as_ref().map_or(0, |n| n.len() * 12);
    if normals_len > 0 {
        offset += align4(normals_len);
    }

    let indices_offset = offset;
    let indices_len = mesh.indices.as_ref().map_or(0, |i| i.len() * 4);
    if indices_len > 0 {
        offset += align4(indices_len);
    }

    let colors_offset = offset;
    let colors_len = mesh.colors.as_ref().map_or(0, |c| c.len() * 4);
    if colors_len > 0 {
        offset += align4(colors_len);
    }

    let texcoords_offset = offset;
    let texcoords_len = mesh.texcoords.as_ref().map_or(0, |t| t.len() * 8);
    if texcoords_len > 0 {
        offset += align4(texcoords_len);
    }

    // Feature IDs (per-vertex u32)
    let feature_ids_offset = offset;
    let feature_ids_len = mesh.feature_ids.as_ref().map_or(0, |f| f.len() * 4);
    if feature_ids_len > 0 {
        offset += align4(feature_ids_len);
    }

    // Texture image data (raw bytes, no byte stride)
    let texture_offset = offset;
    let texture_len = mesh.texture.as_ref().map_or(0, |t| t.image_data.len());
    if texture_len > 0 {
        offset += align4(texture_len);
    }

    // Metadata property buffers
    let mut metadata_segments = Vec::new();
    if let Some(ref meta) = mesh.metadata {
        for prop in &meta.properties {
            match &prop.values {
                MetadataValues::Float32(v) => {
                    let len = v.len() * 4;
                    metadata_segments.push(MetadataBufferSegment {
                        offset,
                        len,
                        string_offsets_offset: 0,
                        string_offsets_len: 0,
                    });
                    offset += align4(len);
                }
                MetadataValues::Int32(v) => {
                    let len = v.len() * 4;
                    metadata_segments.push(MetadataBufferSegment {
                        offset,
                        len,
                        string_offsets_offset: 0,
                        string_offsets_len: 0,
                    });
                    offset += align4(len);
                }
                MetadataValues::Uint8(v) => {
                    let len = v.len();
                    metadata_segments.push(MetadataBufferSegment {
                        offset,
                        len,
                        string_offsets_offset: 0,
                        string_offsets_len: 0,
                    });
                    offset += align4(len);
                }
                MetadataValues::String(strings) => {
                    // Concatenated UTF-8 bytes
                    let total_bytes: usize = strings.iter().map(|s| s.len()).sum();
                    let data_offset = offset;
                    offset += align4(total_bytes);

                    // u32 byte offsets (count + 1 entries)
                    let offsets_len = (strings.len() + 1) * 4;
                    let offsets_offset = offset;
                    offset += align4(offsets_len);

                    metadata_segments.push(MetadataBufferSegment {
                        offset: data_offset,
                        len: total_bytes,
                        string_offsets_offset: offsets_offset,
                        string_offsets_len: offsets_len,
                    });
                }
            }
        }
    }

    BufferLayout {
        positions_offset,
        positions_len,
        normals_offset,
        normals_len,
        indices_offset,
        indices_len,
        colors_offset,
        colors_len,
        texcoords_offset,
        texcoords_len,
        feature_ids_offset,
        feature_ids_len,
        texture_offset,
        texture_len,
        metadata_segments,
        total_len: offset,
    }
}

fn compute_aabb(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in positions {
        for i in 0..3 {
            if p[i] < min[i] {
                min[i] = p[i];
            }
            if p[i] > max[i] {
                max[i] = p[i];
            }
        }
    }
    (min, max)
}

fn build_json(mesh: &GlbMesh, layout: &BufferLayout) -> String {
    let mut buffer_views = Vec::new();
    let mut accessors = Vec::new();
    let mut attributes = serde_json::Map::new();
    let mut bv_idx = 0u32;
    let mut acc_idx = 0u32;

    // Positions
    buffer_views.push(serde_json::json!({
        "buffer": 0,
        "byteOffset": layout.positions_offset,
        "byteLength": layout.positions_len,
        "byteStride": 12,
        "target": 34962 // ARRAY_BUFFER
    }));
    let (min, max) = compute_aabb(&mesh.positions);
    accessors.push(serde_json::json!({
        "bufferView": bv_idx,
        "byteOffset": 0,
        "componentType": COMPONENT_FLOAT,
        "count": mesh.positions.len(),
        "type": "VEC3",
        "min": [min[0], min[1], min[2]],
        "max": [max[0], max[1], max[2]]
    }));
    attributes.insert("POSITION".to_string(), serde_json::json!(acc_idx));
    bv_idx += 1;
    acc_idx += 1;

    // Normals
    if let Some(ref normals) = mesh.normals {
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": layout.normals_offset,
            "byteLength": layout.normals_len,
            "byteStride": 12,
            "target": 34962
        }));
        accessors.push(serde_json::json!({
            "bufferView": bv_idx,
            "byteOffset": 0,
            "componentType": COMPONENT_FLOAT,
            "count": normals.len(),
            "type": "VEC3"
        }));
        attributes.insert("NORMAL".to_string(), serde_json::json!(acc_idx));
        bv_idx += 1;
        acc_idx += 1;
    }

    // Indices accessor (no byteStride for ELEMENT_ARRAY_BUFFER)
    let mut indices_accessor = None;
    if let Some(ref indices) = mesh.indices {
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": layout.indices_offset,
            "byteLength": layout.indices_len,
            "target": 34963 // ELEMENT_ARRAY_BUFFER
        }));
        accessors.push(serde_json::json!({
            "bufferView": bv_idx,
            "byteOffset": 0,
            "componentType": COMPONENT_UNSIGNED_INT,
            "count": indices.len(),
            "type": "SCALAR"
        }));
        indices_accessor = Some(acc_idx);
        bv_idx += 1;
        acc_idx += 1;
    }

    // Colors
    if let Some(ref colors) = mesh.colors {
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": layout.colors_offset,
            "byteLength": layout.colors_len,
            "byteStride": 4,
            "target": 34962
        }));
        accessors.push(serde_json::json!({
            "bufferView": bv_idx,
            "byteOffset": 0,
            "componentType": COMPONENT_UNSIGNED_BYTE,
            "count": colors.len(),
            "type": "VEC4",
            "normalized": true
        }));
        attributes.insert("COLOR_0".to_string(), serde_json::json!(acc_idx));
        bv_idx += 1;
        acc_idx += 1;
    }

    // Texcoords
    if let Some(ref texcoords) = mesh.texcoords {
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": layout.texcoords_offset,
            "byteLength": layout.texcoords_len,
            "byteStride": 8,
            "target": 34962
        }));
        accessors.push(serde_json::json!({
            "bufferView": bv_idx,
            "byteOffset": 0,
            "componentType": COMPONENT_FLOAT,
            "count": texcoords.len(),
            "type": "VEC2"
        }));
        attributes.insert("TEXCOORD_0".to_string(), serde_json::json!(acc_idx));
        bv_idx += 1;
        acc_idx += 1;
    }

    let mut primitive = serde_json::json!({ "attributes": attributes });
    if let Some(idx_acc) = indices_accessor {
        primitive["indices"] = serde_json::json!(idx_acc);
    }

    // Material — the texture image and its buffer view, the base colour, or both
    let mut images_json = Vec::new();
    let mut textures_json = Vec::new();
    let mut samplers_json = Vec::new();
    let mut materials_json = Vec::new();
    if mesh.texture.is_some() || mesh.base_color_factor.is_some() {
        let mut pbr = serde_json::json!({
            "metallicFactor": 0.0,
            "roughnessFactor": 1.0
        });

        if let Some(ref tex) = mesh.texture {
            // Buffer view for the texture image blob (no byte stride, no target)
            buffer_views.push(serde_json::json!({
                "buffer": 0,
                "byteOffset": layout.texture_offset,
                "byteLength": layout.texture_len
            }));
            let tex_bv = bv_idx;
            bv_idx += 1;

            images_json.push(serde_json::json!({
                "bufferView": tex_bv,
                "mimeType": tex.mime_type
            }));
            samplers_json.push(serde_json::json!({
                "magFilter": 9729,
                "minFilter": 9987,
                "wrapS": 10497,
                "wrapT": 10497
            }));
            textures_json.push(serde_json::json!({
                "source": 0,
                "sampler": 0
            }));
            pbr["baseColorTexture"] = serde_json::json!({ "index": 0 });
        }

        if let Some(factor) = mesh.base_color_factor {
            pbr["baseColorFactor"] = serde_json::json!(factor);
        }

        materials_json.push(serde_json::json!({ "pbrMetallicRoughness": pbr }));
        primitive["material"] = serde_json::json!(0);
    }

    // Feature IDs accessor (for EXT_mesh_features)
    if let Some(ref fids) = mesh.feature_ids {
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": layout.feature_ids_offset,
            "byteLength": layout.feature_ids_len,
            "target": 34962
        }));
        accessors.push(serde_json::json!({
            "bufferView": bv_idx,
            "byteOffset": 0,
            "componentType": COMPONENT_UNSIGNED_INT,
            "count": fids.len(),
            "type": "SCALAR"
        }));
        let fid_acc = acc_idx;
        bv_idx += 1;
        acc_idx += 1;
        let _ = acc_idx; // suppress unused warning

        // Add EXT_mesh_features to primitive
        primitive["extensions"] = serde_json::json!({
            "EXT_mesh_features": {
                "featureIds": [{
                    "featureCount": fids.iter().copied().max().map(|m| m + 1).unwrap_or(0),
                    "attribute": 0
                }]
            }
        });
        attributes.insert("_FEATURE_ID_0".to_string(), serde_json::json!(fid_acc));
        // Re-set attributes since we added _FEATURE_ID_0
        primitive["attributes"] = serde_json::json!(attributes);
    }

    // Build metadata extension JSON
    let mut extensions = serde_json::Map::new();
    let mut extensions_used = Vec::new();

    if let Some(ref meta) = mesh.metadata {
        // Property table buffer views
        let mut property_table_props = serde_json::Map::new();
        let mut schema_props = serde_json::Map::new();
        let feature_count = meta
            .properties
            .first()
            .map(|p| match &p.values {
                MetadataValues::String(v) => v.len(),
                MetadataValues::Float32(v) => v.len(),
                MetadataValues::Int32(v) => v.len(),
                MetadataValues::Uint8(v) => v.len(),
            })
            .unwrap_or(0);

        for (i, prop) in meta.properties.iter().enumerate() {
            let seg = &layout.metadata_segments[i];

            let (type_str, component_type) = match &prop.values {
                MetadataValues::Float32(_) => ("SCALAR", Some("FLOAT32")),
                MetadataValues::Int32(_) => ("SCALAR", Some("INT32")),
                MetadataValues::Uint8(_) => ("SCALAR", Some("UINT8")),
                MetadataValues::String(_) => ("STRING", None),
            };

            // Schema property
            let mut schema_prop = serde_json::Map::new();
            schema_prop.insert("type".to_string(), serde_json::json!(type_str));
            if let Some(ct) = component_type {
                schema_prop.insert("componentType".to_string(), serde_json::json!(ct));
            }
            schema_props.insert(prop.name.clone(), serde_json::Value::Object(schema_prop));

            // Buffer view for property data
            buffer_views.push(serde_json::json!({
                "buffer": 0,
                "byteOffset": seg.offset,
                "byteLength": seg.len
            }));
            let values_bv = bv_idx;
            bv_idx += 1;

            // Property table entry
            let mut pt_prop = serde_json::Map::new();
            pt_prop.insert("values".to_string(), serde_json::json!(values_bv));

            // String offsets buffer view
            if matches!(&prop.values, MetadataValues::String(_)) {
                buffer_views.push(serde_json::json!({
                    "buffer": 0,
                    "byteOffset": seg.string_offsets_offset,
                    "byteLength": seg.string_offsets_len
                }));
                pt_prop.insert("stringOffsets".to_string(), serde_json::json!(bv_idx));
                bv_idx += 1;
            }

            property_table_props.insert(prop.name.clone(), serde_json::Value::Object(pt_prop));
        }

        extensions.insert(
            "EXT_structural_metadata".to_string(),
            serde_json::json!({
                "schema": {
                    "classes": {
                        &meta.class_name: {
                            "properties": schema_props
                        }
                    }
                },
                "propertyTables": [{
                    "class": &meta.class_name,
                    "count": feature_count,
                    "properties": property_table_props
                }]
            }),
        );

        extensions_used.push("EXT_structural_metadata");
        extensions_used.push("EXT_mesh_features");
    }

    let _ = bv_idx;

    let mut root = serde_json::json!({
        "asset": { "version": "2.0", "generator": "tiletopia" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [primitive] }],
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{ "byteLength": layout.total_len }]
    });

    if !images_json.is_empty() {
        root["images"] = serde_json::json!(images_json);
        root["textures"] = serde_json::json!(textures_json);
        root["samplers"] = serde_json::json!(samplers_json);
    }
    if !materials_json.is_empty() {
        root["materials"] = serde_json::json!(materials_json);
    }

    if !extensions.is_empty() {
        root["extensions"] = serde_json::Value::Object(extensions);
    }
    if !extensions_used.is_empty() {
        root["extensionsUsed"] = serde_json::json!(extensions_used);
    }

    serde_json::to_string(&root).expect("JSON serialisation failed")
}

fn write_bin<W: Write>(mesh: &GlbMesh, layout: &BufferLayout, w: &mut W) -> io::Result<()> {
    // Positions
    for p in &mesh.positions {
        for v in p {
            w.write_all(&v.to_le_bytes())?;
        }
    }
    write_padding(w, layout.positions_len)?;

    // Normals
    if let Some(ref normals) = mesh.normals {
        for n in normals {
            for v in n {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        write_padding(w, layout.normals_len)?;
    }

    // Indices
    if let Some(ref indices) = mesh.indices {
        for &i in indices {
            w.write_all(&i.to_le_bytes())?;
        }
        write_padding(w, layout.indices_len)?;
    }

    // Colors
    if let Some(ref colors) = mesh.colors {
        for c in colors {
            w.write_all(c)?;
        }
        write_padding(w, layout.colors_len)?;
    }

    // Texcoords
    if let Some(ref texcoords) = mesh.texcoords {
        for t in texcoords {
            for v in t {
                w.write_all(&v.to_le_bytes())?;
            }
        }
        write_padding(w, layout.texcoords_len)?;
    }

    // Feature IDs
    if let Some(ref fids) = mesh.feature_ids {
        for &id in fids {
            w.write_all(&id.to_le_bytes())?;
        }
        write_padding(w, layout.feature_ids_len)?;
    }

    // Texture image data
    if let Some(ref tex) = mesh.texture {
        w.write_all(&tex.image_data)?;
        write_padding(w, layout.texture_len)?;
    }

    // Metadata property values
    if let Some(ref meta) = mesh.metadata {
        for (i, prop) in meta.properties.iter().enumerate() {
            let seg = &layout.metadata_segments[i];
            match &prop.values {
                MetadataValues::Float32(v) => {
                    for &val in v {
                        w.write_all(&val.to_le_bytes())?;
                    }
                    write_padding(w, seg.len)?;
                }
                MetadataValues::Int32(v) => {
                    for &val in v {
                        w.write_all(&val.to_le_bytes())?;
                    }
                    write_padding(w, seg.len)?;
                }
                MetadataValues::Uint8(v) => {
                    w.write_all(v)?;
                    write_padding(w, seg.len)?;
                }
                MetadataValues::String(strings) => {
                    // Concatenated UTF-8 bytes
                    for s in strings {
                        w.write_all(s.as_bytes())?;
                    }
                    write_padding(w, seg.len)?;

                    // u32 byte offsets
                    let mut byte_offset = 0u32;
                    w.write_all(&byte_offset.to_le_bytes())?;
                    for s in strings {
                        byte_offset += s.len() as u32;
                        w.write_all(&byte_offset.to_le_bytes())?;
                    }
                    write_padding(w, seg.string_offsets_len)?;
                }
            }
        }
    }

    Ok(())
}

fn write_padding<W: Write>(w: &mut W, data_len: usize) -> io::Result<()> {
    let pad = align4(data_len) - data_len;
    for _ in 0..pad {
        w.write_all(&[0u8])?;
    }
    Ok(())
}

/// Write mesh data as a binary glTF 2.0 (GLB) file.
pub fn write_glb<W: Write>(mesh: &GlbMesh, writer: &mut W) -> io::Result<()> {
    let layout = compute_layout(mesh);

    let json_str = build_json(mesh, &layout);
    let json_bytes = json_str.as_bytes();
    let json_padded_len = align4(json_bytes.len());

    let bin_padded_len = layout.total_len; // already 4-byte aligned per layout

    let total_len: u32 = 12  // GLB header
        + 8 + json_padded_len as u32  // JSON chunk header + data
        + if bin_padded_len > 0 { 8 + bin_padded_len as u32 } else { 0 }; // BIN chunk

    // GLB header
    writer.write_all(&GLB_MAGIC.to_le_bytes())?;
    writer.write_all(&GLB_VERSION.to_le_bytes())?;
    writer.write_all(&total_len.to_le_bytes())?;

    // JSON chunk
    writer.write_all(&(json_padded_len as u32).to_le_bytes())?;
    writer.write_all(&CHUNK_TYPE_JSON.to_le_bytes())?;
    writer.write_all(json_bytes)?;
    for _ in 0..(json_padded_len - json_bytes.len()) {
        writer.write_all(b" ")?;
    }

    // BIN chunk (only if there is data)
    if bin_padded_len > 0 {
        writer.write_all(&(bin_padded_len as u32).to_le_bytes())?;
        writer.write_all(&CHUNK_TYPE_BIN.to_le_bytes())?;
        write_bin(mesh, &layout, writer)?;
    }

    Ok(())
}

/// Write mesh data as a GLB file to disk.
pub fn write_glb_file(mesh: &GlbMesh, path: &Path) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    write_glb(mesh, &mut file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle_mesh() -> GlbMesh {
        GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: None,
        }
    }

    #[test]
    fn empty_mesh() {
        let mesh = GlbMesh {
            positions: vec![],
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        // Header
        assert!(buf.len() >= 12);
        assert_eq!(&buf[0..4], &GLB_MAGIC.to_le_bytes());
        assert_eq!(&buf[4..8], &GLB_VERSION.to_le_bytes());
    }

    #[test]
    fn header_magic_and_version() {
        let mesh = triangle_mesh();
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), GLB_MAGIC);
        assert_eq!(
            u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            GLB_VERSION
        );
        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn simple_triangle() {
        let mesh = triangle_mesh();
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        // Must have GLB header + JSON chunk + BIN chunk
        assert!(buf.len() > 12 + 8 + 8);

        // JSON chunk type
        let json_chunk_type = u32::from_le_bytes(buf[16..20].try_into().unwrap());
        assert_eq!(json_chunk_type, CHUNK_TYPE_JSON);
    }

    #[test]
    fn mesh_with_normals_and_indices() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn mesh_with_all_attributes() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: Some(vec![[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]),
            texcoords: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn round_trip_gltf_crate() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("gltf crate should parse our GLB");
        assert_eq!(glb.header.version, 2);

        let json: serde_json::Value =
            serde_json::from_slice(&glb.json).expect("JSON chunk should be valid");
        let accessors = json["accessors"].as_array().unwrap();
        // positions + normals + indices = 3 accessors
        assert_eq!(accessors.len(), 3);

        let pos_acc = &accessors[0];
        assert_eq!(pos_acc["type"], "VEC3");
        assert_eq!(pos_acc["count"], 3);
    }

    #[test]
    fn metadata_float32_property() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: None,
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: None,
            metadata: Some(TileMetadata {
                class_name: "building".to_string(),
                properties: vec![MetadataProperty {
                    name: "height".to_string(),
                    values: MetadataValues::Float32(vec![10.0, 20.0, 15.0]),
                }],
            }),
            feature_ids: Some(vec![0, 1, 2]),
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        // Verify extensionsUsed
        let ext_used = json["extensionsUsed"].as_array().unwrap();
        assert!(ext_used.iter().any(|v| v == "EXT_structural_metadata"));
        assert!(ext_used.iter().any(|v| v == "EXT_mesh_features"));

        // Verify EXT_structural_metadata
        let ext = &json["extensions"]["EXT_structural_metadata"];
        assert!(ext.is_object(), "EXT_structural_metadata missing");

        let classes = &ext["schema"]["classes"];
        assert!(classes["building"].is_object());
        assert_eq!(
            classes["building"]["properties"]["height"]["type"],
            "SCALAR"
        );
        assert_eq!(
            classes["building"]["properties"]["height"]["componentType"],
            "FLOAT32"
        );

        let tables = ext["propertyTables"].as_array().unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0]["class"], "building");
        assert_eq!(tables[0]["count"], 3);

        // Verify total length
        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn metadata_string_property() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: Some(TileMetadata {
                class_name: "poi".to_string(),
                properties: vec![MetadataProperty {
                    name: "name".to_string(),
                    values: MetadataValues::String(vec!["Hello".to_string(), "World".to_string()]),
                }],
            }),
            feature_ids: Some(vec![0, 1]),
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        let ext = &json["extensions"]["EXT_structural_metadata"];
        let props = &ext["schema"]["classes"]["poi"]["properties"];
        assert_eq!(props["name"]["type"], "STRING");

        let table = &ext["propertyTables"][0];
        assert!(table["properties"]["name"]["stringOffsets"].is_number());

        // Verify string data in binary chunk
        let bin = glb.bin.unwrap();
        let values_bv_idx = table["properties"]["name"]["values"].as_u64().unwrap() as usize;
        let bv = &json["bufferViews"][values_bv_idx];
        let offset = bv["byteOffset"].as_u64().unwrap() as usize;
        let len = bv["byteLength"].as_u64().unwrap() as usize;
        let string_data = &bin[offset..offset + len];
        assert_eq!(string_data, b"HelloWorld");

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn metadata_mixed_properties() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]],
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: Some(TileMetadata {
                class_name: "building".to_string(),
                properties: vec![
                    MetadataProperty {
                        name: "height".to_string(),
                        values: MetadataValues::Float32(vec![10.0, 20.0]),
                    },
                    MetadataProperty {
                        name: "floors".to_string(),
                        values: MetadataValues::Int32(vec![3, 5]),
                    },
                    MetadataProperty {
                        name: "type".to_string(),
                        values: MetadataValues::Uint8(vec![1, 2]),
                    },
                    MetadataProperty {
                        name: "name".to_string(),
                        values: MetadataValues::String(vec![
                            "Office".to_string(),
                            "Residence".to_string(),
                        ]),
                    },
                ],
            }),
            feature_ids: Some(vec![0, 1]),
            texture: None,
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        let classes = &json["extensions"]["EXT_structural_metadata"]["schema"]["classes"];
        let props = &classes["building"]["properties"];
        assert_eq!(props["height"]["componentType"], "FLOAT32");
        assert_eq!(props["floors"]["componentType"], "INT32");
        assert_eq!(props["type"]["componentType"], "UINT8");
        assert_eq!(props["name"]["type"], "STRING");

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn mesh_with_texture() {
        // Create a minimal 2x2 PNG in memory.
        let mut png_buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png_buf, image::ImageFormat::Png)
            .unwrap();
        let png_bytes = png_buf.into_inner();

        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            metadata: None,
            feature_ids: None,
            texture: Some(TextureData {
                image_data: png_bytes,
                mime_type: "image/png".to_string(),
                width: 2,
                height: 2,
            }),
            base_color_factor: None,
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB with texture");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        // Verify texture-related JSON entries exist.
        assert!(json["images"].is_array());
        assert_eq!(json["images"][0]["mimeType"], "image/png");
        assert!(json["textures"].is_array());
        assert!(json["samplers"].is_array());
        assert!(json["materials"].is_array());
        assert_eq!(
            json["materials"][0]["pbrMetallicRoughness"]["metallicFactor"],
            0.0
        );

        // Primitive should reference material 0.
        let prim = &json["meshes"][0]["primitives"][0];
        assert_eq!(prim["material"], 0);

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }

    #[test]
    fn base_color_factor_writes_a_material_of_its_own() {
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: None,
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: Some([0.25, 0.5, 0.75, 1.0]),
        };
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        let pbr = &json["materials"][0]["pbrMetallicRoughness"];
        assert_eq!(
            pbr["baseColorFactor"],
            serde_json::json!([0.25, 0.5, 0.75, 1.0])
        );
        assert!(pbr.get("baseColorTexture").is_none());
        assert!(json.get("images").is_none());
        assert_eq!(json["meshes"][0]["primitives"][0]["material"], 0);

        let total = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        assert_eq!(total as usize, buf.len());
    }
}
