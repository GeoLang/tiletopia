# 🌍 TileTopia

[![CI](https://github.com/GeoLang/tiletopia/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/tiletopia/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Fast open-source 3D Tiles server — self-hosted Cesium Ion replacement.**

Ingest point clouds, tile them into OGC 3D Tiles 1.1, and serve them with view-dependent streaming. Quantized-mesh terrain from a DEM or a prebuilt bundle. Compatible with CesiumJS, Cesium for Unreal, Cesium for Unity, and any 3D Tiles client. Meshes and vector files tile through mago-3d-tiler, IFC through this repository's own reader and mesh tiler.

**Website:** https://geolang.github.io/tiletopia

---

## Features

### Tiling Engine
- OGC 3D Tiles 1.1 point-cloud tiles (`.pnts`)
- Octree spatial partitioning with geometric error-based LOD
- Parallel tiling across CPU cores (Rayon)
- Optional GPU point-cloud decimation via wgpu (`--features gpu`)
- Draco/meshopt compression for tile delivery

The job queue tiles point clouds with the native tiler. Meshes (glTF, glb, OBJ, FBX) and vector files (GeoJSON, GeoPackage, KML, CityGML) go to [mago-3d-tiler](https://github.com/Gaia3D/mago-3d-tiler) (MPL-2.0), which the Docker image bundles with a JRE 21. `TILETOPIA_MAGO_JAR` points at the jar, and without it a mesh or vector job fails naming the variable. The jar ships natives for Linux and Windows x64 only, so on macOS those jobs fail inside mago. IFC goes to the native mesh tiler instead, reached by this repository's own IFC reader, and needs no jar. It is placed by the upload's `longitude` and `latitude`, or by the `IfcSite` reference coordinates when the upload leaves them out, and an IFC with neither fails rather than landing at the centre of the earth. `crs` is ignored on that path. DAE uploads still fail with an error naming the format: neither tiler takes it. An upload whose extension is not on the list answers 400.

A mesh or vector upload takes optional `longitude`, `latitude` and `crs` fields beside the file. Longitude and latitude place a model that has local coordinates, and one without the other is refused. The tiles land at `/api/v1/assets/{id}/tileset.json`, with content under `/api/v1/assets/{id}/data/{file}` from mago and `/api/v1/assets/{id}/tiles/{file}` from the native tiler.

### Tile Server
- REST API for assets, tiling jobs, access control
- Multipart upload
- Tile streaming for CesiumJS
- CORS
- WebSocket at `/api/v1/realtime/{room}` for presence, cursors, chat and view sync. Six message types. Anything else is logged and dropped.
- JWT authentication (`TILETOPIA_JWT_SECRET`, 32+ bytes, required to serve. `TILETOPIA_AUTH_DISABLED=true` to opt out)
- 3D annotation layers, persisted, writes are owner-or-admin

### Terrain
- Quantized mesh terrain from DEM/DTM heightmaps
- Multi-LOD terrain with quadtree tiling
- Bilinear interpolation
- Delta + zigzag encoding per Cesium terrain spec
- Prebuilt terrain bundles: serve a `ctb-tile` directory as a Cesium terrain source, no Ion token
- `/api/v1/terrain/` reads DEM files under `<data-dir>/dem` first, then SRTM tiles. A failed download answers 503 rather than a flat mesh
- `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` hillshade, slope or ndvi tiles. Defaults to the loaded DEM store and a synthetic field. See Quick Start for the bbox variables that put them on real elevation

### Storage
- Local filesystem (default)
- Amazon S3
- Google Cloud Storage (feature-gated)
- Azure Blob (feature-gated)
- Hybrid: hot tiles local, cold tiles in cloud

### Deployment
- Single binary
- Docker image: `docker run -p 3000:3000 tiletopia`
- JWT roles `admin` / `editor` / `viewer` plus per-asset ownership on writes
- Web dashboard: upload, monitor jobs, preview in CesiumJS
- Prometheus metrics at `/metrics`

### 2D Map Tiles
- XYZ raster tiles: proxy and cache OSM or another slippy-map source
- MapLibre GL style JSON
- TileJSON 3.0.0
- Tile cache with TTL

### Cesium Ion Compatibility
- Ion REST compatibility for asset id and endpoint resolution
- A terrain asset in that layer resolves to a prebuilt bundle named after the asset id

### OSM Buildings (viewer)
The `gui/` globe can extrude OpenStreetMap building footprints in the browser via Overpass. That path does not go through the tiling job queue.

Not implemented, whatever the code in the repository suggests:

| Subsystem | State |
|-----------|-------|
| DAE tiling | Neither the native tiler nor mago-3d-tiler takes DAE, so those jobs fail. Point clouds, meshes, vector files and IFC do tile |
| Scheduler | `spawn()` has no callers. `create_job` ignores cron. `Scheduler::new()` seeds three fabricated jobs. No job has ever run |
| Webhooks | HMAC delivery is written in `process_pending`, which nothing calls. Subscribe is read-only. Seeded with demo secrets |
| STAC search | Ignores bbox, datetime, collections and limit. Returns one hardcoded item |
| COG tiles | Offsets are fabricated. There is no HTTP client |
| Static maps | Fills a flat grey buffer. Encodes WebP as JPEG, PDF and SVG as PNG |
| Elevation | Always a synthetic sine field, reported as `source: Srtm30m` |
| API keys | Seeds three fake keys. `get_by_hash` has no callers, so a key cannot authenticate |
| Kriging | Inverse-variogram weighting, no solve. Ordinary / Simple / Universal run the same code |
| Geoprocessing buffer / union | Buffer scales vertices radially. Union is a convex hull |
| Terrain analysis | No aspect, watershed, flow accumulation, or viewshed ray casting |
| Temporal versioning, CRDT, federation, CI/CD validation, multi-tenant isolation, leader election, priority queue / SLA, white-label, marketplace, geofencing, encryption, custom dashboards, AR/VR foveated rendering, cinematic flythrough, scripting, offline viewer export | `pub mod` lines with unit tests. No route, CLI or render loop calls them |

The modules stay. Wiring or deleting them is a product call, recorded in `viewtopia/DESIGN_TODO.md`. Do not start from the scheduler or webhook facades.

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

- `TILETOPIA_MAGO_JAR` — path to the mago-3d-tiler jar that tiles meshes and
  vector files. The Docker image sets it. Outside the image, download
  `mago-3d-tiler-1.16.2.jar` from the Gaia3D releases and point this at it, with
  a JDK 21 on `PATH`. Unset means mesh and vector jobs fail naming the variable.

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
| `GET` | `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` | Hillshade, slope or ndvi tiles, rendered on demand |
| `GET` | `/metrics` | Prometheus metrics |

Other `/api/v1/*` geospatial and premium routes are mounted. They are the facades in the table above: STAC search returns one hardcoded item, COG offsets are fabricated, static maps fill a grey buffer, elevation reports `Srtm30m` while serving a sine field, API keys cannot authenticate, scheduler and webhook workers have no callers. They are listed in OpenAPI because the router mounts them, not because they work.

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

What this server actually does, against Cesium Ion as a hosted 3D Tiles host.

| Feature | TileTopia | Cesium Ion |
|---------|-----------|------------|
| OGC 3D Tiles 1.1 point clouds | yes | yes |
| Terrain generation and prebuilt bundles | yes | yes |
| CesiumJS compatible | yes | yes |
| REST API | yes | yes |
| Web dashboard | yes | yes |
| Self-hosted / on-premises | yes | no |
| GPU-accelerated tiling (optional wgpu) | yes | no |
| WebSocket presence, cursors, chat | yes | no |
| 3D annotation layers | yes | no |
| Local filesystem storage | yes | no |
| Open source | AGPL-3.0 | proprietary |
| 3D model / BIM / vector tiling | no (readers exist, the queue does not call them) | yes |
| Temporal versioning, webhooks, scheduler | no | mixed |

Price is not a capability. Ion is a hosted product. This is a binary you run.

---

## Tests

```bash
cargo test
```

746 tests (721 Rust + 25 GUI) on default features, counted per crate:
- Core (112): AABB, octree, LOD, .pnts format, tileset serialization, coordinate transforms, CRS reprojection, diff detection, plugins, spatial queries, point cloud classification, change detection, implicit tiling, colorization, glTF structural metadata, 3D measurement, anomaly detection, predictive analytics, BIM clash detection, plus 8 stress tests
- Server (492): health, assets, tilesets, prebuilt terrain bundles, Ion asset id and endpoint resolution, auth and roles, role and ownership gates on asset, annotation and plugin writes, asset list visibility, annotations, temporal versioning, multi-tenancy, offline export, federated mesh, CRDT collaboration, rules engine (module only, no route reaches it), audit log, leader election, priority queue, webhooks, branding, marketplace, geofencing (module only, no route reaches it), retention, encryption, dashboards, stories, foveated rendering, flythrough, site reports, API keys, metering, scheduler, mobile, plus the geospatial services (geocoding, STAC, routing, isochrone, geoprocessing, features, elevation, map matching, static map, flight planning, scan registration, issues, terrain analysis, geostatistics, multispectral, COG, map tiles, analysis xyz tiles)
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
