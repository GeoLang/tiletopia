# tiletopia

Fast open-source 3D Tiles server — self-hosted alternative to Cesium Ion.

Ingest raw geospatial data, tile it into OGC 3D Tiles 1.1, and serve it with view-dependent streaming. Compatible with CesiumJS, Cesium for Unreal, Cesium for Unity, and any 3D Tiles client.

## Features

### Tiling Engine
- **OGC 3D Tiles 1.1** output (batched 3D models, point clouds, implicit tiling)
- **Octree spatial partitioning** with geometric error-based LOD
- **Parallel tiling** across all CPU cores (Rayon work-stealing)
- **GPU-accelerated** point cloud decimation (optional, via wgpu — Metal/Vulkan/DX12)
- **Incremental tiling** — resume interrupted jobs, re-tile only changed regions
- **Draco/meshopt compression** for compact tile delivery

### Input Formats
- **Point clouds:** LAS, LAZ, E57, PLY
- **3D models:** glTF, GLB, OBJ, FBX, IFC, CityGML, Collada
- **Terrain:** GeoTIFF, DTED, HGT, USGS DEM
- **Vector:** Shapefile, GeoJSON, KML, GeoPackage
- **Imagery:** GeoTIFF, JPEG2000, MBTiles (terrain draping)

### Tile Server
- **REST API** for asset management, tiling jobs, access control
- **Tile streaming** with view-dependent LOD and frustum culling
- **CORS support** for cross-origin CesiumJS access
- **WebSocket real-time layer** for live IoT/sensor data overlay
- **Temporal engine** for time-series 3D state management

### Terrain
- **Quantized mesh terrain** from DEM/DTM heightmaps
- **Delaunay triangulation** with geometric error-based simplification
- **Multi-resolution terrain** with quadtree tiling

### Digital Twin Support
- **Real-time data injection** — push live sensor values into 3D scene via WebSocket
- **Temporal versioning** — serve different model states over time
- **Entity linking** — map 3D tiles to metadata (building ID → sensor readings)
- **Update API** — push model changes without full re-tile

### Storage
- **Local filesystem** — zero-config default
- **S3 / GCS / Azure Blob** — cloud storage backends
- **Hybrid** — hot tiles local, cold tiles in cloud

### Desktop & Cloud
- **CLI tool** — tile locally without running a server
- **Docker image** — deploy anywhere
- **Web dashboard** — upload, monitor jobs, preview in CesiumJS
- **Prometheus metrics** — `/metrics` endpoint for monitoring

## Architecture

```
tiletopia/
├── crates/
│   ├── tiletopia-core/       # Tiling engine, spatial indexing, LOD, 3D Tiles types
│   ├── tiletopia-server/     # Axum REST API, tile streaming, WebSocket
│   ├── tiletopia-worker/     # Async tiling pipeline, job queue
│   ├── tiletopia-ingest/     # Format readers (LAS, CityGML, GeoTIFF, glTF, etc.)
│   ├── tiletopia-terrain/    # Quantized mesh terrain generation
│   ├── tiletopia-store/      # Storage abstraction (local, S3, GCS, Azure)
│   └── tiletopia-cli/        # CLI binary
├── gui/                      # Web dashboard (Vite + CesiumJS preview)
└── docs/                     # Documentation & GitHub Pages
```

## Quick Start

### Build from source

```bash
git clone https://github.com/tiletopia/tiletopia.git
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

## REST API

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/api/v1/health` | Server health check |
| `GET` | `/api/v1/assets` | List all assets |
| `POST` | `/api/v1/assets` | Create/upload asset |
| `GET` | `/api/v1/assets/{id}` | Get asset details |
| `GET` | `/api/v1/assets/{id}/tileset.json` | Serve tileset |
| `GET` | `/api/v1/assets/{id}/tiles/{path}` | Serve individual tile |
| `POST` | `/api/v1/assets/{id}/tile` | Start tiling job |
| `GET` | `/api/v1/jobs` | List tiling jobs |
| `GET` | `/api/v1/jobs/{id}` | Job status & progress |
| `WS` | `/api/v1/realtime/{id}` | WebSocket for live data |

## GPU Acceleration

GPU compute is optional and auto-detected:

| Platform | Backend | Notes |
|----------|---------|-------|
| macOS (Apple Silicon) | Metal via wgpu | M1/M2/M3/M4/M5 |
| Linux/Windows (NVIDIA) | Vulkan via wgpu | Optional CUDA for max perf |
| Linux/Windows (AMD/Intel) | Vulkan via wgpu | |
| Web | WebGPU | Browser-native |

Build with GPU support:
```bash
cargo build --release --features gpu
```

Without `--features gpu`, all computation uses CPU (Rayon parallel).

## Comparison with Cesium Ion

| Feature | tiletopia | Cesium Ion |
|---------|-----------|------------|
| OGC 3D Tiles 1.1 | ✅ | ✅ |
| Point cloud tiling | ✅ | ✅ |
| Terrain generation | ✅ | ✅ |
| CityGML/BIM tiling | ✅ | ✅ |
| REST API | ✅ | ✅ |
| CesiumJS compatible | ✅ | ✅ |
| Self-hosted | ✅ | ❌ |
| GPU acceleration (Metal/Vulkan) | ✅ | ❌ |
| WebSocket real-time layer | ✅ | ❌ |
| Temporal/time-series | ✅ | ❌ |
| Digital twin support | ✅ | ❌ |
| Local filesystem storage | ✅ | ❌ |
| Open source | ✅ (GPL-3.0) | ❌ |
| **Price** | **Free** | **$150+/month** |

## License

GPL-3.0 — see [LICENSE](LICENSE).
