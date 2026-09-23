# Tiletopia → Cesium Ion Parity Roadmap

## Current State

Tiletopia has **real, working** code for:
- Point cloud tiling pipeline (LAS/LAZ/E57/PLY → octree → .pnts 3D Tiles)
- 13 format readers: LAS/LAZ, E57, PLY, GeoTIFF, DTED, HGT, USGS DEM, glTF/GLB, OBJ, FBX, CityGML, CityJSON, IFC
- Quantized-mesh terrain generation from heightmaps, and prebuilt ctb-tile bundles
- HTTP tile server (Axum) with JWT auth, multipart upload, Prometheus metrics
- Local filesystem tile store
- CesiumJS/deck.gl/MapLibre web viewer with asset management
- meshopt simplification for mesh LODs. `implicit_tiling`, `draco_encode_mesh` and the `gpu` decimation module compile but nothing in the tiling pipeline calls any of them
- CLI: `tile`, `serve`, `info`, `validate`, `set-role`, `edge`

Each item below says whether it is done or still open.

---

## Phase 1: Complete the Core Pipeline
*Goal: "upload any supported file, get streamable 3D Tiles"*

### 1.1 GLB/glTF tile output
- **Done**: `tiletopia-core/src/glb_writer.rs` writes `.glb` tiles, carrying UVs and the crop of a texture a tile's triangles reach

### 1.2 Mesh tiling pipeline
- **Done**: `tiletopia-core/src/mesh_tiler.rs` subdivides a mesh and builds LODs with `meshopt` simplification. CityGML, IFC, glTF, OBJ and FBX uploads tile through it. The CityJSON reader exists, and the upload route takes no CityJSON extension

### 1.3 Persistent asset database
- **Current**: SQLite via sqlx in `tiletopia-server/src/db.rs`. Tables: assets, ion_id_counter, jobs, tilesets, api_keys, webhook_subscriptions, scheduled_jobs, users, organizations, annotations, stories, plugins, portal_items, audit_entries. `Database::migrate` runs idempotent `CREATE TABLE IF NOT EXISTS` at startup
- **Need**: Versioned migrations. Each added column (assets `owner_id` and `ion_id`, several on jobs, api_keys `token`) is a hand-written `pragma_table_info` check plus `ALTER TABLE`
- **Work**: `schema_version` table, ordered migration steps applied in sequence
- **Impact**: Schema can evolve without per-column pragma probes

### 1.4 Async job queue
- **Current**: `job_queue::JobQueue` writes jobs to the `jobs` table and a background tokio task claims queued rows. A point cloud, mesh or vector upload returns 201 with a `job_id` field, `POST /api/v1/assets/{id}/tile` returns 202 with the job record, and either id polls at `GET /api/v1/jobs/{id}`
- **Need**: Progress between queued and done (it jumps 0.0 → 1.0), cancellation, requeue of rows left `running` by a crashed process, and more than one concurrent worker
- **Work**: Progress callback through `tile_point_cloud`, notify instead of the 2 s poll, `cancelled` status, stale-job sweep at startup
- **Impact**: Real progress bars and a queue that survives a restart mid-job

### 1.5 Wire TileStore into server
- **Done**: `AppState` holds an `Arc<dyn TileStore>`, the tile and tileset routes read through it, and the job queue writes through it

---

## Phase 2: Format & Quality Parity
*Goal: "match Ion's input format support and output quality"*

### 2.1 Automatic CRS reprojection
- **Current**: `tiletopia_ingest::crs_detect` reads the CRS from a LAS GeoKey VLR, GeoTIFF tags or a `.prj` sidecar and reprojects to WGS84 through projicio. Only the LAS/LAZ export calls it. The tiling job reads point clouds without reprojecting
- **Need**: Reproject every input to WGS84/ECEF before tiling
- **Work**: Call CRS detection from the tiling job and the mesh readers
- **Impact**: Users don't need to manually reproject

### 2.2 DEM tile download & caching
- **Done**: `/api/v1/terrain/` reads DEM files under `<data-dir>/dem` first, then downloads and caches SRTM tiles. A failed download answers 503 naming the tile rather than a flat mesh. Analysis tiles can read Copernicus GLO-30 over STAC instead, see `TILETOPIA_ANALYSIS_DEM_BBOX`

### 2.3 3D Tiles Next output (EXT_structural_metadata)
- **Done**: `glb_writer` declares `EXT_structural_metadata` and writes per-feature properties into the tile. An IFC element's GlobalId rides there as `asset_id`, so a client picking a tile feature reads the id back

### 2.4 Imagery/raster tiling
- **Current**: No imagery pipeline
- **Need**: GeoTIFF/ortho → TMS/WMTS map tiles (PNG/JPEG/WebP pyramids)
- **Work**: New `imagery_tiler.rs`: tile pyramid generation with overviews
- **Impact**: Aerial/satellite imagery serving (like Ion's imagery assets)

### 2.5 Photogrammetry mesh support
- **Current**: The mesh tiler carries diffuse textures and crops them per tile. There is no photogrammetry-specific pipeline
- **Need**: Handle large textured meshes from reality capture
- **Work**: Texture atlas packing in the mesh tiler
- **Impact**: Pix4D/RealityCapture/Metashape output → 3D Tiles

---

## Phase 3: Production Readiness
*Goal: "deployable as a self-hosted Ion replacement"*

### 3.1 User & organization management
- **Done**: `POST /api/v1/auth/signup` and `/login` answer a 24-hour JWT, roles are admin, editor and viewer, orgs have their own routes, and asset, tileset, annotation and story writes check ownership. `tiletopia set-role` promotes the first admin against the database
- **Left**: no password reset and no session revocation. A minted token stands until it expires

### 3.2 Admin dashboard
- **Current**: `/api/v1/admin/stats`, `/health`, `/jobs` and `/users` (list, delete, set role, set org) answer, admin only. The GUI has no admin pages
- **Need**: Usage monitoring, job status, user management and storage metrics in the GUI
- **Work**: Admin pages in the GUI over those routes
- **Impact**: Ops visibility

### 3.3 Asset management improvements
- **Current**: Basic CRUD
- **Need**: Tagging, search, thumbnails, preview generation, attribution
- **Work**: Add metadata fields to asset schema, auto-generate thumbnails
- **Impact**: Usable asset library

### 3.4 Viewer tools
- **Current**: The GUI measures distance, area and height, places annotations, picks features and has a style editor
- **Need**: Volume measurement
- **Work**: A volume tool beside the measurement tools
- **Impact**: Matches Ion's viewer capabilities

### 3.5 Containerized deployment
- **Done**: `Dockerfile`, `docker-compose.yml`, a Helm chart under `deploy/helm` and terraform under `deploy/terraform`

---

## Phase 4: Competitive Differentiation
*Goal: "reasons to choose Tiletopia over Ion"*

### 4.1 Cesium Stories equivalent
- **Done on the server**: stories are stored with their slides, an editor authors one, and `GET /api/v1/stories/share/{token}` serves a shared story to anyone holding the token
- **Left**: no story editor in the GUI

### 4.2 Curated open data catalog
- **Current**: `catalog.rs` lists open datasets (Copernicus, SRTM, OSM and others) at `GET /api/v1/catalog`. `POST /api/v1/catalog/{id}/add` creates an asset and queues a job whose input is the dataset URL, and the job queue only reads local files, so that job fails
- **Need**: One-click add of curated datasets to your workspace that fetches the data
- **Work**: Wire catalog to DEM downloader, OSM Buildings extrusion

### 4.3 Real-time collaboration
- **Current**: `/api/v1/realtime/{room}` relays six message types, presence and cursors among them. Nothing is persisted
- **Need**: concurrent editing. No merge or CRDT code exists

### 4.4 Plugin marketplace
- **Current**: `plugin_registry` installs from an owner-controlled registry, admin only, and the `wasm-plugins` feature runs a plugin under wasmtime
- **Need**: discovery and a public listing

### 4.5 Self-hosted Ion API compatibility
- **Current**: `/v1/assets`, `/v1/assets/{id}`, `/v1/assets/{id}/endpoint` and `/v1/tokens` answer, so asset id and endpoint resolution work. A terrain asset resolves to a prebuilt bundle named after the asset id
- **Need**: the rest of Ion's contract. The four reads need no credential. `POST /v1/assets` (editor) and `POST /v1/tokens` (admin) are the only writes

---

## Estimated Effort by Phase

| Phase | Description | Scope |
|-------|-------------|-------|
| 1 | Core Pipeline | 5 work items, most critical |
| 2 | Format & Quality | 5 work items, broadens use cases |
| 3 | Production | 5 work items, deployment readiness |
| 4 | Differentiation | 5 work items, competitive features |

**Minimum viable Ion replacement**: Phase 1 + items 2.1, 2.2, 3.5
This gets you: upload → auto-reproject → tile → serve off the local filesystem
with terrain, in Docker. Local disk is the only store.

**Full parity**: Phase 1 + Phase 2 + Phase 3
**Exceeds Ion**: All four phases (self-hosted, open source, extensible)

---

## What Tiletopia Already Does Better Than Ion

1. **Self-hosted**: runs on your own machine. Air-gapped needs staged DEM files under `<data-dir>/dem`, since terrain otherwise downloads SRTM tiles, and the ndvi analysis layer always reads an upstream STAC API
2. **Open source (AGPL-3.0)**
3. **Multiple viewer engines**: CesiumJS, deck.gl and MapLibre, where Ion's viewer is CesiumJS
4. **More input formats**: IFC, FBX and GeoPackage tile, and DTED and HGT upload as terrain assets
5. **Edge deployment**: a small binary. `tiletopia edge` prints the cross-build commands for a target, it does not run them
