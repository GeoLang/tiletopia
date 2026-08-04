# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased] - 2026-08-04

### Added
- `GET /api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` serves hillshade and slope
  tiles rendered on demand by the geoplumb pull engine, over the same elevation
  store and with the same colors as `POST /api/v1/analysis/terrain`. Hillshade
  takes `azimuth` and `altitude` query parameters, defaulting to 315 and 45.
  Engines are built on first use and cached per op and parameter set, so a DEM
  loaded after that is not picked up until restart.
- The analysis tile routes are anonymous reads, like the 3D Tiles and terrain
  tiles: a map library cannot send an Authorization header. The rest of
  `/api/v1/analysis/` stays gated.

### Changed
- `terrano-core` now tracks master rather than the v0.1.0 tag, so tiletopia and
  geoplumb share one copy of it.
- `AppState::elevation_store` is an `Arc`, shared with the tile engines.

### Security
- Analysis tile renders are capped at one per core, and a request over the cap
  is answered `503` with `Retry-After` rather than queued. A cold tile is a few
  hundred milliseconds of CPU and the route is anonymous, so uncapped it let one
  caller pin every core.
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
