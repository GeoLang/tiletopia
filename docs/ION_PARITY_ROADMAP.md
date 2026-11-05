# Tiletopia → Cesium Ion Parity Roadmap

## Current State

Tiletopia has **real, working** code for:
- Point cloud tiling pipeline (LAS/LAZ/E57/PLY → octree → .pnts 3D Tiles)
- 15 format readers: LAS/LAZ, E57, PLY, GeoTIFF, DTED, HGT, USGS DEM, glTF/GLB, OBJ, FBX, DAE, DXF, CityGML, CityJSON, IFC, plus a TopoJSON to GeoJSON decoder in front of mago-3d-tiler
- Quantized-mesh terrain generation from heightmaps, prebuilt ctb-tile bundles, and global terrain from Copernicus DEM GLO-30
- HTTP tile server (Axum) with JWT auth, multipart upload, Prometheus metrics
- Local filesystem tile store
- CesiumJS/deck.gl/MapLibre web viewer with asset management
- meshopt simplification for mesh LODs, meshopt or Draco geometry compression and KTX2 textures on mesh tiles. `implicit_tiling` compiles but nothing in the tiling pipeline calls it
- Gaussian splat tiling (3DGS PLY → octree with coarser levels → SPZ-compressed `KHR_gaussian_splatting` .glb)
- CLI: `tile`, `serve`, `info`, `validate`, `set-role`, `edge`

Each item below says whether it is done or still open.

---

## Phase 1: Complete the Core Pipeline
*Goal: "upload any supported file, get streamable 3D Tiles"*

### 1.1 GLB/glTF tile output
- **Done**: `tiletopia-core/src/glb_writer.rs` writes `.glb` tiles, carrying UVs and the crop of a texture a tile's triangles reach

### 1.2 Mesh tiling pipeline
- **Done**: `tiletopia-core/src/mesh_tiler.rs` subdivides a mesh and builds LODs with `meshopt` simplification. CityGML, IFC, glTF, OBJ, FBX, DAE and DXF uploads tile through it. The CityJSON reader exists, and the upload route takes no CityJSON extension

### 1.3 Persistent asset database
- **Current**: SQLite via sqlx in `tiletopia-server/src/db.rs`. Tables: assets, ion_id_counter, jobs, tilesets, api_keys, webhook_subscriptions, scheduled_jobs, users, login_failures, organizations, annotations, stories, plugins, portal_items, audit_entries, asset_versions, change_reports, bcf_topics, bcf_projects. `Database::migrate` runs idempotent `CREATE TABLE IF NOT EXISTS` at startup
- **Need**: Versioned migrations. Each added column (assets `owner_id` and `ion_id`, several on jobs, api_keys `token`) is a hand-written `pragma_table_info` check plus `ALTER TABLE`
- **Work**: `schema_version` table, ordered migration steps applied in sequence
- **Impact**: Schema can evolve without per-column pragma probes

### 1.4 Async job queue
- **Current**: `job_queue::JobQueue` writes jobs to the `jobs` table and a background tokio task claims queued rows. A point cloud, mesh, vector or DEM upload returns 201 with a `job_id` field, `POST /api/v1/assets/{id}/tile` returns 202 with the job record, and either id polls at `GET /api/v1/jobs/{id}`. `serve` startup fails every row left `running` by an earlier process, with the restart in the job's `error`
- **Need**: Progress between queued and done (it jumps 0.0 → 1.0), cancellation, and more than one concurrent worker
- **Work**: Progress callback through `tile_point_cloud`, notify instead of the 2 s poll, `cancelled` status
- **Impact**: Real progress bars

### 1.5 Wire TileStore into server
- **Done**: `AppState` holds an `Arc<dyn TileStore>`, the tile and tileset routes read through it, and the job queue writes through it

---

## Phase 2: Format & Quality Parity
*Goal: "match Ion's input format support and output quality"*

### 2.1 Automatic CRS reprojection
- **Done for point clouds**: the tiling job calls `read_point_cloud_ecef`, which takes the CRS from `crs_detect` (a LAS GeoKey VLR, GeoTIFF tags or a `.prj` sidecar) or else from the upload's `crs` field, and reprojects to ECEF through projicio. A point cloud with no CRS from either is tiled in its own coordinates, with a warning naming the file
- **Left**: the native mesh path places a mesh by longitude and latitude alone, and an upload with `crs` answers 400. Reprojecting a mesh needs f64 positions, since f32 keeps about 0.5 m at a UTM northing, and a rule for which axis is up in a y-up format

### 2.2 DEM tile download & caching
- **Done**: `/api/v1/terrain/` reads DEM files under `<data-dir>/dem` first, then downloads and caches SRTM tiles. A failed download answers 503 naming the tile rather than a flat mesh. Analysis tiles can read Copernicus GLO-30 over STAC instead, see `TILETOPIA_ANALYSIS_DEM_BBOX`. A DEM upload builds a quantized-mesh bundle named after the asset id
- **Done**: `/api/v1/terrain/global/` serves the whole Earth from Copernicus GLO-30 COGs read by `Range`, with ellipsoidal heights through a geoid grid and a capped disk cache, the counterpart of Cesium World Terrain
- **Left**: levels 0 to 6 carry the geoid alone, since a tile there touches up to 16 one-degree cells at level 6 and 32,400 at level 0. Relief there needs an offline pyramid, or a global coarse DEM in one COG. No vertex normals or water mask

### 2.3 3D Tiles Next output (EXT_structural_metadata)
- **Done**: `glb_writer` declares `EXT_structural_metadata` and writes per-feature properties into the tile. An IFC element's GlobalId rides there as `asset_id`, so a client picking a tile feature reads the id back

### 2.4 Imagery/raster tiling
- **Done**: Georeferenced GeoTIFF, COG, JPEG, PNG and JPEG 2000 uploads serve XYZ PNG tiles and TileJSON, rendered on demand from COG overviews through geoplumb. Uploads that are not COGs convert in a fixed amount of memory up to 65,535 px a side, with JPEG capped near 6300x6300 because it decodes whole. The Ion endpoint answers `URL_TEMPLATE`, the case `IonImageryProvider.fromAssetId` handles in the CesiumJS 1.147 source, and no CesiumJS run has loaded it yet. The same tiles are served as OGC WMTS 1.0.0, KVP and RESTful, in `GoogleMapsCompatible`, and as an OSGeo TMS 1.0 TileMap with rows from the south
- **Impact**: Aerial/satellite imagery serving (like Ion's imagery assets)

### 2.5 Photogrammetry mesh support
- **Current**: The mesh tiler carries diffuse textures and crops them per tile. A zip of geotagged JPEGs goes through a NodeODM server to a textured mesh and 3D Tiles
- **Need**: Handle large textured meshes from reality capture
- **Work**: Texture atlas packing in the mesh tiler
- **Impact**: Pix4D/RealityCapture/Metashape output → 3D Tiles

### 2.6 Gaussian splats
- **Done**: `tiletopia-core/src/splat_tiler.rs` tiles a 3DGS PLY, which `tiletopia-ingest/src/splat_ply_reader.rs` streams, into SPZ-compressed glTF with `KHR_gaussian_splatting` and `KHR_gaussian_splatting_compression_spz_2`, the pair the CesiumJS 1.147 source reads as splats, and no CesiumJS run has drawn them yet. A PLY upload with splat properties goes there, placed by longitude and latitude. Coarser tiles keep the splats with the largest opacity times area. Spherical harmonics up to degree 3 are kept. The job fits 4,805,821 splats at degree 3
- **Left**: SPZ uploads, merged or enlarged splats at coarser levels, a re-tile that keeps the placement, and the CLI `tile` command
- **Impact**: 3DGS captures stream to CesiumJS as they do from Ion

### 2.7 CAD and vector inputs
- **Done**: ASCII DXF tiles natively from 3DFACE, polyface and polygon mesh POLYLINE, MESH and nested INSERT blocks, scaled by `$INSUNITS`. TopoJSON is decoded to GeoJSON for mago-3d-tiler. CZML is stored, served as uploaded, and resolves through the Ion endpoint as type `CZML`, the way Ion hosts CZML without tiling
- **Left**: binary DXF, ACIS solids (3DSOLID, BODY, REGION), 2D entities extruded by a thickness, DXF colours, DWG, DGN and RVT. KML and GeoJSON hosted as uploaded, which Ion also offers
- **Impact**: CAD drawings and CZML scenes from Ion workflows load without conversion

---

## Phase 3: Production Readiness
*Goal: "deployable as a self-hosted Ion replacement"*

### 3.1 User & organization management
- **Done**: `POST /api/v1/auth/signup` and `/login` answer a 24-hour JWT, roles are admin, editor and viewer, orgs have their own routes, and asset, tileset, annotation and story writes check ownership. `tiletopia set-role` promotes the first admin against the database
- **Left**: no password reset and no session revocation. A minted token stands until it expires

### 3.2 Admin dashboard
- **Current**: `/api/v1/admin/stats`, `/health`, `/jobs` and `/users` (list, delete, set role, set org) answer, admin only. The GUI's admin view shows the audit trail from `GET /api/v1/audit` and nothing from those routes
- **Need**: Usage monitoring, job status, user management and storage metrics in the GUI
- **Work**: Add those routes to the GUI's admin view
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
- **Current**: `catalog.rs` lists open datasets (Copernicus, SRTM, OSM and others) at `GET /api/v1/catalog`. `POST /api/v1/catalog/{id}/add` creates an asset and queues a job whose input is the dataset URL, and the job queue only reads local files, so that job fails. `POST /api/v1/assets/{id}/tile` on that asset answers 404
- **Need**: One-click add of curated datasets to your workspace that fetches the data
- **Work**: Wire catalog to DEM downloader, OSM Buildings extrusion. Global terrain needs no download, see 2.2. OSM Buildings is an offline per-region build, not built yet

### 4.3 Real-time collaboration
- **Current**: `/api/v1/realtime/{room}` relays six message types, presence and cursors among them. Nothing is persisted
- **Need**: concurrent editing. No merge or CRDT code exists

### 4.4 Plugin marketplace
- **Current**: `plugin_registry` installs from an owner-controlled registry, admin only, and the `wasm-plugins` feature runs a plugin under wasmtime
- **Need**: discovery and a public listing

### 4.5 Self-hosted Ion API compatibility
- **Current**: `/v1/assets`, `/v1/assets/{id}` and `/v1/assets/{id}/endpoint` answer, so asset id and endpoint resolution work. A terrain asset resolves to a bundle named after the asset id, which a DEM upload builds itself, and a CZML asset resolves to its document. `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID` resolves one id to the global terrain and new assets never take it, so with that id set to 1 this server answers the endpoint request `createWorldTerrainAsync` makes in the CesiumJS 1.147 source. No CesiumJS run has loaded it yet. `POST /v1/assets` answers an S3 location on this server and `POST /v1/assets/{id}/uploadComplete` queues the tiling job. `position` on `POINT_CLOUD` places a Gaussian splat PLY, and `BIM_CAD` takes a DXF that names its unit, since Ion's spec has no `units` option. A source is one zip or several files uploaded under the prefix, and the one file whose extension the source type takes is the main file. Object keys with subdirectories answer 403. An IFC gets a per-asset element store, and `/v1/assets/{id}/elements` and `/tree` serve list with filter, get, search, aggregate with `groupBy`, properties and property-index, with GlobalIds as element ids and IFC's spatial tree mapped onto Ion's node kinds. `/v1/assets/{id}/section` answers each element's cross-section on an axis-aligned plane as polygons in the tileset's local frame, for a viewer to fill the section caps CesiumJS clipping planes leave open. These need a JWT that passes `may_view_asset`
- **Need**: the rest of Ion's contract: merging several models into one asset, reconstruction from photos under `RASTER_IMAGERY`, `DATA_ERROR`, the options that answer 400 today, element snap, distinct values per element property, and Ion's token API, while `/api/v1/api-keys` mints this server's credentials. `/v1/assets`, `/v1/assets/{id}` and `/v1/assets/{id}/endpoint` need a JWT in the `Authorization` header and pass the owner check, because Ion's `access_token` query parameter is not read as a credential. The endpoint of the configured global terrain id needs no credential. `POST /v1/assets` and `POST /v1/assets/{id}/uploadComplete` (editor) are the only writes

---

## Estimated Effort by Phase

| Phase | Description | Scope |
|-------|-------------|-------|
| 1 | Core Pipeline | 5 work items, most critical |
| 2 | Format & Quality | 7 work items, broadens use cases |
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
4. **More input formats**: E57, PLY, FBX, DXF and GeoPackage tile, and DTED and HGT upload as terrain assets with a quantized-mesh bundle. Ion also tiles IFC and GeoTIFF
5. **Edge deployment**: a small binary. `tiletopia edge` prints the cross-build commands for a target, it does not run them
