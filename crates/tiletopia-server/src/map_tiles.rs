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
        let mut tj = serde_json::json!({
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
        });

        // Include vector_layers for MVT/vector sources (TileJSON 3.0.0 spec)
        if (source.source_type == TileSourceType::VectorGeoJson
            || source.source_type == TileSourceType::VectorPostGis)
            && let Some(layers) = self.vector_layers(source_id)
        {
            let vl: Vec<serde_json::Value> = layers
                .iter()
                .map(|l| {
                    let fields: serde_json::Map<String, serde_json::Value> = l
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                serde_json::Value::String(match f.field_type {
                                    FieldType::String => "String".into(),
                                    FieldType::Number => "Number".into(),
                                    FieldType::Boolean => "Boolean".into(),
                                }),
                            )
                        })
                        .collect();
                    serde_json::json!({
                        "id": l.id,
                        "description": l.name,
                        "minzoom": l.min_zoom,
                        "maxzoom": l.max_zoom,
                        "fields": fields,
                    })
                })
                .collect();
            tj["vector_layers"] = serde_json::Value::Array(vl);
        }

        Some(tj)
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

/// Martin-core backed tile serving: production-grade MBTiles, PMTiles,
/// and PostGIS tile sources via the same engine that powers MapLibre Martin.
///
/// Enable with `--features martin`.
#[cfg(feature = "martin")]
pub mod martin_backend {
    use martin_core::CacheZoomRange;
    use martin_core::tiles::BoxedSource;
    use martin_core::tiles::mbtiles::MbtSource;
    use martin_core::tiles::pmtiles::{PmtCache, PmtCacheInstance, PmtilesSource};
    use martin_core::tiles::postgres::{PostgresPool, PostgresSource, PostgresSqlInfo};
    use martin_tile_utils::{TileCoord as MartinTileCoord, TileInfo};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::RwLock;

    /// Source type discriminant for catalog entries.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum MartinSourceKind {
        MBTiles,
        PMTiles,
        PostGIS,
    }

    /// Catalog entry describing a registered source.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct SourceInfo {
        pub id: String,
        pub kind: MartinSourceKind,
        pub content_type: String,
    }

    /// Thread-safe registry of martin-core tile sources.
    ///
    /// Supports MBTiles, PMTiles, and PostGIS data sources with a unified
    /// tile-fetching API. Each source is registered with a unique string ID
    /// and can be queried, listed, or removed at runtime.
    #[derive(Clone)]
    pub struct MartinTileBackend {
        sources: Arc<RwLock<HashMap<String, BoxedSource>>>,
        kinds: Arc<RwLock<HashMap<String, MartinSourceKind>>>,
        pmt_cache: Arc<PmtCache>,
        pmt_counter: Arc<AtomicUsize>,
    }

    impl MartinTileBackend {
        /// Create a new empty backend.
        pub fn new() -> Self {
            Self {
                sources: Arc::new(RwLock::new(HashMap::new())),
                kinds: Arc::new(RwLock::new(HashMap::new())),
                pmt_cache: Arc::new(PmtCache::new(128 * 1024 * 1024, None, None)),
                pmt_counter: Arc::new(AtomicUsize::new(0)),
            }
        }

        // ── MBTiles ──────────────────────────────────────────────────────

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
            self.sources
                .write()
                .await
                .insert(id.clone(), Box::new(source));
            self.kinds
                .write()
                .await
                .insert(id, MartinSourceKind::MBTiles);
            Ok(())
        }

        // ── PMTiles ──────────────────────────────────────────────────────

        /// Register a local PMTiles file as a tile source.
        pub async fn add_pmtiles(
            &self,
            id: impl Into<String>,
            path: impl AsRef<Path>,
        ) -> Result<(), String> {
            let id = id.into();
            let abs = std::fs::canonicalize(path.as_ref())
                .map_err(|e| format!("Cannot resolve {}: {e}", path.as_ref().display()))?;
            let parent = abs.parent().unwrap_or_else(|| Path::new("/"));
            let filename = abs
                .file_name()
                .ok_or("Invalid PMTiles path")?
                .to_string_lossy()
                .to_string();

            let store = object_store::local::LocalFileSystem::new_with_prefix(parent)
                .map_err(|e| format!("Cannot create object store: {e}"))?;

            let cache_id = self.pmt_counter.fetch_add(1, Ordering::Relaxed);
            let cache_instance = PmtCacheInstance::new(cache_id, (*self.pmt_cache).clone());

            let source = PmtilesSource::new(
                cache_instance,
                id.clone(),
                Box::new(store),
                filename,
                CacheZoomRange::default(),
            )
            .await
            .map_err(|e| format!("Failed to open PMTiles {}: {e}", path.as_ref().display()))?;

            self.sources
                .write()
                .await
                .insert(id.clone(), Box::new(source));
            self.kinds
                .write()
                .await
                .insert(id, MartinSourceKind::PMTiles);
            Ok(())
        }

        // ── PostGIS ──────────────────────────────────────────────────────

        /// Register a PostGIS table/function as a tile source.
        ///
        /// `connection_string` is a standard `postgresql://` URL.
        /// `query` is the SQL query that generates MVT tile bytes, e.g.:
        /// ```sql
        /// SELECT ST_AsMVT(q, 'layer', 4096, 'geom')
        /// FROM (
        ///   SELECT id, name, ST_AsMVTGeom(geom, ST_TileEnvelope($1,$2,$3), 4096, 64, true) AS geom
        ///   FROM my_table
        ///   WHERE geom && ST_TileEnvelope($1,$2,$3)
        /// ) q
        /// ```
        pub async fn add_postgis(
            &self,
            id: impl Into<String>,
            connection_string: &str,
            query: &str,
        ) -> Result<(), String> {
            let id = id.into();

            let pool = PostgresPool::new(connection_string, None, None, None, 4)
                .await
                .map_err(|e| format!("PostGIS pool error: {e}"))?;

            let sql_info = PostgresSqlInfo::new(query.to_string(), false, id.clone());

            let mut tilejson = tilejson::tilejson! {
                tiles: vec![format!("/martin/{id}/{{z}}/{{x}}/{{y}}")],
            };
            tilejson.name = Some(id.clone());

            let source = PostgresSource::new(
                id.clone(),
                sql_info,
                tilejson,
                pool,
                TileInfo::new(
                    martin_tile_utils::Format::Mvt,
                    martin_tile_utils::Encoding::Uncompressed,
                ),
                CacheZoomRange::default(),
            );

            self.sources
                .write()
                .await
                .insert(id.clone(), Box::new(source));
            self.kinds
                .write()
                .await
                .insert(id, MartinSourceKind::PostGIS);
            Ok(())
        }

        // ── Unified API ──────────────────────────────────────────────────

        /// List all registered source IDs.
        pub async fn list_source_ids(&self) -> Vec<String> {
            self.sources.read().await.keys().cloned().collect()
        }

        /// Get catalog info for all sources.
        pub async fn catalog(&self) -> Vec<SourceInfo> {
            let sources = self.sources.read().await;
            let kinds = self.kinds.read().await;
            sources
                .iter()
                .map(|(id, src)| SourceInfo {
                    id: id.clone(),
                    kind: kinds.get(id).cloned().unwrap_or(MartinSourceKind::MBTiles),
                    content_type: src.get_tile_info().format.content_type().to_string(),
                })
                .collect()
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

        /// Remove a source by ID.
        pub async fn remove_source(&self, source_id: &str) -> bool {
            let removed = self.sources.write().await.remove(source_id).is_some();
            if removed {
                self.kinds.write().await.remove(source_id);
            }
            removed
        }

        /// Check whether a source ID is registered.
        pub async fn contains(&self, source_id: &str) -> bool {
            self.sources.read().await.contains_key(source_id)
        }

        /// Get the source kind for a given ID.
        pub async fn source_kind(&self, source_id: &str) -> Option<MartinSourceKind> {
            self.kinds.read().await.get(source_id).cloned()
        }
    }

    impl Default for MartinTileBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    // ── Axum route helpers ───────────────────────────────────────────────

    /// Build Axum routes for the Martin backend.
    ///
    /// Mounts:
    /// - `GET  /martin/catalog`                — list all sources
    /// - `GET  /martin/:source_id`             — TileJSON for a source
    /// - `GET  /martin/:source_id/:z/:x/:y`    — fetch a tile
    pub fn martin_routes(backend: MartinTileBackend) -> axum::Router {
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn catalog_handler(State(b): State<MartinTileBackend>) -> impl IntoResponse {
            let entries = b.catalog().await;
            axum::Json(entries).into_response()
        }

        async fn tilejson_handler(
            State(b): State<MartinTileBackend>,
            Path(source_id): Path<String>,
        ) -> impl IntoResponse {
            match b.tilejson(&source_id).await {
                Some(tj) => axum::Json(tj).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }

        async fn tile_handler(
            State(b): State<MartinTileBackend>,
            Path((source_id, z, x, y)): Path<(String, u8, u32, u32)>,
        ) -> impl IntoResponse {
            match b.get_tile(&source_id, z, x, y).await {
                Ok(data) if data.is_empty() => StatusCode::NO_CONTENT.into_response(),
                Ok(data) => {
                    let sources = b.sources.read().await;
                    let content_type = sources
                        .get(&source_id)
                        .map(|s| s.get_tile_info().format.content_type().to_string())
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        data,
                    )
                        .into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }

        axum::Router::new()
            .route("/martin/catalog", get(catalog_handler))
            .route("/martin/{source_id}", get(tilejson_handler))
            .route("/martin/{source_id}/{z}/{x}/{y}", get(tile_handler))
            .with_state(backend)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn backend_default() {
            let backend = MartinTileBackend::default();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let ids = rt.block_on(backend.list_source_ids());
            assert!(ids.is_empty());
        }

        #[test]
        fn backend_contains_empty() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            assert!(!rt.block_on(backend.contains("nonexistent")));
        }

        #[test]
        fn backend_catalog_empty() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let catalog = rt.block_on(backend.catalog());
            assert!(catalog.is_empty());
        }

        #[test]
        fn backend_remove_nonexistent() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            assert!(!rt.block_on(backend.remove_source("missing")));
        }

        #[test]
        fn backend_tilejson_missing_source() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            assert!(rt.block_on(backend.tilejson("missing")).is_none());
        }

        #[test]
        fn backend_get_tile_missing_source() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(backend.get_tile("missing", 0, 0, 0));
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), "Source not found");
        }

        #[test]
        fn backend_source_kind_missing() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            assert!(rt.block_on(backend.source_kind("missing")).is_none());
        }

        #[test]
        fn backend_add_mbtiles_nonexistent_file() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(backend.add_mbtiles("test", "/tmp/nonexistent.mbtiles"));
            assert!(result.is_err());
        }

        #[test]
        fn backend_add_pmtiles_nonexistent_file() {
            let backend = MartinTileBackend::new();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let result = rt.block_on(backend.add_pmtiles("test", "/tmp/nonexistent.pmtiles"));
            assert!(result.is_err());
        }

        #[test]
        fn source_kind_serde_roundtrip() {
            let kind = MartinSourceKind::PMTiles;
            let json = serde_json::to_string(&kind).unwrap();
            let back: MartinSourceKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }

        #[test]
        fn source_info_serialize() {
            let info = SourceInfo {
                id: "my-source".to_string(),
                kind: MartinSourceKind::PostGIS,
                content_type: "application/x-protobuf".to_string(),
            };
            let json = serde_json::to_value(&info).unwrap();
            assert_eq!(json["id"], "my-source");
            assert_eq!(json["kind"], "PostGIS");
            assert_eq!(json["content_type"], "application/x-protobuf");
        }

        #[test]
        fn martin_routes_build() {
            let backend = MartinTileBackend::new();
            let router = martin_routes(backend);
            // Just verify we can build the router without panic
            let _ = router;
        }
    }
}
