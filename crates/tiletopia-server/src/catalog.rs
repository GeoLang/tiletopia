//! Open Data Catalog — curated registry of free geospatial datasets.
//!
//! Provides one-click access to global terrain, 3D buildings, satellite imagery,
//! and community datasets without requiring any paid service subscriptions.

use axum::{Router, extract::State, response::Json, routing::get};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;

/// A dataset available in the open catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogDataset {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: DatasetCategory,
    pub provider: String,
    pub license: String,
    pub url: String,
    pub format: DataFormat,
    pub coverage: Coverage,
    pub resolution: Option<String>,
    pub enabled: bool,
}

/// Category of geospatial dataset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DatasetCategory {
    Terrain,
    Buildings,
    Imagery,
    PointCloud,
    Vector,
    Weather,
}

/// Data format/protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DataFormat {
    /// 3D Tiles (OGC standard)
    Tiles3D,
    /// Quantized Mesh terrain tiles
    QuantizedMesh,
    /// Cloud Optimized GeoTIFF
    Cog,
    /// Web Map Tile Service
    Wmts,
    /// XYZ raster tile scheme
    Xyz,
    /// GeoJSON vector
    GeoJson,
    /// Mapbox Vector Tiles
    Mvt,
    /// OGC WMS
    Wms,
    /// TileJSON metadata
    TileJson,
}

/// Geographic coverage descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub scope: CoverageScope,
    pub bbox: Option<[f64; 4]>, // [west, south, east, north]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CoverageScope {
    Global,
    Regional(String),
    National(String),
    Local(String),
}

/// The open data catalog store.
pub struct OpenDataCatalog {
    datasets: Vec<CatalogDataset>,
}

impl Default for OpenDataCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenDataCatalog {
    /// Create catalog with curated open datasets.
    pub fn new() -> Self {
        Self {
            datasets: Self::curated_datasets(),
        }
    }

    /// List all datasets, optionally filtered by category.
    pub fn list(&self, category: Option<&DatasetCategory>) -> Vec<&CatalogDataset> {
        match category {
            Some(cat) => self.datasets.iter().filter(|d| &d.category == cat).collect(),
            None => self.datasets.iter().collect(),
        }
    }

    /// Get a dataset by ID.
    pub fn get(&self, id: &str) -> Option<&CatalogDataset> {
        self.datasets.iter().find(|d| d.id == id)
    }

    /// Curated list of open geospatial data sources.
    fn curated_datasets() -> Vec<CatalogDataset> {
        vec![
            // ─── Terrain ─────────────────────────────────────────
            CatalogDataset {
                id: "copernicus-dem-30".into(),
                name: "Copernicus DEM GLO-30".into(),
                description: "Global 30m resolution digital elevation model from ESA Copernicus programme. Derived from TanDEM-X mission.".into(),
                category: DatasetCategory::Terrain,
                provider: "European Space Agency (ESA)".into(),
                license: "Copernicus DEM License (free, attribution required)".into(),
                url: "https://registry.opendata.aws/copernicus-dem/".into(),
                format: DataFormat::Cog,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -90.0, 180.0, 90.0]) },
                resolution: Some("30m".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "usgs-3dep-1m".into(),
                name: "USGS 3DEP LiDAR DEM".into(),
                description: "High-resolution 1m terrain from USGS 3D Elevation Program. LiDAR-derived bare earth across the United States.".into(),
                category: DatasetCategory::Terrain,
                provider: "US Geological Survey".into(),
                license: "Public Domain (US Government)".into(),
                url: "https://www.usgs.gov/3d-elevation-program".into(),
                format: DataFormat::Cog,
                coverage: Coverage { scope: CoverageScope::National("United States".into()), bbox: Some([-125.0, 24.0, -66.0, 50.0]) },
                resolution: Some("1m".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "srtm-90m".into(),
                name: "NASA SRTM 90m".into(),
                description: "Shuttle Radar Topography Mission global elevation data at 90m resolution (3 arc-seconds).".into(),
                category: DatasetCategory::Terrain,
                provider: "NASA/USGS".into(),
                license: "Public Domain".into(),
                url: "https://srtm.csi.cgiar.org/".into(),
                format: DataFormat::Cog,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -60.0, 180.0, 60.0]) },
                resolution: Some("90m".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "terrain-quantized-mesh".into(),
                name: "Open Terrain (Quantized Mesh)".into(),
                description: "Pre-built quantized mesh terrain tiles from Mapzen/Tilezen, ready for CesiumJS terrain providers.".into(),
                category: DatasetCategory::Terrain,
                provider: "Mapzen/AWS".into(),
                license: "ODbL".into(),
                url: "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{z}/{x}/{y}.png".into(),
                format: DataFormat::Xyz,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: Some("~30m (zoom 15)".into()),
                enabled: true,
            },

            // ─── 3D Buildings ────────────────────────────────────
            CatalogDataset {
                id: "osm-buildings-3d".into(),
                name: "OpenStreetMap 3D Buildings".into(),
                description: "3D building models from OpenStreetMap data, served as 3D Tiles. Coverage includes most major cities worldwide.".into(),
                category: DatasetCategory::Buildings,
                provider: "OSM Buildings / osmbuildings.org".into(),
                license: "ODbL (OpenStreetMap)".into(),
                url: "https://osmbuildings.org/data/OSMBuildings-3DTiles/".into(),
                format: DataFormat::Tiles3D,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: Some("LOD1/LOD2".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "overture-buildings".into(),
                name: "Overture Maps Buildings".into(),
                description: "2.3 billion building footprints from the Overture Maps Foundation (Meta, Microsoft, AWS, TomTom).".into(),
                category: DatasetCategory::Buildings,
                provider: "Overture Maps Foundation".into(),
                license: "ODbL / CC-BY-4.0".into(),
                url: "https://overturemaps.org/download/".into(),
                format: DataFormat::GeoJson,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -90.0, 180.0, 90.0]) },
                resolution: Some("Building footprints + height".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "google-maps-3d-tiles".into(),
                name: "Google Photorealistic 3D Tiles".into(),
                description: "Photorealistic 3D mesh tiles from Google Maps Platform. Requires API key (generous free tier: 25k loads/month).".into(),
                category: DatasetCategory::Buildings,
                provider: "Google Maps Platform".into(),
                license: "Google Maps ToS (free tier available)".into(),
                url: "https://tile.googleapis.com/v1/3dtiles/root.json".into(),
                format: DataFormat::Tiles3D,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: Some("Photorealistic mesh".into()),
                enabled: false, // requires API key
            },

            // ─── Satellite Imagery ───────────────────────────────
            CatalogDataset {
                id: "sentinel-2-l2a".into(),
                name: "Sentinel-2 L2A (True Color)".into(),
                description: "10m resolution optical satellite imagery updated every 5 days. Surface reflectance product.".into(),
                category: DatasetCategory::Imagery,
                provider: "ESA Copernicus / AWS".into(),
                license: "Copernicus Open Access (free, attribution)".into(),
                url: "https://earth-search.aws.element84.com/v1".into(),
                format: DataFormat::Cog,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -56.0, 180.0, 84.0]) },
                resolution: Some("10m".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "openstreetmap-raster".into(),
                name: "OpenStreetMap Tiles".into(),
                description: "Standard OSM raster map tiles. Ideal for base layer and context.".into(),
                category: DatasetCategory::Imagery,
                provider: "OpenStreetMap Foundation".into(),
                license: "ODbL".into(),
                url: "https://tile.openstreetmap.org/{z}/{x}/{y}.png".into(),
                format: DataFormat::Xyz,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: Some("~1m (zoom 19)".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "esri-world-imagery".into(),
                name: "Esri World Imagery".into(),
                description: "High-resolution satellite and aerial imagery basemap from Esri. Free for non-commercial use.".into(),
                category: DatasetCategory::Imagery,
                provider: "Esri".into(),
                license: "Esri Master License (free non-commercial)".into(),
                url: "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}".into(),
                format: DataFormat::Xyz,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: Some("0.5-1m (varies)".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "open-aerial-map".into(),
                name: "OpenAerialMap".into(),
                description: "Community-contributed open aerial imagery. Drone and satellite imagery with open licenses.".into(),
                category: DatasetCategory::Imagery,
                provider: "OpenAerialMap / HOT".into(),
                license: "CC-BY 4.0 (individual images vary)".into(),
                url: "https://api.openaerialmap.org/".into(),
                format: DataFormat::Cog,
                coverage: Coverage { scope: CoverageScope::Global, bbox: None },
                resolution: Some("cm-level (varies)".into()),
                enabled: true,
            },

            // ─── Point Clouds ────────────────────────────────────
            CatalogDataset {
                id: "opentopography-lidar".into(),
                name: "OpenTopography LiDAR".into(),
                description: "Community LiDAR data portal with high-resolution point clouds from research institutions worldwide.".into(),
                category: DatasetCategory::PointCloud,
                provider: "OpenTopography / NSF".into(),
                license: "CC-BY / Public Domain (varies by dataset)".into(),
                url: "https://opentopography.org/".into(),
                format: DataFormat::Tiles3D,
                coverage: Coverage { scope: CoverageScope::Global, bbox: None },
                resolution: Some("1-50 pts/m² (varies)".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "ahn-netherlands".into(),
                name: "AHN4 (Netherlands)".into(),
                description: "Complete LiDAR coverage of the Netherlands at 8+ points/m². One of the densest national datasets.".into(),
                category: DatasetCategory::PointCloud,
                provider: "Kadaster / Dutch Government".into(),
                license: "CC0 (Public Domain)".into(),
                url: "https://www.ahn.nl/".into(),
                format: DataFormat::Tiles3D,
                coverage: Coverage { scope: CoverageScope::National("Netherlands".into()), bbox: Some([3.37, 50.75, 7.21, 53.47]) },
                resolution: Some("8+ pts/m²".into()),
                enabled: true,
            },
            CatalogDataset {
                id: "entwine-usgs".into(),
                name: "USGS 3DEP Point Clouds (Entwine)".into(),
                description: "US national LiDAR point clouds indexed via Entwine Point Tiles (EPT), served as 3D Tiles.".into(),
                category: DatasetCategory::PointCloud,
                provider: "USGS / Hobu".into(),
                license: "Public Domain (US Government)".into(),
                url: "https://usgs.entwine.io/".into(),
                format: DataFormat::Tiles3D,
                coverage: Coverage { scope: CoverageScope::National("United States".into()), bbox: Some([-125.0, 24.0, -66.0, 50.0]) },
                resolution: Some("2-20 pts/m² (varies)".into()),
                enabled: true,
            },

            // ─── Vector Data ─────────────────────────────────────
            CatalogDataset {
                id: "osm-vector-tiles".into(),
                name: "OpenMapTiles".into(),
                description: "OpenStreetMap data as Mapbox Vector Tiles. Roads, boundaries, land use, POIs.".into(),
                category: DatasetCategory::Vector,
                provider: "OpenMapTiles / MapTiler".into(),
                license: "ODbL (data) / BSD (schema)".into(),
                url: "https://openmaptiles.org/".into(),
                format: DataFormat::Mvt,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -85.0, 180.0, 85.0]) },
                resolution: None,
                enabled: true,
            },
            CatalogDataset {
                id: "natural-earth".into(),
                name: "Natural Earth".into(),
                description: "Public domain cultural and physical vector data at 1:10m, 1:50m, 1:110m scales.".into(),
                category: DatasetCategory::Vector,
                provider: "Natural Earth / NACIS".into(),
                license: "Public Domain".into(),
                url: "https://www.naturalearthdata.com/".into(),
                format: DataFormat::GeoJson,
                coverage: Coverage { scope: CoverageScope::Global, bbox: Some([-180.0, -90.0, 180.0, 90.0]) },
                resolution: None,
                enabled: true,
            },
        ]
    }
}

// ─── API Routes ──────────────────────────────────────────────────────────────

/// Register catalog API routes.
pub fn catalog_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/catalog", get(list_datasets))
        .route("/api/v1/catalog/{id}", get(get_dataset))
}

#[derive(Deserialize, Default)]
struct CatalogQuery {
    category: Option<String>,
}

async fn list_datasets(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<CatalogQuery>,
) -> Json<Vec<CatalogDataset>> {
    let category = params.category.and_then(|c| match c.as_str() {
        "terrain" => Some(DatasetCategory::Terrain),
        "buildings" => Some(DatasetCategory::Buildings),
        "imagery" => Some(DatasetCategory::Imagery),
        "pointcloud" => Some(DatasetCategory::PointCloud),
        "vector" => Some(DatasetCategory::Vector),
        "weather" => Some(DatasetCategory::Weather),
        _ => None,
    });
    let datasets: Vec<CatalogDataset> = state.catalog.list(category.as_ref())
        .into_iter()
        .cloned()
        .collect();
    Json(datasets)
}

async fn get_dataset(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<CatalogDataset>, axum::http::StatusCode> {
    state
        .catalog
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(axum::http::StatusCode::NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_datasets() {
        let catalog = OpenDataCatalog::new();
        assert!(catalog.list(None).len() >= 15);
    }

    #[test]
    fn filter_by_category() {
        let catalog = OpenDataCatalog::new();
        let terrain = catalog.list(Some(&DatasetCategory::Terrain));
        assert!(terrain.len() >= 3);
        for d in &terrain {
            assert_eq!(d.category, DatasetCategory::Terrain);
        }
    }

    #[test]
    fn get_by_id() {
        let catalog = OpenDataCatalog::new();
        let ds = catalog.get("copernicus-dem-30").unwrap();
        assert_eq!(ds.provider, "European Space Agency (ESA)");
    }

    #[test]
    fn all_datasets_have_valid_urls() {
        let catalog = OpenDataCatalog::new();
        for ds in catalog.list(None) {
            assert!(!ds.url.is_empty(), "Dataset {} has empty URL", ds.id);
            assert!(
                ds.url.starts_with("https://"),
                "Dataset {} URL should be HTTPS: {}",
                ds.id,
                ds.url
            );
        }
    }
}
