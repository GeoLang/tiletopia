use crate::fbx_reader::Affine;
use crate::{BYTES_HELD_PER_VERTEX, IngestError, Material, MeshData, Texture, texture_image};
use dae_parser::{
    ArrayElement, ColorParam, Document, Effect, Geometry, GeometryElement, Image, ImageSource,
    Input, InputList, Instance, LocalMap, LookAt, Material as ColladaMaterial, Mesh, Node,
    Primitive, ProfileCommon, Rotate, Semantic, Shader, SurfaceInit, Transform, UpAxis, Url,
    UrlRef,
};
use percent_encoding::percent_decode_str;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

const COLLADA_VERSION_READ: &str = "1.4.1";

// quick-xml before 0.41 spends quadratic time on the attributes of one tag: RUSTSEC-2026-0194
const MAX_ATTRIBUTES_PER_TAG: usize = 256;

// dae-parser parses nested elements recursively and overflows the thread stack without a cap
const MAX_ELEMENT_DEPTH: usize = 128;

const MAX_NODE_VISITS: usize = 1_000_000;

// dae-parser 0.11 panics on the init elements and rejects valid polygons, linestrips, trifans and tristrips
const UNREADABLE_ELEMENTS: [&str; 7] = [
    "polygons",
    "linestrips",
    "trifans",
    "tristrips",
    "init_cube",
    "init_volume",
    "init_planar",
];

fn parse_error(error: dae_parser::Error) -> IngestError {
    IngestError::ParseError(format!("COLLADA parse error: {error:?}"))
}

fn check_tag(tag: &BytesStart<'_>) -> Result<(), IngestError> {
    let local_name = tag.local_name();
    let name = String::from_utf8_lossy(local_name.as_ref());
    if UNREADABLE_ELEMENTS.contains(&name.as_ref()) {
        return Err(IngestError::UnsupportedFormat(format!(
            "COLLADA <{name}>: dae-parser cannot read this element, \
             re-export the model with triangles and 2D textures"
        )));
    }

    let mut attributes = tag.attributes();
    attributes.with_checks(false);
    if attributes.nth(MAX_ATTRIBUTES_PER_TAG).is_some() {
        return Err(IngestError::ParseError(format!(
            "COLLADA <{name}> has more than {MAX_ATTRIBUTES_PER_TAG} attributes"
        )));
    }
    Ok(())
}

fn refuse_other_collada_version(root: &BytesStart<'_>) -> Result<(), IngestError> {
    if root.local_name().as_ref() != b"COLLADA" {
        return Ok(());
    }
    let version = root
        .try_get_attribute("version")
        .map_err(|error| IngestError::ParseError(format!("COLLADA XML error: {error}")))?;
    let Some(version) = version else {
        return Ok(());
    };
    let version = String::from_utf8_lossy(&version.value);
    if version != COLLADA_VERSION_READ {
        return Err(IngestError::UnsupportedFormat(format!(
            "COLLADA {version}: dae-parser reads {COLLADA_VERSION_READ} only, \
             re-export the model as COLLADA {COLLADA_VERSION_READ}"
        )));
    }
    Ok(())
}

fn refuse_unreadable_document(path: &Path) -> Result<(), IngestError> {
    let xml_error =
        |error: quick_xml::Error| IngestError::ParseError(format!("COLLADA XML error: {error}"));
    let mut reader = Reader::from_file(path).map_err(xml_error)?;
    let mut buffer = Vec::new();
    let mut depth = 0;
    let mut root_seen = false;
    let mut check_next_tag = |tag: &BytesStart<'_>| {
        check_tag(tag)?;
        if !root_seen {
            root_seen = true;
            refuse_other_collada_version(tag)?;
        }
        Ok::<(), IngestError>(())
    };
    loop {
        match reader.read_event_into(&mut buffer).map_err(xml_error)? {
            Event::Eof => return Ok(()),
            Event::End(_) => depth -= 1,
            Event::Start(tag) => {
                check_next_tag(&tag)?;
                depth += 1;
                if depth > MAX_ELEMENT_DEPTH {
                    return Err(IngestError::ParseError(format!(
                        "COLLADA nests elements deeper than {MAX_ELEMENT_DEPTH} levels"
                    )));
                }
            }
            Event::Empty(tag) => check_next_tag(&tag)?,
            _ => {}
        }
        buffer.clear();
    }
}

fn up_axis_to_y_up(up_axis: UpAxis) -> Affine {
    let linear = match up_axis {
        UpAxis::XUp => [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
        UpAxis::YUp => Affine::IDENTITY.linear,
        UpAxis::ZUp => [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, -1.0, 0.0]],
    };
    Affine {
        linear,
        ..Affine::IDENTITY
    }
}

pub fn read(path: &Path, max_bytes_held: u64) -> Result<Vec<MeshData>, IngestError> {
    refuse_unreadable_document(path)?;
    let document = Document::from_file(path).map_err(parse_error)?;
    let scene = document.get_visual_scene().ok_or_else(|| {
        IngestError::ParseError("COLLADA has no <scene> naming a visual scene".to_string())
    })?;

    let metres_per_unit = f64::from(document.asset.unit.meter);
    if !metres_per_unit.is_finite() || metres_per_unit <= 0.0 {
        return Err(IngestError::ParseError(format!(
            "COLLADA <unit meter=\"{metres_per_unit}\"> is not a positive length"
        )));
    }
    let root = up_axis_to_y_up(document.asset.up_axis).times(Affine::scaling([metres_per_unit; 3]));

    let mut walk = SceneWalk {
        libraries: SceneLibraries {
            geometries: document.local_map().map_err(parse_error)?,
            nodes: document.local_map().map_err(parse_error)?,
            materials: document.local_map().map_err(parse_error)?,
            effects: document.local_map().map_err(parse_error)?,
            images: document.local_map().map_err(parse_error)?,
            mesh_dir: path.parent().unwrap_or(Path::new(".")),
        },
        diffuse_by_material: HashMap::new(),
        node_visits: 0,
        vertex_count: 0,
        texture_bytes_held: 0,
        max_bytes_held,
        meshes: Vec::new(),
    };
    for node in &scene.nodes {
        walk.visit(node, root, 0)?;
    }

    tracing::info!(
        "Read {} meshes from {} ({} total vertices)",
        walk.meshes.len(),
        path.display(),
        walk.vertex_count,
    );

    Ok(walk.meshes)
}

fn normalized(vector: [f64; 3]) -> Result<[f64; 3], IngestError> {
    let length = vector.iter().map(|c| c * c).sum::<f64>().sqrt();
    if length == 0.0 {
        return Err(IngestError::ParseError(
            "COLLADA transform has a zero-length axis".to_string(),
        ));
    }
    Ok(vector.map(|c| c / length))
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn rotation_matrix(rotate: &Rotate) -> Result<Affine, IngestError> {
    let [x, y, z] = normalized(rotate.axis().map(f64::from))?;
    let (sin, cos) = f64::from(rotate.angle()).to_radians().sin_cos();
    let turn = 1.0 - cos;
    Ok(Affine {
        linear: [
            [
                turn * x * x + cos,
                turn * x * y - sin * z,
                turn * x * z + sin * y,
            ],
            [
                turn * x * y + sin * z,
                turn * y * y + cos,
                turn * y * z - sin * x,
            ],
            [
                turn * x * z - sin * y,
                turn * y * z + sin * x,
                turn * z * z + cos,
            ],
        ],
        ..Affine::IDENTITY
    })
}

fn look_at_matrix(look_at: &LookAt) -> Result<Affine, IngestError> {
    let eye = look_at.eye().map(f64::from);
    let target = look_at.target().map(f64::from);
    let forward = normalized([target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]])?;
    let right = normalized(cross(forward, look_at.up().map(f64::from)))?;
    let up = cross(right, forward);
    Ok(Affine {
        linear: [
            [right[0], up[0], -forward[0]],
            [right[1], up[1], -forward[1]],
            [right[2], up[2], -forward[2]],
        ],
        translation: eye,
    })
}

fn transform_matrix(transform: &Transform) -> Result<Affine, IngestError> {
    match transform {
        Transform::Translate(translate) => Ok(Affine::translation((*translate.0).map(f64::from))),
        Transform::Scale(scale) => Ok(Affine::scaling((*scale.0).map(f64::from))),
        Transform::Rotate(rotate) => rotation_matrix(rotate),
        Transform::LookAt(look_at) => look_at_matrix(look_at),
        // COLLADA writes matrix elements row by row, with the translation in the last column
        Transform::Matrix(matrix) => {
            let element = |row: usize, column: usize| f64::from(matrix.0[row * 4 + column]);
            Ok(Affine {
                linear: [0, 1, 2].map(|row| [0, 1, 2].map(|column| element(row, column))),
                translation: [element(0, 3), element(1, 3), element(2, 3)],
            })
        }
        Transform::Skew(_) => Err(IngestError::UnsupportedFormat(
            "COLLADA <skew>: this reader does not apply skew transforms".to_string(),
        )),
    }
}

struct FloatSource<'a> {
    values: &'a [f32],
    start: usize,
    stride: usize,
    // COLLADA accessors skip the array slot of a param that has no name
    components: Vec<usize>,
}

impl FloatSource<'_> {
    fn read<const N: usize>(&self, index: u32) -> Result<[f32; N], IngestError> {
        let out_of_range = || {
            IngestError::ParseError(format!(
                "COLLADA index {index} is outside its source of {N} component values"
            ))
        };
        if self.components.len() < N {
            return Err(out_of_range());
        }
        // dae-parser accepts any usize for the accessor's offset and stride
        let first = (index as usize)
            .saturating_mul(self.stride)
            .saturating_add(self.start);
        let mut value = [0.0; N];
        for (slot, component) in value.iter_mut().zip(&self.components) {
            *slot = *self
                .values
                .get(first.saturating_add(*component))
                .ok_or_else(out_of_range)?;
            if !slot.is_finite() {
                return Err(IngestError::ParseError(format!(
                    "COLLADA source value {slot} at index {index} is not a finite number"
                )));
            }
        }
        Ok(value)
    }
}

fn float_source<'a>(mesh: &'a Mesh, url: &Url) -> Result<FloatSource<'a>, IngestError> {
    let source = match url {
        Url::Fragment(id) => mesh
            .sources
            .iter()
            .find(|source| source.id.as_deref() == Some(id)),
        Url::Other(_) => None,
    };
    let Some(source) = source else {
        return Err(IngestError::ParseError(format!(
            "COLLADA input names source {url}, which this mesh does not hold"
        )));
    };
    let Some(ArrayElement::Float(array)) = &source.array else {
        return Err(IngestError::ParseError(format!(
            "COLLADA source {url} holds no float array"
        )));
    };
    let accessor = &source.accessor;
    Ok(FloatSource {
        values: &array.val,
        start: accessor.offset,
        stride: accessor.stride,
        components: accessor
            .param
            .iter()
            .enumerate()
            .filter(|(_, param)| param.name.is_some())
            .map(|(position, _)| position)
            .collect(),
    })
}

struct IndexedSource<'a> {
    source: FloatSource<'a>,
    input_offset: usize,
}

struct PrimitiveSources<'a> {
    position: IndexedSource<'a>,
    normal: Option<IndexedSource<'a>>,
    texcoord: Option<IndexedSource<'a>>,
}

fn primitive_sources<'a>(
    mesh: &'a Mesh,
    inputs: &'a InputList,
    texcoord_set: Option<u32>,
) -> Result<PrimitiveSources<'a>, IngestError> {
    let vertex_input = inputs
        .iter()
        .find(|input| input.semantic == Semantic::Vertex)
        .ok_or_else(|| {
            IngestError::ParseError("COLLADA primitive has no VERTEX input".to_string())
        })?;
    let vertices = mesh
        .vertices
        .as_ref()
        .filter(|vertices| matches!(&vertex_input.source, Url::Fragment(id) if *id == vertices.id))
        .ok_or_else(|| {
            IngestError::ParseError(format!(
                "COLLADA VERTEX input names {}, which this mesh does not hold",
                vertex_input.source
            ))
        })?;
    let vertex_offset = vertex_input.offset as usize;

    let beside_position = |input: &Input| -> Result<IndexedSource<'a>, IngestError> {
        Ok(IndexedSource {
            source: float_source(mesh, &input.source)?,
            input_offset: vertex_offset,
        })
    };
    let attribute = |semantic: Semantic, set: Option<u32>| {
        let own_input = inputs.iter().find(|input| {
            input.semantic == semantic && set.is_none_or(|wanted| input.set.unwrap_or(0) == wanted)
        });
        if let Some(input) = own_input {
            return Ok(Some(IndexedSource {
                source: float_source(mesh, &input.source)?,
                input_offset: input.offset as usize,
            }));
        }
        vertices
            .inputs
            .iter()
            .find(|input| input.semantic == semantic)
            .map(beside_position)
            .transpose()
    };

    Ok(PrimitiveSources {
        position: beside_position(vertices.position_input())?,
        normal: attribute(Semantic::Normal, None)?,
        texcoord: attribute(Semantic::TexCoord, texcoord_set)?,
    })
}

struct PrimitiveTriangles<'a> {
    material_symbol: Option<&'a str>,
    inputs: &'a InputList,
    indices: &'a [u32],
    corners: Vec<[usize; 3]>,
}

fn polylist_indices_cover_every_corner(
    vertex_counts: &[u32],
    stride: usize,
    index_count: usize,
) -> bool {
    let corner_count: u64 = vertex_counts.iter().map(|&count| u64::from(count)).sum();
    // dae-parser accepts a <polylist> with no <input>, whose stride of 0 fits any <vcount> total
    stride != 0 && corner_count.checked_mul(stride as u64) == Some(index_count as u64)
}

fn fan_triangles(vertex_counts: &[u32]) -> Vec<[usize; 3]> {
    let mut corners = Vec::new();
    let mut first = 0;
    for &vertex_count in vertex_counts {
        let vertex_count = vertex_count as usize;
        for corner in 1..vertex_count.saturating_sub(1) {
            corners.push([first, first + corner, first + corner + 1]);
        }
        first += vertex_count;
    }
    corners
}

fn primitive_triangles(
    primitive: &Primitive,
) -> Result<Option<PrimitiveTriangles<'_>>, IngestError> {
    match primitive {
        Primitive::Triangles(triangles) => {
            let indices = triangles.data.prim.as_deref().unwrap_or_default();
            let triangle_count = indices
                .len()
                .checked_div(triangles.inputs.stride * 3)
                .unwrap_or(0);
            Ok(Some(PrimitiveTriangles {
                material_symbol: triangles.material.as_deref(),
                inputs: &triangles.inputs,
                indices,
                corners: (0..triangle_count)
                    .map(|triangle| [triangle * 3, triangle * 3 + 1, triangle * 3 + 2])
                    .collect(),
            }))
        }
        Primitive::PolyList(list) => {
            let vertex_counts = &list.data.vcount;
            if !polylist_indices_cover_every_corner(
                vertex_counts,
                list.inputs.stride,
                list.data.prim.len(),
            ) {
                return Err(IngestError::ParseError(
                    "COLLADA <polylist>: <vcount> does not match its <input> offsets \
                     and <p> indices"
                        .to_string(),
                ));
            }
            Ok(Some(PrimitiveTriangles {
                material_symbol: list.material.as_deref(),
                inputs: &list.inputs,
                indices: &list.data.prim,
                corners: fan_triangles(vertex_counts),
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Clone)]
struct DiffuseShading {
    material: Material,
    texcoord_symbol: Option<String>,
}

struct SceneLibraries<'a> {
    geometries: LocalMap<'a, Geometry>,
    nodes: LocalMap<'a, Node>,
    materials: LocalMap<'a, ColladaMaterial>,
    effects: LocalMap<'a, Effect>,
    images: LocalMap<'a, Image>,
    mesh_dir: &'a Path,
}

impl SceneLibraries<'_> {
    fn diffuse_texture(
        &self,
        effect: &Effect,
        profile: &ProfileCommon,
        texture_name: &str,
    ) -> Option<Texture> {
        // some COLLADA exporters put the image id in <texture> instead of a sampler
        let image_id = profile
            .get_param(effect, texture_name)
            .and_then(|param| param.ty.as_sampler2d())
            .and_then(|sampler| profile.get_param(effect, &sampler.source))
            .and_then(|param| param.ty.as_surface())
            .and_then(|surface| match &surface.init {
                SurfaceInit::From { image, .. } => Some(image.as_str()),
                _ => None,
            })
            .unwrap_or(texture_name);

        let Some(image) = self.images.get_str(image_id) else {
            tracing::warn!("COLLADA image {image_id} not found, mesh stays untextured");
            return None;
        };
        match &image.source {
            ImageSource::Data(bytes) => texture_image::from_bytes(bytes.to_vec(), image_id),
            ImageSource::InitFrom(Url::Other(uri)) => {
                let name = percent_decode_str(uri.strip_prefix("file://").unwrap_or(uri))
                    .decode_utf8_lossy();
                texture_image::resolve(self.mesh_dir, &name)
                    .and_then(|path| texture_image::from_file(&path))
            }
            ImageSource::InitFrom(Url::Fragment(_)) => {
                tracing::warn!("COLLADA image {image_id} names no file, mesh stays untextured");
                None
            }
        }
    }

    fn diffuse_shading(&self, target: &UrlRef<ColladaMaterial>) -> Option<DiffuseShading> {
        let effect = self
            .materials
            .get(target)
            .and_then(|material| self.effects.get(&material.instance_effect.url));
        let Some(effect) = effect else {
            tracing::warn!("COLLADA material {target} has no effect, mesh stays uncoloured");
            return None;
        };
        let profile = effect.get_common_profile()?;
        let diffuse = profile
            .technique
            .data
            .shaders
            .iter()
            .find_map(|shader| match shader {
                Shader::Blinn(blinn) => blinn.diffuse.as_ref(),
                Shader::Lambert(lambert) => lambert.diffuse.as_ref(),
                Shader::Phong(phong) => phong.diffuse.as_ref(),
                // COLLADA <constant> shading has no diffuse term and paints its emission colour
                Shader::Constant(constant) => constant.emission.as_ref(),
            })?;
        match &**diffuse {
            ColorParam::Color(rgba) => Some(DiffuseShading {
                material: Material {
                    base_color_factor: **rgba,
                    texture: None,
                },
                texcoord_symbol: None,
            }),
            ColorParam::Texture(texture) => Some(DiffuseShading {
                material: Material {
                    base_color_factor: [1.0; 4],
                    texture: self.diffuse_texture(effect, profile, &texture.texture),
                },
                texcoord_symbol: Some(texture.texcoord.clone()),
            }),
            ColorParam::Param(name) => {
                tracing::warn!("COLLADA diffuse param {name} is not read, mesh stays uncoloured");
                None
            }
        }
    }
}

struct SceneWalk<'a> {
    libraries: SceneLibraries<'a>,
    diffuse_by_material: HashMap<String, Option<DiffuseShading>>,
    node_visits: usize,
    vertex_count: usize,
    texture_bytes_held: usize,
    max_bytes_held: u64,
    meshes: Vec<MeshData>,
}

fn check_bytes_held(
    texture_bytes: usize,
    vertex_count: usize,
    max_bytes_held: u64,
) -> Result<(), IngestError> {
    let vertex_bytes = (vertex_count as u64).saturating_mul(BYTES_HELD_PER_VERTEX);
    let bytes_held = (texture_bytes as u64).saturating_add(vertex_bytes);
    if bytes_held > max_bytes_held {
        return Err(IngestError::ParseError(format!(
            "COLLADA scene has {texture_bytes} bytes of texture copies and {vertex_count} \
             vertices at {BYTES_HELD_PER_VERTEX} bytes each, past the {max_bytes_held} byte limit"
        )));
    }
    Ok(())
}

impl SceneWalk<'_> {
    fn visit(&mut self, node: &Node, parent: Affine, depth: usize) -> Result<(), IngestError> {
        self.node_visits += 1;
        if self.node_visits > MAX_NODE_VISITS {
            return Err(IngestError::ParseError(format!(
                "COLLADA scene expands past {MAX_NODE_VISITS} nodes"
            )));
        }
        if depth >= MAX_ELEMENT_DEPTH {
            return Err(IngestError::ParseError(format!(
                "COLLADA scene nests deeper than {MAX_ELEMENT_DEPTH} levels, \
                 or an <instance_node> names its own ancestor"
            )));
        }

        let mut world = parent;
        for transform in &node.transforms {
            world = world.times(transform_matrix(transform)?);
        }

        for instance in &node.instance_geometry {
            self.add_geometry(instance, &world)?;
        }
        if !node.instance_controller.is_empty() {
            tracing::warn!(
                "COLLADA node {:?}: skinned and morphed geometry is not read",
                node.id
            );
        }
        for instance in &node.instance_node {
            let target = self.libraries.nodes.get(&instance.url).ok_or_else(|| {
                IngestError::ParseError(format!(
                    "COLLADA <instance_node> names {}, which the file does not hold",
                    instance.url
                ))
            })?;
            self.visit(target, world, depth + 1)?;
        }
        for child in &node.children {
            self.visit(child, world, depth + 1)?;
        }
        Ok(())
    }

    fn add_geometry(
        &mut self,
        instance: &Instance<Geometry>,
        world: &Affine,
    ) -> Result<(), IngestError> {
        let geometry = self
            .libraries
            .geometries
            .get(&instance.url)
            .ok_or_else(|| {
                IngestError::ParseError(format!(
                    "COLLADA <instance_geometry> names {}, which the file does not hold",
                    instance.url
                ))
            })?;
        let GeometryElement::Mesh(mesh) = &geometry.element else {
            tracing::warn!(
                "COLLADA geometry {:?}: only <mesh> geometry is read",
                geometry.id
            );
            return Ok(());
        };
        let name = geometry
            .name
            .as_deref()
            .or(geometry.id.as_deref())
            .unwrap_or("unnamed");

        for primitive in &mesh.elements {
            let Some(triangles) = primitive_triangles(primitive)? else {
                continue;
            };
            if triangles.corners.is_empty() {
                continue;
            }

            let binding = triangles
                .material_symbol
                .and_then(|symbol| instance.get_instance_material(symbol));
            let shading = match binding {
                Some(binding) => {
                    let libraries = &self.libraries;
                    self.diffuse_by_material
                        .entry(binding.target.to_string())
                        .or_insert_with(|| libraries.diffuse_shading(&binding.target))
                        .as_ref()
                }
                None => None,
            };
            let texture_bytes = shading
                .and_then(|shading| shading.material.texture.as_ref())
                .map_or(0, |texture| texture.image_data.len());
            self.texture_bytes_held = self.texture_bytes_held.saturating_add(texture_bytes);
            check_bytes_held(
                self.texture_bytes_held,
                self.vertex_count,
                self.max_bytes_held,
            )?;
            let shading = shading.cloned();
            let texcoord_set = binding
                .zip(shading.as_ref())
                .and_then(|(binding, shading)| {
                    let symbol = shading.texcoord_symbol.as_deref()?;
                    binding
                        .bind_vertex_input
                        .iter()
                        .find(|bound| {
                            bound.semantic == symbol && bound.input_semantic == "TEXCOORD"
                        })?
                        .input_set
                });

            let sources = primitive_sources(mesh, triangles.inputs, texcoord_set)?;
            let material = shading.map(|shading| shading.material);
            let read = read_triangles(&triangles, &sources, world, name, material)?;

            self.vertex_count += read.positions.len();
            check_bytes_held(
                self.texture_bytes_held,
                self.vertex_count,
                self.max_bytes_held,
            )?;
            self.meshes.push(read);
        }
        Ok(())
    }
}

fn read_triangles(
    triangles: &PrimitiveTriangles<'_>,
    sources: &PrimitiveSources<'_>,
    world: &Affine,
    name: &str,
    material: Option<Material>,
) -> Result<MeshData, IngestError> {
    let stride = triangles.inputs.stride;
    let index_of = |corner: usize, offset: usize| {
        triangles
            .indices
            .get(corner * stride + offset)
            .copied()
            .ok_or_else(|| IngestError::ParseError("COLLADA <p> holds too few indices".to_string()))
    };

    let mut mesh = MeshData {
        positions: Vec::new(),
        normals: Vec::new(),
        texcoords: Vec::new(),
        indices: Vec::with_capacity(triangles.corners.len() * 3),
        name: name.to_string(),
        material,
        asset_id: None,
        class: None,
        storey: None,
        element_color: None,
    };
    let mut vertex_by_source_indices: HashMap<[u32; 3], u32> = HashMap::new();
    let mirrored = world.is_mirroring();

    for triangle in &triangles.corners {
        let mut vertices = [0; 3];
        for (vertex, &corner) in vertices.iter_mut().zip(triangle) {
            let position_index = index_of(corner, sources.position.input_offset)?;
            let normal_index = match &sources.normal {
                Some(normal) => index_of(corner, normal.input_offset)?,
                None => 0,
            };
            let texcoord_index = match &sources.texcoord {
                Some(texcoord) => index_of(corner, texcoord.input_offset)?,
                None => 0,
            };

            *vertex = match vertex_by_source_indices.entry([
                position_index,
                normal_index,
                texcoord_index,
            ]) {
                Entry::Occupied(known) => *known.get(),
                Entry::Vacant(unseen) => {
                    let position: [f32; 3] = sources.position.source.read(position_index)?;
                    let placed = world.apply(position.map(f64::from));
                    if !placed.iter().all(|c| c.is_finite()) {
                        return Err(IngestError::ParseError(format!(
                            "COLLADA node transforms place position {position:?} at {placed:?}, \
                             which is not finite"
                        )));
                    }
                    mesh.positions.push(placed);
                    if let Some(normal) = &sources.normal {
                        let direction: [f32; 3] = normal.source.read(normal_index)?;
                        mesh.normals.push(
                            world
                                .apply_normal(direction.map(f64::from))
                                .map(|c| c as f32),
                        );
                    }
                    if let Some(texcoord) = &sources.texcoord {
                        let [u, v]: [f32; 2] = texcoord.source.read(texcoord_index)?;
                        // COLLADA counts t from the bottom of the image, glTF from the top
                        mesh.texcoords.push([u, 1.0 - v]);
                    }
                    *unseen.insert(mesh.positions.len() as u32 - 1)
                }
            };
        }
        // mirroring turns a counter-clockwise triangle clockwise
        if mirrored {
            vertices.swap(1, 2);
        }
        mesh.indices.extend_from_slice(&vertices);
    }
    Ok(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/assimp_quad/quad.dae"
    );

    // one triangle, (1,2,3) (4,2,3) (1,5,3), normals +z, uvs at the three image corners
    const TRIANGLE_GEOMETRY: &str = r##"
  <library_geometries>
    <geometry id="tri" name="tri"><mesh>
      <source id="p"><float_array id="p-a" count="9">1 2 3 4 2 3 1 5 3</float_array>
        <technique_common><accessor source="#p-a" count="3" stride="3">
          <param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/>
        </accessor></technique_common></source>
      <source id="n"><float_array id="n-a" count="3">0 0 1</float_array>
        <technique_common><accessor source="#n-a" count="1" stride="3">
          <param name="X" type="float"/><param name="Y" type="float"/><param name="Z" type="float"/>
        </accessor></technique_common></source>
      <source id="t"><float_array id="t-a" count="6">0 0 1 0 0 1</float_array>
        <technique_common><accessor source="#t-a" count="3" stride="2">
          <param name="S" type="float"/><param name="T" type="float"/>
        </accessor></technique_common></source>
      <vertices id="v"><input semantic="POSITION" source="#p"/></vertices>
      <triangles count="1" material="paint">
        <input semantic="VERTEX" source="#v" offset="0"/>
        <input semantic="NORMAL" source="#n" offset="1"/>
        <input semantic="TEXCOORD" source="#t" offset="2" set="0"/>
        <p>0 0 0 1 0 1 2 0 2</p>
      </triangles>
    </mesh></geometry>
  </library_geometries>"##;

    const UNBOUND_TRIANGLE: &str = r##"<instance_geometry url="#tri"/>"##;

    fn document(asset_extra: &str, libraries: &str, scene_nodes: &str) -> String {
        format!(
            r##"<?xml version="1.0" encoding="utf-8"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <asset><created>2020-01-01T00:00:00</created><modified>2020-01-01T00:00:00</modified>{asset_extra}</asset>
  {libraries}
  <library_visual_scenes><visual_scene id="s">{scene_nodes}</visual_scene></library_visual_scenes>
  <scene><instance_visual_scene url="#s"/></scene>
</COLLADA>"##
        )
    }

    fn read_text(text: &str) -> Result<Vec<MeshData>, IngestError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.dae");
        std::fs::write(&path, text).unwrap();
        read(&path, u64::MAX)
    }

    fn read_triangle_under(node_body: &str) -> Vec<MeshData> {
        let node = format!("<node>{node_body}{UNBOUND_TRIANGLE}</node>");
        read_text(&document("", TRIANGLE_GEOMETRY, &node)).unwrap()
    }

    fn first_position_within(meshes: &[MeshData], expected: [f64; 3]) -> bool {
        let actual = meshes[0].positions[0];
        (0..3).all(|axis| (actual[axis] - expected[axis]).abs() < 1e-5)
    }

    fn png_hex(width: u32, height: u32) -> String {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([1, 2, 3, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded
            .into_inner()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn lambert(effect_id: &str, rgba: &str) -> String {
        format!(
            r#"<effect id="{effect_id}"><profile_COMMON><technique sid="common"><lambert>
                 <diffuse><color>{rgba}</color></diffuse>
               </lambert></technique></profile_COMMON></effect>"#
        )
    }

    fn bound_triangle(material_id: &str) -> String {
        format!(
            r##"<instance_geometry url="#tri"><bind_material><technique_common>
                  <instance_material symbol="paint" target="#{material_id}"/>
                </technique_common></bind_material></instance_geometry>"##
        )
    }

    fn unsupported_reason(text: &str) -> String {
        match read_text(text) {
            Err(IngestError::UnsupportedFormat(reason)) => reason,
            other => panic!("expected an unsupported format, got {other:?}"),
        }
    }

    fn parse_failure(text: &str) -> String {
        match read_text(text) {
            Err(IngestError::ParseError(reason)) => reason,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn an_assimp_export_comes_through_with_its_transforms_uvs_and_texture() {
        let meshes = read(Path::new(FIXTURE), u64::MAX).unwrap();
        assert_eq!(meshes.len(), 1);
        let mesh = &meshes[0];

        // the node chain scales the unit quad by 2 and then moves it 10 along x
        assert_eq!(
            mesh.positions,
            vec![
                [10.0, 0.0, 0.0],
                [12.0, 0.0, 0.0],
                [12.0, 2.0, 0.0],
                [10.0, 2.0, 0.0],
            ]
        );
        assert_eq!(mesh.normals, vec![[0.0, 0.0, 1.0]; 4]);
        assert_eq!(mesh.indices, vec![0, 1, 2, 0, 2, 3]);
        // assimp flipped t going from glTF to COLLADA, the reader flips it back
        assert_eq!(
            mesh.texcoords,
            vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
        );

        let material = mesh.material.as_ref().expect("a material");
        assert_eq!(material.base_color_factor, [1.0; 4]);
        let texture = material.texture.as_ref().expect("a texture");
        assert_eq!(texture.mime_type, "image/png");
        assert_eq!((texture.width, texture.height), (8, 4));
    }

    #[test]
    fn read_mesh_takes_an_uppercase_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("QUAD.DAE");
        std::fs::copy(FIXTURE, &path).unwrap();
        assert_eq!(crate::read_mesh(&path, u64::MAX).unwrap().len(), 1);
    }

    #[test]
    fn an_assimp_export_tiles_through_the_mesh_tiler() {
        let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> = read(Path::new(FIXTURE), u64::MAX)
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();
        let dir = tempfile::tempdir().unwrap();

        let stats = tiletopia_core::mesh_tiler::tile_meshes(
            &meshes,
            dir.path(),
            &tiletopia_core::mesh_tiler::MeshTilingConfig::default(),
        )
        .unwrap();
        assert!(stats.tile_count > 0);

        let tile = crate::gltf_reader::read(&dir.path().join("tiles/root.glb")).unwrap();
        let material = tile[0].material.as_ref().expect("a material in the tile");
        assert!(material.texture.is_some(), "the tile lost its texture");
        assert_eq!(tile[0].texcoords.len(), tile[0].positions.len());
    }

    #[test]
    fn z_up_comes_out_y_up_in_positions_and_normals() {
        let up_axis = "<up_axis>Z_UP</up_axis>";
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let meshes = read_text(&document(up_axis, TRIANGLE_GEOMETRY, &node)).unwrap();
        assert_eq!(meshes[0].positions[0], [1.0, 3.0, -2.0]);
        assert_eq!(meshes[0].normals[0], [0.0, 1.0, 0.0]);
    }

    #[test]
    fn x_up_comes_out_y_up() {
        let up_axis = "<up_axis>X_UP</up_axis>";
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let meshes = read_text(&document(up_axis, TRIANGLE_GEOMETRY, &node)).unwrap();
        assert_eq!(meshes[0].positions[0], [-2.0, 1.0, 3.0]);
    }

    #[test]
    fn the_unit_scales_to_metres_and_leaves_normals_unit_length() {
        let unit = r#"<unit name="inch" meter="0.5"/>"#;
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let meshes = read_text(&document(unit, TRIANGLE_GEOMETRY, &node)).unwrap();
        assert_eq!(meshes[0].positions[0], [0.5, 1.0, 1.5]);
        assert_eq!(meshes[0].normals[0], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn transforms_apply_last_written_first() {
        let meshes = read_triangle_under(
            "<translate>10 0 0</translate><rotate>0 0 1 90</rotate><scale>2 2 2</scale>",
        );
        // (1,2,3) scales to (2,4,6), turns to (-4,2,6) and moves to (6,2,6)
        assert!(
            first_position_within(&meshes, [6.0, 2.0, 6.0]),
            "{:?}",
            meshes[0].positions[0]
        );
    }

    #[test]
    fn a_parent_transform_reaches_its_child() {
        let child = format!("<node><scale>2 2 2</scale>{UNBOUND_TRIANGLE}</node>");
        let node = format!("<node><translate>10 0 0</translate>{child}</node>");
        let meshes = read_text(&document("", TRIANGLE_GEOMETRY, &node)).unwrap();
        assert_eq!(meshes[0].positions[0], [12.0, 4.0, 6.0]);
    }

    #[test]
    fn a_lookat_places_the_node_at_the_eye() {
        let meshes = read_triangle_under("<lookat>0 0 5 0 0 0 0 1 0</lookat>");
        assert!(
            first_position_within(&meshes, [1.0, 2.0, 8.0]),
            "{:?}",
            meshes[0].positions[0]
        );
    }

    #[test]
    fn a_library_node_instanced_twice_lands_at_both_places() {
        let libraries = format!(
            "{TRIANGLE_GEOMETRY}<library_nodes><node id=\"part\">{UNBOUND_TRIANGLE}</node></library_nodes>"
        );
        let nodes = r##"
            <node><translate>10 0 0</translate><instance_node url="#part"/></node>
            <node><translate>20 0 0</translate><instance_node url="#part"/></node>"##;
        let meshes = read_text(&document("", &libraries, nodes)).unwrap();
        assert_eq!(meshes.len(), 2);
        assert_eq!(meshes[0].positions[0], [11.0, 2.0, 3.0]);
        assert_eq!(meshes[1].positions[0], [21.0, 2.0, 3.0]);
    }

    #[test]
    fn one_geometry_takes_a_different_material_per_instance() {
        let libraries = format!(
            r##"{TRIANGLE_GEOMETRY}
            <library_effects>{}{}</library_effects>
            <library_materials>
              <material id="red"><instance_effect url="#red-fx"/></material>
              <material id="blue"><instance_effect url="#blue-fx"/></material>
            </library_materials>"##,
            lambert("red-fx", "1 0 0 1"),
            lambert("blue-fx", "0 0 1 1"),
        );
        let nodes = format!(
            "<node>{}</node><node>{}</node>",
            bound_triangle("red"),
            bound_triangle("blue")
        );
        let meshes = read_text(&document("", &libraries, &nodes)).unwrap();
        let colour = |mesh: &MeshData| mesh.material.as_ref().unwrap().base_color_factor;
        assert_eq!(colour(&meshes[0]), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(colour(&meshes[1]), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn two_primitives_in_one_geometry_split_into_two_meshes_by_material() {
        let geometry = TRIANGLE_GEOMETRY.replace(
            "</triangles>",
            r##"</triangles>
            <triangles count="1" material="trim">
              <input semantic="VERTEX" source="#v" offset="0"/>
              <p>0 1 2</p>
            </triangles>"##,
        );
        let libraries = format!(
            r##"{geometry}
            <library_effects>{}{}</library_effects>
            <library_materials>
              <material id="red"><instance_effect url="#red-fx"/></material>
              <material id="blue"><instance_effect url="#blue-fx"/></material>
            </library_materials>"##,
            lambert("red-fx", "1 0 0 1"),
            lambert("blue-fx", "0 0 1 1"),
        );
        let nodes = r##"<node><instance_geometry url="#tri"><bind_material><technique_common>
              <instance_material symbol="paint" target="#red"/>
              <instance_material symbol="trim" target="#blue"/>
            </technique_common></bind_material></instance_geometry></node>"##;
        let meshes = read_text(&document("", &libraries, nodes)).unwrap();
        assert_eq!(meshes.len(), 2);
        let colour = |mesh: &MeshData| mesh.material.as_ref().unwrap().base_color_factor;
        assert_eq!(colour(&meshes[0]), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(colour(&meshes[1]), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn a_mirroring_scale_keeps_triangles_facing_out() {
        let meshes = read_triangle_under("<scale>-1 1 1</scale>");
        assert_eq!(meshes[0].indices, vec![0, 2, 1]);
        assert_eq!(meshes[0].positions[0], [-1.0, 2.0, 3.0]);
        assert_eq!(meshes[0].normals[0], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_polylist_fans_into_triangles() {
        let geometry = TRIANGLE_GEOMETRY
            .replace(
                "count=\"9\">1 2 3 4 2 3 1 5 3",
                "count=\"12\">0 0 0 1 0 0 1 1 0 0 1 0",
            )
            .replace(
                "<accessor source=\"#p-a\" count=\"3\"",
                "<accessor source=\"#p-a\" count=\"4\"",
            );
        let start = geometry.find("<triangles").unwrap();
        let end = geometry.find("</triangles>").unwrap() + "</triangles>".len();
        let polylist = r##"<polylist count="1">
              <input semantic="VERTEX" source="#v" offset="0"/>
              <vcount>4</vcount><p>0 1 2 3</p>
            </polylist>"##;
        let geometry = format!("{}{polylist}{}", &geometry[..start], &geometry[end..]);
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let meshes = read_text(&document("", &geometry, &node)).unwrap();
        assert_eq!(meshes[0].indices, vec![0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn a_polylist_with_no_inputs_fails_before_any_triangles_are_built() {
        let start = TRIANGLE_GEOMETRY.find("<triangles").unwrap();
        let end = TRIANGLE_GEOMETRY.find("</triangles>").unwrap() + "</triangles>".len();
        let geometry = format!(
            r#"{}<polylist count="1"><vcount>4</vcount></polylist>{}"#,
            &TRIANGLE_GEOMETRY[..start],
            &TRIANGLE_GEOMETRY[end..]
        );
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let reason = parse_failure(&document("", &geometry, &node));
        assert!(reason.contains("<vcount>"), "{reason}");
    }

    #[test]
    fn a_vcount_total_that_wraps_past_u64_does_not_match_empty_indices() {
        let two_to_the_31 = 1 << 31;
        assert!(!polylist_indices_cover_every_corner(
            &[u32::MAX, u32::MAX, 2],
            two_to_the_31,
            0
        ));
        assert!(polylist_indices_cover_every_corner(&[3, 4], 2, 14));
    }

    #[test]
    fn an_accessor_stride_past_the_address_space_is_an_index_error() {
        let source = FloatSource {
            values: &[0.0, 0.0, 0.0],
            start: 0,
            stride: usize::MAX,
            components: vec![0, 1, 2],
        };
        let error = source.read::<3>(2).unwrap_err();
        assert!(error.to_string().contains("index 2"), "{error}");
    }

    #[test]
    fn a_position_that_is_not_a_number_fails_naming_it() {
        let geometry = TRIANGLE_GEOMETRY.replace("1 2 3 4 2 3 1 5 3", "NaN 2 3 4 2 3 1 5 3");
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let reason = parse_failure(&document("", &geometry, &node));
        assert!(reason.contains("not a finite number"), "{reason}");
    }

    #[test]
    fn a_transform_that_is_not_a_number_fails_naming_the_position() {
        let node = format!("<node><scale>NaN 1 1</scale>{UNBOUND_TRIANGLE}</node>");
        let reason = parse_failure(&document("", TRIANGLE_GEOMETRY, &node));
        assert!(reason.contains("not finite"), "{reason}");
    }

    #[test]
    fn an_image_embedded_as_hex_reaches_the_material() {
        let libraries = format!(
            r##"{TRIANGLE_GEOMETRY}
            <library_images><image id="img"><data>{}</data></image></library_images>
            <library_effects><effect id="fx"><profile_COMMON>
              <newparam sid="surface"><surface type="2D"><init_from>img</init_from></surface></newparam>
              <newparam sid="sampler"><sampler2D><source>surface</source></sampler2D></newparam>
              <technique sid="common"><lambert><diffuse>
                <texture texture="sampler" texcoord="UVSET"/>
              </diffuse></lambert></technique>
            </profile_COMMON></effect></library_effects>
            <library_materials><material id="m"><instance_effect url="#fx"/></material></library_materials>"##,
            png_hex(4, 2),
        );
        let nodes = format!("<node>{}</node>", bound_triangle("m"));
        let meshes = read_text(&document("", &libraries, &nodes)).unwrap();
        let texture = meshes[0]
            .material
            .as_ref()
            .and_then(|material| material.texture.as_ref())
            .expect("a texture");
        assert_eq!((texture.width, texture.height), (4, 2));
        assert_eq!(meshes[0].texcoords[0], [0.0, 1.0]);
    }

    #[test]
    fn a_percent_encoded_windows_file_uri_finds_the_image_beside_the_model() {
        let dir = tempfile::tempdir().unwrap();
        image::RgbaImage::from_pixel(6, 3, image::Rgba([1, 2, 3, 255]))
            .save(dir.path().join("old wall.png"))
            .unwrap();
        let libraries = format!(
            r##"{TRIANGLE_GEOMETRY}
            <library_images><image id="img">
              <init_from>file:///C:/models/old%20wall.png</init_from>
            </image></library_images>
            <library_effects><effect id="fx"><profile_COMMON>
              <technique sid="common"><lambert><diffuse>
                <texture texture="img" texcoord="UVSET"/>
              </diffuse></lambert></technique>
            </profile_COMMON></effect></library_effects>
            <library_materials><material id="m"><instance_effect url="#fx"/></material></library_materials>"##
        );
        let nodes = format!("<node>{}</node>", bound_triangle("m"));
        let path = dir.path().join("model.dae");
        std::fs::write(&path, document("", &libraries, &nodes)).unwrap();

        let meshes = read(&path, u64::MAX).unwrap();
        let texture = meshes[0]
            .material
            .as_ref()
            .and_then(|material| material.texture.as_ref())
            .expect("a texture");
        assert_eq!((texture.width, texture.height), (6, 3));
    }

    #[test]
    fn a_missing_image_file_leaves_the_mesh_untextured() {
        let libraries = format!(
            r##"{TRIANGLE_GEOMETRY}
            <library_images><image id="img"><init_from>gone.png</init_from></image></library_images>
            <library_effects><effect id="fx"><profile_COMMON>
              <technique sid="common"><lambert><diffuse>
                <texture texture="img" texcoord="UVSET"/>
              </diffuse></lambert></technique>
            </profile_COMMON></effect></library_effects>
            <library_materials><material id="m"><instance_effect url="#fx"/></material></library_materials>"##
        );
        let nodes = format!("<node>{}</node>", bound_triangle("m"));
        let meshes = read_text(&document("", &libraries, &nodes)).unwrap();
        let material = meshes[0].material.as_ref().expect("a material");
        assert!(material.texture.is_none());
        assert_eq!(meshes[0].positions.len(), 3);
    }

    #[test]
    fn the_uv_set_bound_to_the_diffuse_texture_is_the_one_read() {
        let geometry = TRIANGLE_GEOMETRY
            .replace(
                "<vertices id=\"v\">",
                r##"<source id="t1"><float_array id="t1-a" count="6">0.5 0.5 0.25 0.5 0.5 0.25</float_array>
                  <technique_common><accessor source="#t1-a" count="3" stride="2">
                    <param name="S" type="float"/><param name="T" type="float"/>
                  </accessor></technique_common></source>
                <vertices id="v">"##,
            )
            .replace(
                r##"<input semantic="TEXCOORD" source="#t" offset="2" set="0"/>"##,
                r##"<input semantic="TEXCOORD" source="#t" offset="2" set="0"/>
                    <input semantic="TEXCOORD" source="#t1" offset="2" set="1"/>"##,
            );
        let libraries = format!(
            r##"{geometry}
            <library_images><image id="img"><data>{}</data></image></library_images>
            <library_effects><effect id="fx"><profile_COMMON>
              <technique sid="common"><lambert><diffuse>
                <texture texture="img" texcoord="UVSET"/>
              </diffuse></lambert></technique>
            </profile_COMMON></effect></library_effects>
            <library_materials><material id="m"><instance_effect url="#fx"/></material></library_materials>"##,
            png_hex(1, 1),
        );
        let nodes = r##"<node><instance_geometry url="#tri"><bind_material><technique_common>
              <instance_material symbol="paint" target="#m">
                <bind_vertex_input semantic="UVSET" input_semantic="TEXCOORD" input_set="1"/>
              </instance_material>
            </technique_common></bind_material></instance_geometry></node>"##;
        let meshes = read_text(&document("", &libraries, nodes)).unwrap();
        assert_eq!(meshes[0].texcoords[1], [0.25, 0.5]);
    }

    #[test]
    fn collada_1_5_is_refused_naming_the_version_and_the_way_out() {
        let text = r#"<?xml version="1.0"?>
<COLLADA xmlns="http://www.collada.org/2008/03/COLLADASchema" version="1.5.0">
  <asset><created>2020-01-01T00:00:00</created><modified>2020-01-01T00:00:00</modified></asset>
</COLLADA>"#;
        let reason = unsupported_reason(text);
        assert!(reason.contains("1.5.0"), "{reason}");
        assert!(reason.contains("1.4.1"), "{reason}");
    }

    #[test]
    fn elements_dae_parser_cannot_read_are_refused_by_name() {
        for element in UNREADABLE_ELEMENTS {
            let text = document(
                "",
                &format!("<library_geometries><{element}/></library_geometries>"),
                "",
            );
            let reason = unsupported_reason(&text);
            assert!(reason.contains(element), "{reason}");
        }
    }

    #[test]
    fn a_skew_transform_is_refused_by_name() {
        let node = format!("<node><skew>45 1 0 0 0 1 0</skew>{UNBOUND_TRIANGLE}</node>");
        let reason = unsupported_reason(&document("", TRIANGLE_GEOMETRY, &node));
        assert!(reason.contains("skew"), "{reason}");
    }

    #[test]
    fn a_tag_with_too_many_attributes_is_refused_before_the_parser_sees_it() {
        let attributes: String = (0..=MAX_ATTRIBUTES_PER_TAG)
            .map(|position| format!(" a{position}=\"0\""))
            .collect();
        let node = format!("<node{attributes}/>");
        let reason = parse_failure(&document("", "", &node));
        assert!(reason.contains("attributes"), "{reason}");
    }

    #[test]
    fn nesting_at_the_depth_cap_reads_and_one_past_it_is_refused() {
        // the document, the visual scene library and the visual scene hold three of the levels
        let nested = |levels: usize| {
            format!(
                "{}{UNBOUND_TRIANGLE}{}",
                "<node>".repeat(levels),
                "</node>".repeat(levels)
            )
        };
        let at_the_cap = document("", TRIANGLE_GEOMETRY, &nested(MAX_ELEMENT_DEPTH - 3));
        assert_eq!(read_text(&at_the_cap).unwrap().len(), 1);

        let past_the_cap = document("", TRIANGLE_GEOMETRY, &nested(MAX_ELEMENT_DEPTH));
        let reason = parse_failure(&past_the_cap);
        assert!(reason.contains("deeper"), "{reason}");
    }

    #[test]
    fn a_scene_past_the_byte_limit_is_refused() {
        let vertex_bytes = 2 * BYTES_HELD_PER_VERTEX as usize;
        assert!(check_bytes_held(1000 - vertex_bytes, 2, 1000).is_ok());
        let error = check_bytes_held(1000 - vertex_bytes + 1, 2, 1000).unwrap_err();
        assert!(
            error.to_string().contains("past the 1000 byte limit"),
            "{error}"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.dae");
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        std::fs::write(&path, document("", TRIANGLE_GEOMETRY, &node)).unwrap();
        let three_vertices = 3 * BYTES_HELD_PER_VERTEX;
        assert_eq!(read(&path, three_vertices).unwrap()[0].positions.len(), 3);
        assert!(read(&path, three_vertices - 1).is_err());
    }

    #[test]
    fn an_instance_node_cycle_is_refused() {
        let libraries = r##"<library_nodes>
            <node id="a"><instance_node url="#b"/></node>
            <node id="b"><instance_node url="#a"/></node>
          </library_nodes>"##;
        let nodes = r##"<node><instance_node url="#a"/></node>"##;
        let reason = parse_failure(&document("", libraries, nodes));
        assert!(reason.contains("ancestor"), "{reason}");
    }

    #[test]
    fn instance_nodes_doubling_at_every_level_are_refused() {
        let levels: String = (0..40)
            .map(|level| {
                let next = level + 1;
                format!(
                    r##"<node id="n{level}"><instance_node url="#n{next}"/><instance_node url="#n{next}"/></node>"##
                )
            })
            .collect();
        let libraries = format!("<library_nodes>{levels}<node id=\"n40\"/></library_nodes>");
        let nodes = r##"<node><instance_node url="#n0"/></node>"##;
        let reason = parse_failure(&document("", &libraries, nodes));
        assert!(reason.contains("expands past"), "{reason}");
    }

    #[test]
    fn an_index_past_its_source_fails_without_a_panic() {
        let geometry =
            TRIANGLE_GEOMETRY.replace("<p>0 0 0 1 0 1 2 0 2</p>", "<p>9 0 0 1 0 1 2 0 2</p>");
        let node = format!("<node>{UNBOUND_TRIANGLE}</node>");
        let reason = parse_failure(&document("", &geometry, &node));
        assert!(reason.contains("index 9"), "{reason}");
    }

    #[test]
    fn a_reference_to_a_missing_geometry_fails_naming_it() {
        let reason = parse_failure(&document(
            "",
            "",
            r##"<node><instance_geometry url="#gone"/></node>"##,
        ));
        assert!(reason.contains("#gone"), "{reason}");
    }

    #[test]
    fn a_document_without_a_scene_fails_saying_so() {
        let text = r#"<?xml version="1.0"?>
<COLLADA xmlns="http://www.collada.org/2005/11/COLLADASchema" version="1.4.1">
  <asset><created>2020-01-01T00:00:00</created><modified>2020-01-01T00:00:00</modified></asset>
</COLLADA>"#;
        let reason = parse_failure(text);
        assert!(reason.contains("<scene>"), "{reason}");
    }

    #[test]
    fn text_that_is_not_collada_fails_as_a_parse_error() {
        let reason = parse_failure("not really collada");
        assert!(reason.contains("COLLADA"), "{reason}");
    }

    #[test]
    fn an_end_tag_with_no_start_tag_fails_as_a_parse_error() {
        let reason = parse_failure("</COLLADA>");
        assert!(reason.contains("COLLADA XML error"), "{reason}");
    }

    #[test]
    fn a_missing_file_fails() {
        assert!(read(Path::new("/nonexistent/model.dae"), u64::MAX).is_err());
    }
}
