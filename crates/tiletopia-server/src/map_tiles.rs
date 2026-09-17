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
    use martin_core::tiles::postgres::{
        PostgresPool, PostgresSource, PostgresSqlInfo, RetryTimeout,
    };
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

            let pool = PostgresPool::new(
                connection_string,
                None,
                None,
                None,
                4,
                RetryTimeout::default(),
            )
            .await
            .map_err(|e| format!("PostGIS pool error: {e}"))?;

            // the caller's own sql: an empty tile promises nothing about its children, and
            // the query answers one mvt column with no etag beside it
            let sql_info = PostgresSqlInfo::new(query.to_string(), false, false, id.clone(), false);

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

    /// The backend and the tileset registry the routes read owners and names
    /// from.
    #[derive(Clone)]
    pub struct MartinRoutesState {
        backend: MartinTileBackend,
        db: Arc<crate::db::Database>,
    }

    /// The tile URL a client is told to fetch, with the placeholders a tile
    /// library fills in.
    fn tile_template(source_id: &str) -> String {
        format!("/martin/{source_id}/{{z}}/{{x}}/{{y}}")
    }

    /// Fields tippecanoe fills with the absolute paths of the build, which say
    /// nothing a client needs and everything about the server's filesystem.
    const PATH_BEARING_TILEJSON_FIELDS: [&str; 2] = ["description", "generator_options"];

    /// The TileJSON a client is given: the archive's own, with this server's
    /// tile URL put in and the build's paths taken out. `name` becomes the name
    /// the tileset was uploaded under, or the source id for an archive an
    /// operator dropped in the PMTiles directory.
    fn public_tilejson(
        mut tilejson: serde_json::Value,
        source_id: &str,
        name: &str,
    ) -> serde_json::Value {
        let Some(object) = tilejson.as_object_mut() else {
            return tilejson;
        };
        object.insert(
            "tiles".to_string(),
            serde_json::json!([tile_template(source_id)]),
        );
        object.insert("name".to_string(), serde_json::json!(name));
        for field in PATH_BEARING_TILEJSON_FIELDS {
            object.remove(field);
        }
        tilejson
    }

    /// Build Axum routes for the Martin backend.
    ///
    /// Mounts:
    /// - `GET  /martin/catalog`                — sources the caller may see
    /// - `GET  /martin/:source_id`             — TileJSON for a source
    /// - `GET  /martin/:source_id/:z/:x/:y`    — fetch a tile
    pub fn martin_routes(backend: MartinTileBackend, db: Arc<crate::db::Database>) -> axum::Router {
        use axum::extract::{Path, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;

        /// The built tilesets this caller does not own, which the catalog
        /// leaves out. Empty for an admin and for a run with authentication
        /// turned off.
        async fn hidden_source_ids(
            state: &MartinRoutesState,
            headers: &HeaderMap,
        ) -> Result<Vec<String>, StatusCode> {
            // the auth layer already refused a tokenless request unless this run
            // has authentication off, and then there is nobody to filter for
            let Ok(claims) = crate::users::claims_from_headers(headers) else {
                return Ok(Vec::new());
            };
            if claims.can_admin() {
                return Ok(Vec::new());
            }
            let tilesets = state
                .db
                .list_tilesets()
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(tilesets
                .into_iter()
                .filter(|tileset| tileset.owner_id != claims.sub)
                .map(|tileset| tileset.source_id)
                .collect())
        }

        async fn catalog_handler(
            State(state): State<MartinRoutesState>,
            headers: HeaderMap,
        ) -> Result<axum::Json<Vec<SourceInfo>>, StatusCode> {
            let hidden = hidden_source_ids(&state, &headers).await?;
            let entries = state
                .backend
                .catalog()
                .await
                .into_iter()
                .filter(|entry| !hidden.contains(&entry.id))
                .collect();
            Ok(axum::Json(entries))
        }

        async fn tilejson_handler(
            State(state): State<MartinRoutesState>,
            Path(source_id): Path<String>,
        ) -> impl IntoResponse {
            let Some(tilejson) = state.backend.tilejson(&source_id).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            // a built tileset's source id is its row id, and the row carries the
            // name the uploader gave it
            let stored_name = match uuid::Uuid::parse_str(&source_id) {
                Ok(id) => state
                    .db
                    .get_tileset(id)
                    .await
                    .ok()
                    .flatten()
                    .map(|tileset| tileset.name),
                Err(_) => None,
            };
            let name = stored_name.unwrap_or_else(|| source_id.clone());
            axum::Json(public_tilejson(tilejson, &source_id, &name)).into_response()
        }

        async fn tile_handler(
            State(state): State<MartinRoutesState>,
            Path((source_id, z, x, y)): Path<(String, u8, u32, u32)>,
        ) -> impl IntoResponse {
            let b = &state.backend;
            // get_tile reports an unregistered source as an ordinary error string
            if !b.contains(&source_id).await {
                return StatusCode::NOT_FOUND.into_response();
            }
            // a coordinate outside the zoom's grid is a request for a tile that
            // cannot exist, not a fault of this server
            if MartinTileCoord::new_checked(z, x, y).is_none() {
                return StatusCode::NOT_FOUND.into_response();
            }
            match b.get_tile(&source_id, z, x, y).await {
                Ok(data) if data.is_empty() => StatusCode::NO_CONTENT.into_response(),
                Ok(data) => {
                    let sources = b.sources.read().await;
                    let info = sources.get(&source_id).map(|s| s.get_tile_info());
                    let content_type = info
                        .map(|i| i.format.content_type().to_string())
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    let mut response = (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        data,
                    )
                        .into_response();
                    // tippecanoe archives store gzipped mvt, the browser needs told
                    if let Some(value) = info
                        .and_then(|i| i.encoding.compression())
                        .and_then(|compression| axum::http::HeaderValue::from_str(compression).ok())
                    {
                        response
                            .headers_mut()
                            .insert(axum::http::header::CONTENT_ENCODING, value);
                    }
                    response
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }

        axum::Router::new()
            .route("/martin/catalog", get(catalog_handler))
            .route("/martin/{source_id}", get(tilejson_handler))
            .route("/martin/{source_id}/{z}/{x}/{y}", get(tile_handler))
            .with_state(MartinRoutesState { backend, db })
    }

    /// Directory holding the PMTiles archives to serve.
    pub const PMTILES_DIR_ENV: &str = "TILETOPIA_PMTILES_DIR";

    const PMTILES_EXTENSION: &str = "pmtiles";

    /// Register every `*.pmtiles` file sitting directly in `TILETOPIA_PMTILES_DIR`,
    /// each under its filename stem. Unset registers nothing and is not an error.
    /// A directory that cannot be read is, so a typo in the path stops the server
    /// rather than serving an empty catalog. One archive that fails to open is
    /// logged and skipped.
    pub async fn register_pmtiles_dir(backend: &MartinTileBackend) -> Result<(), String> {
        let Some(dir) = std::env::var(PMTILES_DIR_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            return Ok(());
        };

        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("{PMTILES_DIR_ENV}={dir} cannot be read: {e}"))?;

        for entry in entries {
            let path = entry
                .map_err(|e| format!("{PMTILES_DIR_ENV}={dir} cannot be read: {e}"))?
                .path();
            let is_archive = path.is_file()
                && path.extension().and_then(|e| e.to_str()) == Some(PMTILES_EXTENSION);
            if !is_archive {
                continue;
            }
            let Some(source_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                tracing::warn!("skipping PMTiles archive with a non-UTF-8 name: {path:?}");
                continue;
            };
            match backend.add_pmtiles(source_id, &path).await {
                Ok(()) => tracing::info!(
                    "serving PMTiles source '{source_id}' from {}",
                    path.display()
                ),
                Err(e) => tracing::warn!("skipping PMTiles source '{source_id}': {e}"),
            }
        }

        Ok(())
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
            let rt = tokio::runtime::Runtime::new().unwrap();
            let db =
                rt.block_on(async { crate::db::Database::new("sqlite::memory:").await.unwrap() });
            let router = martin_routes(MartinTileBackend::new(), Arc::new(db));
            // Just verify we can build the router without panic
            let _ = router;
        }

        #[test]
        fn the_public_tilejson_names_this_server_and_drops_the_build_paths() {
            let archive = serde_json::json!({
                "tilejson": "3.0.0",
                "tiles": [],
                "name": "/data/tilesets/abc.pmtiles",
                "description": "/data/tilesets/abc.pmtiles",
                "generator_options": "tippecanoe -o /data/tilesets/abc.pmtiles /scratch/source.geojson",
                "vector_layers": [{"id": "roads"}],
            });

            let public = public_tilejson(archive, "abc", "city roads");

            assert_eq!(
                public["tiles"],
                serde_json::json!(["/martin/abc/{z}/{x}/{y}"])
            );
            assert_eq!(public["name"], "city roads");
            assert_eq!(public["vector_layers"][0]["id"], "roads");
            for field in PATH_BEARING_TILEJSON_FIELDS {
                assert!(public.get(field).is_none(), "{field} survived");
            }
            assert!(
                !public.to_string().contains("/data/"),
                "a build path survived: {public}"
            );
        }
    }
}
