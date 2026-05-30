# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
