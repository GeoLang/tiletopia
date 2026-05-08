# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.2.0]: https://github.com/TileTopia-HQ/tiletopia/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/TileTopia-HQ/tiletopia/releases/tag/v0.1.0
