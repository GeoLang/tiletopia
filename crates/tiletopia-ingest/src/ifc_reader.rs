//! IFC geometry reader.

use crate::crs_detect::{WGS84_GEODETIC_EPSG, parse_epsg_code};
use crate::dxf_reader::cross;
use crate::{BYTES_HELD_PER_MESH, BYTES_HELD_PER_VERTEX, IngestError, MeshData};
use ifc_lite_core::{
    AttributeValue, EntityDecoder, EntityScanner, GeoRefExtractor, GeoRefSource, GeoReference,
    GeoReferenceDetails, IfcType, build_entity_index, is_geometry_job, keyword_eq, slot,
};
use ifc_lite_geometry::representation::{Select, product_shape};
use ifc_lite_geometry::{
    GeometryRouter, Mesh, format_unsupported_breakdown, void_index_from_content,
};
use ifc_lite_processing::prepass::{PrepassSpans, ResolveOptions, resolve_prepass};
use ifc_lite_processing::style::{
    Rgba, default_color_for_type, element_material_fallback_color, resolve_element_color,
};
use memmap2::Mmap;
use std::path::Path;
use tiletopia_core::bounding_volume::Aabb;
use tiletopia_core::crs::{Coord3D, CrsDef, Route, Transformer};
use tiletopia_core::spatial::{enu_to_ecef_matrix, geodetic_to_ecef};

/// 0-based attribute positions every IfcRoot subtype shares.
const GLOBAL_ID_INDEX: usize = 0;
const NAME_INDEX: usize = 2;

const MOST_DROPPED_ELEMENT_REASONS: usize = 5;

#[derive(Debug, Default)]
pub struct DroppedElements {
    pub count: usize,
    pub first_reasons: Vec<String>,
}

impl DroppedElements {
    fn record(&mut self, reason: String) {
        self.count += 1;
        if self.first_reasons.len() < MOST_DROPPED_ELEMENT_REASONS {
            self.first_reasons.push(reason);
        }
    }

    pub fn summary(&self) -> Option<String> {
        (self.count > 0).then(|| {
            format!(
                "{} IFC elements lost geometry, first reasons: {}",
                self.count,
                self.first_reasons.join(" | ")
            )
        })
    }
}

pub struct IfcElements {
    pub meshes: Vec<MeshData>,
    pub dropped: DroppedElements,
}

pub fn mapped_ifc(path: &Path) -> Result<Mmap, IngestError> {
    let file = std::fs::File::open(path)?;
    // job inputs are never rewritten after the upload that queues their job
    unsafe { Mmap::map(&file) }.map_err(IngestError::Io)
}

pub fn read(path: &Path, max_bytes_held: u64) -> Result<Vec<MeshData>, IngestError> {
    let elements = read_elements(&mapped_ifc(path)?, max_bytes_held)?;
    if let Some(summary) = elements.dropped.summary() {
        tracing::warn!("{}: {summary}", path.display());
    }
    Ok(elements.meshes)
}

// UTM's central scale of 0.9996 is below this, and it moves a vertex 40 cm per km
const MOST_IGNORED_AXIS_FACTOR_DEVIATION: f64 = 0.001;

pub(crate) fn axis_factors_resizing_the_model(
    source: &[u8],
) -> Result<Option<[f64; 3]>, IngestError> {
    let declared = read_declared_georeference(source)?;
    let Some((georef, _)) = &declared.map_conversion else {
        return Ok(None);
    };
    let is_not_one = |value: f64| (value - 1.0).abs() > MOST_IGNORED_AXIS_FACTOR_DEVIATION;
    let axis_factors = [georef.factor_x, georef.factor_y, georef.factor_z];
    if !axis_factors.into_iter().any(is_not_one) {
        return Ok(None);
    }
    if axis_factors.iter().any(|factor| *factor < 0.0) {
        return Err(IngestError::ParseError(format!(
            "the IFC's IfcMapConversionScaled factors {axis_factors:?} mirror the model, \
             which the tiler does not apply"
        )));
    }
    // ifc-lite meshes in metres, so the factors alone are the resize only where Scale converts the project unit to the map unit
    let map_unit_metres = georef.map_unit_scale.unwrap_or(declared.length_unit_metres);
    if is_not_one(georef.scale * map_unit_metres / declared.length_unit_metres) {
        return Err(IngestError::ParseError(format!(
            "the IFC's IfcMapConversionScaled factors {axis_factors:?} come with a Scale of {}, \
             which does not convert its project unit to its map unit, so the model's size is unknown",
            georef.scale
        )));
    }
    Ok(Some(axis_factors))
}

fn resize_mesh(mesh: &mut MeshData, axis_factors: [f64; 3]) {
    for position in &mut mesh.positions {
        *position = std::array::from_fn(|axis| position[axis] * axis_factors[axis]);
    }
    for normal in &mut mesh.normals {
        let resized: [f64; 3] =
            std::array::from_fn(|axis| f64::from(normal[axis]) / axis_factors[axis]);
        let length = resized
            .iter()
            .map(|component| component * component)
            .sum::<f64>()
            .sqrt();
        // ifc-lite's calculate_normals starts every vertex at zero and degenerate triangles add nothing
        if length > 0.0 {
            *normal = resized.map(|component| (component / length) as f32);
        }
    }
}

fn bytes_held_by_mesh(mesh: &MeshData) -> u64 {
    let text_bytes = mesh.name.len() + mesh.asset_id.as_ref().map_or(0, String::len);
    (mesh.positions.len() as u64)
        .saturating_mul(BYTES_HELD_PER_VERTEX)
        .saturating_add((mesh.indices.len() as u64).saturating_mul(4))
        .saturating_add(text_bytes as u64)
        .saturating_add(BYTES_HELD_PER_MESH)
}

pub fn read_elements(source: &[u8], max_bytes_held: u64) -> Result<IfcElements, IngestError> {
    let index = build_entity_index(source);
    let mut decoder = EntityDecoder::with_index(source, index).with_strict_reals();
    let router = GeometryRouter::with_units(source, &mut decoder);
    if router.length_unit_refused() {
        return Err(IngestError::ParseError(
            "the IFC's length unit has no usable conversion factor".to_string(),
        ));
    }
    let axis_factors = axis_factors_resizing_the_model(source)?;
    let void_index = void_index_from_content(source, &mut decoder);

    let mut style_spans = PrepassSpans::default();
    let mut style_scanner = EntityScanner::new(source);
    while let Some((id, type_name, start, end)) = style_scanner.next_entity() {
        style_spans.stash(type_name, id, start, end);
    }
    let styles = resolve_prepass(
        &style_spans,
        &mut decoder,
        ResolveOptions {
            collect_indexed_colour_full: false,
            defer_attached_styles: false,
        },
    );
    let geometry_colors = styles
        .geometry_style_index
        .iter()
        .map(|(geometry_id, style)| (*geometry_id, style.color))
        .collect();

    let mut meshes = Vec::new();
    let mut bytes_held = 0u64;
    let mut dropped = DroppedElements::default();
    let mut scanner = EntityScanner::new(source);

    while let Some((id, type_name, start, end)) = scanner.next_entity() {
        // is_geometry_job follows the EXPRESS inheritance graph, so an IfcProduct
        // subtype no hardcoded list names still reaches the router
        if !is_geometry_job(type_name, &source[start..end]) {
            continue;
        }
        let ifc_type = IfcType::from_str(type_name);
        // ifc-lite meshes these volumes as solids that fill the rooms and holes they describe
        if ifc_type.is_subtype_of(IfcType::IfcOpeningElement)
            || ifc_type.is_subtype_of(IfcType::IfcSpace)
            || ifc_type.is_subtype_of(IfcType::IfcSpatialZone)
        {
            continue;
        }

        let entity = match decoder.decode_by_id(id) {
            Ok(entity) => entity,
            Err(error) => {
                dropped.record(format!("{type_name} #{id}: {error}"));
                continue;
            }
        };

        let outcome = router.process_element_with_voids_parts(&entity, &mut decoder, &void_index);
        let unsupported_items = router.take_unsupported_items();
        let failed_cuts: usize = router.take_csg_failures().values().map(Vec::len).sum();
        let parts = match outcome {
            Ok(parts) => parts,
            Err(error) => {
                dropped.record(format!("{type_name} #{id}: {error}"));
                continue;
            }
        };

        let meshed_nothing = parts.iter().all(Mesh::is_empty)
            && !router.host_consumed_by_void(id)
            && product_shape(&entity, &mut decoder)
                .ok()
                .flatten()
                .is_some_and(|shape| shape.select(&entity, Select::Meshed).next().is_some());
        if !unsupported_items.is_empty() {
            let breakdown = format_unsupported_breakdown(&unsupported_items);
            dropped.record(format!("{type_name} #{id}: items dropped: {breakdown}"));
        } else if failed_cuts > 0 {
            dropped.record(format!(
                "{type_name} #{id}: {failed_cuts} opening cuts failed"
            ));
        } else if meshed_nothing {
            dropped.record(format!("{type_name} #{id}: no triangles"));
        }

        let color = resolve_element_color(&entity, &geometry_colors, &mut decoder)
            .or_else(|| {
                styles
                    .element_material_colors
                    .get(&id)
                    .and_then(|colors| element_material_fallback_color(colors))
            })
            .map_or_else(|| default_color_for_type(ifc_type), Rgba::from_array);
        let mut element_color = color.to_rgba8();
        // ifc-lite draws alpha from 0.95 up as opaque
        if !color.is_transparent() {
            element_color[3] = u8::MAX;
        }

        for part in parts.into_iter().filter(|part| !part.is_empty()) {
            let mesh = MeshData {
                // ifc-lite stores vertices relative to Mesh.origin for far or void-cut hosts
                positions: part
                    .positions
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|position| {
                        std::array::from_fn(|axis| f64::from(position[axis]) + part.origin[axis])
                    })
                    .collect(),
                normals: part.normals.as_chunks::<3>().0.to_vec(),
                texcoords: Vec::new(),
                indices: part.indices,
                name: entity
                    .get_string(NAME_INDEX)
                    .unwrap_or("ifc_element")
                    .to_string(),
                material: None,
                asset_id: entity.get_string(GLOBAL_ID_INDEX).map(str::to_string),
                class: None,
                storey: None,
                element_color: Some(element_color),
            };
            bytes_held = bytes_held.saturating_add(bytes_held_by_mesh(&mesh));
            if bytes_held > max_bytes_held {
                return Err(IngestError::ParseError(format!(
                    "the IFC's meshes hold {bytes_held} bytes after #{id}, \
                     past the {max_bytes_held} byte limit"
                )));
            }
            meshes.push(mesh);
        }
    }

    if let Some(axis_factors) = axis_factors {
        for mesh in &mut meshes {
            resize_mesh(mesh, axis_factors);
        }
    }

    tracing::info!(
        "Read {} IFC meshes ({} total vertices), {} elements lost geometry",
        meshes.len(),
        meshes.iter().map(|m| m.positions.len()).sum::<usize>(),
        dropped.count,
    );

    Ok(IfcElements { meshes, dropped })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SitePlacement {
    pub longitude: f64,
    pub latitude: f64,
    pub elevation: f64,
    pub x_axis_from_east_radians: f64,
}

// GeoRefExtractor reads these keywords, and IFCPROJECT only for its plane-angle unit
const GEOREFERENCE_KEYWORDS: [&str; 9] = [
    "IFCSITE",
    "IFCPROJECT",
    "IFCPROPERTYSET",
    "IFCPROJECTEDCRS",
    "IFCMAPCONVERSION",
    "IFCMAPCONVERSIONSCALED",
    "IFCGEOGRAPHICCRS",
    "IFCWELLKNOWNTEXT",
    "IFCRIGIDOPERATION",
];

struct DeclaredGeoreference {
    map_conversion: Option<(GeoReference, GeoReferenceDetails)>,
    site: Option<GeoReference>,
    length_unit_metres: f64,
    true_north_from_y_axis_radians: f64,
}

fn parse_error(error: impl std::fmt::Display) -> IngestError {
    IngestError::ParseError(error.to_string())
}

fn read_declared_georeference(source: &[u8]) -> Result<DeclaredGeoreference, IngestError> {
    let mut decoder = EntityDecoder::with_index(source, build_entity_index(source));
    let mut scanner = EntityScanner::new(source);
    let mut entity_types = Vec::new();
    let mut true_north_direction = None;
    while let Some((id, type_name, _start, _end)) = scanner.next_entity() {
        if GEOREFERENCE_KEYWORDS
            .iter()
            .any(|keyword| keyword_eq(type_name, keyword))
        {
            entity_types.push((id, IfcType::from_str(type_name)));
        } else if true_north_direction.is_none()
            && keyword_eq(type_name, "IFCGEOMETRICREPRESENTATIONCONTEXT")
        {
            true_north_direction = decoder
                .decode_by_id(id)
                .map_err(parse_error)?
                .ref_at(slot::IfcGeometricRepresentationContext::TRUE_NORTH);
        }
    }

    let true_north_from_y_axis_radians = match true_north_direction {
        Some(id) => {
            let direction = decoder.decode_by_id(id).map_err(parse_error)?;
            let ratios = direction
                .get_list(slot::IfcDirection::DIRECTION_RATIOS.index())
                .unwrap_or_default();
            let north_x = ratios.first().and_then(AttributeValue::as_float);
            let north_y = ratios.get(1).and_then(AttributeValue::as_float);
            let (Some(north_x), Some(north_y)) = (north_x, north_y) else {
                return Err(IngestError::ParseError(format!(
                    "TrueNorth #{id} is not a direction"
                )));
            };
            north_x.atan2(north_y)
        }
        None => 0.0,
    };

    let map_conversion = GeoRefExtractor::extract_with_details(&mut decoder, &entity_types)
        .map_err(parse_error)?
        .filter(|(georef, _)| {
            matches!(
                georef.source,
                Some(GeoRefSource::MapConversion | GeoRefSource::EPSetMapConversion)
            )
        });
    // GeoRefExtractor reads a site only when the file declares no CRS
    let site_types: Vec<(u32, IfcType)> = entity_types
        .iter()
        .filter(|(_, ifc_type)| *ifc_type == IfcType::IfcSite)
        .cloned()
        .collect();
    let site = GeoRefExtractor::extract(&mut decoder, &site_types).map_err(parse_error)?;

    Ok(DeclaredGeoreference {
        map_conversion,
        site,
        length_unit_metres: decoder.length_unit_scale(),
        true_north_from_y_axis_radians,
    })
}

struct MapPosition {
    longitude: f64,
    latitude: f64,
    elevation: f64,
    grid_north_from_true_north_radians: f64,
    crs_origin: [f64; 2],
    projected_crs_unit_metres: Option<f64>,
}

const GRID_NORTH_PROBE_CRS_UNITS: f64 = 1000.0;

fn map_conversion_crs(
    georef: &GeoReference,
    details: &GeoReferenceDetails,
) -> Result<CrsDef, IngestError> {
    let crs_name = georef.crs_name.as_deref();
    let crs = match (crs_name.and_then(parse_epsg_code), &details.well_known_text) {
        (Some(epsg), _) => CrsDef::Epsg(epsg),
        (None, Some(well_known_text)) => CrsDef::Wkt(well_known_text.clone()),
        (None, None) => {
            return Err(IngestError::ParseError(format!(
                "the IFC's map conversion targets the CRS {}, which has no EPSG code or WKT",
                crs_name.unwrap_or("with no name")
            )));
        }
    };
    if georef.eastings == 0.0 && georef.northings == 0.0 {
        return Err(IngestError::ParseError(
            "the IFC's map conversion has zero eastings and northings".to_string(),
        ));
    }
    Ok(crs)
}

fn map_position(
    georef: &GeoReference,
    crs: CrsDef,
    length_unit_metres: f64,
) -> Result<MapPosition, IngestError> {
    let is_geographic = georef.map_unit.as_deref() == Some("DEGREE");
    let map_unit_metres = georef.map_unit_scale.unwrap_or(length_unit_metres);
    let projected_crs_unit_metres = if is_geographic {
        None
    } else {
        Some(crs.metres_per_axis_unit().ok_or_else(|| {
            IngestError::ParseError(format!(
                "the unit of the IFC's map CRS {} is unknown, so its map coordinates \
                 cannot be converted, upload it with longitude and latitude",
                georef.crs_name.as_deref().unwrap_or("with no name")
            ))
        })?)
    };
    let crs_units_per_map_unit =
        projected_crs_unit_metres.map_or(1.0, |crs_unit_metres| map_unit_metres / crs_unit_metres);
    let x = georef.eastings * crs_units_per_map_unit;
    let y = georef.northings * crs_units_per_map_unit;

    let transformer = Transformer::new(crs, CrsDef::Epsg(WGS84_GEODETIC_EPSG));
    let origin = transformer.transform(Coord3D { x, y, z: 0.0 })?;
    let grid_north_from_true_north_radians = if is_geographic {
        0.0
    } else {
        let north_of_origin = transformer.transform(Coord3D {
            x,
            y: y + GRID_NORTH_PROBE_CRS_UNITS,
            z: 0.0,
        })?;
        let east = (north_of_origin.x - origin.x).to_radians() * origin.y.to_radians().cos();
        let north = (north_of_origin.y - origin.y).to_radians();
        east.atan2(north)
    };

    Ok(MapPosition {
        longitude: origin.x,
        latitude: origin.y,
        elevation: georef.orthogonal_height * map_unit_metres,
        grid_north_from_true_north_radians,
        crs_origin: [x, y],
        projected_crs_unit_metres,
    })
}

fn placement_at(georef: &GeoReference, position: &MapPosition) -> SitePlacement {
    SitePlacement {
        longitude: position.longitude,
        latitude: position.latitude,
        elevation: position.elevation,
        x_axis_from_east_radians: georef.rotation() - position.grid_north_from_true_north_radians,
    }
}

pub fn site_placement(source: &[u8]) -> Result<Option<SitePlacement>, IngestError> {
    let declared = read_declared_georeference(source)?;
    let site_position = |x_axis_from_east_radians: f64| {
        declared.site.as_ref().map(|site| SitePlacement {
            longitude: site.eastings,
            latitude: site.northings,
            elevation: site.orthogonal_height * declared.length_unit_metres,
            x_axis_from_east_radians,
        })
    };

    let Some((georef, details)) = &declared.map_conversion else {
        return Ok(site_position(declared.true_north_from_y_axis_radians));
    };
    let crs = match map_conversion_crs(georef, details) {
        Ok(crs) => crs,
        Err(reason) => {
            return match site_position(georef.rotation()) {
                Some(placement) => {
                    tracing::warn!("placing the IFC by its IfcSite: {reason}");
                    Ok(Some(placement))
                }
                None => Err(reason),
            };
        }
    };
    let position = map_position(georef, crs, declared.length_unit_metres)?;
    Ok(Some(placement_at(georef, &position)))
}

pub fn x_axis_from_east_radians(source: &[u8]) -> Result<f64, IngestError> {
    let declared = read_declared_georeference(source)?;
    let Some((georef, details)) = &declared.map_conversion else {
        return Ok(declared.true_north_from_y_axis_radians);
    };
    let position = map_conversion_crs(georef, details)
        .and_then(|crs| map_position(georef, crs, declared.length_unit_metres));
    match position {
        Ok(position) => Ok(georef.rotation() - position.grid_north_from_true_north_radians),
        Err(reason) => {
            tracing::warn!("leaving out the IFC's grid convergence: {reason}");
            Ok(georef.rotation())
        }
    }
}

// placements are checked to 0.1 m or 0.1 percent of their distance from the map origin, whichever is larger
const LEAST_PLACEMENT_TOLERANCE_METRES: f64 = 0.1;
const PLACEMENT_TOLERANCE_SHARE_OF_DISTANCE: f64 = 0.001;
// Earth's curvature alone uses half of the 0.1 percent at 6.4 km from the origin
const FLAT_FRAME_SHARE_OF_PLACEMENT_TOLERANCE: f64 = 0.5;

pub struct MapConversionPlacement {
    pub origin: SitePlacement,
    to_geodetic: Route,
    crs_origin: [f64; 2],
    crs_unit_metres: f64,
    x_axis: [f64; 2],
    east_north_up_to_ecef: [f64; 16],
}

pub fn map_conversion_placement(
    source: &[u8],
) -> Result<Option<MapConversionPlacement>, IngestError> {
    let declared = read_declared_georeference(source)?;
    let Some((georef, details)) = &declared.map_conversion else {
        return Ok(None);
    };
    // site_placement places a model by its IfcSite when its map CRS has no code or WKT
    let Ok(crs) = map_conversion_crs(georef, details) else {
        return Ok(None);
    };
    let position = map_position(georef, crs.clone(), declared.length_unit_metres)?;
    let Some(crs_unit_metres) = position.projected_crs_unit_metres else {
        return Ok(None);
    };
    let origin = placement_at(georef, &position);
    Ok(Some(MapConversionPlacement {
        to_geodetic: Transformer::new(crs, CrsDef::Epsg(WGS84_GEODETIC_EPSG)).route()?,
        crs_origin: position.crs_origin,
        crs_unit_metres,
        x_axis: [georef.x_axis_abscissa, georef.x_axis_ordinate],
        east_north_up_to_ecef: enu_to_ecef_matrix(
            origin.latitude.to_radians(),
            origin.longitude.to_radians(),
            origin.elevation,
        ),
        origin,
    }))
}

fn bounds_of<'a>(positions: impl IntoIterator<Item = &'a [f64; 3]>) -> Option<Aabb> {
    let mut bounds = Aabb::empty();
    for position in positions {
        bounds.expand_point(*position);
    }
    (bounds.min[0] <= bounds.max[0]).then_some(bounds)
}

impl MapConversionPlacement {
    pub(crate) fn east_north_up_of(&self, local: [f64; 3]) -> Result<[f64; 3], IngestError> {
        let [x, y, up] = local;
        let [abscissa, ordinate] = self.x_axis;
        let geodetic = self.to_geodetic.apply(Coord3D {
            x: self.crs_origin[0] + (abscissa * x - ordinate * y) / self.crs_unit_metres,
            y: self.crs_origin[1] + (ordinate * x + abscissa * y) / self.crs_unit_metres,
            z: 0.0,
        })?;
        let ecef = geodetic_to_ecef(
            geodetic.y.to_radians(),
            geodetic.x.to_radians(),
            self.origin.elevation + up,
        );
        let frame = &self.east_north_up_to_ecef;
        let offset: [f64; 3] = std::array::from_fn(|axis| ecef[axis] - frame[12 + axis]);
        Ok(std::array::from_fn(|column| {
            (0..3)
                .map(|axis| frame[4 * column + axis] * offset[axis])
                .sum()
        }))
    }

    pub fn flat_frame_is_within_tolerance(&self, meshes: &[MeshData]) -> Result<bool, IngestError> {
        let Some(bounds) = bounds_of(meshes.iter().flat_map(|mesh| &mesh.positions)) else {
            return Ok(true);
        };
        let (sine, cosine) = self.origin.x_axis_from_east_radians.sin_cos();
        for corner in 0..8 {
            let local: [f64; 3] = std::array::from_fn(|axis| {
                if (corner >> axis) & 1 == 0 {
                    bounds.min[axis]
                } else {
                    bounds.max[axis]
                }
            });
            let flat = [
                cosine * local[0] - sine * local[1],
                sine * local[0] + cosine * local[1],
                local[2],
            ];
            let exact = self.east_north_up_of(local)?;
            let error = (0..3)
                .map(|axis| (flat[axis] - exact[axis]).powi(2))
                .sum::<f64>()
                .sqrt();
            let distance = local
                .iter()
                .map(|ordinate| ordinate * ordinate)
                .sum::<f64>()
                .sqrt();
            let tolerance = LEAST_PLACEMENT_TOLERANCE_METRES
                .max(PLACEMENT_TOLERANCE_SHARE_OF_DISTANCE * distance);
            if error > FLAT_FRAME_SHARE_OF_PLACEMENT_TOLERANCE * tolerance {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn place_in_east_north_up(&self, meshes: &mut [MeshData]) -> Result<(), IngestError> {
        for mesh in meshes {
            let Some(bounds) = bounds_of(&mesh.positions) else {
                continue;
            };
            let center = bounds.center();
            let at_center = self.east_north_up_of(center)?;
            let mut images_of_the_axes = [[0.0; 3]; 3];
            for (axis, image) in images_of_the_axes.iter_mut().enumerate() {
                let mut one_metre_along = center;
                one_metre_along[axis] += 1.0;
                let moved = self.east_north_up_of(one_metre_along)?;
                *image = std::array::from_fn(|component| moved[component] - at_center[component]);
            }
            let [along_x, along_y, along_z] = images_of_the_axes;
            // normals map by the inverse transpose, which is this cofactor matrix scaled by the determinant
            let cofactors = [
                cross(along_y, along_z),
                cross(along_z, along_x),
                cross(along_x, along_y),
            ];
            for normal in &mut mesh.normals {
                let turned: [f64; 3] = std::array::from_fn(|component| {
                    (0..3)
                        .map(|axis| cofactors[axis][component] * f64::from(normal[axis]))
                        .sum()
                });
                let length = turned
                    .iter()
                    .map(|component| component * component)
                    .sum::<f64>()
                    .sqrt();
                // ifc-lite's calculate_normals starts every vertex at zero and degenerate triangles add nothing
                if length > 0.0 {
                    *normal = turned.map(|component| (component / length) as f32);
                }
            }
            for position in &mut mesh.positions {
                *position = self.east_north_up_of(*position)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANGLE_TOLERANCE_RADIANS: f64 = 1e-3;
    const DEGREE_TOLERANCE: f64 = 1e-3;
    const HEIGHT_TOLERANCE_METRES: f64 = 1e-6;

    fn ifc_file(data: &str) -> Vec<u8> {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\n\
             FILE_NAME('fixture.ifc','2024-01-01',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\n\
             ENDSEC;\nDATA;\n{data}\nENDSEC;\nEND-ISO-10303-21;\n"
        )
        .into_bytes()
    }

    const PROJECT_IN_METRES: &str = "\
#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);
#7=IFCUNITASSIGNMENT((#8));
#8=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
#10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,$);
#11=IFCAXIS2PLACEMENT3D(#12,$,$);
#12=IFCCARTESIANPOINT((0.,0.,0.));
#13=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#10,$,.MODEL_VIEW.,$);
";

    const PROJECT_IN_MILLIMETRES: &str = "\
#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);
#7=IFCUNITASSIGNMENT((#8));
#8=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,$);
#11=IFCAXIS2PLACEMENT3D(#12,$,$);
#12=IFCCARTESIANPOINT((0.,0.,0.));
";

    const WALL_GLOBAL_ID: &str = "0Wall00000000000000000";

    // 4 by 0.3 by 2.5 metres, 3 cubic metres
    const WALL: &str = "\
#20=IFCLOCALPLACEMENT($,#21);
#21=IFCAXIS2PLACEMENT3D(#22,#23,#24);
#22=IFCCARTESIANPOINT((0.,0.,0.));
#23=IFCDIRECTION((0.,0.,1.));
#24=IFCDIRECTION((1.,0.,0.));
#30=IFCRECTANGLEPROFILEDEF(.AREA.,'WallProfile',#31,4.0,0.3);
#31=IFCAXIS2PLACEMENT2D(#32,#33);
#32=IFCCARTESIANPOINT((0.,0.));
#33=IFCDIRECTION((1.,0.));
#40=IFCEXTRUDEDAREASOLID(#30,#41,#42,2.5);
#41=IFCAXIS2PLACEMENT3D(#43,$,$);
#42=IFCDIRECTION((0.,0.,1.));
#43=IFCCARTESIANPOINT((0.,0.,0.));
#50=IFCSHAPEREPRESENTATION(#13,'Body','SweptSolid',(#40));
#51=IFCPRODUCTDEFINITIONSHAPE($,$,(#50));
#100=IFCWALL('0Wall00000000000000000',$,'Wall',$,$,#20,#51,'Tag',$);
";

    // 1.2 by 1.5 metres through the wall's 0.3 metre thickness, 0.54 cubic metres removed
    const OPENING: &str = "\
#110=IFCLOCALPLACEMENT(#20,#111);
#111=IFCAXIS2PLACEMENT3D(#112,#113,#114);
#112=IFCCARTESIANPOINT((0.,-0.5,1.25));
#113=IFCDIRECTION((0.,1.,0.));
#114=IFCDIRECTION((1.,0.,0.));
#127=IFCRECTANGLEPROFILEDEF(.AREA.,'Window',#128,1.2,1.5);
#128=IFCAXIS2PLACEMENT2D(#129,#130);
#129=IFCCARTESIANPOINT((0.,0.));
#130=IFCDIRECTION((1.,0.));
#131=IFCEXTRUDEDAREASOLID(#127,#132,#133,1.0);
#132=IFCAXIS2PLACEMENT3D(#134,$,$);
#133=IFCDIRECTION((0.,0.,1.));
#134=IFCCARTESIANPOINT((0.,0.,0.));
#140=IFCSHAPEREPRESENTATION(#13,'Body','SweptSolid',(#131));
#141=IFCPRODUCTDEFINITIONSHAPE($,$,(#140));
#200=IFCOPENINGELEMENT('0Opening00000000000000',$,'Window',$,$,#110,#141,$,.OPENING.);
";

    const OPENING_VOIDS_WALL: &str =
        "#300=IFCRELVOIDSELEMENT('0Voids000000000000000',$,$,$,#100,#200);\n";

    const SITE_WITHOUT_COORDINATES_WITH_THE_WALLS_BODY: &str =
        "#400=IFCSITE('0Site00000000000000000',$,'Site',$,$,#20,#51,$,.ELEMENT.,$,$,$,$,$);\n";

    fn volume(mesh: &MeshData) -> f64 {
        let point = |index: u32| mesh.positions[index as usize];
        let six_times_signed: f64 = mesh
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|triangle| {
                let (a, b, c) = (point(triangle[0]), point(triangle[1]), point(triangle[2]));
                a[0] * (b[1] * c[2] - b[2] * c[1])
                    + a[1] * (b[2] * c[0] - b[0] * c[2])
                    + a[2] * (b[0] * c[1] - b[1] * c[0])
            })
            .sum();
        (six_times_signed / 6.0).abs()
    }

    #[test]
    fn test_read_nonexistent_file() {
        let result = read(Path::new("/tmp/nonexistent_ifc_file.ifc"), u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_minimal_ifc() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("minimal.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('minimal.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        let result = read(&path, u64::MAX);
        assert!(result.is_ok());
        // Minimal IFC with no geometry should produce empty mesh list
        let meshes = result.unwrap();
        assert!(meshes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_mesh_triangulates_a_real_ifc4x3_file() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bath_csg_solid.ifc");

        let meshes = crate::read_mesh(&path, u64::MAX).expect("the fixture should read");
        let triangles: usize = meshes.iter().map(|m| m.indices.len() / 3).sum();
        assert!(!meshes.is_empty(), "no meshes from the fixture");
        assert!(triangles > 0, "no triangles from the fixture");
    }

    #[test]
    fn site_placement_reads_reference_coordinates() {
        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('site.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,(51,30,0),(-0,-7,-40),12.5,$,$);
ENDSEC;
END-ISO-10303-21;
";

        let placement = site_placement(ifc_content.as_bytes())
            .unwrap()
            .expect("a site placement");
        assert!((placement.latitude - 51.5).abs() < 1e-6, "{placement:?}");
        let expected_longitude = -(7.0 / 60.0 + 40.0 / 3600.0);
        assert!(
            (placement.longitude - expected_longitude).abs() < 1e-6,
            "{placement:?}"
        );
        assert!((placement.elevation - 12.5).abs() < 1e-6, "{placement:?}");
    }

    #[test]
    fn site_placement_is_none_when_the_site_leaves_them_unset() {
        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('minimal.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";

        assert_eq!(site_placement(ifc_content.as_bytes()).unwrap(), None);
    }

    #[test]
    fn test_read_ifc_with_extrusion() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_extrusion_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extrusion.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('extrusion.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#40=IFCWALL('0002',$,'TestWall',$,$,#12,#31,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        let meshes = read(&path, u64::MAX).expect("the extrusion should read");
        let first = meshes.first().expect("a mesh from the extruded wall");
        assert!(!first.positions.is_empty());
        assert!(!first.indices.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn surface_styles_colour_elements_and_only_clear_ones_stay_translucent() {
        let source = ifc_file(
            "#1=IFCPROJECT('0001',$,'Project',$,$,$,$,$,$);
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#13=IFCDIRECTION((0.,0.,1.));
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#22=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#23=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#21));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#32=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#33=IFCPRODUCTDEFINITIONSHAPE($,$,(#32));
#34=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#23));
#35=IFCPRODUCTDEFINITIONSHAPE($,$,(#34));
#40=IFCPLATE('pane',$,'Pane',$,$,#12,#31,$,$);
#41=IFCSLAB('slab',$,'Slab',$,$,#12,#33,$,$);
#42=IFCWALL('wall',$,'Wall',$,$,#12,#35,$,$);
#50=IFCCOLOURRGB($,0.2,0.4,0.6);
#51=IFCSURFACESTYLERENDERING(#50,0.6,$,$,$,$,$,$,.NOTDEFINED.);
#52=IFCSURFACESTYLE('glass',.BOTH.,(#51));
#53=IFCSTYLEDITEM(#21,(#52),$);
#60=IFCCOLOURRGB($,0.4,0.4,0.4);
#61=IFCSURFACESTYLERENDERING(#60,0.02,$,$,$,$,$,$,.NOTDEFINED.);
#62=IFCSURFACESTYLE('concrete',.BOTH.,(#61));
#63=IFCSTYLEDITEM(#22,(#62),$);",
        );

        let elements = read_elements(&source, u64::MAX).expect("the styled model should read");
        let color_of = |global_id: &str| {
            elements
                .meshes
                .iter()
                .find(|mesh| mesh.asset_id.as_deref() == Some(global_id))
                .and_then(|mesh| mesh.element_color)
                .unwrap_or_else(|| panic!("{global_id} has a coloured mesh"))
        };
        assert_eq!(color_of("pane"), [51, 102, 153, 102]);
        assert_eq!(color_of("slab"), [102, 102, 102, 255]);
        assert_eq!(color_of("wall"), [217, 217, 217, 255]);
    }

    #[test]
    fn an_opening_cuts_its_wall_and_is_no_mesh_of_its_own() {
        let uncut = read_elements(
            &ifc_file(&format!("{PROJECT_IN_METRES}{WALL}{OPENING}")),
            u64::MAX,
        )
        .unwrap()
        .meshes;
        let cut = read_elements(
            &ifc_file(&format!(
                "{PROJECT_IN_METRES}{WALL}{OPENING}{OPENING_VOIDS_WALL}"
            )),
            u64::MAX,
        )
        .unwrap()
        .meshes;

        assert_eq!(uncut.len(), 1, "only the wall is a mesh without the void");
        assert_eq!(cut.len(), 1, "only the wall is a mesh with the void");
        assert_eq!(cut[0].asset_id.as_deref(), Some(WALL_GLOBAL_ID));
        assert_ne!(cut[0].indices.len(), uncut[0].indices.len());
        assert!((volume(&uncut[0]) - 3.0).abs() < 0.01);
        assert!((volume(&uncut[0]) - volume(&cut[0]) - 0.54).abs() < 0.04);
    }

    #[test]
    fn spaces_and_spatial_zones_are_not_tiled() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #401=IFCSPACE('0Space0000000000000000',$,'Room',$,$,#20,#51,$,.ELEMENT.,.SPACE.,$);\n\
             #402=IFCSPATIALZONE('0Zone00000000000000000',$,'Zone',$,$,#20,#51,$,$);\n"
        ));

        let elements = read_elements(&source, u64::MAX).unwrap();

        assert_eq!(elements.meshes.len(), 1);
        assert_eq!(elements.meshes[0].asset_id.as_deref(), Some(WALL_GLOBAL_ID));
        assert_eq!(elements.dropped.count, 0);
    }

    #[test]
    fn a_site_with_a_body_is_tiled() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}{SITE_WITHOUT_COORDINATES_WITH_THE_WALLS_BODY}"
        ));

        let meshes = read_elements(&source, u64::MAX).unwrap().meshes;

        assert!(
            meshes
                .iter()
                .any(|mesh| mesh.asset_id.as_deref() == Some("0Site00000000000000000")),
            "the site's body is no mesh"
        );
    }

    #[test]
    fn an_element_that_fails_to_convert_is_counted_and_the_rest_still_read() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #101=IFCWALL('0Broken000000000000000',$,'Broken',$,$,#20,#999,'Tag',$);\n"
        ));

        let elements = read_elements(&source, u64::MAX).unwrap();

        assert_eq!(elements.meshes.len(), 1);
        assert_eq!(elements.dropped.count, 1);
        assert!(
            elements.dropped.first_reasons[0].contains("#101"),
            "{:?}",
            elements.dropped.first_reasons
        );
        let summary = elements.dropped.summary().expect("a summary of the drop");
        assert!(summary.starts_with("1 IFC elements"), "{summary}");
    }

    #[test]
    fn only_the_first_drop_reasons_are_kept_but_every_drop_counts() {
        let broken_walls: String = (500..510)
            .map(|id| format!("#{id}=IFCWALL('0Broken{id}',$,'Broken',$,$,#20,#999,'Tag',$);\n"))
            .collect();
        let source = ifc_file(&format!("{PROJECT_IN_METRES}{WALL}{broken_walls}"));

        let dropped = read_elements(&source, u64::MAX).unwrap().dropped;

        assert_eq!(dropped.count, 10);
        assert_eq!(dropped.first_reasons.len(), MOST_DROPPED_ELEMENT_REASONS);
    }

    #[test]
    fn a_product_with_no_representation_is_not_a_drop() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);\n"
        ));

        let elements = read_elements(&source, u64::MAX).unwrap();

        assert!(elements.meshes.is_empty());
        assert_eq!(elements.dropped.count, 0);
        assert_eq!(elements.dropped.summary(), None);
    }

    #[test]
    fn bytes_that_are_not_utf8_still_read() {
        let mut source = ifc_file(&format!("{PROJECT_IN_METRES}{WALL}"));
        let name_at = source
            .windows(b"'Wall'".len())
            .position(|window| window == b"'Wall'")
            .unwrap();
        source[name_at + 2] = 0xe9;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latin1.ifc");
        std::fs::write(&path, &source).unwrap();

        let meshes = read(&path, u64::MAX).expect("a Latin-1 name is no reason to fail");

        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].asset_id.as_deref(), Some(WALL_GLOBAL_ID));
    }

    #[test]
    fn ref_elevation_is_in_the_project_length_unit() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_MILLIMETRES}\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),12500.,$,$);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a site placement");

        assert!(
            (placement.elevation - 12.5).abs() < HEIGHT_TOLERANCE_METRES,
            "{placement:?}"
        );
    }

    #[test]
    fn true_north_along_the_x_axis_turns_x_to_north() {
        let source = ifc_file(
            "#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);\n\
             #7=IFCUNITASSIGNMENT((#8));\n\
             #8=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
             #10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,#14);\n\
             #11=IFCAXIS2PLACEMENT3D(#12,$,$);\n\
             #12=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #14=IFCDIRECTION((1.,0.));\n\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),$,$,$);\n",
        );

        let placement = site_placement(&source).unwrap().expect("a site placement");

        assert!(
            (placement.x_axis_from_east_radians - std::f64::consts::FRAC_PI_2).abs()
                < ANGLE_TOLERANCE_RADIANS,
            "{placement:?}"
        );
        assert!(
            (x_axis_from_east_radians(&source).unwrap() - std::f64::consts::FRAC_PI_2).abs()
                < ANGLE_TOLERANCE_RADIANS
        );
    }

    // UTM 32N (500000, 0) sits on 9 degrees east at the equator, where grid and true north agree
    const MAP_CONVERSION_TURNED_A_QUARTER: &str = "\
#60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);
#61=IFCMAPCONVERSION(#10,#60,500000.,0.,100.,0.,1.,1.);
";

    #[test]
    fn a_map_conversion_places_and_rotates_the_model() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{MAP_CONVERSION_TURNED_A_QUARTER}"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!(placement.latitude.abs() < DEGREE_TOLERANCE, "{placement:?}");
        assert!(
            (placement.elevation - 100.0).abs() < HEIGHT_TOLERANCE_METRES,
            "{placement:?}"
        );
        assert!(
            (placement.x_axis_from_east_radians - std::f64::consts::FRAC_PI_2).abs()
                < ANGLE_TOLERANCE_RADIANS,
            "{placement:?}"
        );
    }

    #[test]
    fn a_map_conversion_outranks_the_site() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{MAP_CONVERSION_TURNED_A_QUARTER}\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),$,$,$);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(placement.latitude.abs() < DEGREE_TOLERANCE, "{placement:?}");
    }

    #[test]
    fn grid_north_east_of_the_central_meridian_bears_east_of_true_north() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,600000.,5000000.,0.,1.,0.,1.);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        // UTM convergence there is about 0.9 degrees, and it turns the model clockwise
        assert!(
            placement.x_axis_from_east_radians < -0.005
                && placement.x_axis_from_east_radians > -0.03,
            "{placement:?}"
        );
    }

    #[test]
    fn map_coordinates_with_no_map_unit_are_in_the_project_unit() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_MILLIMETRES}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,500000000.,0.,100000.,1.,0.,0.001);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!(
            (placement.elevation - 100.0).abs() < HEIGHT_TOLERANCE_METRES,
            "{placement:?}"
        );
    }

    #[test]
    fn a_map_unit_outranks_the_project_unit() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_MILLIMETRES}\
             #62=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,#62);\n\
             #61=IFCMAPCONVERSION(#10,#60,500000.,0.,100.,1.,0.,0.001);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!(
            (placement.elevation - 100.0).abs() < HEIGHT_TOLERANCE_METRES,
            "{placement:?}"
        );
    }

    const FOOT_UNIT_AS_ENTITY_62: &str = "\
#62=IFCCONVERSIONBASEDUNIT(#63,.LENGTHUNIT.,'FOOT',#64);
#63=IFCDIMENSIONALEXPONENTS(1,0,0,0,0,0,0);
#64=IFCMEASUREWITHUNIT(IFCLENGTHMEASURE(0.3048),#65);
#65=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
";

    #[test]
    fn a_foot_map_unit_converts_to_a_metre_crs() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{FOOT_UNIT_AS_ENTITY_62}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,#62);\n\
             #61=IFCMAPCONVERSION(#10,#60,1640419.9475,0.,328.0839895,1.,0.,1.);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!(placement.latitude.abs() < DEGREE_TOLERANCE, "{placement:?}");
        assert!((placement.elevation - 100.0).abs() < 1e-3, "{placement:?}");
    }

    #[test]
    fn a_foot_project_converts_to_a_us_survey_foot_crs() {
        // NGS mark JV7114 in EPSG:2248, projicio's own check of that CRS's unit
        let source = ifc_file(&format!(
            "#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);\n\
             #7=IFCUNITASSIGNMENT((#62));\n\
             #10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,$);\n\
             #11=IFCAXIS2PLACEMENT3D(#12,$,$);\n\
             #12=IFCCARTESIANPOINT((0.,0.,0.));\n\
             {FOOT_UNIT_AS_ENTITY_62}\
             #60=IFCPROJECTEDCRS('EPSG:2248',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,1268515.15,489532.10,100.,1.,0.,1.);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude + 77.154).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!(
            (placement.latitude - 39.011).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert!((placement.elevation - 30.48).abs() < 1e-3, "{placement:?}");
    }

    const SITE_WITH_COORDINATES: &str = "#2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),$,$,$);\n";

    #[test]
    fn a_crs_of_unknown_unit_refuses_the_placement_by_name_and_keeps_the_rotation() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{SITE_WITH_COORDINATES}\
             #60=IFCPROJECTEDCRS('EPSG:99999',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,1000.,2000.,0.,0.,1.,1.);\n"
        ));

        let error = site_placement(&source).unwrap_err().to_string();

        assert!(error.contains("EPSG:99999"), "{error}");
        assert!(error.contains("longitude and latitude"), "{error}");
        let rotation = x_axis_from_east_radians(&source).unwrap();
        assert!((rotation - std::f64::consts::FRAC_PI_2).abs() < ANGLE_TOLERANCE_RADIANS);
    }

    fn extents(mesh: &MeshData) -> [f64; 3] {
        std::array::from_fn(|axis| {
            let values = mesh.positions.iter().map(|position| position[axis]);
            values.clone().fold(f64::NEG_INFINITY, f64::max) - values.fold(f64::INFINITY, f64::min)
        })
    }

    fn assert_extents(mesh: &MeshData, expected: [f64; 3]) {
        let measured = extents(mesh);
        for axis in 0..3 {
            assert!(
                (measured[axis] - expected[axis]).abs() < 1e-3,
                "{measured:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn map_conversion_factors_resize_the_model_wherever_it_is_placed() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}{SITE_WITH_COORDINATES}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,0.,1.,$,0.3048,0.3048,0.3048);\n"
        ));

        let meshes = read_elements(&source, u64::MAX).unwrap().meshes;

        assert_extents(&meshes[0], [1.2192, 0.09144, 0.762]);
        assert!((volume(&meshes[0]) - 3.0 * 0.3048f64.powi(3)).abs() < 1e-3);
        let placement = site_placement(&source).unwrap().expect("a placement");
        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        let rotation = x_axis_from_east_radians(&source).unwrap();
        assert!((rotation - std::f64::consts::FRAC_PI_2).abs() < ANGLE_TOLERANCE_RADIANS);
    }

    #[test]
    fn each_factor_resizes_its_own_axis_and_normals_stay_unit_length() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,1.,0.,$,0.5,1.,2.);\n"
        ));

        let meshes = read_elements(&source, u64::MAX).unwrap().meshes;

        assert_extents(&meshes[0], [2.0, 0.3, 5.0]);
        for normal in &meshes[0].normals {
            let length = normal
                .map(|component| component * component)
                .iter()
                .sum::<f32>()
                .sqrt();
            assert!((length - 1.0).abs() < 1e-5, "{normal:?}");
        }
    }

    #[test]
    fn factors_with_a_scale_that_converts_the_units_are_the_whole_resize() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_MILLIMETRES}\
             #62=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,#62);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,1.,0.,0.001,0.5,0.5,0.5);\n"
        ));

        assert_eq!(
            axis_factors_resizing_the_model(&source).unwrap(),
            Some([0.5, 0.5, 0.5])
        );
    }

    #[test]
    fn factors_with_a_scale_that_does_not_convert_the_units_refuse_the_model_by_name() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}{FOOT_UNIT_AS_ENTITY_62}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,#62);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,1.,0.,$,0.5,0.5,0.5);\n"
        ));

        let error = read_elements(&source, u64::MAX)
            .err()
            .expect("a refusal")
            .to_string();

        assert!(error.contains("IfcMapConversionScaled"), "{error}");
        assert!(error.contains("Scale"), "{error}");
    }

    #[test]
    fn a_negative_factor_refuses_the_model_by_name() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,1.,0.,$,-1.,1.,1.);\n"
        ));

        let error = read_elements(&source, u64::MAX)
            .err()
            .expect("a refusal")
            .to_string();

        assert!(error.contains("IfcMapConversionScaled"), "{error}");
        assert!(error.contains("mirror"), "{error}");
    }

    #[test]
    fn a_grid_scale_factor_leaves_the_model_size_and_the_placement_alone() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSIONSCALED(#10,#60,500000.,0.,0.,1.,0.,$,0.9996,0.9996,1.);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!(
            (placement.longitude - 9.0).abs() < DEGREE_TOLERANCE,
            "{placement:?}"
        );
        assert_extents(
            &read_elements(&source, u64::MAX).unwrap().meshes[0],
            [4.0, 0.3, 2.5],
        );
    }

    #[test]
    fn walls_sharing_one_body_are_charged_per_wall_past_the_byte_limit() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #101=IFCWALL('0SharedWall00000000000',$,'Wall',$,$,#20,#51,'Tag',$);\n"
        ));
        let meshes = read_elements(&source, u64::MAX).unwrap().meshes;
        assert_eq!(meshes.len(), 2);
        let both_walls: u64 = meshes.iter().map(bytes_held_by_mesh).sum();

        assert!(read_elements(&source, both_walls).is_ok());
        let error = read_elements(&source, both_walls - 1)
            .err()
            .expect("a refusal")
            .to_string();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn each_mesh_is_charged_more_than_its_vertices_indices_and_text() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{WALL}\
             #101=IFCWALL('0SharedWall00000000000',$,'Wall',$,$,#20,#51,'Tag',$);\n"
        ));
        let meshes = read_elements(&source, u64::MAX).unwrap().meshes;
        let vertices_indices_and_text: u64 = meshes
            .iter()
            .map(|mesh| {
                mesh.positions.len() as u64 * BYTES_HELD_PER_VERTEX
                    + mesh.indices.len() as u64 * 4
                    + (mesh.name.len() + mesh.asset_id.as_ref().map_or(0, String::len)) as u64
            })
            .sum();

        let error = read_elements(&source, vertices_indices_and_text)
            .err()
            .expect("a refusal")
            .to_string();

        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn a_hostile_well_known_text_under_a_geographic_crs_is_refused_by_name() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}\
             #60=IFCGEOGRAPHICCRS('custom',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,10.,20.,0.,0.,1.,1.);\n\
             #63=IFCWELLKNOWNTEXT('{}',#60);\n",
            "A[".repeat(1000)
        ));

        let error = site_placement(&source).unwrap_err().to_string();

        assert!(error.contains("nesting"), "{error}");
        let rotation = x_axis_from_east_radians(&source).unwrap();
        assert!((rotation - std::f64::consts::FRAC_PI_2).abs() < ANGLE_TOLERANCE_RADIANS);
    }

    const CONVERSION_TO_A_CRS_WITH_NO_CODE: &str = "\
#60=IFCPROJECTEDCRS('LocalGrid',$,$,$,$,$,$);
#61=IFCMAPCONVERSION(#10,#60,1000.,2000.,0.,0.,1.,1.);
";

    #[test]
    fn a_crs_with_no_code_fails_naming_it_when_there_is_no_site() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{CONVERSION_TO_A_CRS_WITH_NO_CODE}"
        ));

        let error = site_placement(&source).unwrap_err().to_string();

        assert!(error.contains("LocalGrid"), "{error}");
    }

    #[test]
    fn a_crs_with_no_code_falls_back_to_the_site_and_keeps_the_conversion_rotation() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{CONVERSION_TO_A_CRS_WITH_NO_CODE}\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),$,$,$);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!((placement.latitude - 51.5).abs() < 1e-6, "{placement:?}");
        assert!(
            (placement.x_axis_from_east_radians - std::f64::consts::FRAC_PI_2).abs()
                < ANGLE_TOLERANCE_RADIANS,
            "{placement:?}"
        );
    }

    #[test]
    fn a_map_conversion_with_zero_offsets_places_nothing() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION(#10,#60,0.,0.,0.,1.,0.,1.);\n\
             #2=IFCSITE('0Site00000000000000000',$,'Site',$,$,$,$,$,.ELEMENT.,(51,30,0),(0,7,40),$,$,$);\n"
        ));

        let placement = site_placement(&source).unwrap().expect("a placement");

        assert!((placement.latitude - 51.5).abs() < 1e-6, "{placement:?}");
    }

    #[test]
    fn the_rotation_for_an_upload_placed_model_needs_no_readable_crs() {
        let source = ifc_file(&format!(
            "{PROJECT_IN_METRES}{CONVERSION_TO_A_CRS_WITH_NO_CODE}"
        ));

        let rotation = x_axis_from_east_radians(&source).unwrap();

        assert!((rotation - std::f64::consts::FRAC_PI_2).abs() < ANGLE_TOLERANCE_RADIANS);
    }
}
