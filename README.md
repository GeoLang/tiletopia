# 🌍 TileTopia

**Fast open-source 3D Tiles server — self-hosted Cesium Ion replacement.**

Ingest raw geospatial data (point clouds, terrain, BIM), tile it into OGC 3D Tiles 1.1, and serve it with view-dependent streaming. Compatible with CesiumJS, Cesium for Unreal, Cesium for Unity, and any 3D Tiles client.

**Website:** https://tiletopia-hq.github.io/tiletopia

---

## Features

### Tiling Engine
- **OGC 3D Tiles 1.1** output (batched 3D models, point clouds, implicit tiling)
- **Octree spatial partitioning** with geometric error-based LOD
- **Parallel tiling** across all CPU cores (Rayon work-stealing)
- **GPU-accelerated** point cloud decimation (optional, via wgpu — Metal/Vulkan/DX12)
- **Diff-based incremental tiling** — only re-tile changed regions
- **Draco/meshopt compression** for compact tile delivery

### Input Formats
- **Point clouds:** LAS, LAZ, E57, PLY
- **3D models:** glTF, GLB, OBJ, FBX, IFC, CityGML, CityJSON
- **Terrain:** GeoTIFF, DTED, HGT, USGS DEM
- **Vector:** Shapefile, GeoJSON, KML, GeoPackage

### Tile Server
- **REST API** for asset management, tiling jobs, access control
- **Multipart upload** — stream large files directly
- **Tile streaming** with geometric error-based LOD (client-side frustum culling via CesiumJS)
- **CORS support** for cross-origin CesiumJS access
- **WebSocket real-time layer** for live IoT/sensor data overlay
- **JWT authentication** (opt-in via `TILETOPIA_JWT_SECRET`)

### Terrain
- **Quantized mesh terrain** from DEM/DTM heightmaps
- **Multi-LOD terrain** with quadtree tiling
- **Bilinear interpolation** for smooth subsampling
- **Delta + zigzag encoding** per Cesium terrain spec

### Digital Twin Support
- **Real-time data injection** — push live sensor values into 3D scene via WebSocket
- **Temporal versioning** — serve different model states over time
- **Entity linking** — map 3D tiles to metadata (building ID → sensor readings)
- **3D annotation layers** — persist user-drawn annotations server-side
- **Update API** — push model changes without full re-tile
- **Change detection & time slider** — compare point clouds across epochs, generate heatmaps
- **Scripting / rules engine** — trigger alerts when sensor thresholds are crossed
- **CRDT collaborative editing** — conflict-free concurrent annotations via hybrid logical clocks

### Advanced Analytics
- **Spatial queries** — radius search, k-nearest neighbor, bounding box, polygon clip, volume calculation
- **AI point cloud classification** — automatic ground/vegetation/building/water classification (ASPRS LAS standard)
- **Colorization from imagery** — project ortho photos or camera images onto point clouds
- **glTF structural metadata** — queryable per-feature properties (EXT_structural_metadata)

### Interoperability
- **3D Tiles Next implicit tiling** — Morton-coded subtrees with availability bitstreams
- **Federated mesh networking** — peer-to-peer tile routing across distributed instances
- **CI/CD pipeline validation** — tileset schema checks, GitHub Actions workflow generation
- **Photogrammetry (SfM)** — feature detection, matching, triangulation from photos
- **Global DEM terrain** — SRTM/Copernicus/ASTER with TMS tiling and bilinear sampling
- **Edge deployment** — cross-compile for ARM/embedded with offline bundles

### Storage
- **Local filesystem** — zero-config default
- **Amazon S3** — full CRUD via aws-sdk-s3
- **Google Cloud Storage** — full CRUD via cloud-storage (feature-gated)
- **Azure Blob** — full CRUD via azure-storage-blobs (feature-gated)
- **Hybrid** — hot tiles local, cold tiles in cloud

### Deployment
- **Single binary** — no runtime dependencies
- **Docker image** — `docker run -p 3000:3000 tiletopia`
- **Air-gapped / offline** — works without internet
- **Multi-tenant** — per-org/project isolation
- **Web dashboard** — upload, monitor jobs, preview in CesiumJS
- **Prometheus metrics** — `/metrics` endpoint

### Premium / Enterprise Features
- **3D Measurement Tools** — distance, area, volume, cut/fill, slope, bearing calculations
- **Anomaly Detection** — deformation monitoring, encroachment alerts, construction deviation, statistical outlier removal
- **Predictive Analytics** — linear regression, exponential smoothing, trend analysis, seasonal decomposition
- **BIM Clash Detection** — hard/soft clashes, design deviation from point cloud reality capture
- **RBAC + OIDC** — role-based access control with OpenID Connect SSO
- **Audit Logging** — immutable event trail with query/export for compliance
- **Raft Consensus Clustering** — high-availability multi-node deployment
- **Priority Queue + SLA** — tenant-tiered job scheduling with deadline guarantees
- **Webhook Delivery** — event-driven integrations with HMAC-signed payloads
- **White-Label Branding** — custom logos, colors, domains per organization
- **Geospatial Marketplace** — publish/discover/license 3D datasets with metered access
- **Data Residency Geofencing** — enforce storage regions for GDPR/sovereignty compliance
- **Retention Lifecycle** — automated tiering, archival, GDPR right-to-erasure policies
- **Field-Level Encryption** — AES-256/ChaCha20 at rest with key rotation
- **Custom Dashboards** — drag-and-drop widget layouts for KPIs and monitoring
- **Narrated Presentations (Stories)** — guided slide-based tours with camera animations
- **AR/VR Foveated Rendering** — eye-tracked LOD for XR headsets (Quest/Vision Pro/HoloLens)
- **Cinematic Flythrough** — keyframed camera paths with easing for presentations
- **Automated Site Reports** — scheduled HTML/PDF report generation from templates
- **API Key Management** — create/revoke keys, per-key rate limiting, usage tracking
- **Usage Metering & Billing** — track API calls, storage, compute per tenant (free/pro/enterprise tiers)
- **Task Scheduler** — cron-like recurring jobs with stats, recent runs, failure handling
- **Plugin System** — custom format adapters, processing pipelines, type-safe registry
- **Mobile SDK** — adaptive quality based on device GPU/memory/network, offline packages
- **Multi-Format Export** — GeoJSON, Shapefile, KML, DXF, OBJ, LAS, GeoTIFF, 3D PDF, FBX

### 2D Map Tiles
- **XYZ Raster Tiles** — proxy + cache OSM, Stamen, or any slippy map source
- **Vector Tiles (MVT/PBF)** — generate Mapbox Vector Tiles from GeoJSON/PostGIS
- **MapLibre GL Styles** — serve style JSON with sources + layers for web map clients
- **TileJSON 3.0.0** — auto-discovery metadata for tile consumers
- **Tile Caching** — LRU cache with TTL, hit rate tracking, invalidation
- **Custom Overlays** — render asset footprints and BIM zones as vector tile layers

### OSM Buildings
- **OSM Building Extrusion** — parse OpenStreetMap building footprints into 3D meshes
- **Tiered building profiles** — multi-level setbacks (Empire State Building-style)
- **Roof shapes** — flat, gabled, hipped, pyramidal, skillion, dome
- **Overpass API parsing** — directly ingest Overpass JSON responses
- **Batch extrusion** — extrude entire city regions at once

### Cesium Ion Compatibility
- **Ion REST API compatibility layer** — drop-in replacement for Cesium Ion REST endpoints
- **Asset catalog** — searchable/filterable asset catalog with pagination
- **CRS auto-detection & reprojection** — automatic coordinate reference system detection via proj4rs
- **Imagery tiling** — XYZ/TMS tile pyramid generation from raster sources
- **DEM tile caching** — LRU cache for terrain DEM tiles

### Geospatial Services
- **Photogrammetry (SfM/MVS)** — Structure from Motion + Multi-View Stereo pipeline with quality presets
- **Point Cloud Classification** — ASPRS-standard classes (ground/vegetation/building/water), ensemble decision tree classifier with height/density/planarity features, optional PyTorch PointNet sidecar for ML inference (`--features ml`)
- **Real-Time Collaboration** — multi-user sessions with 3D cursors, viewports, annotations, and replies
- **Asset Versioning** — full version history, diffs, change regions between versions
- **BIM 4D Scheduling** — construction timeline, phases, Gantt keyframes, progress tracking
- **Geocoding** — forward/reverse/batch address lookup with confidence scores
- **STAC Catalog** — OGC SpatioTemporal Asset Catalog (v1.0.0) with collections + item search
- **Indoor Mapping** — floor plans, room navigation, BLE beacon positioning, accessibility routing
- **Cloud Optimized GeoTIFF (COG)** — range-request tile serving with overviews and band statistics
- **Routing & Navigation** — Dijkstra/A* shortest path, multi-profile (driving/walking/cycling), turn-by-turn directions
- **Isochrone / Travel-Time Analysis** — compute reachable areas by time (5/10/15 min), multi-profile polygons
- **Geoprocessing** — buffer, convex hull, centroid, simplify, union, intersection, difference, Voronoi
- **Feature Service (WFS)** — vector feature CRUD, spatial queries, field schemas, layer management
- **Elevation Service** — point elevation lookup, elevation profiles along paths, batch queries
- **Map Matching** — snap GPS traces to road network with confidence scoring
- **Static Map Rendering** — server-side PNG/JPEG/WebP/SVG/PDF map images with markers and overlays
- **Drone Flight Planning** — grid/orbit mission generation, GSD calculation, waypoint export
- **Scan Registration (ICP)** — multi-scan point cloud alignment (Point-to-Point/Point-to-Plane/NDT)
- **Issue / Defect Tracking** — location-pinned construction issues with status workflows and attachments
- **Terrain Analysis** — slope, aspect, hillshade, viewshed, watershed, contour lines from DEMs
- **Geostatistics** — IDW, kriging (ordinary/universal/simple), variograms, Moran's I autocorrelation
- **Multispectral Imagery** — NDVI, EVI, SAVI, thermal anomaly detection, band math, spectral indices

---

## Architecture

```
tiletopia/
├── crates/
│   ├── tiletopia-core/       # Octree tiling engine, LOD, .pnts writer
│   ├── tiletopia-server/     # Axum REST API, WebSocket, JWT auth
│   ├── tiletopia-worker/     # Async tiling pipeline, job queue
│   ├── tiletopia-ingest/     # LAS, GeoTIFF, glTF readers
│   ├── tiletopia-terrain/    # Quantized mesh terrain generation
│   ├── tiletopia-store/      # Storage: local + S3 + GCS + Azure
│   └── tiletopia-cli/        # CLI binary (tile / serve / info)
├── gui/                      # Web dashboard (Vite + CesiumJS)
└── docs/                     # GitHub Pages site
```

---

## Quick Start

### Build from source

```bash
git clone https://github.com/TileTopia-HQ/tiletopia.git
cd tiletopia
cargo build --release
```

### Tile a point cloud

```bash
tiletopia tile --input scan.las --output ./tileset --max-error 1.0
```

### Start the server

```bash
tiletopia serve --data-dir ./data --port 3000
```

### Use with CesiumJS

```javascript
const viewer = new Cesium.Viewer('cesiumContainer');
const tileset = await Cesium.Cesium3DTileset.fromUrl(
    'http://localhost:3000/api/v1/assets/{id}/tileset.json'
);
viewer.scene.primitives.add(tileset);
```

### Docker

```bash
docker run -p 3000:3000 -v /path/to/data:/data tiletopia serve --data-dir /data
```

### Zero-Config Viewer (no API keys needed)

The built-in CesiumJS viewer works out-of-the-box with open data — **no Cesium Ion token, no API keys, no server required** for a basic 3D globe experience:

```bash
cd gui
npm install
npm run dev        # opens http://localhost:5173
```

| Feature | Source | Cesium Ion Equivalent |
|---------|--------|-----------------------|
| Base imagery | OpenStreetMap raster tiles | Bing Maps Aerial |
| 3D Buildings | Overpass API (client-side) | Cesium OSM Buildings |
| Geocoding | Nominatim (OpenStreetMap) | Ion geocoder |
| Terrain | TileTopia server quantized-mesh (if running) | Cesium World Terrain |
| 3D Tilesets | TileTopia server | Ion asset hosting |
| Photorealistic 3D | Set `VITE_GOOGLE_3D_TILES_KEY` env var | Google Photorealistic 3D Tiles |

Additional imagery layers (Stamen Toner, ESRI World Imagery) are available via the layer picker.

When the TileTopia server is running on port 3000, the viewer automatically connects and loads terrain + tilesets.  Without the server, you still get a full globe with buildings and geocoding.

---

## REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/health` | Server health check |
| `GET` | `/api/v1/assets` | List all assets |
| `POST` | `/api/v1/assets` | Upload asset (multipart) |
| `GET` | `/api/v1/assets/{id}` | Get asset details |
| `DELETE` | `/api/v1/assets/{id}` | Delete asset |
| `POST` | `/api/v1/assets/{id}/tile` | Start tiling job |
| `GET` | `/api/v1/assets/{id}/tileset.json` | Serve tileset |
| `GET` | `/api/v1/assets/{id}/tiles/{path}` | Serve individual tile |
| `WS` | `/api/v1/realtime/{id}` | WebSocket for live data |
| `GET` | `/api/v1/tiles/sources` | List 2D tile sources (OSM, etc.) |
| `GET` | `/api/v1/tiles/styles` | MapLibre GL style JSON |
| `GET` | `/api/v1/stac` | STAC catalog root |
| `GET` | `/api/v1/stac/collections` | STAC collections |
| `GET` | `/api/v1/stac/search` | STAC item search |
| `GET` | `/api/v1/geocoding/search` | Forward geocode |
| `GET` | `/api/v1/geocoding/reverse` | Reverse geocode |
| `GET` | `/api/v1/routing/route` | Compute route |
| `GET` | `/api/v1/cog/datasets` | Cloud Optimized GeoTIFF datasets |
| `GET` | `/api/v1/indoor/buildings` | Indoor floor plans |
| `GET` | `/api/v1/isochrone/compute` | Compute isochrone polygons |
| `GET` | `/api/v1/geoprocessing/operations` | List geoprocessing ops |
| `GET` | `/api/v1/features/layers` | List feature service layers |
| `GET` | `/api/v1/elevation/point` | Point elevation lookup |
| `GET` | `/api/v1/elevation/profile` | Elevation profile along path |
| `GET` | `/api/v1/map-matching/match` | Snap GPS trace to road |
| `GET` | `/api/v1/static-map/render` | Render static map image |
| `GET` | `/api/v1/flight-planning/generate` | Generate drone flight plan |
| `GET` | `/api/v1/scan-registration/demo` | ICP scan alignment |
| `GET` | `/api/v1/issues` | List location-pinned issues |
| `GET` | `/api/v1/terrain-analysis/operations` | Terrain analysis ops |
| `GET` | `/api/v1/geostatistics/methods` | List interpolation methods |
| `GET` | `/api/v1/multispectral/indices` | Spectral indices (NDVI, etc.) |
| `GET` | `/metrics` | Prometheus metrics |

---

## GPU Acceleration

GPU compute is optional and auto-detected:

| Platform | Backend | Notes |
|----------|---------|-------|
| macOS (Apple Silicon) | Metal via wgpu | M1–M5 |
| Linux/Windows (NVIDIA) | Vulkan via wgpu | Optional CUDA for max perf |
| Linux/Windows (AMD/Intel) | Vulkan via wgpu | |
| Web | WebGPU | Browser-native |

```bash
cargo build --release --features gpu
```

Without `--features gpu`, all computation uses CPU (Rayon parallel).

---

## TileTopia vs Cesium Ion

| Feature | TileTopia | Cesium Ion |
|---------|-----------|------------|
| OGC 3D Tiles 1.1 | ✅ | ✅ |
| Point cloud tiling (LAS/LAZ) | ✅ | ✅ |
| Terrain generation (GeoTIFF) | ✅ | ✅ |
| 3D model tiling (glTF) | ✅ | ✅ |
| CesiumJS compatible | ✅ | ✅ |
| REST API | ✅ | ✅ |
| Web dashboard | ✅ | ✅ |
| **Self-hosted / on-premises** | ✅ | ❌ |
| **GPU-accelerated tiling (Metal/Vulkan)** | ✅ | ❌ |
| **WebSocket real-time layer** | ✅ | ❌ |
| **Digital twin support** | ✅ | ❌ |
| **Temporal versioning (time-series)** | ✅ | ❌ |
| **Air-gapped / offline deployment** | ✅ | ❌ |
| **Custom CRS / reprojection** | ✅ | ❌ |
| **Diff-based incremental tiling** | ✅ | ❌ |
| **Native BIM/IFC with metadata** | ✅ | ❌ |
| **Streaming upload (no file size limit)** | ✅ | ❌ |
| **Multi-tenant isolation** | ✅ | ❌ |
| **3D annotation layers** | ✅ | ❌ |
| **Offline viewer export (USB delivery)** | ✅ | ❌ |
| **Plugin system (custom formats)** | ✅ | ❌ |
| **Local filesystem storage** | ✅ | ❌ |
| **Open source** | ✅ AGPL-3.0 | ❌ Proprietary |
| **2D map tiles (XYZ/MVT)** | ✅ | ❌ |
| **STAC catalog (OGC)** | ✅ | ❌ |
| **Geocoding (forward/reverse)** | ✅ | ❌ |
| **Routing & navigation** | ✅ | ❌ |
| **Indoor mapping** | ✅ | ❌ |
| **Real-time collaboration** | ✅ | ❌ |
| **Isochrone / travel-time** | ✅ | ❌ |
| **Geoprocessing engine** | ✅ | ❌ |
| **Feature service (WFS)** | ✅ | ❌ |
| **Elevation service** | ✅ | ❌ |
| **Map matching** | ✅ | ❌ |
| **Static map rendering** | ✅ | ❌ |
| **Drone flight planning** | ✅ | ❌ |
| **Scan registration (ICP)** | ✅ | ❌ |
| **Issue / defect tracking** | ✅ | ❌ |
| **Terrain analysis** | ✅ | ❌ |
| **Geostatistics (kriging/IDW)** | ✅ | ❌ |
| **Multispectral indices** | ✅ | ❌ |
| **Price** | **Free forever** | $150–$3,750/month |

**34 exclusive features** that Cesium Ion cannot match.

---

## Tests

```bash
cargo test
```

535 tests (510 Rust + 25 GUI) covering:
- Core: AABB, octree, LOD, .pnts format, tileset serialization, coordinate transforms, CRS reprojection, GPU compute, diff detection, plugins (21)
- Core: spatial queries (radius/kNN/bbox/polygon clip/volume), AI point cloud classification, change detection & time slider, implicit tiling (3D Tiles Next), colorization from imagery, glTF structural metadata (35)
- Core: 3D measurement (distance/area/volume/cut-fill/slope/bearing), anomaly detection (deformation/encroachment/deviation/outlier), predictive analytics (linear regression/exponential smoothing/trend/seasonal), BIM clash detection (hard/soft/design deviation) (22)
- Store: local filesystem CRUD, path traversal (6)
- Server: health, assets, tilesets, security, annotations, temporal versioning, multi-tenancy, offline export (13)
- Server: federated mesh networking, CRDT collaborative editing, scripting/rules engine, CI/CD pipeline validation (19)
- Server: RBAC with OIDC, audit logging, Raft consensus clustering, priority queue with SLA, webhook delivery, white-label branding, geospatial marketplace, data residency geofencing, retention lifecycle/GDPR, field-level encryption, custom dashboards, narrated presentations (Stories), AR/VR foveated rendering, cinematic flythrough, automated site reports (75)
- Server: API keys, metering, webhooks, workspaces, export, scheduler, plugins, mobile (22)
- Server: photogrammetry, classification, collaboration, versioning, BIM 4D, geocoding, STAC, indoor, COG, routing, map tiles (47)
- Server: isochrone, geoprocessing, feature service, elevation, map matching, static map, flight planning, scan registration, issue tracking, terrain analysis, geostatistics, multispectral (51)
- Ingest: LAS/LAZ, GeoTIFF, BIM/IFC readers, photogrammetry (SfM) (13)
- Terrain: quantized mesh generation, global DEM terrain (12)
- Worker: background job processing (5)

---

## License

AGPL-3.0 — see [LICENSE](LICENSE).
