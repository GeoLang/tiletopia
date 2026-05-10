# Tiletopia → Cesium Ion Parity Roadmap

## Current State

Tiletopia has **real, working** code for:
- Point cloud tiling pipeline (LAS/LAZ/E57/PLY → octree → .pnts 3D Tiles)
- 20+ format readers (point clouds, meshes, terrain, vector)
- Quantized-mesh terrain generation from heightmaps
- HTTP tile server (Axum) with JWT auth, streaming upload, Prometheus metrics
- Storage backends (local, S3, GCS, Azure) with hot/cold tiering
- CesiumJS/deck.gl/MapLibre web viewer with asset management
- Implicit tiling, Draco/meshopt compression
- CLI: `tile`, `serve`, `info`, `validate`

**What's missing for Ion parity** (in priority order):

---

## Phase 1: Complete the Core Pipeline
*Goal: "upload any supported file, get streamable 3D Tiles"*

### 1.1 GLB/glTF tile output
- **Current**: Only `.pnts` (point cloud) tiles are written
- **Need**: Write `.glb` tiles for meshes (b3dm-equivalent via glTF)
- **Work**: Add GLB writer to `tiletopia-core/src/tile.rs` using `gltf` crate
- **Impact**: Unlocks mesh visualization in CesiumJS

### 1.2 Mesh tiling pipeline
- **Current**: Mesh readers work, but no spatial subdivision or LOD for meshes
- **Need**: Partition meshes spatially (BVH or octree), generate LODs via meshopt simplification, write GLB tiles per node
- **Work**: New `mesh_tiler.rs` in tiletopia-core, extend `tileset.rs` pipeline
- **Impact**: CityGML/IFC/glTF/OBJ → streamable 3D Tiles

### 1.3 Persistent asset database
- **Current**: Assets stored in `Vec<Asset>` in memory (lost on restart)
- **Need**: SQLite database (rusqlite) for assets, jobs, users, API keys
- **Work**: Add `tiletopia-db` crate or embed in server. Schema: assets, jobs, users, tokens
- **Impact**: Production-grade persistence

### 1.4 Async job queue
- **Current**: `run_job()` is synchronous, no persistence
- **Need**: Background worker with job queue (SQLite-backed or Redis)
- **Work**: Tokio task spawning, job table in DB, progress polling endpoint
- **Impact**: Non-blocking upload → tile workflow

### 1.5 Wire TileStore into server
- **Current**: Server reads/writes tiles directly to filesystem, ignoring TileStore trait
- **Need**: Route all tile I/O through TileStore so S3/GCS/Azure backends work
- **Work**: Replace `fs::read`/`fs::write` calls in server with TileStore calls
- **Impact**: Cloud-native deployment (tiles on S3, server stateless)

---

## Phase 2: Format & Quality Parity
*Goal: "match Ion's input format support and output quality"*

### 2.1 Automatic CRS reprojection
- **Current**: CRS module has UTM↔WGS84 + proj4rs fallback, but not wired into ingest pipeline
- **Need**: Auto-detect source CRS (from GeoTIFF tags, LAS VLR, PRJ files), reproject to WGS84/ECEF
- **Work**: Add CRS detection to each reader, reproject before tiling
- **Impact**: Users don't need to manually reproject

### 2.2 DEM tile download & caching
- **Current**: Terrain API serves flat tiles when no local DEM exists
- **Need**: Download SRTM/Copernicus DEM tiles on-demand, cache locally
- **Work**: HTTP fetcher for SRTM HGT files (public S3 bucket), local cache directory
- **Impact**: Real terrain without user-supplied data

### 2.3 3D Tiles Next output (EXT_structural_metadata)
- **Current**: Metadata types defined but not serialized into tiles
- **Need**: Embed per-feature metadata (classification, IFC properties, CityGML attributes) in GLB tiles
- **Work**: Extend GLB writer to include EXT_structural_metadata extension
- **Impact**: Feature picking, styling by attribute in CesiumJS

### 2.4 Imagery/raster tiling
- **Current**: No imagery pipeline
- **Need**: GeoTIFF/ortho → TMS/WMTS map tiles (PNG/JPEG/WebP pyramids)
- **Work**: New `imagery_tiler.rs` — tile pyramid generation with overviews
- **Impact**: Aerial/satellite imagery serving (like Ion's imagery assets)

### 2.5 Photogrammetry mesh support
- **Current**: Can read glTF meshes but no photogrammetry-specific pipeline
- **Need**: Handle large textured meshes from reality capture (texture atlas, mesh splitting)
- **Work**: Extend mesh tiler for textured meshes, texture atlas packing
- **Impact**: Pix4D/RealityCapture/Metashape output → 3D Tiles

---

## Phase 3: Production Readiness
*Goal: "deployable as a self-hosted Ion replacement"*

### 3.1 User & organization management
- **Current**: JWT middleware exists, no user CRUD
- **Need**: User signup/login, org/team model, per-asset permissions
- **Work**: Auth endpoints, password hashing (argon2), session management
- **Impact**: Multi-user deployment

### 3.2 Admin dashboard
- **Current**: Web viewer shows assets, no admin features
- **Need**: Usage monitoring, job status, user management, storage metrics
- **Work**: Extend GUI with admin pages, add server endpoints for stats
- **Impact**: Ops visibility

### 3.3 Asset management improvements
- **Current**: Basic CRUD
- **Need**: Tagging, search, thumbnails, preview generation, attribution
- **Work**: Add metadata fields to asset schema, auto-generate thumbnails
- **Impact**: Usable asset library

### 3.4 Viewer tools
- **Current**: CesiumJS viewer loads tilesets
- **Need**: Measurement (distance, area, volume), annotations, feature picking, styling
- **Work**: CesiumJS measurement widgets, annotation layer, style editor
- **Impact**: Matches Ion's viewer capabilities

### 3.5 Containerized deployment
- **Current**: Dockerfile exists
- **Need**: Docker Compose with worker, DB, object store (MinIO). Helm chart for k8s
- **Work**: Compose file + Helm chart + deployment docs
- **Impact**: One-command deployment

---

## Phase 4: Competitive Differentiation
*Goal: "reasons to choose Tiletopia over Ion"*

### 4.1 Cesium Stories equivalent
- **Need**: No-code 3D presentation builder with sharing
- **Work**: Story editor in GUI (waypoints, annotations, styles), shareable URLs

### 4.2 Curated open data catalog
- **Current**: `catalog.rs` has a registry of open datasets (Copernicus, SRTM, OSM)
- **Need**: One-click add of curated datasets to your workspace
- **Work**: Wire catalog to DEM downloader, OSM Buildings extrusion

### 4.3 Real-time collaboration
- **Current**: WebSocket broadcast infrastructure exists
- **Need**: CRDT-based concurrent editing, presence indicators
- **Work**: Integrate Yjs or Automerge via WebSocket

### 4.4 Plugin marketplace
- **Current**: Plugin loading framework exists
- **Need**: Plugin registry, discovery, sandboxing
- **Work**: Plugin manifest format, registry API, WASM sandboxing

### 4.5 Self-hosted Ion API compatibility
- **Need**: Drop-in replacement for Ion REST API so existing CesiumJS apps work unchanged
- **Work**: Implement Ion's asset/token API contract

---

## Estimated Effort by Phase

| Phase | Description | Scope |
|-------|-------------|-------|
| 1 | Core Pipeline | 5 work items — most critical |
| 2 | Format & Quality | 5 work items — broadens use cases |
| 3 | Production | 5 work items — deployment readiness |
| 4 | Differentiation | 5 work items — competitive features |

**Minimum viable Ion replacement**: Phase 1 + items 2.1, 2.2, 3.5
This gets you: upload → auto-reproject → tile → serve over S3 with terrain, in Docker.

**Full parity**: Phase 1 + Phase 2 + Phase 3
**Exceeds Ion**: All four phases (self-hosted, open source, extensible)

---

## What Tiletopia Already Does Better Than Ion

1. **Self-hosted / air-gapped** — no cloud dependency
2. **Open source (AGPL-3.0)** — full code transparency
3. **Multiple viewer engines** — CesiumJS, deck.gl, MapLibre (Ion locks you to CesiumJS)
4. **More input formats** — IFC, CityJSON, FBX, GeoPackage, DTED, HGT (Ion supports fewer)
5. **Pluggable storage** — hot/cold tiering, bring your own S3/GCS/Azure
6. **Edge deployment** — small binary, runs on a Raspberry Pi
