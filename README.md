# 🌍 TileTopia

[![CI](https://github.com/GeoLang/tiletopia/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/tiletopia/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Fast open-source 3D Tiles server — self-hosted Cesium Ion replacement.**

Ingest raw geospatial data (point clouds, terrain, BIM), tile it into OGC 3D Tiles 1.1, and serve it with view-dependent streaming. Compatible with CesiumJS, Cesium for Unreal, Cesium for Unity, and any 3D Tiles client.

**Website:** https://geolang.github.io/tiletopia

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
- **JWT authentication** (`TILETOPIA_JWT_SECRET`, 32+ bytes, required to serve; `TILETOPIA_AUTH_DISABLED=true` to opt out)

### Terrain
- **Quantized mesh terrain** from DEM/DTM heightmaps
- **Multi-LOD terrain** with quadtree tiling
- **Bilinear interpolation** for smooth subsampling
- **Delta + zigzag encoding** per Cesium terrain spec
- **Prebuilt terrain bundles** — serve a `ctb-tile` directory as a Cesium terrain source, no Ion token

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
- **Martin integration** — PMTiles and PostGIS vector tile sources via `martin-tile-utils`, MBTiles metadata parsing (`--features martin`)

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
- **Role-based access control** — admin/editor/viewer tiers from the JWT `role` claim plus per-asset ownership, enforced on every write route (no OIDC/SAML federation)
- **Audit Logging** — immutable event trail with query/export for compliance
- **Leader Election** — single-process election with Raft-style terms (multi-node HA is not implemented)
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
- **Cinematic Flythrough** — keyframed camera paths with easing, H.264 MP4/WebM export via `--features video`
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
- **CRS auto-detection & reprojection** — automatic coordinate reference system detection via projicio
- **DEM tile caching** — LRU cache for terrain DEM tiles

### Geospatial Services
- **Photogrammetry (SfM/MVS)** — Structure from Motion + Multi-View Stereo pipeline with quality presets
- **Point Cloud Classification** — ASPRS-standard classes (ground/vegetation/building/water), ensemble decision tree classifier with height/density/planarity features, optional in-process ONNX inference (`--features onnx`, ort 2.0) or an external ML service (`--features ml`)
- **AI Agent (GeoLang)** — natural language control of the 3D viewer via LLM-powered agent. Chat with the agent to fly to locations, classify point clouds, overlay GeoJSON layers, run spatial analysis, and generate reports — no GIS expertise required. Backed by the [sibyl](https://github.com/GeoLang/sibyl) agent loop and 36 geospatial tools.
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

## AI Agent (GeoLang Integration)

GeoLang includes an LLM-powered geospatial agent that lets users control the 3D viewer through natural language conversation. Instead of clicking through menus, type what you want:

- **"Fly to the Sydney Opera House"** — geocodes the location and moves the 3D camera
- **"Classify this point cloud"** — applies ASPRS classification coloring
- **"Show me flood risk for sites near Manchester"** — chains geocoding → environmental analysis → visualization
- **"Load the building survey tileset"** — adds a 3D Tiles dataset to the scene
- **"Compare elevation profiles between two sites"** — runs terrain analysis and displays results

The agent loop is [sibyl](https://github.com/GeoLang/sibyl), a Rust service that calls an OpenAI-compatible LLM endpoint and keeps sessions and history in sqlite. Tool calls are dispatched over HTTP to the GeoLang service, which runs its 36 geospatial tools in-process: geocoding, isochrones, clustering, Voronoi, terrain profiles, environmental risk assessment, routing, and more. There is no cross-session memory, each session starts clean.

### Viewer Commands

The agent can programmatically control the CesiumJS/deck.gl/MapLibre viewer:

| Command | Description |
|---------|-------------|
| `fly_to` | Animate camera to coordinates |
| `set_view` | Set camera position + orientation |
| `add_marker` | Place a labeled pin |
| `load_tileset` | Load a 3D Tiles dataset |
| `classify` | Apply ASPRS classification colors |
| `add_geojson` | Overlay GeoJSON layer |
| `set_time` | Set the time slider |
| `clear_entities` | Remove all markers/entities |
| `screenshot` | Capture the current view |

### Setup

The agent runs as part of the platform stack, brought up from the viewtopia repo:

```bash
cd ../viewtopia
scripts/platform-up.sh
# equivalently: docker compose --env-file .env.platform -f docker-compose.platform.yml up -d
```

That starts sibyl, the GeoLang API, tiletopia, and the ViewTopia viewer, which talks
AG-UI to `POST /chat/agui` on the GeoLang API.

The dashboard in `gui/` still posts to the older `/agent/chat/stream` endpoint, which
the GeoLang API no longer serves, so its chat panel needs porting to AG-UI before it
works again. Everything else in the dashboard runs against `tiletopia serve`.

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
git clone https://github.com/GeoLang/tiletopia.git
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

Analysis tiles (`/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png`) render from the
loaded DEM store and a synthetic field by default. Two variables put them on
real elevation instead:

- `TILETOPIA_ANALYSIS_DEM_BBOX` — `west,south,east,north` in degrees. Setting it
  switches the analysis tiles to Copernicus GLO-30 COGs, streamed over STAC as
  each tile needs them. Unset means the DEM store, and nothing on the network.
- `TILETOPIA_ANALYSIS_STAC_API` — STAC API root, defaults to
  `https://earth-search.aws.element84.com/v1`.

The bbox only anchors the raster grid on its most recent item: tiles anywhere
resolve through lazy per-window STAC searches, cached in two-degree blocks for
the engine's lifetime. A tile needing more than 32 cold block searches fails,
so below about zoom 6 the layer answers 500 rather than mosaicking thousands
of items. A malformed bbox refuses startup, and a search that fails answers
500 rather than serving synthetic terrain under a layer that is meant to be
real.

The `ndvi` op reads sentinel-2 L2A red and nir over the same STAC API, reduced
per pixel to a median of the last month's items so clouds fall out of the
stack. It has no synthetic fallback: without the bbox variable, ndvi tiles
answer 500. The trailing window anchors when the op's engine is first used and
holds until restart. A cold tile reads every item behind the median, a few
dozen COGs over sequential range requests, which takes minutes today: only a
tile already in the chunk cache comes back instantly. Treat the layer as batch
until geoplumb reads composite items in parallel.

The `/api/v1/terrain/` routes read DEM files under `<data-dir>/dem` first and
fall back to SRTM tiles downloaded from `https://elevation-tiles-prod.s3.amazonaws.com/skadi`,
which `TILETOPIA_SRTM_BASE_URL` overrides. A download that fails answers 503
naming the tile, rather than a flat mesh that would read as terrain at sea
level.

### Prebuilt terrain bundles

For terrain that never reaches upstream, drop a prebuilt bundle under
`<data-dir>/terrain_bundles/<name>/`: a `layer.json` beside a
`{z}/{x}/{y}.terrain` tree, which is what `ctb-tile` writes and what the
`terrain_bundle` export format produces. `GET /api/v1/terrain/bundles` lists
the names, and each one is a terrain source of its own:

```javascript
const terrain = await Cesium.CesiumTerrainProvider.fromUrl(
    'http://localhost:3000/api/v1/terrain/bundles/alps/'
);
viewer.terrainProvider = terrain;
```

The bundle's own `layer.json` is served with its `tiles` template replaced by a
relative one, so a bundle built against another host still resolves back here.
Tiles gzipped in place go out with `Content-Encoding: gzip`. A bundle whose
`layer.json` carries no `available` array gets one read off the tile tree,
because CesiumJS throws on the first tile without it. That walk touches every
tile, so ship `available` in the bundle if it is a large one. Bundles must be
`quantized-mesh-1.x`, on `tms` or `slippyMap` in EPSG:4326 or EPSG:3857.
Anything else is refused with the reason in the log, rather than served for
CesiumJS to reject.

A terrain asset in the Ion-compat layer resolves to a bundle too. Name the
bundle directory after the asset id and `GET /v1/assets/{id}/endpoint` answers
with `/api/v1/terrain/bundles/{id}/`, which is what `CesiumTerrainProvider`
takes: it appends `layer.json` to that URL itself. An asset with no bundle
under `<data-dir>/terrain_bundles/{id}/` gets a 404 naming the directory,
because a URL CesiumJS cannot read as terrain would not fail loudly. A missed
`layer.json` is read as a pre-metadata heightmap layer, and the failure only
shows up as every tile 404ing.

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
pnpm install
pnpm run dev        # opens http://localhost:5173
```

| Feature | Source | Cesium Ion Equivalent |
|---------|--------|-----------------------|
| Base imagery | OpenStreetMap raster tiles | Bing Maps Aerial |
| 3D Buildings | Overpass API (client-side) | Cesium OSM Buildings |
| Geocoding | Nominatim (OpenStreetMap) | Ion geocoder |
| Terrain | GeoLang server quantized-mesh (if running) | Cesium World Terrain |
| 3D Tilesets | GeoLang server | Ion asset hosting |
| Photorealistic 3D | Set `VITE_GOOGLE_3D_TILES_KEY` env var | Google Photorealistic 3D Tiles |

Additional imagery layers (Stamen Toner, ESRI World Imagery) are available via the layer picker.

When the GeoLang server is running on port 3000, the viewer automatically connects and loads terrain + tilesets.  Without the server, you still get a full globe with buildings and geocoding.

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
| `GET` | `/api/v1/assets/{id}/jobs` | The asset's tiling jobs, newest first |
| `GET` | `/api/v1/assets/{id}/tileset.json` | Serve tileset |
| `GET` | `/api/v1/assets/{id}/tiles/{path}` | Serve individual tile |
| `GET` | `/api/v1/terrain/layer.json` | Quantized-mesh layer metadata, generated from DEM |
| `GET` | `/api/v1/terrain/{z}/{x}/{y}.terrain` | Quantized-mesh tile, generated from DEM |
| `GET` | `/api/v1/terrain/bundles` | Prebuilt terrain bundles this server hosts |
| `GET` | `/api/v1/terrain/bundles/{name}/layer.json` | A bundle's layer metadata |
| `GET` | `/api/v1/terrain/bundles/{name}/{z}/{x}/{y}.terrain` | A bundle's quantized-mesh tile |
| `GET` | `/api/v1/terrain/rgb/{z}/{x}/{y}.png` | Terrain-RGB tile for MapLibre |
| `WS` | `/api/v1/realtime/{room}` | WebSocket for live data and collaboration |
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
| `GET` | `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` | Hillshade, slope or ndvi tiles, rendered on demand |
| `GET` | `/api/v1/geostatistics/methods` | List interpolation methods |
| `GET` | `/api/v1/multispectral/indices` | Spectral indices (NDVI, etc.) |
| `GET` | `/metrics` | Prometheus metrics |

Tile data reads are anonymous: `tileset.json`, `tiles/{path}`, everything under
`/api/v1/terrain/` (the generated quantized-mesh routes, the prebuilt bundles
and their listing, terrain-RGB) and the `/api/v1/analysis/xyz/` analysis tiles,
none of which a map library can send a header with. The rest of
`/api/v1/analysis/` is compute and stays gated. Everything else needs
`Authorization: Bearer <jwt>`, and writes need the editor or admin role.

The `role` claim must be exactly `admin`, `editor` or `viewer`. Any other value
is refused rather than treated as a default, so a token from a service with its
own role names gets no tier here.

On top of the role tier:

| Route | Rule |
|---|---|
| `DELETE /api/v1/assets/{id}`, `POST /api/v1/assets/{id}/tile` | editor + owner of the asset, or admin |
| `POST`/`DELETE /api/v1/assets/{id}/annotations` | editor + owner of the asset, or admin. Deletes are scoped to the asset in the path |
| `GET /api/v1/assets` | token required; lists your own assets plus ownerless legacy rows, admins see all |
| `POST`/`PUT`/`DELETE /api/v1/plugins/registry/...` | admin, because a plugin runs server-wide |

Assets created before ownership existed have no owner and stay writable by any
editor. Hiding an asset from the list does not hide its tiles: tile URLs are
public by design.

The realtime websocket needs any valid JWT. Browsers cannot set the
Authorization header on a websocket handshake, so the token is offered as a
subprotocol instead:

```js
new WebSocket(`ws://host/api/v1/realtime/${room}`, ["bearer", jwt])
```

That sends `Sec-WebSocket-Protocol: bearer, <jwt>`. The order is fixed, marker
first. The 101 response echoes `Sec-WebSocket-Protocol: bearer`, never the token.
Non-browser clients can send `Authorization: Bearer <jwt>` and offer no
subprotocol. Query strings are never credentials.

The server stamps every collaboration message with the sender's JWT `sub` as
`user_id` before rebroadcasting it, so a client's own `user_id` is ignored.
`user_name` stays client-chosen.

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

## GeoLang vs Cesium Ion

| Feature | GeoLang | Cesium Ion |
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

746 tests (721 Rust + 25 GUI) on default features, counted per crate:
- Core (112): AABB, octree, LOD, .pnts format, tileset serialization, coordinate transforms, CRS reprojection, diff detection, plugins, spatial queries, point cloud classification, change detection, implicit tiling, colorization, glTF structural metadata, 3D measurement, anomaly detection, predictive analytics, BIM clash detection, plus 8 stress tests
- Server (492): health, assets, tilesets, prebuilt terrain bundles, Ion asset id and endpoint resolution, auth and roles, role and ownership gates on asset, annotation and plugin writes, asset list visibility, annotations, temporal versioning, multi-tenancy, offline export, federated mesh, CRDT collaboration, rules engine, audit log, leader election, priority queue, webhooks, branding, marketplace, geofencing, retention, encryption, dashboards, stories, foveated rendering, flythrough, site reports, API keys, metering, scheduler, mobile, plus the geospatial services (geocoding, STAC, routing, isochrone, geoprocessing, features, elevation, map matching, static map, flight planning, scan registration, issues, terrain analysis, geostatistics, multispectral, COG, map tiles, analysis xyz tiles)
- Ingest (72): LAS/LAZ, GeoTIFF, BIM/IFC readers, CRS detection, photogrammetry (SfM)
- Terrain (28): quantized mesh generation, global DEM terrain
- Store (12): local filesystem CRUD, path traversal
- Worker (5): background job processing

GUI: `cd gui && pnpm run test:all` (10 vitest unit tests + 15 Playwright e2e).

Feature-gated tests (`gpu`, `onnx`, `video`, `ml`, `martin`, `wasm-plugins`, cloud
stores) are not in the 721 and need their feature enabled to run.

---

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
