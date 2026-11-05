//! GLB (binary glTF 2.0) tile writer for mesh data.
//!
//! Produces spec-compliant GLB files suitable for 3D Tiles mesh content.
//! See: <https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#glb-file-format-specification>

use draco_gltf::{
    ComponentType, CompressionOptions, DracoPrimitiveEncoding, EXT_MESH_GPU_INSTANCING,
    EXT_MESHOPT_COMPRESSION, KHR_DRACO_MESH_COMPRESSION, PackedAttribute, PackedGeometry,
    PackedIndices, PrimitiveMode, QuantizationBits,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;

const GLB_MAGIC: u32 = 0x46546C67; // "glTF"
const GLB_VERSION: u32 = 2;
const CHUNK_TYPE_JSON: u32 = 0x4E4F534A; // "JSON"
const CHUNK_TYPE_BIN: u32 = 0x004E4942; // "BIN\0"

pub(crate) const COMPONENT_FLOAT: u32 = 5126;
const COMPONENT_UNSIGNED_INT: u32 = 5125;
pub(crate) const COMPONENT_UNSIGNED_BYTE: u32 = 5121;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeshCompression {
    #[default]
    None,
    Meshopt,
    Draco,
}

pub const KTX2_MIME_TYPE: &str = "image/ktx2";
const KTX2_TEXTURE_EXTENSION: &str = "KHR_texture_basisu";
const EXT_INSTANCE_FEATURES: &str = "EXT_instance_features";
const EXT_MESH_PRIMITIVE_EDGE_VISIBILITY: &str = "EXT_mesh_primitive_edge_visibility";

pub struct EdgeVisibility {
    pub visibility: Vec<u8>,
    pub silhouette_normals: Vec<[f32; 3]>,
}

pub struct InstancedMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Option<Vec<[f32; 3]>>,
    pub indices: Vec<u32>,
    pub node_translation: [f64; 3],
    pub instance_translations: Vec<[f32; 3]>,
    pub instance_rotations: Option<Vec<[f32; 4]>>,
    pub instance_feature_ids: Option<Vec<u32>>,
}

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
    pub node_translation: Option<[f64; 3]>,
    pub instanced_meshes: Vec<InstancedMesh>,
    pub edge_visibility: Option<EdgeVisibility>,
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

pub(crate) fn align4(n: usize) -> usize {
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

/// Rows in the property table, taken from its first property.
fn property_table_count(metadata: &TileMetadata) -> usize {
    metadata
        .properties
        .first()
        .map(|property| match &property.values {
            MetadataValues::String(values) => values.len(),
            MetadataValues::Float32(values) => values.len(),
            MetadataValues::Int32(values) => values.len(),
            MetadataValues::Uint8(values) => values.len(),
        })
        .unwrap_or(0)
}

fn writes_direct_mesh(mesh: &GlbMesh) -> bool {
    !mesh.positions.is_empty() || mesh.instanced_meshes.is_empty()
}

fn build_document(mesh: &GlbMesh, layout: &BufferLayout) -> serde_json::Value {
    let mut buffer_views = Vec::new();
    let mut accessors = Vec::new();
    let mut attributes = serde_json::Map::new();
    let mut bv_idx = 0u32;
    let mut acc_idx = 0u32;

    // Positions
    if writes_direct_mesh(mesh) {
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
    }

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
            textures_json.push(if tex.mime_type == KTX2_MIME_TYPE {
                serde_json::json!({
                    "sampler": 0,
                    "extensions": { KTX2_TEXTURE_EXTENSION: { "source": 0 } }
                })
            } else {
                serde_json::json!({
                    "source": 0,
                    "sampler": 0
                })
            });
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
            // gltf forbids UNSIGNED_INT vertex attributes, and EXT_mesh_features takes FLOAT ids up to 2^24
            "componentType": COMPONENT_FLOAT,
            "count": fids.len(),
            "type": "SCALAR"
        }));
        let fid_acc = acc_idx;
        bv_idx += 1;
        acc_idx += 1;
        let _ = acc_idx; // suppress unused warning

        // a property table, where there is one, defines the feature set the ids
        // index into
        let mut feature_id = serde_json::json!({
            "featureCount": fids.iter().copied().max().map(|m| m + 1).unwrap_or(0),
            "attribute": 0
        });
        if let Some(count) = mesh.metadata.as_ref().map(property_table_count) {
            feature_id["featureCount"] = serde_json::json!(count);
            feature_id["propertyTable"] = serde_json::json!(0);
        }
        primitive["extensions"] = serde_json::json!({
            "EXT_mesh_features": { "featureIds": [feature_id] }
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
        let feature_count = property_table_count(meta);

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
                    "id": "tiletopia",
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
    }

    if mesh.feature_ids.is_some() {
        extensions_used.push("EXT_mesh_features");
    }

    let has_ktx2_texture = mesh
        .texture
        .as_ref()
        .is_some_and(|texture| texture.mime_type == KTX2_MIME_TYPE);
    if has_ktx2_texture {
        extensions_used.push(KTX2_TEXTURE_EXTENSION);
    }

    let _ = bv_idx;

    let mut node = serde_json::json!({ "mesh": 0 });
    if let Some(translation) = mesh.node_translation {
        node["translation"] = serde_json::json!(translation);
    }
    let (scene_nodes, nodes, meshes) = if writes_direct_mesh(mesh) {
        (
            vec![serde_json::json!(0)],
            vec![node],
            vec![serde_json::json!({ "primitives": [primitive] })],
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    let mut root = serde_json::json!({
        "asset": { "version": "2.0", "generator": "tiletopia" },
        "scene": 0,
        "scenes": [{ "nodes": scene_nodes }],
        "nodes": nodes,
        "meshes": meshes,
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
    if has_ktx2_texture {
        root["extensionsRequired"] = serde_json::json!([KTX2_TEXTURE_EXTENSION]);
    }

    root
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
            w.write_all(&float_feature_id(id)?.to_le_bytes())?;
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

fn float_feature_id(id: u32) -> io::Result<f32> {
    if id > HIGHEST_FLOAT_FEATURE_ID {
        return Err(io::Error::other(format!(
            "feature id {id} is above the {HIGHEST_FLOAT_FEATURE_ID} a float attribute holds exactly"
        )));
    }
    Ok(id as f32)
}

struct AppendedView {
    target: Option<u64>,
    byte_stride: Option<usize>,
}

const VERTEX_VIEW: AppendedView = AppendedView {
    target: Some(TARGET_ARRAY_BUFFER),
    byte_stride: Some(12),
};
const INDEX_VIEW: AppendedView = AppendedView {
    target: Some(TARGET_ELEMENT_ARRAY_BUFFER),
    byte_stride: None,
};
// draco cannot hold instance attributes, and both compression passes copy a view with no target as it is
const INSTANCE_VIEW: AppendedView = AppendedView {
    target: None,
    byte_stride: None,
};

fn append_accessor(
    document: &mut serde_json::Value,
    bin: &mut Vec<u8>,
    bytes: &[u8],
    view: AppendedView,
    mut accessor: serde_json::Value,
) -> usize {
    let views = document["bufferViews"]
        .as_array_mut()
        .expect("a bufferViews array");
    let mut buffer_view = serde_json::json!({
        "buffer": 0,
        "byteOffset": append_aligned(bin, bytes),
        "byteLength": bytes.len()
    });
    if let Some(target) = view.target {
        buffer_view["target"] = serde_json::json!(target);
    }
    if let Some(byte_stride) = view.byte_stride {
        buffer_view["byteStride"] = serde_json::json!(byte_stride);
    }
    accessor["bufferView"] = serde_json::json!(views.len());
    accessor["byteOffset"] = serde_json::json!(0);
    views.push(buffer_view);

    let accessors = document["accessors"]
        .as_array_mut()
        .expect("an accessors array");
    accessors.push(accessor);
    accessors.len() - 1
}

fn vec3_accessor(values: &[[f32; 3]]) -> serde_json::Value {
    let (min, max) = compute_aabb(values);
    serde_json::json!({
        "componentType": COMPONENT_FLOAT,
        "count": values.len(),
        "type": "VEC3",
        "min": min,
        "max": max
    })
}

fn append_instanced_meshes(
    document: &mut serde_json::Value,
    bin: &mut Vec<u8>,
    mesh: &GlbMesh,
) -> io::Result<()> {
    if mesh.instanced_meshes.is_empty() {
        return Ok(());
    }
    let has_material = document.get("materials").is_some();
    let property_table_rows = mesh.metadata.as_ref().map(property_table_count);

    for instanced in &mesh.instanced_meshes {
        let mut attributes = serde_json::Map::new();
        let positions = append_accessor(
            document,
            bin,
            bytemuck::cast_slice(&instanced.positions),
            VERTEX_VIEW,
            vec3_accessor(&instanced.positions),
        );
        attributes.insert("POSITION".to_string(), serde_json::json!(positions));
        if let Some(normals) = &instanced.normals {
            let normals = append_accessor(
                document,
                bin,
                bytemuck::cast_slice(normals),
                VERTEX_VIEW,
                serde_json::json!({
                    "componentType": COMPONENT_FLOAT,
                    "count": normals.len(),
                    "type": "VEC3"
                }),
            );
            attributes.insert("NORMAL".to_string(), serde_json::json!(normals));
        }
        let indices = append_accessor(
            document,
            bin,
            bytemuck::cast_slice(&instanced.indices),
            INDEX_VIEW,
            serde_json::json!({
                "componentType": COMPONENT_UNSIGNED_INT,
                "count": instanced.indices.len(),
                "type": "SCALAR"
            }),
        );
        let mut primitive = serde_json::json!({ "attributes": attributes, "indices": indices });
        if has_material {
            primitive["material"] = serde_json::json!(0);
        }

        let mut instance_attributes = serde_json::Map::new();
        let translations = append_accessor(
            document,
            bin,
            bytemuck::cast_slice(&instanced.instance_translations),
            INSTANCE_VIEW,
            vec3_accessor(&instanced.instance_translations),
        );
        instance_attributes.insert("TRANSLATION".to_string(), serde_json::json!(translations));
        if let Some(rotations) = &instanced.instance_rotations {
            let rotations = append_accessor(
                document,
                bin,
                bytemuck::cast_slice(rotations),
                INSTANCE_VIEW,
                serde_json::json!({
                    "componentType": COMPONENT_FLOAT,
                    "count": rotations.len(),
                    "type": "VEC4"
                }),
            );
            instance_attributes.insert("ROTATION".to_string(), serde_json::json!(rotations));
        }

        let mut node_extensions = serde_json::Map::new();
        if let Some(feature_ids) = &instanced.instance_feature_ids {
            let values: Vec<f32> = feature_ids
                .iter()
                .map(|&id| float_feature_id(id))
                .collect::<io::Result<_>>()?;
            let feature_ids_accessor = append_accessor(
                document,
                bin,
                bytemuck::cast_slice(&values),
                INSTANCE_VIEW,
                serde_json::json!({
                    "componentType": COMPONENT_FLOAT,
                    "count": values.len(),
                    "type": "SCALAR"
                }),
            );
            instance_attributes.insert(
                "_FEATURE_ID_0".to_string(),
                serde_json::json!(feature_ids_accessor),
            );
            let mut feature_id = serde_json::json!({
                "featureCount": feature_ids.iter().copied().max().map_or(0, |highest| highest + 1),
                "attribute": 0
            });
            if let Some(rows) = property_table_rows {
                feature_id["featureCount"] = serde_json::json!(rows);
                feature_id["propertyTable"] = serde_json::json!(0);
            }
            node_extensions.insert(
                EXT_INSTANCE_FEATURES.to_string(),
                serde_json::json!({ "featureIds": [feature_id] }),
            );
        }
        node_extensions.insert(
            EXT_MESH_GPU_INSTANCING.to_string(),
            serde_json::json!({ "attributes": instance_attributes }),
        );

        let meshes = document["meshes"].as_array_mut().expect("a meshes array");
        meshes.push(serde_json::json!({ "primitives": [primitive] }));
        let mesh_index = meshes.len() - 1;
        let nodes = document["nodes"].as_array_mut().expect("a nodes array");
        nodes.push(serde_json::json!({
            "mesh": mesh_index,
            "translation": instanced.node_translation,
            "extensions": node_extensions
        }));
        let node_index = nodes.len() - 1;
        document["scenes"][0]["nodes"]
            .as_array_mut()
            .expect("a scene node list")
            .push(serde_json::json!(node_index));
    }

    document["buffers"][0]["byteLength"] = serde_json::json!(bin.len());
    // EXT_mesh_gpu_instancing has no non-instanced fallback, so its README asks for extensionsRequired
    require_extension(document, EXT_MESH_GPU_INSTANCING);
    let has_instance_features = mesh
        .instanced_meshes
        .iter()
        .any(|instanced| instanced.instance_feature_ids.is_some());
    if has_instance_features {
        declare_extension(document, "extensionsUsed", EXT_INSTANCE_FEATURES);
    }
    Ok(())
}

const EDGE_VISIBILITY_VIEW: AppendedView = AppendedView {
    target: None,
    byte_stride: None,
};

fn append_edge_visibility(document: &mut serde_json::Value, bin: &mut Vec<u8>, mesh: &GlbMesh) {
    let Some(edges) = &mesh.edge_visibility else {
        return;
    };
    let visibility = append_accessor(
        document,
        bin,
        &edges.visibility,
        EDGE_VISIBILITY_VIEW,
        serde_json::json!({
            "componentType": COMPONENT_UNSIGNED_BYTE,
            "count": edges.visibility.len(),
            "type": "SCALAR"
        }),
    );
    let mut extension = serde_json::json!({ "visibility": visibility });
    if !edges.silhouette_normals.is_empty() {
        let silhouette_normals = append_accessor(
            document,
            bin,
            bytemuck::cast_slice(&edges.silhouette_normals),
            EDGE_VISIBILITY_VIEW,
            serde_json::json!({
                "componentType": COMPONENT_FLOAT,
                "count": edges.silhouette_normals.len(),
                "type": "VEC3"
            }),
        );
        extension["silhouetteNormals"] = serde_json::json!(silhouette_normals);
    }
    document["meshes"][0]["primitives"][0]["extensions"][EXT_MESH_PRIMITIVE_EDGE_VISIBILITY] =
        extension;
    document["buffers"][0]["byteLength"] = serde_json::json!(bin.len());
    declare_extension(
        document,
        "extensionsUsed",
        EXT_MESH_PRIMITIVE_EDGE_VISIBILITY,
    );
}

fn view_bytes<'a>(view: &serde_json::Value, bin: &'a [u8]) -> &'a [u8] {
    let offset = view["byteOffset"].as_u64().expect("a view byteOffset") as usize;
    let length = view["byteLength"].as_u64().expect("a view byteLength") as usize;
    &bin[offset..offset + length]
}

fn append_aligned(packed: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let offset = packed.len();
    packed.extend_from_slice(bytes);
    packed.resize(align4(packed.len()), 0);
    offset
}

fn declare_extension(document: &mut serde_json::Value, field: &str, name: &str) {
    document
        .as_object_mut()
        .expect("a JSON object document")
        .entry(field)
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .expect("an extension name list")
        .push(serde_json::json!(name));
}

fn require_extension(document: &mut serde_json::Value, name: &str) {
    for field in ["extensionsUsed", "extensionsRequired"] {
        declare_extension(document, field, name);
    }
}

// EXT_meshopt_compression requires attribute streams to start with 0xa0, which only vertex codec version 0 writes
const MESHOPT_VERTEX_CODEC_VERSION: i32 = 0;
const MESHOPT_VERTEX_CODEC_LEVEL: i32 = 2;

fn encode_meshopt_attribute(bytes: &[u8], byte_stride: usize) -> meshopt::Result<Vec<u8>> {
    fn encode<const STRIDE: usize>(bytes: &[u8]) -> meshopt::Result<Vec<u8>> {
        meshopt::encode_vertex_buffer_level(
            bytes.as_chunks::<STRIDE>().0,
            MESHOPT_VERTEX_CODEC_LEVEL,
            Some(MESHOPT_VERTEX_CODEC_VERSION),
        )
    }
    match byte_stride {
        4 => encode::<4>(bytes),
        8 => encode::<8>(bytes),
        12 => encode::<12>(bytes),
        16 => encode::<16>(bytes),
        other => unreachable!("no attribute view has byte stride {other}"),
    }
}

const TARGET_ARRAY_BUFFER: u64 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u64 = 34963;

fn accessor_element_bytes(accessor: &serde_json::Value) -> usize {
    let components = match accessor["type"].as_str() {
        Some("SCALAR") => 1,
        Some("VEC2") => 2,
        Some("VEC3") => 3,
        Some("VEC4") => 4,
        other => unreachable!("no accessor is written with type {other:?}"),
    };
    let component_bytes = match accessor["componentType"].as_u64().map(|value| value as u32) {
        Some(COMPONENT_UNSIGNED_BYTE) => 1,
        Some(COMPONENT_UNSIGNED_INT | COMPONENT_FLOAT) => 4,
        other => unreachable!("no accessor is written with componentType {other:?}"),
    };
    components * component_bytes
}

struct PrimitiveGeometry<'a> {
    positions: &'a [[f32; 3]],
    normals: Option<&'a [[f32; 3]]>,
    colors: Option<&'a [[u8; 4]]>,
    texcoords: Option<&'a [[f32; 2]]>,
    feature_ids: Option<&'a [u32]>,
    indices: &'a [u32],
    has_edge_visibility: bool,
}

fn primitive_geometries(mesh: &GlbMesh) -> Vec<PrimitiveGeometry<'_>> {
    let direct = writes_direct_mesh(mesh).then(|| PrimitiveGeometry {
        positions: &mesh.positions,
        normals: mesh.normals.as_deref(),
        colors: mesh.colors.as_deref(),
        texcoords: mesh.texcoords.as_deref(),
        feature_ids: mesh.feature_ids.as_deref(),
        indices: mesh.indices.as_deref().unwrap_or_default(),
        has_edge_visibility: mesh.edge_visibility.is_some(),
    });
    let instanced = mesh
        .instanced_meshes
        .iter()
        .map(|instanced| PrimitiveGeometry {
            positions: &instanced.positions,
            normals: instanced.normals.as_deref(),
            colors: None,
            texcoords: None,
            feature_ids: None,
            indices: &instanced.indices,
            has_edge_visibility: false,
        });
    direct.into_iter().chain(instanced).collect()
}

fn instance_attribute_views(document: &serde_json::Value) -> HashSet<u64> {
    let Some(nodes) = document["nodes"].as_array() else {
        return HashSet::new();
    };
    nodes
        .iter()
        .filter_map(|node| node["extensions"][EXT_MESH_GPU_INSTANCING]["attributes"].as_object())
        .flat_map(|attributes| attributes.values())
        .filter_map(|accessor| {
            let accessor = accessor.as_u64()?;
            document["accessors"][accessor as usize]["bufferView"].as_u64()
        })
        .collect()
}

fn apply_meshopt(
    document: &mut serde_json::Value,
    bin: &[u8],
    geometries: &[PrimitiveGeometry<'_>],
) -> io::Result<Vec<u8>> {
    let mut packed = Vec::new();
    let mut fallback_len = 0;
    let geometry_by_index_view: HashMap<u64, &PrimitiveGeometry<'_>> = geometries
        .iter()
        .enumerate()
        .filter_map(|(mesh_index, geometry)| {
            let accessor = document["meshes"][mesh_index]["primitives"][0]["indices"].as_u64()?;
            let view = document["accessors"][accessor as usize]["bufferView"].as_u64()?;
            Some((view, geometry))
        })
        .collect();
    let element_bytes_by_view: HashMap<u64, usize> = document["accessors"]
        .as_array()
        .expect("an accessors array")
        .iter()
        .map(|accessor| {
            let view = accessor["bufferView"]
                .as_u64()
                .expect("every accessor names a bufferView");
            (view, accessor_element_bytes(accessor))
        })
        .collect();
    let instance_views = instance_attribute_views(document);
    let views = document["bufferViews"]
        .as_array_mut()
        .expect("a bufferViews array");
    for (index, view) in views.iter_mut().enumerate() {
        let bytes = view_bytes(view, bin);
        let target = view.get("target").and_then(serde_json::Value::as_u64);
        if target.is_none() && !instance_views.contains(&(index as u64)) {
            view["byteOffset"] = serde_json::json!(append_aligned(&mut packed, bytes));
            continue;
        }

        let byte_stride = match view.get("byteStride").and_then(serde_json::Value::as_u64) {
            Some(stride) => stride as usize,
            // gltf views with no byteStride are tightly packed, one accessor element per stride
            None => *element_bytes_by_view
                .get(&(index as u64))
                .expect("every targeted bufferView has an accessor"),
        };
        let index_geometry = (target == Some(TARGET_ELEMENT_ARRAY_BUFFER)).then(|| {
            *geometry_by_index_view
                .get(&(index as u64))
                .expect("every index view belongs to a primitive")
        });
        let (encoded, mode) = match index_geometry {
            None => (encode_meshopt_attribute(bytes, byte_stride), "ATTRIBUTES"),
            // meshopt_encodeIndexBuffer rotates triangle corners, which moves each edge visibility value onto another edge
            Some(geometry) if geometry.has_edge_visibility => (
                meshopt::encode_index_sequence(geometry.indices, geometry.positions.len()),
                "INDICES",
            ),
            Some(geometry) => (
                meshopt::encode_index_buffer(geometry.indices, geometry.positions.len()),
                "TRIANGLES",
            ),
        };
        let encoded = encoded.map_err(io::Error::other)?;

        view["extensions"] = serde_json::json!({
            EXT_MESHOPT_COMPRESSION: {
                "buffer": 0,
                "byteOffset": append_aligned(&mut packed, &encoded),
                "byteLength": encoded.len(),
                "byteStride": byte_stride,
                "count": bytes.len() / byte_stride,
                "mode": mode
            }
        });
        view["buffer"] = serde_json::json!(1);
        view["byteOffset"] = serde_json::json!(fallback_len);
        fallback_len += align4(bytes.len());
    }

    document["buffers"] = serde_json::json!([
        { "byteLength": packed.len() },
        {
            "byteLength": fallback_len,
            "extensions": { EXT_MESHOPT_COMPRESSION: { "fallback": true } }
        }
    ]);
    require_extension(document, EXT_MESHOPT_COMPRESSION);
    Ok(packed)
}

const HIGHEST_FLOAT_FEATURE_ID: u32 = 1 << 24;

fn packed_draco_geometry(geometry: &PrimitiveGeometry<'_>) -> io::Result<PackedGeometry> {
    let vertex_count = geometry.positions.len();
    let attribute = |semantic: &str, components, component_type, normalized, bytes: &[u8]| {
        PackedAttribute::new(
            semantic,
            vertex_count,
            components,
            component_type,
            normalized,
            bytes.to_vec(),
        )
        .map_err(io::Error::other)
    };

    let mut attributes = vec![attribute(
        "POSITION",
        3,
        ComponentType::F32,
        false,
        bytemuck::cast_slice(geometry.positions),
    )?];
    if let Some(normals) = geometry.normals {
        attributes.push(attribute(
            "NORMAL",
            3,
            ComponentType::F32,
            false,
            bytemuck::cast_slice(normals),
        )?);
    }
    if let Some(colors) = geometry.colors {
        attributes.push(attribute(
            "COLOR_0",
            4,
            ComponentType::U8,
            true,
            bytemuck::cast_slice(colors),
        )?);
    }
    if let Some(texcoords) = geometry.texcoords {
        attributes.push(attribute(
            "TEXCOORD_0",
            2,
            ComponentType::F32,
            false,
            bytemuck::cast_slice(texcoords),
        )?);
    }
    if let Some(feature_ids) = geometry.feature_ids {
        // draco-gltf fails on UNSIGNED_INT custom attributes, and EXT_mesh_features takes FLOAT up to 2^24
        let highest = feature_ids.iter().copied().max().unwrap_or(0);
        if highest <= u32::from(u16::MAX) {
            let narrowed: Vec<u16> = feature_ids.iter().map(|&id| id as u16).collect();
            attributes.push(attribute(
                "_FEATURE_ID_0",
                1,
                ComponentType::U16,
                false,
                bytemuck::cast_slice(&narrowed),
            )?);
        } else if highest <= HIGHEST_FLOAT_FEATURE_ID {
            let widened: Vec<f32> = feature_ids.iter().map(|&id| id as f32).collect();
            attributes.push(attribute(
                "_FEATURE_ID_0",
                1,
                ComponentType::F32,
                false,
                bytemuck::cast_slice(&widened),
            )?);
        } else {
            return Err(io::Error::other(format!(
                "feature id {highest} is above the {HIGHEST_FLOAT_FEATURE_ID} a Draco float attribute holds exactly"
            )));
        }
    }

    let indices = PackedIndices::new(
        geometry.indices.len(),
        ComponentType::U32,
        bytemuck::cast_slice(geometry.indices).to_vec(),
    )
    .map_err(io::Error::other)?;
    PackedGeometry::new(PrimitiveMode::Triangles, attributes, Some(indices))
        .map_err(io::Error::other)
}

fn detach_from_buffer_view(accessor: &mut serde_json::Value, count: usize) {
    let fields = accessor.as_object_mut().expect("a JSON object accessor");
    fields.remove("bufferView");
    fields.remove("byteOffset");
    fields.insert("count".to_string(), serde_json::json!(count));
}

const DRACO_SEQUENTIAL_ENCODING: i32 = 1;

fn apply_draco(
    document: &mut serde_json::Value,
    bin: &[u8],
    geometries: &[PrimitiveGeometry<'_>],
) -> io::Result<Vec<u8>> {
    // draco-gltf's generic attribute bits quantize feature ids, which must stay exact
    let options = CompressionOptions {
        quantization: QuantizationBits {
            generic: None,
            ..QuantizationBits::GLTF
        },
        ..CompressionOptions::default()
    };
    // draco EdgeBreaker reorders triangles and their corners, sequential connectivity keeps both in order
    let sequential_options = CompressionOptions {
        encoding_method: DRACO_SEQUENTIAL_ENCODING,
        ..options
    };
    let mut encodings = Vec::with_capacity(geometries.len());
    for (mesh_index, geometry) in geometries.iter().enumerate() {
        let geometry_options = if geometry.has_edge_visibility {
            &sequential_options
        } else {
            &options
        };
        let encoded =
            DracoPrimitiveEncoding::encode(&packed_draco_geometry(geometry)?, geometry_options)
                .map_err(io::Error::other)?;

        let primitive = &document["meshes"][mesh_index]["primitives"][0];
        let attribute_accessors = primitive["attributes"].clone();
        let indices_accessor = primitive["indices"].as_u64().expect("an indices accessor");
        for declared in encoded.accessors() {
            let accessor_index = attribute_accessors[declared.semantic()]
                .as_u64()
                .expect("an accessor for every encoded attribute");
            let accessor = &mut document["accessors"][accessor_index as usize];
            detach_from_buffer_view(accessor, declared.count());
            accessor["componentType"] = serde_json::json!(declared.component_type().to_gltf());
            if let Some((min, max)) = declared.bounds() {
                accessor["min"] = serde_json::json!(min);
                accessor["max"] = serde_json::json!(max);
            }
        }
        detach_from_buffer_view(
            &mut document["accessors"][indices_accessor as usize],
            encoded.index_count(),
        );
        encodings.push(encoded);
    }

    let views = std::mem::take(
        document["bufferViews"]
            .as_array_mut()
            .expect("a bufferViews array"),
    );
    let mut packed = Vec::new();
    let mut kept_views = Vec::new();
    let mut new_view_indices = vec![None; views.len()];
    for (old_index, mut view) in views.into_iter().enumerate() {
        if view.get("target").is_some() {
            continue;
        }
        let offset = append_aligned(&mut packed, view_bytes(&view, bin));
        view["byteOffset"] = serde_json::json!(offset);
        new_view_indices[old_index] = Some(kept_views.len());
        kept_views.push(view);
    }

    let reindex = |view_reference: &mut serde_json::Value| {
        let old_index = view_reference.as_u64().expect("a bufferView index") as usize;
        *view_reference = serde_json::json!(new_view_indices[old_index]);
    };
    let images = document
        .get_mut("images")
        .and_then(serde_json::Value::as_array_mut);
    if let Some(images) = images {
        for image in images {
            reindex(&mut image["bufferView"]);
        }
    }
    let properties = document
        .pointer_mut("/extensions/EXT_structural_metadata/propertyTables/0/properties")
        .and_then(serde_json::Value::as_object_mut);
    for property in properties
        .into_iter()
        .flat_map(|fields| fields.values_mut())
    {
        reindex(&mut property["values"]);
        if property.get("stringOffsets").is_some() {
            reindex(&mut property["stringOffsets"]);
        }
    }
    // draco-gltf encodes every primitive attribute, so only instance attributes and edge visibility still name a view
    let accessors = document["accessors"]
        .as_array_mut()
        .expect("an accessors array");
    for accessor in accessors {
        if accessor.get("bufferView").is_some() {
            reindex(&mut accessor["bufferView"]);
        }
    }

    for (mesh_index, encoded) in encodings.iter().enumerate() {
        let draco_view = kept_views.len();
        let draco_offset = append_aligned(&mut packed, encoded.bytes());
        kept_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": draco_offset,
            "byteLength": encoded.bytes().len()
        }));

        let attribute_ids: serde_json::Map<String, serde_json::Value> = encoded
            .extension(draco_view)
            .attributes()
            .map(|(semantic, id)| (semantic.to_string(), serde_json::json!(id)))
            .collect();
        document["meshes"][mesh_index]["primitives"][0]["extensions"][KHR_DRACO_MESH_COMPRESSION] = serde_json::json!({
            "bufferView": draco_view,
            "attributes": attribute_ids
        });
    }
    document["bufferViews"] = serde_json::json!(kept_views);
    document["buffers"] = serde_json::json!([{ "byteLength": packed.len() }]);
    require_extension(document, KHR_DRACO_MESH_COMPRESSION);
    Ok(packed)
}

/// Write mesh data as a binary glTF 2.0 (GLB) file.
pub fn write_glb<W: Write>(mesh: &GlbMesh, writer: &mut W) -> io::Result<()> {
    let (document, bin) = uncompressed_parts(mesh)?;
    write_container(&document, &bin, writer)
}

fn uncompressed_parts(mesh: &GlbMesh) -> io::Result<(serde_json::Value, Vec<u8>)> {
    let layout = compute_layout(mesh);
    let mut bin = Vec::with_capacity(layout.total_len);
    write_bin(mesh, &layout, &mut bin)?;
    let mut document = build_document(mesh, &layout);
    append_edge_visibility(&mut document, &mut bin, mesh);
    append_instanced_meshes(&mut document, &mut bin, mesh)?;
    Ok((document, bin))
}

pub fn write_compressed_glb<W: Write>(
    mesh: &GlbMesh,
    compression: MeshCompression,
    writer: &mut W,
) -> io::Result<()> {
    let has_indices = mesh
        .indices
        .as_ref()
        .is_some_and(|indices| !indices.is_empty());
    if compression == MeshCompression::None || (!has_indices && mesh.instanced_meshes.is_empty()) {
        return write_glb(mesh, writer);
    }
    let (mut document, bin) = uncompressed_parts(mesh)?;
    let geometries = primitive_geometries(mesh);
    let packed = if compression == MeshCompression::Draco {
        apply_draco(&mut document, &bin, &geometries)?
    } else {
        apply_meshopt(&mut document, &bin, &geometries)?
    };
    write_container(&document, &packed, writer)
}

pub(crate) fn write_container<W: Write>(
    document: &serde_json::Value,
    bin: &[u8],
    writer: &mut W,
) -> io::Result<()> {
    let json_str = serde_json::to_string(document).expect("JSON serialisation failed");
    let json_bytes = json_str.as_bytes();
    let json_padded_len = align4(json_bytes.len());

    let bin_padded_len = bin.len();

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
        writer.write_all(bin)?;
    }

    Ok(())
}

/// Write mesh data as a GLB file to disk.
pub fn write_glb_file(mesh: &GlbMesh, path: &Path) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    write_glb(mesh, &mut file)
}

pub fn write_compressed_glb_file(
    mesh: &GlbMesh,
    compression: MeshCompression,
    path: &Path,
) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    write_compressed_glb(mesh, compression, &mut file)
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        }
    }

    #[test]
    fn accessor_elements_are_their_component_count_times_the_component_size() {
        let accessor = |kind: &str, component_type: u32| serde_json::json!({ "bufferView": 0, "type": kind, "componentType": component_type });
        assert_eq!(
            accessor_element_bytes(&accessor("SCALAR", COMPONENT_UNSIGNED_INT)),
            4
        );
        assert_eq!(
            accessor_element_bytes(&accessor("SCALAR", COMPONENT_FLOAT)),
            4
        );
        assert_eq!(
            accessor_element_bytes(&accessor("VEC2", COMPONENT_FLOAT)),
            8
        );
        assert_eq!(
            accessor_element_bytes(&accessor("VEC3", COMPONENT_FLOAT)),
            12
        );
        assert_eq!(
            accessor_element_bytes(&accessor("VEC4", COMPONENT_UNSIGNED_BYTE)),
            4
        );
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
        assert_eq!(ext["schema"]["id"], "tiletopia");

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

    fn mesh_with_feature_ids(feature_ids: Vec<u32>) -> GlbMesh {
        GlbMesh {
            positions: vec![[0.0, 0.0, 0.0]; feature_ids.len()],
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: Some(feature_ids),
            texture: None,
            base_color_factor: None,
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        }
    }

    #[test]
    fn feature_ids_are_written_as_floats() {
        let mesh = mesh_with_feature_ids(vec![0, 1, 1 << 24]);
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        let primitive = &json["meshes"][0]["primitives"][0];
        let accessor_index = primitive["attributes"]["_FEATURE_ID_0"].as_u64().unwrap() as usize;
        let accessor = &json["accessors"][accessor_index];
        assert_eq!(accessor["componentType"], COMPONENT_FLOAT);

        let view = &json["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        let offset = view["byteOffset"].as_u64().unwrap() as usize;
        let length = view["byteLength"].as_u64().unwrap() as usize;
        let bin = glb.bin.unwrap();
        let written: Vec<f32> = bin[offset..offset + length]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        assert_eq!(written, vec![0.0, 1.0, 16_777_216.0]);
    }

    #[test]
    fn feature_id_above_float_precision_is_refused() {
        let mesh = mesh_with_feature_ids(vec![(1 << 24) + 1]);
        let mut buf = Vec::new();
        let error = write_glb(&mesh, &mut buf).unwrap_err();
        assert!(error.to_string().contains("16777217"), "{error}");
    }

    fn triangle_with_edges() -> GlbMesh {
        GlbMesh {
            indices: Some(vec![0, 1, 2]),
            feature_ids: Some(vec![0, 0, 0]),
            edge_visibility: Some(EdgeVisibility {
                visibility: vec![26],
                silhouette_normals: vec![[0.0, 0.0, 1.0], [0.0, 0.6, 0.8]],
            }),
            ..triangle_mesh()
        }
    }

    fn accessor_bytes<'a>(json: &serde_json::Value, bin: &'a [u8], accessor: u64) -> &'a [u8] {
        let view = json["accessors"][accessor as usize]["bufferView"]
            .as_u64()
            .expect("an accessor with a buffer view");
        view_bytes(&json["bufferViews"][view as usize], bin)
    }

    #[test]
    fn edge_visibility_sits_beside_mesh_features_on_the_primitive() {
        let mesh = triangle_with_edges();
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        let bin = glb.bin.unwrap();
        assert_eq!(json["buffers"][0]["byteLength"], bin.len());
        let extensions = &json["meshes"][0]["primitives"][0]["extensions"];
        assert!(extensions["EXT_mesh_features"].is_object());
        let edges = &extensions[EXT_MESH_PRIMITIVE_EDGE_VISIBILITY];

        let visibility = edges["visibility"].as_u64().expect("a visibility accessor");
        let accessor = &json["accessors"][visibility as usize];
        assert_eq!(accessor["componentType"], COMPONENT_UNSIGNED_BYTE);
        assert_eq!(accessor["type"], "SCALAR");
        assert_eq!(accessor["count"], 1);
        assert_eq!(accessor_bytes(&json, &bin, visibility), [26]);

        let normals = edges["silhouetteNormals"]
            .as_u64()
            .expect("a silhouette normal accessor");
        let accessor = &json["accessors"][normals as usize];
        assert_eq!(accessor["componentType"], COMPONENT_FLOAT);
        assert_eq!(accessor["type"], "VEC3");
        assert_eq!(accessor["count"], 2);
        let written: Vec<[f32; 3]> =
            bytemuck::pod_collect_to_vec(accessor_bytes(&json, &bin, normals));
        assert_eq!(written, [[0.0, 0.0, 1.0], [0.0, 0.6, 0.8]]);

        let used = json["extensionsUsed"].as_array().unwrap();
        assert!(used.contains(&serde_json::json!(EXT_MESH_PRIMITIVE_EDGE_VISIBILITY)));
        assert!(json.get("extensionsRequired").is_none());
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
    fn a_node_translation_is_written_at_full_precision_and_left_out_when_absent() {
        let mut mesh = triangle_mesh();
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();
        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        assert!(json["nodes"][0].get("translation").is_none());

        mesh.node_translation = Some([500_000.125, 5_800_000.375, -12.5]);
        let mut buf = Vec::new();
        write_glb(&mesh, &mut buf).unwrap();
        let glb = gltf::Glb::from_slice(&buf).expect("should parse GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        assert_eq!(
            json["nodes"][0]["translation"],
            serde_json::json!([500_000.125, 5_800_000.375, -12.5])
        );
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
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
