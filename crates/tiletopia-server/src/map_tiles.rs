//! 2D Map Tile serving — XYZ raster tiles, Mapbox Vector Tiles (MVT), and
//! style management for web map clients (Leaflet, MapLibre GL, OpenLayers).
//!
//! Supports:
//! - XYZ raster tile serving (proxy + cache)
//! - Vector tile (MVT/PBF) generation from GeoJSON sources
//! - MapLibre style JSON management
//! - Tile caching with TTL
//! - Custom overlay layers from asset footprints

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// ─── Tile Coordinates ────────────────────────────────────────────────────────

/// ZXY tile coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    /// Get the geographic bounds of this tile (Web Mercator → WGS84).
    pub fn bounds(&self) -> TileBounds {
        let n = 2u32.pow(self.z as u32) as f64;
        let lon_min = self.x as f64 / n * 360.0 - 180.0;
        let lon_max = (self.x + 1) as f64 / n * 360.0 - 180.0;
        let lat_max = (std::f64::consts::PI * (1.0 - 2.0 * self.y as f64 / n))
            .sinh()
            .atan()
            .to_degrees();
        let lat_min = (std::f64::consts::PI * (1.0 - 2.0 * (self.y + 1) as f64 / n))
            .sinh()
            .atan()
            .to_degrees();
        TileBounds {
            west: lon_min,
            south: lat_min,
            east: lon_max,
            north: lat_max,
        }
    }
}

/// Geographic bounds of a tile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileBounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

// ─── Tile Sources ────────────────────────────────────────────────────────────

/// A registered tile source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileSource {
    pub id: Uuid,
    pub name: String,
    pub source_type: TileSourceType,
    pub url_template: String,
    pub attribution: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub format: TileFormat,
    pub bounds: Option<[f64; 4]>, // [west, south, east, north]
    pub created_at: DateTime<Utc>,
}

/// Type of tile source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TileSourceType {
    /// Proxy/cache remote XYZ tiles (e.g., OSM, Stamen, etc.)
    RasterProxy,
    /// Locally-generated raster tiles from GeoTIFF/COG
    RasterLocal,
    /// Vector tiles generated from GeoJSON
    VectorGeoJson,
    /// Vector tiles generated from PostGIS
    VectorPostGis,
    /// Custom overlay (asset footprints, boundaries)
    OverlayLayer,
}

/// Tile output format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TileFormat {
    Png,
    Jpeg,
    Webp,
    Pbf, // Mapbox Vector Tile (protobuf)
    Mvt, // alias for Pbf
}

// ─── Vector Tile Layer ───────────────────────────────────────────────────────

/// A vector tile layer definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorLayer {
    pub id: String,
    pub name: String,
    pub geometry_type: GeometryType,
    pub fields: Vec<FieldDef>,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// Geometry type for vector features.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GeometryType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

/// Field definition in a vector layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    pub field_type: FieldType,
}

/// Field data type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FieldType {
    String,
    Number,
    Boolean,
}

// ─── Map Style ───────────────────────────────────────────────────────────────

/// A MapLibre/Mapbox GL style document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapStyle {
    pub id: Uuid,
    pub name: String,
    pub version: u8, // always 8 for GL styles
    pub sources: HashMap<String, StyleSource>,
    pub layers: Vec<StyleLayer>,
    pub center: Option<[f64; 2]>,
    pub zoom: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A source definition within a style.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleSource {
    #[serde(rename = "type")]
    pub source_type: String, // "raster", "vector", "geojson"
    pub tiles: Option<Vec<String>>,
    pub url: Option<String>,
    #[serde(rename = "tileSize")]
    pub tile_size: Option<u32>,
    pub attribution: Option<String>,
}

/// A layer in a map style.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleLayer {
    pub id: String,
    #[serde(rename = "type")]
    pub layer_type: String, // "raster", "fill", "line", "circle", "symbol"
    pub source: String,
    #[serde(rename = "source-layer")]
    pub source_layer: Option<String>,
    pub paint: Option<serde_json::Value>,
    pub layout: Option<serde_json::Value>,
    pub minzoom: Option<f64>,
    pub maxzoom: Option<f64>,
}

// ─── Tile Cache ──────────────────────────────────────────────────────────────

/// Cache entry for a tile.
#[derive(Debug, Clone)]
pub struct CachedTile {
    pub coord: TileCoord,
    pub data: Vec<u8>,
    pub content_type: String,
    pub cached_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Cache statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub total_entries: usize,
    pub total_size_bytes: u64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub hit_rate: f64,
    pub oldest_entry: Option<DateTime<Utc>>,
}

// ─── Map Tile Engine ─────────────────────────────────────────────────────────

/// The main map tile engine.
pub struct MapTileEngine {
    sources: Vec<TileSource>,
    styles: Vec<MapStyle>,
    cache_stats: CacheStats,
}

impl MapTileEngine {
    /// Create engine with demo data.
    pub fn new() -> Self {
        Self {
            sources: demo_sources(),
            styles: vec![demo_style()],
            cache_stats: CacheStats {
                total_entries: 12847,
                total_size_bytes: 487_000_000,
                hit_count: 94521,
                miss_count: 8234,
                hit_rate: 0.92,
                oldest_entry: Some(Utc::now() - chrono::Duration::hours(6)),
            },
        }
    }

    /// List all registered tile sources.
    pub fn list_sources(&self) -> &[TileSource] {
        &self.sources
    }

    /// Get a tile source by ID.
    pub fn get_source(&self, id: Uuid) -> Option<&TileSource> {
        self.sources.iter().find(|s| s.id == id)
    }

    /// List all map styles.
    pub fn list_styles(&self) -> &[MapStyle] {
        &self.styles
    }

    /// Get a style by ID.
    pub fn get_style(&self, id: Uuid) -> Option<&MapStyle> {
        self.styles.iter().find(|s| s.id == id)
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> &CacheStats {
        &self.cache_stats
    }

    /// Resolve the upstream URL for a tile request.
    pub fn resolve_tile_url(&self, source_id: Uuid, coord: TileCoord) -> Option<String> {
        let source = self.get_source(source_id)?;
        let url = source
            .url_template
            .replace("{z}", &coord.z.to_string())
            .replace("{x}", &coord.x.to_string())
            .replace("{y}", &coord.y.to_string());
        Some(url)
    }

    /// Get available vector layers for a vector source.
    pub fn vector_layers(&self, source_id: Uuid) -> Option<Vec<VectorLayer>> {
        let source = self.get_source(source_id)?;
        if source.source_type != TileSourceType::VectorGeoJson
            && source.source_type != TileSourceType::VectorPostGis
        {
            return None;
        }
        Some(demo_vector_layers())
    }

    /// TileJSON metadata for a source (used by MapLibre).
    pub fn tilejson(&self, source_id: Uuid) -> Option<serde_json::Value> {
        let source = self.get_source(source_id)?;
        Some(serde_json::json!({
            "tilejson": "3.0.0",
            "name": source.name,
            "description": format!("Tile source: {}", source.name),
            "version": "1.0.0",
            "attribution": source.attribution,
            "scheme": "xyz",
            "tiles": [format!("/api/v1/tiles/{}/{{z}}/{{x}}/{{y}}", source.id)],
            "minzoom": source.min_zoom,
            "maxzoom": source.max_zoom,
            "bounds": source.bounds.unwrap_or([-180.0, -85.0511, 180.0, 85.0511]),
            "center": [0.0, 0.0, source.min_zoom],
        }))
    }

    /// Fetch a tile from the upstream source, caching it locally.
    /// Returns the tile bytes and content type.
    pub async fn fetch_tile(
        &mut self,
        source_id: Uuid,
        coord: TileCoord,
        cache_dir: &std::path::Path,
    ) -> Result<(Vec<u8>, String), String> {
        let cache_key = format!("{source_id}/{}/{}/{}", coord.z, coord.x, coord.y);
        let cache_path = cache_dir.join(&cache_key);

        // Check local cache
        if cache_path.exists() {
            let bytes = std::fs::read(&cache_path).map_err(|e| e.to_string())?;
            self.cache_stats.hit_count += 1;
            let content_type = self.content_type_for_source(source_id);
            return Ok((bytes, content_type));
        }

        self.cache_stats.miss_count += 1;

        // Fetch from upstream
        let url = self
            .resolve_tile_url(source_id, coord)
            .ok_or("Source not found")?;

        let client = reqwest::Client::builder()
            .user_agent("tiletopia/0.3.0")
            .build()
            .map_err(|e| e.to_string())?;

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Upstream fetch failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Upstream returned {}", resp.status()));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let bytes = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();

        // Write to cache
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cache_path, &bytes);

        self.cache_stats.total_entries += 1;
        self.cache_stats.total_size_bytes += bytes.len() as u64;

        Ok((bytes, content_type))
    }

    fn content_type_for_source(&self, source_id: Uuid) -> String {
        self.get_source(source_id)
            .map(|s| match s.format {
                TileFormat::Png => "image/png",
                TileFormat::Jpeg => "image/jpeg",
                TileFormat::Webp => "image/webp",
                TileFormat::Pbf | TileFormat::Mvt => "application/x-protobuf",
            })
            .unwrap_or("application/octet-stream")
            .to_string()
    }
}

impl Default for MapTileEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Demo Data ───────────────────────────────────────────────────────────────

fn demo_sources() -> Vec<TileSource> {
    vec![
        TileSource {
            id: Uuid::new_v4(),
            name: "OpenStreetMap".into(),
            source_type: TileSourceType::RasterProxy,
            url_template: "https://tile.openstreetmap.org/{z}/{x}/{y}.png".into(),
            attribution: "© OpenStreetMap contributors".into(),
            min_zoom: 0,
            max_zoom: 19,
            format: TileFormat::Png,
            bounds: Some([-180.0, -85.0511, 180.0, 85.0511]),
            created_at: Utc::now() - chrono::Duration::days(30),
        },
        TileSource {
            id: Uuid::new_v4(),
            name: "Stamen Terrain".into(),
            source_type: TileSourceType::RasterProxy,
            url_template: "https://tiles.stadiamaps.com/tiles/stamen_terrain/{z}/{x}/{y}.png"
                .into(),
            attribution: "Map tiles by Stamen Design, data by OpenStreetMap".into(),
            min_zoom: 0,
            max_zoom: 18,
            format: TileFormat::Png,
            bounds: Some([-180.0, -85.0, 180.0, 85.0]),
            created_at: Utc::now() - chrono::Duration::days(30),
        },
        TileSource {
            id: Uuid::new_v4(),
            name: "Asset Footprints".into(),
            source_type: TileSourceType::VectorGeoJson,
            url_template: "/api/v1/tiles/footprints/{z}/{x}/{y}.pbf".into(),
            attribution: "TileTopia".into(),
            min_zoom: 0,
            max_zoom: 16,
            format: TileFormat::Pbf,
            bounds: Some([-122.52, 37.70, -122.35, 37.82]),
            created_at: Utc::now() - chrono::Duration::days(7),
        },
        TileSource {
            id: Uuid::new_v4(),
            name: "Construction Zones".into(),
            source_type: TileSourceType::OverlayLayer,
            url_template: "/api/v1/tiles/construction/{z}/{x}/{y}.pbf".into(),
            attribution: "TileTopia BIM 4D".into(),
            min_zoom: 10,
            max_zoom: 18,
            format: TileFormat::Pbf,
            bounds: Some([-122.42, 37.77, -122.39, 37.80]),
            created_at: Utc::now() - chrono::Duration::days(2),
        },
    ]
}

fn demo_style() -> MapStyle {
    let mut sources = HashMap::new();
    sources.insert(
        "osm".into(),
        StyleSource {
            source_type: "raster".into(),
            tiles: Some(vec!["/api/v1/tiles/osm/{z}/{x}/{y}.png".into()]),
            url: None,
            tile_size: Some(256),
            attribution: Some("© OpenStreetMap contributors".into()),
        },
    );
    sources.insert(
        "footprints".into(),
        StyleSource {
            source_type: "vector".into(),
            tiles: Some(vec!["/api/v1/tiles/footprints/{z}/{x}/{y}.pbf".into()]),
            url: None,
            tile_size: Some(512),
            attribution: None,
        },
    );

    MapStyle {
        id: Uuid::new_v4(),
        name: "TileTopia Default".into(),
        version: 8,
        sources,
        layers: vec![
            StyleLayer {
                id: "osm-basemap".into(),
                layer_type: "raster".into(),
                source: "osm".into(),
                source_layer: None,
                paint: Some(serde_json::json!({"raster-opacity": 0.8})),
                layout: None,
                minzoom: None,
                maxzoom: None,
            },
            StyleLayer {
                id: "asset-footprints-fill".into(),
                layer_type: "fill".into(),
                source: "footprints".into(),
                source_layer: Some("footprints".into()),
                paint: Some(serde_json::json!({
                    "fill-color": "#4A90D9",
                    "fill-opacity": 0.3
                })),
                layout: None,
                minzoom: Some(10.0),
                maxzoom: None,
            },
            StyleLayer {
                id: "asset-footprints-outline".into(),
                layer_type: "line".into(),
                source: "footprints".into(),
                source_layer: Some("footprints".into()),
                paint: Some(serde_json::json!({
                    "line-color": "#2171B5",
                    "line-width": 2
                })),
                layout: None,
                minzoom: Some(10.0),
                maxzoom: None,
            },
        ],
        center: Some([-122.4194, 37.7749]),
        zoom: Some(12.0),
        created_at: Utc::now() - chrono::Duration::days(14),
        updated_at: Utc::now() - chrono::Duration::hours(2),
    }
}

fn demo_vector_layers() -> Vec<VectorLayer> {
    vec![
        VectorLayer {
            id: "footprints".into(),
            name: "Asset Footprints".into(),
            geometry_type: GeometryType::Polygon,
            fields: vec![
                FieldDef {
                    name: "asset_id".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "name".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "asset_type".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "point_count".into(),
                    field_type: FieldType::Number,
                },
            ],
            min_zoom: 0,
            max_zoom: 16,
        },
        VectorLayer {
            id: "scan-trajectories".into(),
            name: "Scan Trajectories".into(),
            geometry_type: GeometryType::LineString,
            fields: vec![
                FieldDef {
                    name: "scan_id".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "timestamp".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "speed_mps".into(),
                    field_type: FieldType::Number,
                },
            ],
            min_zoom: 8,
            max_zoom: 16,
        },
        VectorLayer {
            id: "poi".into(),
            name: "Points of Interest".into(),
            geometry_type: GeometryType::Point,
            fields: vec![
                FieldDef {
                    name: "name".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "category".into(),
                    field_type: FieldType::String,
                },
                FieldDef {
                    name: "has_3d_model".into(),
                    field_type: FieldType::Boolean,
                },
            ],
            min_zoom: 6,
            max_zoom: 16,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_coord_bounds() {
        let coord = TileCoord { z: 0, x: 0, y: 0 };
        let bounds = coord.bounds();
        assert!((bounds.west - (-180.0)).abs() < 0.001);
        assert!((bounds.east - 180.0).abs() < 0.001);
    }

    #[test]
    fn test_tile_coord_z1() {
        let coord = TileCoord { z: 1, x: 0, y: 0 };
        let bounds = coord.bounds();
        assert!((bounds.west - (-180.0)).abs() < 0.001);
        assert!((bounds.east - 0.0).abs() < 0.001);
        assert!(bounds.north > 0.0);
    }

    #[test]
    fn test_engine_list_sources() {
        let engine = MapTileEngine::new();
        let sources = engine.list_sources();
        assert_eq!(sources.len(), 4);
        assert!(sources.iter().any(|s| s.name == "OpenStreetMap"));
    }

    #[test]
    fn test_resolve_tile_url() {
        let engine = MapTileEngine::new();
        let osm_source = engine
            .list_sources()
            .iter()
            .find(|s| s.name == "OpenStreetMap")
            .unwrap();
        let url = engine
            .resolve_tile_url(
                osm_source.id,
                TileCoord {
                    z: 10,
                    x: 512,
                    y: 340,
                },
            )
            .unwrap();
        assert_eq!(url, "https://tile.openstreetmap.org/10/512/340.png");
    }

    #[test]
    fn test_list_styles() {
        let engine = MapTileEngine::new();
        let styles = engine.list_styles();
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].version, 8);
        assert!(styles[0].sources.contains_key("osm"));
    }

    #[test]
    fn test_vector_layers() {
        let engine = MapTileEngine::new();
        let vector_source = engine
            .list_sources()
            .iter()
            .find(|s| s.source_type == TileSourceType::VectorGeoJson)
            .unwrap();
        let layers = engine.vector_layers(vector_source.id).unwrap();
        assert_eq!(layers.len(), 3);
        assert!(layers.iter().any(|l| l.id == "footprints"));
    }

    #[test]
    fn test_tilejson() {
        let engine = MapTileEngine::new();
        let source = &engine.list_sources()[0];
        let tj = engine.tilejson(source.id).unwrap();
        assert_eq!(tj["tilejson"], "3.0.0");
        assert_eq!(tj["minzoom"], 0);
    }

    #[test]
    fn test_cache_stats() {
        let engine = MapTileEngine::new();
        let stats = engine.cache_stats();
        assert!(stats.hit_rate > 0.9);
        assert!(stats.total_entries > 0);
    }
}

// ─── Martin-core integration ────────────────────────────────────────────────

/// Martin-core backed tile serving: production-grade MBTiles, PMTiles, COG,
/// and PostGIS tile sources via the same engine that powers MapLibre Martin.
///
/// Enable with `--features martin`.
#[cfg(feature = "martin")]
pub mod martin_backend {
    use martin_core::tiles::mbtiles::MbtSource;
    use martin_core::tiles::BoxedSource;
    use martin_core::CacheZoomRange;
    use martin_tile_utils::TileCoord as MartinTileCoord;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Thread-safe registry of martin-core tile sources.
    #[derive(Clone)]
    pub struct MartinTileBackend {
        sources: Arc<RwLock<HashMap<String, BoxedSource>>>,
    }

    impl MartinTileBackend {
        pub fn new() -> Self {
            Self {
                sources: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        /// Register an MBTiles file as a tile source.
        pub async fn add_mbtiles(
            &self,
            id: impl Into<String>,
            path: impl AsRef<Path>,
        ) -> Result<(), String> {
            let id = id.into();
            let source = MbtSource::new(
                id.clone(),
                path.as_ref().to_path_buf(),
                CacheZoomRange::default(),
            )
            .await
            .map_err(|e| format!("Failed to open MBTiles {}: {e}", path.as_ref().display()))?;
            self.sources.write().await.insert(id, Box::new(source));
            Ok(())
        }

        /// List all registered source IDs.
        pub async fn list_source_ids(&self) -> Vec<String> {
            self.sources.read().await.keys().cloned().collect()
        }

        /// Get TileJSON metadata for a source.
        pub async fn tilejson(&self, source_id: &str) -> Option<serde_json::Value> {
            let sources = self.sources.read().await;
            let source = sources.get(source_id)?;
            let tj = source.get_tilejson();
            Some(serde_json::to_value(tj).unwrap_or_default())
        }

        /// Fetch a tile from a martin-core source.
        pub async fn get_tile(
            &self,
            source_id: &str,
            z: u8,
            x: u32,
            y: u32,
        ) -> Result<Vec<u8>, String> {
            let sources = self.sources.read().await;
            let source = sources.get(source_id).ok_or("Source not found")?;

            let coord = MartinTileCoord { z, x, y };
            let data = source
                .get_tile(coord, None)
                .await
                .map_err(|e| format!("Tile fetch failed: {e}"))?;
            Ok(data)
        }
    }

    impl Default for MartinTileBackend {
        fn default() -> Self {
            Self::new()
        }
    }
}
