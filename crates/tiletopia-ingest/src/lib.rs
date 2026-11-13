pub mod citygml_reader;
pub mod cityjson_reader;
pub mod crs_detect;
pub mod dae_reader;
pub mod dem_wgs84;
pub mod dted_reader;
pub mod dxf_reader;
pub mod e57_reader;
pub mod fbx_reader;
pub mod gltf_reader;
pub mod hgt_reader;
pub mod ifc_alignment;
pub mod ifc_edit;
pub mod ifc_elements;
pub mod ifc_property_write;
pub mod ifc_reader;
pub mod ifc_split;
pub mod las_reader;
pub mod obj_reader;
pub mod ply_reader;
pub mod splat_ply_reader;
pub mod texture_image;
pub mod tiff_reader;
pub mod usgs_dem_reader;

use thiserror::Error;

// MeshData and the tiler's material group each have 24 + 12 + 8 bytes of position, normal and texcoord a vertex, and the tiler holds 12 more for simplification and 4 for colour
// TODO: measure peak bytes per vertex on a large public model before changing this or JOB_BUDGET_BYTES
const BYTES_HELD_PER_VERTEX: u64 = 104;
const BYTES_HELD_PER_MESH: u64 =
    std::mem::size_of::<MeshData>() as u64 + tiletopia_core::mesh_tiler::TILER_BYTES_PER_MESH;

/// Ingest errors.
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("reprojection error: {0}")]
    Reprojection(#[from] tiletopia_core::crs::ReprojError),
    #[error("{0}")]
    DemRefused(String),
    #[error("{0}")]
    SplatRefused(String),
    #[error("{0}")]
    ElementRefused(String),
}

/// A 3D point with optional colour and classification.
#[derive(Debug, Clone, Copy)]
pub struct Point3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub classification: u8,
    pub intensity: u16,
}

impl Point3D {
    /// Convert to nubis Point3 (drops RGB, preserves position/intensity/class).
    pub fn to_nubis(&self) -> nubis_core::Point3 {
        let mut p = nubis_core::Point3::new(self.x, self.y, self.z);
        p.intensity = self.intensity;
        p.classification = nubis_core::Classification::from_u8(self.classification);
        p
    }

    /// Create from nubis Point3 (RGB will be zero).
    pub fn from_nubis(p: &nubis_core::Point3) -> Self {
        Self {
            x: p.x,
            y: p.y,
            z: p.z,
            r: 0,
            g: 0,
            b: 0,
            classification: p.classification as u8,
            intensity: p.intensity,
        }
    }
}

/// Convert a slice of Point3D to a nubis PointCloud for processing.
pub fn to_nubis_cloud(points: &[Point3D]) -> nubis_core::PointCloud {
    let nubis_points: Vec<nubis_core::Point3> = points.iter().map(|p| p.to_nubis()).collect();
    nubis_core::PointCloud::from_points(nubis_points)
}

/// Thin a point cloud using nubis voxel grid downsampling.
pub fn thin_voxel(points: &[Point3D], voxel_size: f64) -> Vec<Point3D> {
    let cloud = to_nubis_cloud(points);
    let thinned = nubis_core::thin_voxel(&cloud, voxel_size);
    thinned.points().iter().map(Point3D::from_nubis).collect()
}

/// Remove statistical outliers from a point cloud using nubis.
pub fn remove_outliers(points: &[Point3D], k_neighbors: usize, std_ratio: f64) -> Vec<Point3D> {
    let cloud = to_nubis_cloud(points);
    let cleaned = nubis_core::statistical_outlier_removal(&cloud, k_neighbors, std_ratio);
    cleaned.points().iter().map(Point3D::from_nubis).collect()
}

/// Ground-filter a point cloud using nubis.
pub fn ground_filter(points: &[Point3D], cell_size: f64, height_threshold: f64) -> Vec<Point3D> {
    let mut cloud = to_nubis_cloud(points);
    nubis_core::ground_filter_simple(&mut cloud, cell_size, height_threshold);
    cloud.points().iter().map(Point3D::from_nubis).collect()
}

/// Source data type.
#[derive(Debug, Clone)]
pub enum SourceData {
    PointCloud(Vec<Point3D>),
    Heightmap(Heightmap),
    Mesh(MeshData),
}

/// A 2D grid of elevation values.
#[derive(Debug, Clone)]
pub struct Heightmap {
    pub width: usize,
    pub height: usize,
    /// Row-major elevation values in meters.
    pub elevations: Vec<f64>,
    /// Geographic bounds [west, south, east, north] in degrees.
    pub bounds: [f64; 4],
    /// No-data value.
    pub nodata: Option<f64>,
}

impl Heightmap {
    pub fn elevation_at(&self, x: f64, y: f64) -> Option<f64> {
        let [west, south, east, north] = self.bounds;
        let last_column = (self.width - 1) as f64;
        let last_row = (self.height - 1) as f64;
        // tiff_reader::read bounds run to the pixel edges, dem_wgs84 moves them onto the outer samples
        let column = (x - west) / (east - west) * last_column;
        let row = (north - y) / (north - south) * last_row;
        if !(0.0..=last_column).contains(&column) || !(0.0..=last_row).contains(&row) {
            return None;
        }

        let (left, top) = (column as usize, row as usize);
        let right = (left + 1).min(self.width - 1);
        let bottom = (top + 1).min(self.height - 1);
        let (east_weight, south_weight) = (column - left as f64, row - top as f64);
        let at = |row: usize, column: usize| self.elevations[row * self.width + column];

        // nan times a zero weight is still nan
        let weighted = |value: f64, weight: f64| if weight == 0.0 { 0.0 } else { value * weight };
        let top_edge =
            weighted(at(top, left), 1.0 - east_weight) + weighted(at(top, right), east_weight);
        let bottom_edge = weighted(at(bottom, left), 1.0 - east_weight)
            + weighted(at(bottom, right), east_weight);
        let elevation =
            weighted(top_edge, 1.0 - south_weight) + weighted(bottom_edge, south_weight);
        elevation.is_finite().then_some(elevation)
    }
}

/// Mesh data from glTF/OBJ/etc.
///
/// One material per mesh: a source mesh painted with several materials is read
/// as several `MeshData`, which is what the GLB writer takes.
#[derive(Debug, Clone)]
pub struct MeshData {
    pub positions: Vec<[f64; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// glTF UV space, origin at the top left. Empty, or one per position.
    pub texcoords: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub name: String,
    pub material: Option<Material>,
    /// The source element's stable identity, carried into the tiles as the
    /// `asset_id` feature property. IFC fills it with the element's GlobalId.
    pub asset_id: Option<String>,
    pub class: Option<String>,
    pub storey: Option<String>,
    pub element_color: Option<[u8; 4]>,
}

/// The single material a `MeshData` is painted with.
#[derive(Debug, Clone)]
pub struct Material {
    /// glTF `baseColorFactor`, RGBA.
    pub base_color_factor: [f32; 4],
    pub texture: Option<Texture>,
}

/// A texture image in its file encoding, with the dimensions it decodes to.
#[derive(Debug, Clone)]
pub struct Texture {
    pub image_data: Vec<u8>,
    /// `image/png` or `image/jpeg`, the two glTF takes.
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

impl From<MeshData> for tiletopia_core::mesh_tiler::MeshData {
    fn from(mesh: MeshData) -> Self {
        let (base_color_factor, texture) = match mesh.material {
            Some(material) => (Some(material.base_color_factor), material.texture),
            None => (None, None),
        };
        Self {
            positions: mesh.positions,
            normals: mesh.normals,
            texcoords: mesh.texcoords,
            indices: mesh.indices,
            name: mesh.name,
            asset_id: mesh.asset_id,
            class: mesh.class,
            storey: mesh.storey,
            element_color: mesh.element_color,
            base_color_factor,
            texture: texture.map(|texture| tiletopia_core::glb_writer::TextureData {
                image_data: texture.image_data,
                mime_type: texture.mime_type,
                width: texture.width,
                height: texture.height,
            }),
        }
    }
}

/// Read a point cloud file (LAS/LAZ).
pub fn read_point_cloud(path: &std::path::Path) -> Result<Vec<Point3D>, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("las" | "laz") => las_reader::read(path),
        Some("e57") => e57_reader::read(path),
        Some("ply") => ply_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}

/// Read a point cloud file and reproject to WGS84 if needed.
pub fn read_point_cloud_wgs84(path: &std::path::Path) -> Result<Vec<Point3D>, IngestError> {
    let mut points = read_point_cloud(path)?;
    let crs = crs_detect::detect_crs(path);
    if !matches!(
        crs,
        crs_detect::DetectedCrs::Wgs84 | crs_detect::DetectedCrs::Unknown
    ) {
        reproject_positions(&mut points, |coords| {
            crs_detect::reproject_to_wgs84(coords, &crs)
        });
    }
    Ok(points)
}

pub fn read_point_cloud_ecef(
    path: &std::path::Path,
    fallback_source_epsg: Option<u32>,
) -> Result<Vec<Point3D>, IngestError> {
    let mut points = read_point_cloud(path)?;
    let Some(source_epsg) = crs_detect::detect_crs(path)
        .to_epsg()
        .or(fallback_source_epsg)
    else {
        tracing::warn!(
            "{}: no coordinate reference system found, tiling its coordinates unchanged",
            path.display()
        );
        return Ok(points);
    };
    reproject_positions(&mut points, |coords| {
        crs_detect::reproject_to_ecef(coords, source_epsg)
    })?;
    Ok(points)
}

fn reproject_positions<T>(
    points: &mut [Point3D],
    reproject: impl FnOnce(&mut [[f64; 3]]) -> T,
) -> T {
    let mut coords: Vec<[f64; 3]> = points.iter().map(|p| [p.x, p.y, p.z]).collect();
    let result = reproject(&mut coords);
    for (p, c) in points.iter_mut().zip(coords.iter()) {
        p.x = c[0];
        p.y = c[1];
        p.z = c[2];
    }
    result
}

/// Read a heightmap from a GeoTIFF, DTED, or HGT file.
pub fn read_heightmap(path: &std::path::Path) -> Result<Heightmap, IngestError> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    match extension.as_deref() {
        Some("tif" | "tiff") => tiff_reader::read(path),
        Some("dt0" | "dt1" | "dt2") => dted_reader::read(path),
        Some("hgt") => hgt_reader::read(path),
        Some("dem") => usgs_dem_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}

pub fn read_mesh(
    path: &std::path::Path,
    max_bytes_held: u64,
) -> Result<Vec<MeshData>, IngestError> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("gltf" | "glb") => gltf_reader::read(path),
        Some("obj") => obj_reader::read(path),
        Some("fbx") => fbx_reader::read(path),
        Some("dae") => dae_reader::read(path, max_bytes_held),
        Some("ifc") => ifc_reader::read(path, max_bytes_held),
        Some("gml" | "xml") => citygml_reader::read(path),
        Some("json") => {
            // Check if it's CityJSON by peeking at the content
            let data = std::fs::read_to_string(path)?;
            if data.contains("\"CityJSON\"") {
                // Re-read via the CityJSON reader (it re-reads the file, but keeps the API clean)
                cityjson_reader::read(path)
            } else {
                Err(IngestError::UnsupportedFormat(
                    "json (not CityJSON)".to_string(),
                ))
            }
        }
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}
