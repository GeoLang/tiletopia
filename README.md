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
- **Tile streaming** with view-dependent LOD and frustum culling
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

### Storage
- **Local filesystem** — zero-config default
- **Amazon S3** — full CRUD via aws-sdk-s3
- **Google Cloud Storage** — planned
- **Azure Blob** — planned
- **Hybrid** — hot tiles local, cold tiles in cloud

### Deployment
- **Single binary** — no runtime dependencies
- **Docker image** — `docker run -p 3000:3000 tiletopia`
- **Air-gapped / offline** — works without internet
- **Multi-tenant** — per-org/project isolation
- **Web dashboard** — upload, monitor jobs, preview in CesiumJS
- **Prometheus metrics** — `/metrics` endpoint

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
| **Open source** | ✅ GPL-3.0 | ❌ Proprietary |
| **Price** | **Free forever** | $150–$3,750/month |

**16 exclusive features** that Cesium Ion cannot match.

---

## Tests

```bash
cargo test
```

53 unit tests covering:
- Core: AABB, octree, LOD, .pnts format, tileset serialization, coordinate transforms, CRS reprojection, GPU compute, diff detection, plugins (21)
- Store: local filesystem CRUD, path traversal (6)
- Server: health, assets, tilesets, security, annotations, temporal versioning, multi-tenancy, offline export (13)
- Ingest: LAS/LAZ, GeoTIFF, BIM/IFC readers (8)
- Terrain: quantized mesh generation (5)
- Terrain: quantized mesh, multi-LOD, interpolation (5)

---

## License

GPL-3.0 — see [LICENSE](LICENSE).
