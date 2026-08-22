# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] - 2026-08-22

### Removed

- The unused readers in `tiletopia-ingest`: photogrammetry (SfM), imagery
  tiling, the BIM reader, and the GeoJSON, Shapefile, KML and GeoPackage
  vector readers, along with `read_vector`, `VectorFeature` and
  `VectorGeometry`. Nothing in the workspace called any of them.
- The dependencies no remaining ingest file imports: `rayon`, `memmap2`,
  `serde`, `geojson`, `shapefile`, `geo-types`, `rusqlite` and `image`. The
  `geojson`, `shapefile` and `rusqlite` entries also leave
  `[workspace.dependencies]`, as no other crate used them.

### Changed

- glTF, glb, OBJ, FBX and CityGML uploads go to mago-3d-tiler when
  `TILETOPIA_MAGO_JAR` is set and to this repository's own mesh tiler
  otherwise, so the mesh readers have a caller without a jar installed. The
  native path drops textures and materials, since the readers carry positions,
  normals and indices only, and places the tileset from the upload's
  `longitude` and `latitude`. A mesh with neither a placement nor a jar fails
  naming both. GeoJSON, GeoPackage and KML stay mago only and still fail
  naming the variable. IFC stays native and still falls back to its `IfcSite`
  coordinates.
- `tiler_for` takes whether a jar is configured and returns the tiler it
  picked, so one table decides the routing and whether the source is z-up.

## [Unreleased] - 2026-08-22

### Added

- IFC uploads are tiled to 3D Tiles by this repository's own IFC reader and
  mesh tiler, with no external tiler involved. The job places the tileset with
  a root `transform` built from the upload's `longitude` and `latitude`, or
  from the `IfcSite` reference latitude, longitude and elevation when the
  upload leaves them out. An IFC with neither fails rather than landing at the
  centre of the earth, and one that yields no geometry fails saying so. `crs`
  is ignored on this path.
- `MeshTilingConfig::root_transform` writes a `transform` on the tileset's root
  tile. Absent by default, so existing mesh callers are unchanged.

### Changed

- The IFC reader asks ifc-lite whether an entity class carries geometry instead
  of matching a hardcoded type list, so `IfcProduct` subtypes the list never
  named, such as `IfcSanitaryTerminal`, now reach the tileset.
- `MeshTilingConfig::content_y_up` rotates z-up input into the y-up glTF tile
  content 3D Tiles expects, which the runtime rotates back by π/2 about x.
  Only the written glTF turns: the bounding volumes stay in the z-up frame the
  tile transform names. The native IFC path sets it, other callers do not.
- A Model or Vector upload whose extension has no tiler behind it, DAE being
  the only one left, fails saying that neither the native tiler nor the
  external one takes the format.

## [Unreleased] - 2026-08-22

### Added

- Mesh and vector uploads are tiled to 3D Tiles by mago-3d-tiler, called from
  the tiling job queue. glTF, glb, OBJ, FBX, GeoJSON, GeoPackage, KML and
  CityGML all queue a job on upload, beside point clouds, which keep the native
  tiler. The upload takes optional `longitude`, `latitude` and `crs` fields, and
  refuses one of longitude/latitude without the other. `TILETOPIA_MAGO_JAR`
  points at the jar; the Docker image bundles it with a JRE 21 and sets the
  variable. IFC and DAE uploads fail with an error naming the format.
- `GET /api/v1/assets/{id}/data/{path}` serves the tile content mago-3d-tiler
  references from tileset.json, open to anonymous reads like `/tiles/{path}`.

### Changed

- An upload whose extension is not recognised answers 400 naming the accepted
  extensions. It used to be filed as a point cloud by a catch-all arm and tiled
  into a failing job.
- `GET /api/v1/assets/{id}/tileset.json` returns the stored bytes instead of a
  parsed and re-serialised `Tileset`, which cannot represent the region bounding
  volumes and nested children mago-3d-tiler writes.
- `jobs` gains nullable `longitude`, `latitude` and `crs` columns, added to
  existing databases on migrate.

## [Unreleased] - 2026-08-21

### Changed

- README and `docs/index.html` now describe the product that runs: point-cloud
  3D Tiles, quantized-mesh terrain, JWT, annotations, presence websocket.
  Input-format, digital-twin, premium, geospatial-service and 47/47 comparison
  claims are gone. A "Not implemented" table names the mounted routes that
  ignore input or have no callers. The modules stay. Wiring or deleting them
  is still a product call in viewtopia's DESIGN_TODO.

## [Unreleased] - 2026-08-14

### Changed

- 2026-08-15: docs test count is 737. `docs/ecosystem.html` puts fenestra
  under Platform (server) and fluvius under Streaming.
- The README no longer sells the realtime websocket as a sensor feed. The
  socket at `/api/v1/realtime/{room}` is real, mounted and JWT-gated, but it
  carries a fixed set of collaboration messages, Join, Leave, Cursor, Chat,
  Presence and ViewChanged. Anything else that arrives is logged and dropped,
  so no IoT reading can travel over it. The feature list and the Cesium Ion
  comparison row now say presence, cursors and chat. The test inventory marks
  rules engine and geofencing as modules no route reaches, which is their
  actual state: both are written and unit-tested, neither is constructed by
  the binary.

### Removed

- Three digital-twin README claims that nothing in the shipped server backs.
  "Real-time data injection" described pushing sensor values into the scene
  over the websocket: the `push_update` broadcast helper exists but no route
  and no other module ever calls it. "Entity linking" described mapping
  building ids to sensor readings: the three `GET /api/v1/entity-links` routes
  are mounted, but the store is built empty at startup and its create, update
  and delete methods are unreachable from any route, so the endpoints can only
  ever answer an empty list. "Scripting / rules engine" described firing alerts
  on sensor thresholds: the engine is written and unit-tested, with threshold
  triggers and alert actions, but `pub mod scripting` is the only reference to
  it anywhere, so the binary never constructs it and no request can reach it.
  The code stays, the claims go until a route exposes them.

## [Unreleased] - 2026-08-13

### Fixed
- `GET /api/v1/terrain/bundles` answers 500 and logs the reason when the
  bundles directory cannot be read, instead of an empty array that reads as a
  server hosting nothing. A missing `<data-dir>/terrain_bundles/` is still an
  empty list, because a server with no bundles configured never has one, but a
  permissions or I/O failure is no longer indistinguishable from that.
- The id `GET /v1/assets` hands out is the id `GET /v1/assets/{id}` and
  `GET /v1/assets/{id}/endpoint` take back. The list rendered a number folded
  out of the asset's uuid, half its bytes dropped and the sign thrown away,
  while the id routes parsed a uuid, so a client that read an id off the list
  had nothing it could ask for the asset with. The number is all an Ion client
  ever has, and `IonImageryProvider.fromAssetId` refuses an id that is not one.
  Every asset now carries a stored ion id,
  taken from a counter that only ever climbs and held unique by an index, so
  two assets can never share a number and a deleted asset's number is not
  handed out again. A database written before the column gets it added and its
  rows numbered oldest first. The id routes still take a uuid, so a link built
  against the native asset id keeps working.
- `GET /v1/assets/{id}/endpoint` refuses an imagery asset with 501 and a
  message saying why, instead of answering `IMAGERY` with a `tileset.json` url.
  Nothing here can serve imagery: the worker rejects a raster upload as an
  unsupported format and no route serves image tiles, so there was never
  anything behind that url. CesiumJS hands the url from an `IMAGERY` endpoint
  to a TMS provider, which goes looking for `tilemapresource.xml` beside it, so
  the old answer could only fail in the client. Same shape of bug as the
  terrain endpoint below.
- A tiling job is no longer announced as `Done` before the asset status write
  lands. The worker wrote the job record first and the asset second, so a
  client that polled the job, saw `Done` and read the asset straight after
  could get `Tiling` instead of `Ready`, and it never corrected because the
  client had stopped polling. The asset write now goes first, and a failed one
  is logged instead of discarded. `job_lifecycle_queued_to_running_to_done`
  spins between reads rather than sleeping, so it reads the asset in the
  instant the job settles: it failed 40 runs out of 60 against the old order
  and 0 out of 60 against the new one.
- `GET /v1/assets/{id}/endpoint` answers a terrain asset with the directory of
  its prebuilt bundle, `/api/v1/terrain/bundles/{asset-id}/`, instead of a
  `tileset.json` URL no terrain client can read. `CesiumTerrainProvider.fromUrl`
  appends `layer.json` to whatever URL arrives, and a 404 there is not an error
  to CesiumJS: it reads the miss as a pre-metadata heightmap layer and then
  404s every tile, so the old answer failed silently. An asset with no bundle
  under `<data-dir>/terrain_bundles/<asset-id>/` gets 404 with a message naming
  the directory to put one in, rather than a URL that cannot work.
- The endpoint response carries an `attributions` array. CesiumJS maps that
  field without checking it is there when it builds a provider's credits, so
  every Ion-compat asset threw before its first tile.

### Changed
- The README no longer lists imagery tiling under Cesium Ion compatibility. A
  tile pyramid generator sits in tiletopia-ingest, but no upload, worker or
  route reaches it, and the parity roadmap already records the pipeline as
  unbuilt.
- `docs/ecosystem.html` describes panoptes as imagery feature extraction,
  fluvius as a real-time stream processor, fenestra as an OGC services gateway
  and ptolemy as a versioned geodatabase, each matching what the repo says it
  is. The old lines named work those repos do not do.

### Added
- Prebuilt quantized-mesh terrain bundles are served from
  `<data-dir>/terrain_bundles/<name>/`, so a viewer can have terrain with no
  Ion token and no reach upstream. `GET /api/v1/terrain/bundles` lists them,
  `GET /api/v1/terrain/bundles/{name}/layer.json` and
  `GET /api/v1/terrain/bundles/{name}/{z}/{x}/{y}.terrain` are the pair
  `CesiumTerrainProvider.fromUrl` asks for. The layout is what `ctb-tile`
  writes and what the `terrain_bundle` export format already produces, so
  nothing has to be converted on the way in. Anonymous like the rest of
  `/api/v1/terrain/`, because a terrain provider cannot send a header.

  The bundle's own `layer.json` goes out with its `tiles` template replaced by
  a relative one, so a bundle built against another host resolves back here
  instead of sending the viewer off the server it is being hosted on. Tiles a
  tiler gzipped in place carry `Content-Encoding: gzip`, without which the
  browser hands Cesium the gzip container as a mesh. A bundle with no
  `available` array gets one read off its tile tree, because CesiumJS builds a
  child mask from that array and throws on the first tile when it is missing.
  Bundles must be `quantized-mesh-1.x` on a scheme and projection CesiumJS
  accepts, and one that is not is refused with the reason logged rather than
  served for the client to reject.

## [Unreleased] - 2026-08-09

### Changed
- Isochrone contours are a concave hull instead of a convex one. A convex hull
  spans every bay and dead end in the reachable area, so `GET /api/v1/isochrone/compute`
  claimed reach over ground nothing can get to. The request carries a `concavity`
  field and the endpoint an optional `concavity` query parameter, both defaulting
  to 2.0. Lower values hug the reachable area more closely, infinity reproduces
  the old convex contour. Both the grid and graph paths honour it, and
  `DEFAULT_CONCAVITY` comes from `itinera_core` so the two repos cannot drift.

### Fixed
- `GET /api/v1/isochrone/compute` rejects bad parameters instead of quietly
  substituting its own. It defaulted a missing `lon`/`lat` to San Francisco,
  dropped any `minutes` entry that would not parse, and turned an unknown
  `profile` into driving, so a typo came back as a plausible-looking isochrone
  of somewhere else. `lon` and `lat` are now required and range-checked, and a
  `minutes`, `profile` or `concavity` value that is present but unusable returns
  400 with the reason. Omitting an optional parameter still takes the default.
- `GET /api/v1/isochrone/profiles` lists the three profiles the compute endpoint
  actually accepts. It advertised `PublicTransit`, which does not exist, and
  capitalised the names, which the parser did not match.

### Added
- `GET /api/v1/assets/{id}/jobs` lists an asset's tiling jobs, newest first.
  The job id came back on the upload response alone, so only the session that
  uploaded could read progress and an asset listed on a later page load showed
  its status by itself. Needs a token, like the rest of the job surface.

### Changed
- `POST /api/v1/assets` reports the tiling job it queued. The handler discarded
  the `JobRecord` that point cloud uploads create, so a client had no id to poll
  `GET /api/v1/jobs/{id}` with and could not show tiling progress. The response
  now carries a `job_id` alongside the asset fields, omitted for asset types
  that tile on demand rather than on upload.

## [Unreleased] - 2026-08-08

### Changed
- CRS reprojection runs on `projicio-core` instead of proj4rs. The old
  `transform_proj4` fed `+init=epsg:XXXX` to a proj4rs build with no EPSG
  database, so it could only ever error, and the hand-rolled UTM series did
  the real work for the four zone ranges it covered. `transform_between_epsg_codes`
  replaces it and works, and `Transformer` now reaches every CRS projicio
  knows, from an EPSG code, a projstring or a WKT definition. EPSG:4978 keeps
  a separate path, since it is the one 3D pair and projicio transforms x and y
  only. `ReprojError::Proj4` is now `ReprojError::Projicio`, and
  `ReprojError::OutOfRange` is gone with the UTM series that raised it.
- `reproject_to_wgs84` transforms the whole point slice in one batch, so the
  transform is built once per call rather than once per point.

## [Unreleased] - 2026-08-05

### Added
- Asset exports are reachable: `POST /api/v1/exports` (editor tier) creates a
  job for `{asset_id, format, bounds?}` and runs the already-real export
  engine in the background, `GET /api/v1/exports/{id}` polls it, and
  `GET /api/v1/exports/download/{id}` streams the finished file with a
  content-disposition filename (404 until ready). The engine and its
  encoders existed since July, nothing routed creation, status or download.
  `EXPORT_FORMATS` is now the single table the formats endpoint renders and
  the parser accepts, so the advertised and accepted sets cannot drift. The
  JWT carries no tenant claim, so the caller's user id is the tenant: get,
  download and the listing are all tenant-scoped (the listing previously
  returned every tenant's jobs plus the demo jobs).

### Fixed
- A terrain tile whose SRTM download fails is answered `503` naming the tile
  instead of `200` with a zero-elevation mesh, which read as terrain that was
  enabled and perfectly flat. Skadi covers the whole globe, so an unreachable
  tile is upstream trouble, never missing data; tiles served from local DEM,
  and tiles too wide to fetch at all, are unchanged. `TILETOPIA_SRTM_BASE_URL`
  points the fetch somewhere else, which is how the refusal is tested.

## [Unreleased] - 2026-08-04

### Added
- `GET /api/v1/analysis/export/{op}?bbox=west,south,east,north&resolution=<m/px>`
  renders one analysis raster over a whole bbox and answers a deflate web
  mercator COG (512 px tiles, overviews down to one tile) as an attachment.
  The grid anchors on the bbox's north-west corner and snaps outward to whole
  pixels, latitudes clamp to the mercator domain, and an export is capped at
  4096x4096 pixels (400 past it). Auth-gated, unlike the tile route, and it
  takes the same render slot: one export is one render.
- `ndvi` joins the analysis tile ops: sentinel-2 L2A red and nir read over
  STAC as one two-band raster (geoplumb's multi-asset source), reduced per
  pixel to a median of the last month's items, band math `(nir - red) /
  (nir + red - 2000)` in digital numbers (the baseline 04.00 offset cancels
  in the numerator only), reprojected to web mercator and painted over a
  brown-tan-green diverging ramp. Requires `TILETOPIA_ANALYSIS_DEM_BBOX`:
  there is no synthetic vegetation, unset answers 500 naming the variable.
  The trailing window anchors at engine build, like every source read.
- `GET /api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` serves hillshade and slope
  tiles rendered on demand by the geoplumb pull engine, over the same elevation
  store and with the same colors as `POST /api/v1/analysis/terrain`. Hillshade
  takes `azimuth` and `altitude` query parameters, defaulting to 315 and 45.
  Engines are built on first use and cached per op and parameter set, so a DEM
  loaded after that is not picked up until restart.
- The analysis tile routes are anonymous reads, like the 3D Tiles and terrain
  tiles: a map library cannot send an Authorization header. The rest of
  `/api/v1/analysis/` stays gated.
- `TILETOPIA_ANALYSIS_DEM_BBOX` (`west,south,east,north` in degrees) puts the
  analysis tiles on Copernicus GLO-30 COGs streamed over STAC instead of the DEM
  store, with `TILETOPIA_ANALYSIS_STAC_API` overriding the Earth Search default.
  Unset, nothing reaches the network. A malformed bbox refuses startup, and a
  failed search answers 500 rather than falling back to synthetic terrain.

### Changed
- `terrano-core` now tracks master rather than the v0.1.0 tag, so tiletopia and
  geoplumb share one copy of it.
- `AppState::elevation_store` is an `Arc`, shared with the tile engines.

### Security
- Analysis tile renders are capped at one per core. A request over the cap waits
  up to two seconds for a slot, then is answered `503` with `Retry-After`, so a
  viewer opening a screen of tiles queues rather than losing the ones past the
  cap. A cold tile is a few hundred milliseconds of CPU and the route is
  anonymous, so uncapped it let one caller pin every core.
- `azimuth` and `altitude` are folded into a turn and a quarter turn before they
  key an engine, and a non-finite angle is a `400`. The engine map is a cache of
  eight, so unfolded angles let a caller evict every entry and force a fresh
  graph solve per request.

## [Unreleased] - 2026-08-02

### Added
- Tests covering asset and job persistence across a database reopen, and the job
  lifecycle from queued through the background worker to done.

### Changed
- `deny.toml` allows `0BSD`, needed by varint-rs 2.2.1.
- Roadmap phases 1.3 and 1.4 now describe the shipped SQLite store and job
  worker, and list what is still open on each.

## [Unreleased] - 2026-08-01

### Security
- Annotation writes (`POST`/`DELETE /api/v1/assets/{id}/annotations`) now need the
  editor or admin role plus ownership of the target asset, the same gate as asset
  delete and retile. Creating one records the author's JWT `sub` as `created_by`.
- Annotation delete is scoped to the asset in the path, so owning one asset is no
  longer a way to delete an annotation on another. Unknown pairs return 404.
- Plugin registry mutations (install, uninstall, config, enable, disable) now need
  the admin role. A plugin runs server-wide, so the editor tier is not enough.
- `GET /api/v1/assets` now requires a token and lists only assets the caller owns,
  plus legacy ownerless rows. Admins still see everything. Tile data stays
  anonymous, this hides other tenants' asset metadata.
- Role checks read the JWT `role` claim through `UserRole::from_claim`, which
  rejects anything that is not exactly `admin`, `editor` or `viewer`. An unknown
  role now lands in no tier instead of being compared as a raw string.

### Removed
- The `rbac` module (casbin enforcer, `RbacStore`, OIDC claim validation). It was
  never called from a route and modelled per-asset grants and orgs that do not
  exist. The live authz primitives are the JWT role tiers and per-asset
  ownership. `/api/v1/demo/rbac` keeps serving its canned sample data.

## [0.3.0] - 2026-05-08

### Added
- **Open Data Catalog** — curated registry of 16 free geospatial datasets across 5 categories
  - Terrain: Copernicus DEM GLO-30, USGS 3DEP (1m), NASA SRTM, Mapzen terrain tiles
  - Buildings: OSM 3D Buildings, Overture Maps (2.3B footprints), Google Photorealistic 3D Tiles
  - Imagery: Sentinel-2 L2A (10m), OpenStreetMap, Esri World Imagery, OpenAerialMap
  - Point Clouds: OpenTopography, AHN4 Netherlands, USGS Entwine
  - Vector: OpenMapTiles (MVT), Natural Earth
  - REST API: `GET /api/v1/catalog`, `GET /api/v1/catalog/{id}`, filter by `?category=`
- **Terrain Tile Server** — serves quantized-mesh terrain tiles from open DEM data
  - Endpoint: `GET /api/v1/terrain/{z}/{x}/{y}` + `GET /api/v1/terrain/layer.json`
  - Quantized-mesh binary encoding (CesiumJS-compatible)
  - WGS84 ECEF bounding sphere computation
  - Delta-encoded + zigzag-encoded vertex arrays
  - High-water-mark triangle index encoding
  - Edge indices for seamless tile stitching
  - Auto-loads DEM tiles from disk, falls back to flat terrain
- **Multi-Renderer Support** — switch between 3 rendering engines at runtime
  - CesiumJS: 3D globe, quantized-mesh terrain, OGC 3D Tiles
  - deck.gl: WebGL2 GPU-instanced visualization, loaders.gl 3D Tiles
  - MapLibre GL JS: vector tiles, 3D terrain exaggeration, 3D buildings
  - UI: renderer dropdown selector in top-right corner
- **Frontend catalog panel** — browse datasets by category with metadata (provider, format, resolution, coverage, license)

### Changed
- `tiletopia-server` now depends on `tiletopia-terrain` for terrain tile generation
- AppState includes `catalog: OpenDataCatalog` field
- Added `gui/src/renderers.js` module for renderer abstraction

## [0.2.0] - 2026-05-08

### Added
- Demo API endpoints (`/api/v1/demo/*`) serving real computed data from core modules
  - `/demo/measurement` — 3D distance, polyline length, polygon area, mesh volume, cut/fill, slope, bearing
  - `/demo/anomaly` — deformation detection, encroachment zones, statistical outlier removal
  - `/demo/clash` — BIM hard/soft clash detection with element IDs and distances
  - `/demo/audit` — full audit trail with filtering by user, action, resource type
  - `/demo/rbac` — RBAC user/role listing with OIDC provider info
  - `/demo/stories` — narrated presentation data with slides and camera paths
- Frontend panels for all 5 premium feature categories (Measurement, Anomaly, Clash, Admin, Stories)
- Real screenshots of live application in `docs/screenshots/`
- Audit endpoint supports query parameters: `?user_id=`, `?action=`, `?resource_type=`, `?limit=`

### Fixed
- Bearing measurement now normalized to 0–360° range
- Added soft clash detection (clearance violations) alongside hard clashes

## [0.1.0] - 2026-05-07

### Added
- Initial release with 19 premium feature modules
- 7-crate workspace: core, server, worker, ingest, terrain, store, CLI
- CesiumJS 3D viewer with OpenStreetMap base layer
- Point cloud & terrain ingestion pipeline
- 3D Tiles serving with REST API
- WebSocket real-time collaboration
- 213 tests passing across all crates
- GitHub Pages documentation site
- CI/CD with GitHub Actions

[0.2.0]: https://github.com/GeoLang/tiletopia/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/GeoLang/tiletopia/releases/tag/v0.1.0
