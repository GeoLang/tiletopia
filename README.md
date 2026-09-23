# TileTopia

[![CI](https://github.com/GeoLang/tiletopia/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/tiletopia/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Self-hosted 3D Tiles server, an open-source alternative to Cesium Ion.**

TileTopia tiles point clouds, meshes, IFC and vector files into OGC 3D Tiles 1.1 and serves them to CesiumJS or any other 3D Tiles client. It also serves quantized-mesh terrain from a DEM or a prebuilt bundle. Point clouds, meshes and IFC go through this repository's own readers and tilers, and vector files go through mago-3d-tiler. Tiles from an IFC carry each element's GlobalId as the `asset_id` feature property, so a client that picks a tile feature reads the id back.

**Website:** https://geolang.github.io/tiletopia

---

## Features

### Tiling

- OGC 3D Tiles 1.1 point-cloud tiles (`.pnts`) from an octree with geometric-error LOD, built in parallel with Rayon
- Mesh tiles as `.glb`, with lower LODs from meshopt simplification. Tiles are written uncompressed

The job queue tiles point clouds (LAS, LAZ, E57, PLY) and meshes (glTF, GLB, OBJ, FBX, CityGML, IFC) with the native tiler, whatever `TILETOPIA_MAGO_JAR` is set to. The readers carry UVs, diffuse textures and diffuse colours through to the tile GLBs, and a tile holding part of a textured mesh carries the crop of the texture its triangles reach. A mesh with a texture the readers cannot find or decode tiles untextured.

A point cloud is placed by the CRS in its LAS GeoKey record, and `tiletopia tile` also reads a `.prj` file beside the input. Heights are taken as ellipsoidal. A point cloud with no CRS is tiled in its own coordinates, with a warning naming the file. The upload's `crs` field is not used for point clouds.

A mesh is placed by the upload's `longitude` and `latitude`, and one without them fails naming them. IFC falls back to the `IfcSite` reference coordinates, and an IFC with neither fails. `crs` is ignored on the native path.

Vector files (GeoJSON, GeoPackage, KML) go to [mago-3d-tiler](https://github.com/Gaia3D/mago-3d-tiler) (MPL-2.0), which the Docker image bundles with a JRE 21. Without the jar they fail naming the variable. The jar ships natives for Linux and Windows x64 only, so on macOS the mago jobs fail inside mago.

DAE uploads are accepted and their jobs fail with an error naming the format, since neither tiler takes it. An upload whose extension is not on the list answers 400. DEM rasters (tif, tiff, hgt, dt0, dt1, dt2) and images (jpg, jpeg, png, jp2) are stored as assets and never tiled: a tiling request for one answers that terrain and imagery assets are not tiled to 3D Tiles.

A mesh or vector upload takes optional `longitude`, `latitude` and `crs` fields beside the file. One of longitude and latitude without the other is refused. The tiles land at `/api/v1/assets/{id}/tileset.json`, with content under `/api/v1/assets/{id}/tiles/{file}` from the native tiler and `/api/v1/assets/{id}/data/{file}` from mago.

### Tile server

- REST API for assets, tiling jobs and access control, with multipart upload and CORS
- WebSocket at `/api/v1/realtime/{room}` for presence, cursors, chat and view sync. It relays six message types and logs and drops anything else
- JWT authentication. `TILETOPIA_JWT_SECRET` (32 bytes or more) is required to serve, and `TILETOPIA_AUTH_DISABLED=true` turns auth off
- `POST /api/v1/auth/signup` and `POST /api/v1/auth/login` answer a token good for 24 hours. A signup gets the `viewer` role. `tiletopia set-role <email> admin` promotes a user directly in the SQLite database, so the first admin needs no admin route
- Roles `admin`, `editor` and `viewer`, plus per-asset ownership on writes
- 3D annotation layers, persisted, with owner-or-admin writes
- Admin routes under `/api/v1/admin/` for stats, health, jobs and user management
- Prometheus metrics at `/metrics`

### Terrain and analysis

- Quantized-mesh terrain from DEM files, quadtree tiled
- Prebuilt terrain bundles: serve a `ctb-tile` directory as a Cesium terrain source with no Ion token
- `/api/v1/terrain/` reads DEM files under `<data-dir>/dem` first, then SRTM tiles. A failed download answers 503 rather than a flat mesh
- `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders hillshade, slope or ndvi tiles from the same DEM the elevation routes read, or from Copernicus GLO-30 (see [Analysis tiles](#analysis-tiles))
- `/api/v1/elevation/point?lat=&lon=` and `/api/v1/elevation/profile?path=lon,lat;lon,lat` read the same DEM and report which store answered. Ground no DEM covers is a 404
- `POST /api/v1/analysis/terrain` runs slope, aspect, hillshade, contours, flow direction, flow accumulation or watershed over a bbox
- `POST /api/v1/analysis/viewshed` casts rays from an observer and returns the visible cells
- `POST /api/v1/analysis/flood` returns the cells under a water level as GeoJSON, with their count and area
- `POST /api/v1/analysis/solar` renders daily irradiance for a date over a bbox
- `GET /api/v1/analysis/export/{op}?bbox=west,south,east,north&resolution=<m/px>` runs one analysis op over a bbox and returns a web mercator COG, capped at 4096x4096 pixels. It needs a token, unlike the tile route

Storage is the local filesystem.

### Webhooks

- `POST /api/v1/webhooks` registers a target URL and the events it wants, and returns a `whsec_` signing secret once. Editor or admin, and a subscription belongs to whoever created it
- Three events: `job.completed` and `job.failed` when a tiling job settles, `asset.deleted` when an asset is removed
- Every delivery carries `X-TileTopia-Signature: sha256=<hex>`, the HMAC-SHA256 of the request body under that secret
- A failed delivery is retried three times, 30, 60 and 120 seconds apart, then dropped. `GET /api/v1/webhooks/deliveries` reports what happened
- Pending deliveries are held in memory, so a restart drops what has not gone out

### Exports

- `POST /api/v1/exports` takes an `asset_id` and a `format`, answers 202 with a queued job, and encodes in the background. Editor or admin. `GET /api/v1/exports/{id}` polls it, and `GET /api/v1/exports/download/{id}` returns the file while the job reads `Ready`. Another caller's job id answers 404
- `GET /api/v1/exports/formats` lists the formats: `3dtiles_zip`, `las`, `laz`, `terrain_bundle`, `geojson`, `png`, `citygml`, `obj`, `glb`, `offline_viewer`
- `offline_viewer` zips the asset's tileset with a CesiumJS page and a `serve.py` that serves the directory over HTTP. The original upload is left out
- The bundle includes CesiumJS when `TILETOPIA_CESIUM_DIR` names a `Build/Cesium` directory to copy in. The Docker image ships CesiumJS 1.119 at `/opt/tiletopia/cesium` and sets the variable, so a bundle from the image opens with no network. With the variable unset, the page loads CesiumJS from cesium.com and says on screen that it needs the network
- A finished export is downloadable for 7 days. After that the job reads `Expired` and the download answers 410 with the expiry time
- Export job records are held in memory. The `prune_export_files` scheduled action removes the directories of expired jobs and, separately, directories whose newest file is older than a configured age, which covers files whose job record a restart dropped

### Scheduled jobs

- `POST /api/v1/scheduler/jobs` stores a job: a name, an action, a schedule, and whether it is enabled. Editor or admin, and a job belongs to whoever created it
- Three actions: `retile_asset` puts an asset back on the tiling queue with the placement its last job carried, `prune_export_files` removes expired and aged export directories, `prune_finished_jobs` removes settled rows from the `jobs` table past an age
- Three schedules: `interval` in seconds, `cron` as five standard fields (minute hour day-of-month month day-of-week, at second 0, UTC), and `one_shot` at a future time
- A worker wakes every second, runs what is due, and writes `last_run`, `last_outcome` and `run_count` (finished runs only) to the row. A one-shot disables itself after it runs
- A failed run keeps its error on the row and is retried on the next tick. Three failures in a row disable the job

### Audit trail

- Every mutation that passed a role gate and answered 2xx writes one row to SQLite: asset create, delete and re-tile, annotation writes, export start, tileset create and delete, plugin install, uninstall, config and enable/disable, webhook and scheduled job writes, org create, admin user delete, role change and org change, API key mint, revoke and delete, story create, update and delete, portal item create and delete, adding a catalog dataset, `PUT /api/v1/users/me`, and the Ion-compat asset create and token mint
- A row records the JWT `sub`, the action (`Create`, `Delete`, `PermissionChange` and so on), the resource type and id, the method, path and status, and the peer address. Refusals are not recorded, so a caller cannot fill the table by being refused in a loop
- The address is the direct socket peer, which is the proxy when one fronts the server. `X-Forwarded-For` is not read
- `GET /api/v1/audit` reads the rows back newest first, instance-admin only, filtered by `user_id`, `action`, `resource_type`, `from`, `to` and `limit` (100 by default, 1000 at most)
- `TILETOPIA_AUDIT_RETENTION_DAYS` sets how long a row is kept, 30 by default. A sweep runs hourly. `0`, a negative number, or anything that is not a whole number of days turns the sweep off

### Cesium Ion compatibility

- `GET /v1/assets`, `/v1/assets/{id}`, `/v1/assets/{id}/endpoint` and `/v1/tokens` resolve asset ids and endpoints the way Ion does. `POST /v1/assets` (editor) and `POST /v1/tokens` (admin) create them
- A terrain asset resolves to a prebuilt bundle named after the asset id

### Not implemented

These exist in the code or answer on a route, and do not do what their names say:

| Subsystem | State |
|-----------|-------|
| DAE tiling | Neither the native tiler nor mago-3d-tiler takes DAE, so those jobs fail |
| Draco tile compression | `draco_encode_mesh` compiles under the default `draco` feature and no tiling code calls it |
| Implicit tiling | `tiletopia_core::implicit_tiling` has no caller. Tilesets are written with explicit children |
| Photogrammetry, BIM 4D, indoor | `GET /api/v1/photogrammetry/projects`, `/bim4d/projects` and `/indoor/buildings` answer example rows compiled into the binary. There is no SfM pipeline, schedule engine or indoor graph behind them |
| Catalog add | `POST /api/v1/catalog/{id}/add` queues a job whose input is the dataset URL, and the job queue only reads local files, so the job fails |

The `gui/` globe extrudes OpenStreetMap building footprints in the browser from Overpass. That path does not use the server.

---

## Quick Start

### Build

```bash
git clone https://github.com/GeoLang/tiletopia.git
cd tiletopia
cargo build --release                      # target/release/tiletopia
cargo build --release --bin tiletopia --features martin    # adds vector tilesets and /martin
```

### Tile a point cloud

```bash
tiletopia tile --input scan.las --output ./tileset --max-error 1.0
```

`tile` reads point clouds only. Meshes, IFC and vector files tile through the server's job queue.

### Start the server

```bash
TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) tiletopia serve --data-dir ./data --port 3000
```

- `TILETOPIA_MAGO_JAR`: path to the mago-3d-tiler jar that tiles vector files. The Docker image sets it. Outside the image, download `mago-3d-tiler-1.16.2.jar` from the Gaia3D releases, point this at it, and put a JDK 21 on `PATH`. Unset, vector jobs fail naming the variable. Meshes never need it.
- `TILETOPIA_CESIUM_DIR`: a CesiumJS `Build/Cesium` directory copied into every `offline_viewer` export. The Docker image sets it. Outside the image, unzip `Build/Cesium` from a [CesiumJS release](https://github.com/CesiumGS/cesium/releases), or run `pnpm --dir gui build` and use `gui/dist/cesium`.
- `TILETOPIA_PMTILES_DIR`: directory of PMTiles archives to serve under `/martin`. Each `*.pmtiles` file directly in it, subdirectories excluded, is registered under its filename stem: `basemap.pmtiles` answers at `/martin/basemap/{z}/{x}/{y}`. Unset serves nothing. A directory that cannot be read refuses startup, and a single archive that fails to open is logged and skipped. Needs the `martin` feature. These routes need a JWT like the rest of the API, so a tile client has to send a token.
- `TILETOPIA_ION_BASE_URL`: the origin the Ion-compat endpoints write into the URLs they return, `http://localhost:3000` when unset. Set it to the address clients reach this server at.

### Docker

Tagged releases publish `ghcr.io/geolang/tiletopia`, built with the `martin` feature and carrying tippecanoe, mago-3d-tiler and CesiumJS:

```bash
docker run -p 3000:3000 -e TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) -v /path/to/data:/data ghcr.io/geolang/tiletopia
```

### Vector tilesets

`POST /api/v1/tilesets` takes a `.geojson`, `.geojson.gz`, `.fgb` or `.csv` file, answers 202, and builds it into one PMTiles archive with tippecanoe. The archive is served as a martin source named after the tileset id, so a ready tileset answers at `/martin/{id}/{z}/{x}/{y}` with its TileJSON at `/martin/{id}`. The `job_id` in the 202 is the tileset id, and a client polls `GET /api/v1/tilesets/{id}` until `status` leaves `building`. A failed build puts the tail of tippecanoe's stderr in `error`. Nothing rebuilds on its own, so re-uploading makes a new archive. These routes and `/martin` need the `martin` feature.

The build runs `tippecanoe -o {id}.pmtiles -l {stem} -zg --drop-densest-as-needed`, with the layer name taken from the uploaded filename, and the row records the argv it ran. The upload streams to disk and takes a body up to 4 GiB, so a reverse proxy in front needs its own limit raised to match.

Outside the Docker image, build tippecanoe from the pinned tag, since Debian bookworm and Fedora have no package:

```bash
sudo apt-get install gcc g++ make libsqlite3-dev zlib1g-dev  # Fedora: gcc-c++ make sqlite-devel zlib-ng-compat-devel
git clone --depth 1 -b 2.79.0 https://github.com/felt/tippecanoe.git
make -C tippecanoe -j"$(nproc)"
make -C tippecanoe install PREFIX=/usr/local
```

- `TILETOPIA_TILESET_DIR`: where the built archives go, `<data-dir>/tilesets` when unset. Kept apart from `TILETOPIA_PMTILES_DIR` because these archives re-register from the database at startup, not from a directory scan.
- `TILETOPIA_TILESET_TIMEOUT_SECS`: how long one build may run before it is killed, 3600 by default.
- `TILETOPIA_TILESET_MEMORY_MB`: address space the build may map, 4096 by default. tippecanoe runs out of memory easily on a large input.
- `TILETOPIA_TILESET_DISK_MB`: largest single file the build may write, the archive included, 20480 by default.

### Analysis tiles

`/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders from the server's own DEM by default, the same stores the elevation routes read. Two variables switch it to Copernicus GLO-30:

- `TILETOPIA_ANALYSIS_DEM_BBOX`: `west,south,east,north` in degrees. Setting it switches the analysis tiles to Copernicus GLO-30 COGs, read over STAC as each tile needs them.
- `TILETOPIA_ANALYSIS_STAC_API`: STAC API root, `https://earth-search.aws.element84.com/v1` by default.

The bbox only anchors the raster grid on its most recent item. Tiles anywhere resolve through per-window STAC searches, cached in two-degree blocks until restart. A tile that needs more than 32 uncached block searches fails, so below about zoom 6 the layer answers 500. A malformed bbox refuses startup, and a failed search answers 500 rather than serving other terrain.

The `ndvi` op reads Sentinel-2 L2A red and nir over the same STAC API and takes the per-pixel median of the last month's items, which drops most clouds. It has no fallback: without the bbox variable, ndvi tiles answer 500. The one-month window is fixed when the op is first used and holds until restart. An uncached tile reads every item behind the median, a few dozen COGs, so it is slow. A tile already in the chunk cache returns at once.

### STAC search and collections

`GET /api/v1/stac/search` forwards an item search to an upstream STAC API and returns its item collection unchanged. It takes `bbox=west,south,east,north` in degrees, `datetime` as a STAC instant or interval, `collections` as a comma-separated list, and `limit`, capped at 500 and 10 by default. `GET /api/v1/stac/collections` returns the upstream's `/collections` unchanged.

- `TILETOPIA_STAC_API`: upstream STAC API root, for example `https://example.org/stac/v1`. Any catalog with an item-search endpoint works.

Unset, both routes answer 503 naming the variable. An upstream that cannot be reached or refuses the call answers 502, and so does a 200 with no `features` or `collections` array. `GET /api/v1/stac` is this server's own catalog root, and with no upstream configured it links to neither route and claims only the core conformance class.

### COG reads

- `TILETOPIA_COG_SOURCES`: the COGs to serve, comma-separated, each a local path or an http(s) URL. Each is keyed by its filename stem, so `/data/ramp.tif` and `https://example.org/cog/ramp.tif` both answer as `ramp`. Unset serves nothing.

Every entry is opened at startup, and `GET /api/v1/cog/datasets` reports what its header declares: size, dimensions, band count, EPSG, bounds in the file's own CRS units, internal tile size and overview levels. A local path that cannot be opened refuses startup. A remote href that cannot be opened is logged and skipped, and so is a host that answers 200 to a `Range` request, since every tile read would fetch the whole file.

`GET /api/v1/cog/datasets/{id}/window?level=0&col=&row=&cols=&rows=` reads pixels from one resolution level, in that level's pixel coordinates, and returns one row-major plane per band with nodata and pixels past the edge as null. Local sources are read by seek and remote ones by HTTP `Range`, so a window costs only the internal tiles it touches. A window is capped at 512x512 pixels.

### Terrain sources

The `/api/v1/terrain/` routes read DEM files under `<data-dir>/dem` first and fall back to SRTM tiles downloaded from `https://elevation-tiles-prod.s3.amazonaws.com/skadi`, which `TILETOPIA_SRTM_BASE_URL` overrides. A failed download answers 503 naming the tile.

For terrain with no upstream at all, put a prebuilt bundle under `<data-dir>/terrain_bundles/<name>/`: a `layer.json` beside a `{z}/{x}/{y}.terrain` tree, which is what `ctb-tile` writes and what the `terrain_bundle` export produces. `GET /api/v1/terrain/bundles` lists the names, and each one is a terrain source:

```javascript
const terrain = await Cesium.CesiumTerrainProvider.fromUrl(
    'http://localhost:3000/api/v1/terrain/bundles/alps/'
)
viewer.terrainProvider = terrain
```

- The bundle's `layer.json` is served with its `tiles` template made relative, so a bundle built for another host resolves here.
- Tiles gzipped in place go out with `Content-Encoding: gzip`.
- A `layer.json` with no `available` array gets one built from the tile tree, because CesiumJS throws on the first tile without it. That walk touches every tile, so ship `available` in a large bundle.
- Bundles must be `quantized-mesh-1.x`, on `tms` or `slippyMap`, in EPSG:4326 or EPSG:3857. Anything else is refused with the reason in the log.

A terrain asset in the Ion-compat layer resolves to a bundle too. Name the bundle directory after the asset id, and `GET /v1/assets/{id}/endpoint` answers with `/api/v1/terrain/bundles/{id}/`, which `CesiumTerrainProvider` takes as is. An asset with no bundle under `<data-dir>/terrain_bundles/{id}/` gets a 404 naming the directory.

### Use with CesiumJS

```javascript
const viewer = new Cesium.Viewer('cesiumContainer')
const tileset = await Cesium.Cesium3DTileset.fromUrl(
    'http://localhost:3000/api/v1/assets/{id}/tileset.json'
)
viewer.scene.primitives.add(tileset)
```

### Dashboard

```bash
cd gui
pnpm install
pnpm run dev        # http://localhost:5173, proxies /api to localhost:3000
```

The globe needs no Cesium Ion token and no API key:

| Layer | Source |
|---------|--------|
| Base imagery | OpenStreetMap raster tiles, with Stamen Toner and Esri World Imagery in the layer picker |
| 3D buildings | Overpass API, extruded in the browser |
| Geocoding | Nominatim |
| Terrain | this server's `/api/v1/terrain/`, or a flat ellipsoid when it does not answer |
| Photorealistic 3D | Google 3D Tiles, when `VITE_GOOGLE_3D_TILES_KEY` is set |

The dashboard sends no token. Against a server with auth on, its asset list, upload and annotation calls answer 401, so run the server with `TILETOPIA_AUTH_DISABLED=true` for local dashboard use. Its chat panel posts to `/agent/chat/stream`, which nothing serves, so the panel does not work.

---

## Chat agent

The chat agent is not in this repository. [sibyl](https://github.com/GeoLang/sibyl) runs the agent loop against an OpenAI-compatible LLM endpoint, the [GeoLang API](https://github.com/GeoLang/geolang) runs the geospatial tools, and [viewtopia](https://github.com/GeoLang/viewtopia) is the viewer it drives. `scripts/platform-up.sh` in the viewtopia repo starts them together with this server.

---

## Architecture

```
tiletopia/
├── crates/
│   ├── tiletopia-core/       # Octree tiling engine, LOD, .pnts and .glb writers
│   ├── tiletopia-server/     # Axum REST API, WebSocket, JWT auth
│   ├── tiletopia-ingest/     # Point cloud, DEM and mesh readers
│   ├── tiletopia-terrain/    # Quantized mesh terrain generation
│   ├── tiletopia-store/      # Local filesystem storage
│   └── tiletopia-cli/        # tiletopia binary: tile, serve, info, validate, set-role, edge
├── gui/                      # Web dashboard (Vite + CesiumJS)
└── docs/                     # GitHub Pages site
```

`tiletopia edge` prints the cross-build commands for a target and does not run them.

---

## REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/health` | Health check |
| `POST` | `/api/v1/auth/signup` | Create a viewer account, returns a 24-hour JWT |
| `POST` | `/api/v1/auth/login` | Exchange email and password for a 24-hour JWT |
| `GET` | `/api/v1/assets` | List the caller's assets |
| `POST` | `/api/v1/assets` | Upload an asset (multipart) |
| `GET` | `/api/v1/assets/{id}` | Asset details |
| `DELETE` | `/api/v1/assets/{id}` | Delete an asset |
| `POST` | `/api/v1/assets/{id}/tile` | Start a tiling job |
| `GET` | `/api/v1/assets/{id}/jobs` | The asset's tiling jobs, newest first |
| `GET` | `/api/v1/jobs/{id}` | One tiling job, used to poll the `job_id` from an upload |
| `GET` | `/api/v1/assets/{id}/tileset.json` | Tileset |
| `GET` | `/api/v1/assets/{id}/tiles/{path}` | One tile |
| `GET` | `/api/v1/tilesets` | Built vector tilesets |
| `POST` | `/api/v1/tilesets` | Upload a vector file and queue its build (multipart) |
| `GET` | `/api/v1/tilesets/{id}` | One tileset, with build status and the stderr tail on failure |
| `DELETE` | `/api/v1/tilesets/{id}` | Delete the archive, its row and its martin source |
| `GET` | `/martin/catalog` | Sources the caller may see: operator archives plus their own tilesets |
| `GET` | `/martin/{source}` | TileJSON for a PMTiles source |
| `GET` | `/martin/{source}/{z}/{x}/{y}` | Vector tile from a PMTiles source |
| `GET` | `/api/v1/terrain/layer.json` | Quantized-mesh layer metadata, generated from the DEM |
| `GET` | `/api/v1/terrain/{z}/{x}/{y}.terrain` | Quantized-mesh tile, generated from the DEM |
| `GET` | `/api/v1/terrain/bundles` | Prebuilt terrain bundles |
| `GET` | `/api/v1/terrain/bundles/{name}/layer.json` | A bundle's layer metadata |
| `GET` | `/api/v1/terrain/bundles/{name}/{z}/{x}/{y}.terrain` | A bundle's quantized-mesh tile |
| `GET` | `/api/v1/terrain/rgb/{z}/{x}/{y}.png` | Terrain-RGB tile for MapLibre |
| `WS` | `/api/v1/realtime/{room}` | Presence, cursors, chat and view sync |
| `GET` | `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` | Hillshade, slope or ndvi tile |
| `GET` | `/api/v1/analysis/export/{op}` | One analysis op over a bbox as a web mercator COG |
| `GET` | `/api/v1/audit` | Audit trail, newest first. Instance-admin only |
| `GET` | `/metrics` | Prometheus metrics |

Other routes that do real work on request input: STAC search proxies `TILETOPIA_STAC_API`, COG windows read `TILETOPIA_COG_SOURCES`, `/api/v1/static-map/` renders the DEM to PNG, JPEG, WebP, SVG or PDF, `POST /api/v1/geostatistics/interpolate` runs IDW or kriging over posted samples, `POST /api/v1/geoprocessing/run` runs buffer, simplify and boolean overlays, `/api/v1/geocoding/` asks Nominatim and falls back to a built-in list, webhooks deliver signed events, the scheduler runs the jobs it stores, and API keys (`X-Api-Key`, admin-minted, hashed at rest) authenticate read routes. What answers fixed data instead is in [Not implemented](#not-implemented).

### Access

Tile data reads are anonymous, since a map library cannot send a header with them: `tileset.json`, `tiles/{path}`, `data/{path}`, everything under `/api/v1/terrain/` (generated quantized-mesh, prebuilt bundles and their listing, terrain-RGB) and the `/api/v1/analysis/xyz/` tiles. The rest of `/api/v1/analysis/` stays gated.

Also anonymous, which matters before this goes on the internet:

- `/api/v1/auth/signup` and `/api/v1/auth/login`
- `/api/v1/stories/share/{token}`, a story shared by its token
- `GET /v1/assets/...` and `GET /v1/tokens`, the Ion-compat reads
- `/metrics`

Everything else needs `Authorization: Bearer <jwt>`, and writes need the editor or admin role.

The `role` claim must be exactly `admin`, `editor` or `viewer`. Any other value is refused, so a token from a service with its own role names gets no access here.

On top of the role tier:

| Route | Rule |
|---|---|
| `DELETE /api/v1/assets/{id}`, `POST /api/v1/assets/{id}/tile` | editor and owner of the asset, or admin |
| `POST`/`DELETE /api/v1/assets/{id}/annotations` | editor and owner of the asset, or admin. Deletes are scoped to the asset in the path |
| `GET /api/v1/assets` | token required. Lists your own assets plus ownerless legacy rows, admins see all |
| `POST /api/v1/tilesets` | editor or admin |
| `GET`/`DELETE /api/v1/tilesets/{id}`, `GET /api/v1/tilesets` | owner of the tileset, or admin |
| `POST`/`PUT`/`DELETE /api/v1/plugins/registry/...` | admin, because a plugin runs server-wide |
| `POST /api/v1/stories` | editor or admin, and the story records the caller as its author |
| `PUT`/`DELETE /api/v1/stories/{id}` | editor and author of the story, or admin. Reads take any valid token |

Assets created before ownership existed have no owner and stay writable by any editor. Hiding an asset from the list does not hide its tiles, since tile URLs are public. The `/martin` routes need a token but not ownership, so any signed-in caller who knows a source id can read it. `/martin/catalog` lists only the archives from `TILETOPIA_PMTILES_DIR` plus the caller's own tilesets, so nobody can list another owner's source ids. Admins see every source.

The realtime websocket needs any valid JWT. Browsers cannot set the Authorization header on a websocket handshake, so the token goes in as a subprotocol:

```js
new WebSocket(`ws://host/api/v1/realtime/${room}`, ["bearer", jwt])
```

That sends `Sec-WebSocket-Protocol: bearer, <jwt>`, marker first. The 101 response echoes `Sec-WebSocket-Protocol: bearer`, never the token. Non-browser clients can send `Authorization: Bearer <jwt>` instead. Query strings are never credentials.

The server sets `user_id` on every collaboration message to the sender's JWT `sub` before rebroadcasting it. `user_name` stays client-chosen.

---

## TileTopia vs Cesium Ion

| Feature | TileTopia | Cesium Ion |
|---------|-----------|------------|
| OGC 3D Tiles 1.1 point clouds | yes | yes |
| Terrain generation and prebuilt bundles | yes | yes |
| CesiumJS compatible | yes | yes |
| REST API | yes | yes |
| Web dashboard | yes | yes |
| Self-hosted / on-premises | yes | yes, the commercial Cesium ion Self-Hosted |
| WebSocket presence, cursors, chat | yes | no |
| 3D annotation layers | yes | no |
| Open source | AGPL-3.0 | proprietary |
| 3D model / BIM / vector tiling | yes | yes |
| Temporal versioning | no | mixed |

TileTopia is one AGPL binary you run.

---

## Tests

```bash
cargo test
cd gui && pnpm run test:all   # vitest unit tests, then Playwright e2e
```

Feature-gated tests need their feature, for example `cargo test -p tiletopia-server --features martin`.

---

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
