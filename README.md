# TileTopia

[![CI](https://github.com/GeoLang/tiletopia/actions/workflows/ci.yml/badge.svg)](https://github.com/GeoLang/tiletopia/actions)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)

**Self-hosted 3D Tiles server, an open-source alternative to Cesium Ion.**

TileTopia tiles point clouds, meshes, IFC and vector files into OGC 3D Tiles 1.1 and serves them to CesiumJS or any other 3D Tiles client. It also serves quantized-mesh terrain from a DEM or a prebuilt bundle, and hosts CZML documents as uploaded. Point clouds, meshes and IFC go through this repository's own readers and tilers, and vector files go through mago-3d-tiler. Tiles from an IFC carry each element's GlobalId as the `asset_id` feature property, so a client that picks a tile feature reads the id back.

**Website:** https://geolang.github.io/tiletopia

---

## Features

### Tiling

- OGC 3D Tiles 1.1 point-cloud tiles (`.pnts`) from an octree with geometric-error LOD, built in parallel with Rayon
- Mesh tiles as `.glb`, with lower LODs from meshopt simplification per source mesh. Source meshes stay whole in one tile unless they exceed the tile's triangle budget, and geometry repeated within a tile is written once through `EXT_mesh_gpu_instancing` with `EXT_instance_features` ids. Geometry is compressed with `EXT_meshopt_compression` or `KHR_draco_mesh_compression`, and textures can be written as KTX2 through `KHR_texture_basisu`
- 3D Gaussian splat tiles as `.glb` from a 3DGS training PLY, with `KHR_gaussian_splatting` and SPZ compression through `KHR_gaussian_splatting_compression_spz_2`, see [Gaussian splats](#gaussian-splats)

The job queue tiles point clouds (LAS, LAZ, E57, PLY) and meshes (glTF, GLB, OBJ, FBX, DAE, DXF, CityGML, IFC, IFCZIP) with the native tiler, whatever `TILETOPIA_MAGO_JAR` is set to. The readers carry UVs, diffuse textures and diffuse colours through to the tile GLBs, and a tile holding part of a textured mesh carries the crop of the texture its triangles reach. A mesh with a texture the readers cannot find or decode tiles untextured, and so does one naming a texture outside its directory, unless a file of that name sits beside the mesh. An OBJ `mtllib` outside the OBJ's directory is not read, so the model tiles without those materials. A `.gltf` naming a buffer or image outside its directory fails.

A point cloud is placed by the CRS in its LAS GeoKey record, and `tiletopia tile` also reads a `.prj` file beside the input. When the file carries neither, the upload's `crs` field or `tiletopia tile --crs` names the source CRS as an EPSG code, such as `EPSG:32632` or `32632`. Heights are taken as ellipsoidal. A point cloud with no CRS from any of these is tiled in its own coordinates, with a warning naming the file.

A mesh is placed by the upload's `longitude` and `latitude`, and one without them fails naming them. An IFC without them is placed by its `IfcMapConversion` or `ePSet_MapConversion` when the target CRS has an EPSG code or a WKT definition, and otherwise by the `IfcSite` reference coordinates. An IFC with none of these fails. Map coordinates are in the CRS's `MapUnit`, or the project length unit when it has none, and they are converted to the unit of the EPSG code, feet included. `RefElevation` is in the project length unit. `OrthogonalHeight` and `RefElevation` are taken as ellipsoidal. The job fails naming the reason, and does not fall back to the site, when a map conversion's CRS unit is unknown, which includes a WKT with no EPSG code. Upload coordinates avoid that failure. An `IfcMapConversionScaled` factor that differs from 1 by more than 0.1 percent resizes the model along its axis, whatever places the model. The job fails naming the factors when one is negative, or when `Scale` does not convert the project unit to the map unit, with or without upload coordinates. A WKT definition past 64 KiB or nested past 32 levels fails the job when it places the model, and it is left out of the rotation with upload coordinates. The job also fails when the IFC's meshes pass the job's memory budget. An IFC's x axis is turned to the map conversion's rotation plus the grid convergence, or to its `TrueNorth`, including when the upload places the model. `crs` is ignored on the native path.

An IFC placed by a map conversion to a projected CRS is tiled in one flat east-north-up frame at the conversion origin, turned as above, while that frame is within half the placement tolerance of each corner of the model's bounding box. The tolerance is 0.1 m or 0.1 percent of the corner's distance from the origin, whichever is larger. Earth's curvature, d²/2R, reaches half of 0.1 percent at 6.4 km from the origin, and with UTM's central scale of 0.9996 the limit is about 3.8 km. A larger model has each vertex taken through the map conversion's eastings, northings, rotation and height into the map CRS, through projicio to longitude and latitude, and to ECEF, and is written in the unturned east-north-up frame at the origin. Its normals turn with the vertices, and its element store locations come from the same vertices. Both paths leave `Scale` out, and the per-vertex path reads a local metre as a metre of the map grid. A model in a geographic map CRS is always tiled in the flat frame.

An IFC's walls and slabs are cut by their `IfcOpeningElement`s, and the openings, `IfcSpace`s and `IfcSpatialZone`s are not tiled. An `IfcSite` with a body is. A finished job whose IFC lost geometry reports the count and the first five reasons in `error`, with `status` still `done`. A `.ifczip` upload unpacks to the single `.ifc` inside, under the limits of an Ion zip upload, and any other number of `.ifc` files answers 400.

IFC tiles carry CAD edges through `EXT_mesh_primitive_edge_visibility` with every `mesh_compression`. Vertices are welded by position within each element. An edge with one triangle, more than two, or a fold of 30° or more is a hard edge. A fold between 1° and 30° is a silhouette edge with both face normals, and a smaller fold is hidden. The edge values follow the triangle corner order, which the meshopt `TRIANGLES` codec rotates and Draco's EdgeBreaker reorders. So a primitive with edges stores its indices with the meshopt `INDICES` codec or Draco sequential connectivity, which take more bytes per triangle than `TRIANGLES` and EdgeBreaker. Geometry written through `EXT_mesh_gpu_instancing` carries no edges, since CesiumJS 1.147 draws them without the instance transforms. An element split across tiles gets a hard edge along the split.

Vector files (GeoJSON, TopoJSON, GeoPackage, KML) go to [mago-3d-tiler](https://github.com/Gaia3D/mago-3d-tiler) (MPL-2.0), which the Docker image bundles with a JRE 21. Without the jar they fail naming the variable. The jar ships natives for Linux and Windows x64 only, so on macOS the mago jobs fail inside mago.

A TopoJSON is decoded to GeoJSON before mago reads it, following the [TopoJSON specification](https://github.com/topojson/topojson-specification): quantized arcs are delta-decoded through the `transform`, shared arcs are stitched into each line and ring, and each member of a top-level GeometryCollection becomes one feature carrying its `id` and `properties`. The file is decoded in memory, so a TopoJSON over 29,826,161 bytes fails the job naming the size.

A DAE must be COLLADA 1.4.1, read with its scene node transforms, `up_axis` and `unit`, one mesh per instanced `<triangles>` or `<polylist>`. A texture is read from `<data>` in the file or from a file beside it, so an upload of the `.dae` alone keeps only embedded textures. These fail naming the reason: COLLADA 1.5, `<polygons>`, `<linestrips>`, `<trifans>`, `<tristrips>`, cube, volume and planar surfaces, and `<skew>`. Skinned and morphed geometry is skipped with a warning, and a job whose scene holds nothing else fails as having no geometry. `<unit>` and `<up_axis>` are read from the document's top `<asset>` only. Every mesh has its own copy of its texture, and a scene fails when those copies plus 100 bytes a vertex add up past a third of the 1 GiB task.

A DXF must be ASCII DXF. Its 3DFACE, polyface and polygon mesh POLYLINE and MESH entities are tiled, in model space and inside the blocks an INSERT places, with the INSERT's insertion point, scale, rotation, extrusion direction, row and column array and nesting. `$INSUNITS` sets the unit. A DXF upload takes an optional `units` field, one of the `$INSUNITS` unit names: `inches`, `feet`, `miles`, `millimetres`, `centimetres`, `metres`, `kilometres`, `microinches`, `mils`, `yards`, `angstroms`, `nanometres`, `micrometres`, `decimetres`, `decametres`, `hectometres`, `gigametres`, `astronomical-units`, `light-years`, `parsecs` or `us-survey-feet`. The field names the unit of a drawing whose `$INSUNITS` is missing or 0, and such a drawing without it answers 400. A field that disagrees with a nonzero `$INSUNITS` answers 400, and so do a name off that list and the field on any other upload. The job stores the field and a scheduled re-tile reuses it. A binary DXF answers 400 at upload. Entities with no surface, such as LINE, LWPOLYLINE, TEXT and HATCH, are skipped with a warning that counts them, and so are 2D entities extruded by a thickness. A MESH is tiled as its control mesh, unsmoothed. These fail the job naming the reason: a drawing with nothing 3D in model space, 3DSOLID, BODY and REGION (ACIS solids, binary SAT data), a smoothed polygon mesh, and an INSERT of a block the file does not define. A drawing fails when its stored vertices plus the vertices its INSERTs place pass 3,579,139, 100 bytes each in a third of the 1 GiB task. The tiles are untextured and carry no DXF colour.

A CZML upload is not tiled. It must be a JSON array whose first packet has id `document` and a `version` of the form `1.<minor>`, the rule CesiumJS's `CzmlDataSource` applies, and anything else answers 400. The asset is `ready` at once and the file is served unchanged as `application/json` at `/api/v1/assets/{id}/czml`, with no token. Files a CZML names by relative URL are not hosted.

An upload whose extension is not on the list answers 400. DEM rasters (tif, tiff, hgt, dt0, dt1, dt2) are tiled to a quantized-mesh bundle, see [Terrain sources](#terrain-sources). Georeferenced GeoTIFF, COG, JPEG, PNG and JPEG 2000 images serve XYZ tiles, see [Imagery tiles](#imagery-tiles). A JPEG 2000 upload answers 400 on a server built without the `jp2` feature.

A mesh or vector upload takes optional `longitude` and `latitude` fields beside the file, and a file that goes to mago also takes `crs`. A point cloud or DEM upload takes `crs`, and a Gaussian splat PLY takes `longitude` and `latitude`. One of longitude and latitude without the other is refused. A mesh upload also takes `texture_compression`, `none` by default. With `ktx2`, the native tiler writes each tile's diffuse texture as a KTX2 file with ETC1S and BasisLZ, declared through `KHR_texture_basisu` with no PNG or JPEG fallback, scaled up to the next multiple of 4 pixels and carrying a full mip pyramid. Any other value answers 400, and so does `ktx2` on a file the native tiler does not take. The value is stored on the job and a scheduled re-tile keeps it. The tiles land at `/api/v1/assets/{id}/tileset.json`, with content under `/api/v1/assets/{id}/tiles/{file}` from the native tiler and `/api/v1/assets/{id}/data/{file}` from mago.

A mesh upload takes a `mesh_compression` field: `meshopt` (the default), `draco` or `none`. Any other value answers 400. The job stores the value, and a scheduled re-tile reuses it. `POST /api/v1/assets/{id}/tile` writes uncompressed tiles. Point clouds and files that go to mago ignore the field.

`meshopt` writes `EXT_meshopt_compression` and keeps vertex values exact. `draco` writes `KHR_draco_mesh_compression` and quantizes each tile to 14 position bits, 10 normal bits and 12 UV bits. Both list their extension in `extensionsRequired` and carry no uncompressed copy, so a viewer without the decoder cannot load the tile.

To check tiles against the decoders CesiumJS ships, write the fixtures with `TILETOPIA_TILE_FIXTURE_DIR=<dir> cargo test -p tiletopia-core --test compressed_glb`, then run `node scripts/check_compressed_tiles_with_cesium.mjs <cesium package dir> <dir>`.

### Gaussian splats

A `.ply` upload is a Gaussian splat capture when its vertex element has any of `f_dc_0` to `f_dc_2`, `opacity`, `scale_0` to `scale_2`, `rot_0` to `rot_3` or an `f_rest_` property, the layout 3DGS training writes. Any other PLY tiles as a point cloud. A capture must be `binary_little_endian`, with `x`, `y`, `z` and all of those as `float`, and 0, 9, 24 or 45 `f_rest_` properties for spherical harmonics degree 0 to 3. The job fails naming a missing property, a NaN or infinite value, or a rotation of zero length. Other vertex properties, such as `nx`, are skipped.

A capture is placed like a mesh. Its `x`, `y` and `z` are metres east, north and up from the upload's `longitude` and `latitude` at height 0, and a capture without them fails naming them. `crs` is ignored. The spz library reads 3DGS PLY files as right, down, forward, so a capture trained in camera coordinates needs rotating to z up before upload.

Each tile is a GLB with one POINTS primitive whose data is one SPZ blob, listing `KHR_gaussian_splatting` and `KHR_gaussian_splatting_compression_spz_2` in `extensionsRequired`. The tileset names both under `3DTILES_content_gltf`, which is what CesiumJS checks before it draws tiles as splats. The SPZ is version 3, which spz 2.0.0 writes and Cesium ion's own splat tiles carry. It stores log scales, opacity before the sigmoid and the DC coefficients, and the decoder turns them into the linear scale, the 0 to 1 opacity in `COLOR_0` alpha and the colour `0.5 + 0.282095 * dc`. The ratified `KHR_gaussian_splatting` asks for an `OPACITY` attribute, and these tiles carry opacity in `COLOR_0` alpha as the SPZ extension maps it, which is what CesiumJS reads.

SPZ is lossy. Positions keep 12 fractional bits, about a quarter millimetre, and fewer in a tile more than 4 km across. Log scales keep steps of 1/16 between -10 and 5.94, rotations 9 bits a component, opacity and colour 8 bits, and higher spherical harmonics 8 bits between -1 and 1. Values outside those ranges are clamped, as the spz encoder clamps them. Spherical harmonics up to degree 3 are kept, since both the extension and CesiumJS 1.147 read them.

The tile tree is an octree over the splat centres. A cell with up to 32,768 splats is a leaf. A larger cell is split in eight and gets a coarser tile of its own, holding the 32,768 splats of its subtree with the largest opacity times the area of their two largest axes. Kept splats are not enlarged, so a coarse tile shows gaps between them. Each tile's bounding box holds every splat of its subtree out to 3 times its largest scale, the 3 sigma cut-off of the `KHR_gaussian_splatting` ellipse kernel, so it also holds its children's boxes. CesiumJS 1.147 draws a splat out to 2 times its largest scale. Cesium ion's own splat tiles bound only the centres. Refinement is `REPLACE`, leaves have geometric error 0 and a coarser tile has half the diagonal of its octree cell, as point clouds do. More than 32,768 splats in one cell 20 levels down fails the job.

The job reads the PLY a record at a time and holds 27 bytes a splat at degree 0, up to 72 at degree 3. In a third of the 1 GiB task that fits 12,979,364 splats at degree 0, 9,709,947 at degree 1, 6,825,167 at degree 2 and 4,805,821 at degree 3. A larger capture fails before its splats are read, naming its size.

No CesiumJS run has drawn these tiles yet. What is checked: the CesiumJS 1.147 source names these two extensions and loads `ThirdParty/wasm_splats_bg.wasm` for splats, and the tests decode the tiles written here. From that source, `Cesium3DTileset.fromUrl` with no options is expected to draw them. It needs WebGL 2 and `ThirdParty/wasm_splats_bg.wasm` from the Cesium build. A client without the SPZ extension cannot load them, since the tiles carry no uncompressed copy. The SPZ extension draft asks for left, up, front packing. These tiles hold the capture's own z up axes with a node matrix that turns them y up, as Cesium ion's splat tiles do, and CesiumJS 1.147 unpacks SPZ with no axis conversion. A decoder that converts from left, up, front negates the stored x and z.

### Photo sets

`POST /api/v1/assets` with a `.zip` of geotagged JPEG photos runs [OpenDroneMap](https://github.com/OpenDroneMap/ODM) on them through a [NodeODM](https://github.com/OpenDroneMap/NodeODM) server and tiles the textured mesh it writes. This server does no reconstruction of its own, so `TILETOPIA_NODEODM_URL` has to name a node:

```bash
docker run -p 3001:3000 opendronemap/nodeodm
TILETOPIA_NODEODM_URL=http://localhost:3001 TILETOPIA_ION_BASE_URL=http://localhost:3000 TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) tiletopia serve --data-dir ./data --port 3000
```

Without the variable a zip upload answers 400 naming it, and a re-tile of a stored photo set fails its job naming it. No token is sent to the node, so a node started with `--token` refuses every call.

Every file in the zip must be a `.jpg` or `.jpeg` with EXIF `GPSLatitude` and `GPSLongitude`, at most 128 MiB, and with a file name no other photo in the zip has, folders aside. Photos that unpack to more than twice the zip's size are refused. Each of these answers 400 naming the entry.

The upload takes `photogrammetry_preset`, `default` when absent, and `mesh_compression` as a mesh upload does. `fast`, `default` and `high_quality` set ODM's `feature-quality`, `pc-quality`, `mesh-size` and `mesh-octree-depth`, and `GET /api/v1/photogrammetry/presets` lists them with their ODM options. `default` is ODM's own defaults. Any other value answers 400, and so does the field on a file that is not a zip. `longitude`, `latitude`, `crs` and `texture_compression=ktx2` on a zip answer 400.

The upload answers 201 with the asset's `id` and a `job_id`. `GET /api/v1/jobs/{id}` reports `progress`, 0 to 0.1 while the photos go to the node and 0.1 to 0.9 while ODM runs. A task that fails puts ODM's error and the last 20 lines of its console in the job's `error`. Deleting the asset, or passing `TILETOPIA_PHOTOGRAMMETRY_TIMEOUT_SECS`, cancels and removes the task on the node, and stops a running download of its result. A tiler already running when the asset is deleted runs to its end, then the job removes what it wrote and fails naming the delete. Other jobs keep running while the node works, and NodeODM queues tasks past its own `parallel_queue_processing` limit, so this server has no limit of its own on reconstructions. Sending the photos takes one of the two build slots that imagery and DEM builds use. Tiling the result takes its turn with every other tiling job, one at a time, so `progress` can stay at 0.9 while the job waits. A re-tile of an asset stays `queued` until the asset's running reconstruction ends. A job running when the server stops fails at the next `serve` naming the restart, and `POST /api/v1/assets/{id}/tile` runs it again. Its task stays on the node until NodeODM's `cleanup_tasks_after` removes it once finished.

The tiles land at `/api/v1/assets/{id}/tileset.json`. The model is placed by the UTM zone and offset in ODM's `odm_georeferencing/coords.txt`, with ODM's heights unchanged. Texture pages over 4096 pixels on a side are scaled down to JPEG first, and a model with no readable texture fails its job.

### Tile server

- REST API for assets, tiling jobs and access control, with multipart upload and CORS
- WebSocket at `/api/v1/realtime/{room}` for presence, cursors, chat and view sync. It relays six message types and logs and drops anything else
- JWT authentication. `TILETOPIA_JWT_SECRET` (32 bytes or more) is required to serve, and `TILETOPIA_AUTH_DISABLED=true` turns the auth middleware off. Routes with no role or owner check of their own are then open to anonymous callers, and routes that check a role or an owner still need a token
- `POST /api/v1/auth/signup` and `POST /api/v1/auth/login` answer a token good for 24 hours. A signup gets the `viewer` role. `tiletopia set-role <email> admin` promotes a user directly in the SQLite database, so the first admin needs no admin route
- Roles `admin`, `editor` and `viewer`, plus per-asset ownership on writes
- 3D annotation layers, persisted, with owner-or-admin writes
- Admin routes under `/api/v1/admin/` for stats, health, jobs and user management
- Prometheus metrics at `/metrics`

### Terrain and analysis

- Quantized-mesh terrain from DEM files, quadtree tiled, and from DEM uploads, which need no extra step (see [Terrain sources](#terrain-sources))
- Prebuilt terrain bundles: serve a `ctb-tile` directory as a Cesium terrain source with no Ion token
- `/api/v1/terrain/` reads DEM files under `<data-dir>/dem` first, then SRTM tiles. A failed download answers 503 rather than a flat mesh
- `/api/v1/terrain/global/` serves the whole Earth from Copernicus DEM GLO-30, generated on request and cached on disk (see [Global terrain](#global-terrain))
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
- Loopback, private and link-local addresses are refused at subscription and at delivery. `TILETOPIA_WEBHOOK_ALLOW_PRIVATE_HOSTS=true` allows them, for a receiver on a local network. With it set, anyone who can create a subscription can make the server post to those addresses, cloud metadata addresses included
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

- `GET /v1/assets`, `/v1/assets/{id}` and `/v1/assets/{id}/endpoint` resolve asset ids and endpoints the way Ion does. Ion's token API is not implemented: `/api/v1/api-keys` mints this server's credentials
- The three asset reads need `Authorization: Bearer <jwt>` and answer only for assets the caller may view, see [Access](#access). CesiumJS sends `Ion.defaultAccessToken` as an `access_token` query parameter, which this server does not read, so give it the header on the server resource: `Cesium.Ion.defaultServer = new Cesium.Resource({ url: 'https://tiles.example.com/', headers: { Authorization: 'Bearer <jwt>' } })`
- A terrain asset resolves to a bundle named after the asset id, which a DEM upload builds itself
- An imagery asset resolves to `externalType: URL_TEMPLATE`. Checked: `IonImageryProvider.fromAssetId` in the CesiumJS 1.147 source handles that answer's `type`, `externalType` and `options`, and a test fetches a tile from its URL. No CesiumJS run has loaded one yet
- A CZML asset resolves to type `CZML` with the document's own URL. Checked: in the CesiumJS 1.147 source `IonResource` takes that `url` and `CzmlDataSource.load` fetches a resource as JSON. No CesiumJS run has loaded one yet
- `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID` resolves one id to the global terrain, with the Copernicus notice in `attributions`. New assets never take that id

Ion's upload flow runs against this server:

- `POST /v1/assets` (editor) takes `name`, `description`, `type` and `options.sourceType`, and answers 200 with `assetMetadata`, `uploadLocation` and `onComplete`. `3DTILES` takes `POINT_CLOUD`, `3D_MODEL`, `3D_CAPTURE`, `BIM_CAD`, `CITYGML`, `KML`, `VECTOR_DATA` and `GEOJSON`. `IMAGERY` takes `RASTER_IMAGERY` and `TERRAIN` takes `RASTER_TERRAIN`
- Options this server does not apply answer 400 instead of being dropped, and so do `from` and a non-empty `attribution`. That includes `clampToTerrain` and `baseTerrainId`. The options it applies are `position` with a height of 0 and `geometryCompression` and `textureFormat` on meshes, and `inputCrs` and `position` on point clouds
- A mesh other than an IFC needs `position`. Without it the tiling job fails and the asset goes to `ERROR`
- `POINT_CLOUD` applies `position` to a Gaussian splat PLY only. A splat PLY without it, and any other point cloud with it, answers 400 at `uploadComplete`
- `RASTER_IMAGERY` takes `.tif`, `.tiff` and `.jp2`, which needs the `jp2` feature and a GeoJP2 or GMLJP2 box. `.j2k`, JPEG and PNG answer 400, since Ion's options cannot send a placement
- `BIM_CAD` takes `.ifc`, `.gltf`, `.glb`, `.obj` and `.dxf`, and `.ifczip` unpacks like a zip to its `.ifc`. Ion's spec has no `units` option, so a DXF whose `$INSUNITS` names no unit answers 400 at `uploadComplete`. `VECTOR_DATA` takes `.geojson`, `.topojson`, `.gpkg` and `.kml`
- `uploadLocation` is an S3 location on this server, at `TILETOPIA_ION_BASE_URL`, with credentials that last 12 hours. It takes PutObject and multipart uploads signed with Signature Version 4. Signed chunks (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`) answer 501. A proxy in front of the server must keep the `Host` header, because the signature covers it
- The files and multipart parts one set of credentials holds total at most `MAX_UPLOAD_BYTES` (4 GiB), counted as each request streams, and a request past that answers S3's `EntityTooLarge` with 400 and stores nothing. A repeat of the same key or part number replaces the earlier copy and counts once. The count is taken from the stored files again after a restart, and a request's first scan of an asset deletes files cut off by the restart
- One set of credentials holds 20,000 files and parts together, the 10,000 entries of a zip plus the 10,000 parts of one S3 multipart upload, and past that a request answers 400 `TooManyFiles`. It holds 819 open multipart uploads, which is 4 GiB over S3's 5 MiB smallest part, and past that `CreateMultipartUpload` answers 400 `TooManyMultipartUploads`
- `CompleteMultipartUpload` closes its upload before it joins the parts, so a second complete or a late part answers `NoSuchUpload`, and the join carries on if the client disconnects. A retry after a disconnect answers `NoSuchUpload` with the file already stored or about to be. While a join runs, the joined file counts against the 4 GiB cap beside its parts, so a join that does not fit answers `EntityTooLarge` and its upload stays open
- One editor has 16 assets awaiting files at most. `POST /v1/assets` past that answers 429 until an upload completes or expires
- An asset's source is one zip, or any number of files uploaded directly under the prefix. Object keys under the prefix have no subdirectories, so a source with nested sidecars goes in a zip
- The main file is the one file whose extension the source type takes. `.mtl`, `.bin`, textures and anything else are kept beside it, and `__MACOSX` is skipped. No such file or several of them answer 400 naming the first 20 and counting the rest. The main file's directory becomes the asset's input, so sidecars must sit in or below that directory
- A zip may unpack to 100 times its own size and at most `MAX_UPLOAD_BYTES` (4 GiB), and may hold 10,000 entries. A symbolic link, an absolute path, a `..` step, a zip uploaded beside other files, or a compression method other than stored and deflate answers 400. Every refusal at `uploadComplete` puts the asset in `ERROR`
- A zip of photos is not reconstructed on this route. `3D_CAPTURE` takes a mesh already built by photogrammetry, and Ion's own reconstruction options sit under `RASTER_IMAGERY` with type `3DTILES`, which this server refuses. Send photos to `POST /api/v1/assets`
- `POST /v1/assets/{id}/uploadComplete` answers 204 and queues the tiling job. With no file uploaded it answers 400 and the asset keeps waiting. An imagery asset is `COMPLETE` as soon as it answers
- `status` is `AWAITING_FILES`, `NOT_STARTED`, `IN_PROGRESS`, `COMPLETE` or `ERROR`. `DATA_ERROR` is never reported
- `percentComplete` is the running job's `progress` while `IN_PROGRESS`. Only a photo set reports progress, up to 90. Every other tiling job stays at 0 until it finishes and the asset reads 100
- An upload with no `uploadComplete` 12 hours after its create call goes to `ERROR` and its files are deleted. At startup, any asset still `uploading` with no job and no upload credentials goes to `error`

### IFC elements

A tiling job on an `.ifc` or `.ifczip` writes every `IfcProject` and `IfcProduct` subtype, geometry or not, to `<asset_dir>/elements.sqlite`. The file is rebuilt on a re-tile and deleted with the asset. A store that cannot be built fails the job, which includes a file that passes a budget: property rows and object and property set pairs (each the larger of 4,000,000 and one per input byte), text bytes (the larger of 1 GiB and 16 per input byte), property visits (64 per allowed row), or 10 minutes. A stored text is cut at 64 KiB, and a list past 10000 entries is cut or left out, each with a warning on `property-index`. Materials, layers, profiles and constituents that do not decode, quantity set members that are not quantities, and quantities that are not finite are left out and counted there too.

These routes follow Ion's Asset Elements API. `{id}` is the Ion id or the asset's UUID, and an element id is its IFC `GlobalId`:

| Route | Answers |
|---|---|
| `GET /v1/assets/{id}/elements` | Elements matching `filter`, in pages of `top`, followed by `continuationToken` |
| `GET /v1/assets/{id}/elements/{elementId}` | One element and its properties |
| `GET /v1/assets/{id}/elements/search?search=` | Best matches over the GlobalId, name, class and property values, 50 a page, with `total` counting every match |
| `GET /v1/assets/{id}/elements/aggregate` | Count, min, max, avg, sum and units of `aggregations` over `filter`, per `groupBy` value when it is sent |
| `GET /v1/assets/{id}/elements/properties` | Every property key and its type |
| `GET /v1/assets/{id}/elements/property-index` | `Completed`, and what the extraction dropped or repaired |
| `GET /v1/assets/{id}/tree/children?nodeKey=` | Children of a node in the spatial tree |
| `GET /v1/assets/{id}/tree/parents?elementId=` | The path from the root to an element |

- Property keys are `Pset_WallCommon.FireRating` and `Qto_WallBaseQuantities.Length` for property and quantity sets, and `Type.Name`, `Type.Class`, `Type.GlobalId`, `Spatial.Storey`, `Element.Tag` and `Material[0].Name` for the rest. An element inherits its type's sets and its own value wins
- An `IfcBuildingStorey` carries `baseHeight` in `tree/children` and `elements/{elementId}`: metres above the WGS84 ellipsoid, the frame of `location.center.height`. It is the origin of the storey's `ObjectPlacement`, resolved through every `IfcLocalPlacement` it is relative to and converted from the project length unit, then placed on the ellipsoid the way the model's meshes are, through the map conversion for a model whose vertices are each moved through it. IFC makes `Elevation` informational and the placement authoritative, so neither `Elevation` nor the building's `ElevationOfRefHeight` moves it. A storey with no `ObjectPlacement` takes its `Elevation` above the origin of its building's placement, and `property-index` counts it under `storey_height_from_elevation`. A storey with no placement and no `Elevation` or no building counts under `storey_without_height`, one whose placement does not resolve counts under `unresolved_storey_placement`, one with no placement whose building's placement does not resolve counts under `unresolved_building_placement`, and none of these has a `baseHeight`. No other element has one
- `filter` is JSON with `$and`, `$or`, `$eq`, `$ne`, `$in`, `$like`, `$gt`, `$gte`, `$lt` and `$lte`, under Ion's limits of 100 clauses and 65536 characters. `@label`, `@classFullName`, `@id` and `@storey` filter on those fields. Any other operator or `@` field answers 400 naming it
- A query running longer than 10 seconds answers 422
- A numeric value stays in the unit the file wrote, and `units` on an aggregation lists every symbol it summed
- Every route needs `Authorization: Bearer <jwt>`, from the asset's owner or an admin. Another caller gets the same 404 as a missing asset. Any valid token reads an asset with no owner
- An asset with no store, which is anything but an IFC that finished tiling, answers 404 with code `ASSET_HAS_NO_ELEMENT_STORE`

CesiumJS 1.147's `IonAssetElementService` reaches these routes when it is built with a `Resource` carrying the header, because `fromAssetId` sends its token in the query string:

```js
const resource = new Cesium.Resource({
  url: `${origin}/v1/assets/${assetId}/`,
  headers: { Authorization: `Bearer ${jwt}` },
});
const elements = new Cesium.IonAssetElementService({ assetId, resource });
```

### Model coordination

A re-upload with `base_asset_id` on `POST /api/v1/assets`, or `options.baseAssetId` with `existingFileBehavior: KEEP` under `BIM_CAD` on `POST /v1/assets`, creates a new asset that is the model's next version, with its own tiles and element store.

| Route | Answers |
|---|---|
| `GET /v1/assets/{id}/tileset-revisions` | The model's versions, newest first |
| `POST /v1/assets/compare` | Queues a comparison of two versions and answers its `CHANGE_DETECTION` report |
| `GET /v1/assets/{id}/change-reports` | Reports naming this version |
| `GET /v1/change-reports/{id}` | A report's status and counts |
| `GET /v1/change-reports/{id}/changes?kind=` | Added, removed and changed elements, with changed property keys and geometry flags |
| `POST /v1/assets/{id}/bcf/import` | Stores the topics of a BCF 3.0 zip and reports unknown element ids by count |
| `GET /v1/assets/{id}/bcf/export` | The model's topics as a BCF 3.0 zip |
| `GET /v1/assets/{id}/bcf/topics` | Topic summaries |
| `GET`/`PUT`/`DELETE /v1/assets/{id}/bcf/topics/{guid}` | One topic |
| `GET /v1/assets/{id}/bcf/topics/{guid}/viewpoints/{viewpointGuid}/snapshot` | A viewpoint's snapshot |
| `GET /v1/assets/{id}/dxf` | ASCII DXF R2000 (`AC1015`) with `$INSUNITS` metres. `cutHeight` writes `3DFACE` clipped at or below that height. `axis` and `position` write the IFC section's closed rings as `LWPOLYLINE`, and are refused together with `cutHeight` |
| `GET /v1/assets/{id}/section?axis=&position=` | Each IFC element's cross-section on the plane where `axis` (`x`, `y` or `z`) equals `position` in metres |
| `POST /v1/assets/{id}/ids` | Checks the model's IFC against the `.ids` file in the multipart field `file` and answers 202 `{"status": "running"}` |
| `GET /v1/assets/{id}/ids` | The last check: `idsFileName`, `status` (`running`, `done` or `failed`), `checkedAt`, `error` and `report` |

- A model keeps its newest 5 `ready` versions and at most 20 GiB across them, checked when a tiling job on a version succeeds. The newest `ready` version always stays, and an unfinished version is left alone. Older versions and the reports comparing them are deleted, with an `asset.deleted` webhook
- Topics belong to the model and stay until its last version is deleted. A BCF zip is unpacked under the limits of an Ion zip upload. A document type declaration, a version other than 3.0 or a file name leaving its topic folder answers 400 and stores nothing. An import whose snapshots cannot be written answers 500 and stores no topic
- A new version takes the base asset's owner, and a version of an asset with no owner has none
- BCF documents, internal BIM snippets and viewpoint bitmaps are counted in the import's answer and not stored, so an export lacks them
- A geometry change is a hash of an element's triangles rounded to the millimetre, so a re-export reordering them reads as changed. A version whose element store was built before these hashes were stored answers 409 `ELEMENT_STORE_OUTDATED` to `compare`, and a re-tile rebuilds it
- A report's `percentComplete` is 0 before it starts and on `ERROR`, 100 at `COMPLETE`, and absent while it runs. A report whose status cannot be written goes to `ERROR` naming the failed write, so `compare` can replace it
- A DXF export reads the upload again, and its coordinates are metres in the tileset's local frame, the one `root.transform` maps to ECEF, z up. An IFC tiled with each vertex moved through its map conversion is exported with the same vertices. `cutHeight` clips the model's faces. `axis` and `position` are the same pair `GET /v1/assets/{id}/section` takes, and the file is one closed `LWPOLYLINE` per ring on layer 0, in the plane's two axes: z is XY, x is YZ, y is ZX. The first point is not repeated. A chain that does not close is left out. An empty section answers 400. A source other than a tiled IFC answers 404 `ASSET_HAS_NO_MESH_SOURCE` on that pair, the same answer as the section route. `cutHeight` with `axis` or `position` answers 400. `LWPOLYLINE` entities are left out of a mesh read, so a section export is not a mesh upload
- A section answers `{"axis", "position", "openChainsDropped", "polygons": [{"elementId", "rings"}]}`, with the element's GlobalId and rings of `[x, y, z]` points not closed by repeating the first. The first ring is the outer boundary and later rings are holes, found by containment. Coordinates are metres in the tileset's local frame, the one `root.transform` maps to ECEF, z up, so a model tiled with each vertex moved through its map conversion is cut in that frame too. A vertex within 0.1 mm of the plane counts as on it, and a plane level with a face cuts that face's outline, whichever side of it the solid is on. So a plane level with the bottom of a storey's walls cuts them, and so does one level with their top. Cut ends within 0.1 mm are welded into rings, and a chain that does not close, such as the cut of an open surface, is counted in `openChainsDropped` and left out. A missing or unknown `axis` or a `position` that is not a finite number answers 400. An asset that is not a tiled IFC answers 404 `ASSET_HAS_NO_MESH_SOURCE`. Each request reads the IFC's geometry again, the same read a tiling job starts with, so a section of a large model is slow
- An IDS check runs [IfcOpenShell's ifctester](https://docs.ifcopenshell.org/ifctester.html) in the `sidecars/ids-check` service, which `TILETOPIA_IDS_CHECK_URL` names. Unset, `POST /v1/assets/{id}/ids` answers 503 `IDS_CHECK_UNAVAILABLE` and stores no row. The sidecar reads the upload and the IDS file from this server's data directory, so it must mount the same directory at `/data`. One check runs in the sidecar at a time across all assets, and the others wait in the order they were sent, still `running`
- An `.ifc` or `.ifczip` upload can include the IDS file in the multipart field `ids`. It is stored as `ids/specification.ids` with the original file name in `ids/file-name`, and the check starts when the tiling job sets the asset `ready`. A new version with no `ids` field is checked against the newest earlier version's stored file, copied onto the new asset before older version directories are removed. With `TILETOPIA_IDS_CHECK_URL` unset, that check is stored `failed` and its error names the variable
- The IDS file is up to 16 MiB and must end in `.ids`, and a larger body answers 400. An `ids` field longer than 16 MiB answers 400 before a job starts. The asset must be `ready` (409 `ASSET_NOT_READY`), its source must be an `.ifc` or `.ifczip` (409 `ASSET_NOT_IFC`) and still on disk (404 `ASSET_HAS_NO_IFC_SOURCE`). A `POST` while a check of the asset runs answers 409 `IDS_CHECK_RUNNING`, and a new check replaces the last one
- `report` is the output of ifctester's JSON reporter as the sidecar wrote it, present when `status` is `done`. `checkedAt` is when the check ended, null while it runs. A failed check has the sidecar's error in `error`, such as an IDS file that does not validate against the IDS 1.0 schema. A check running when the server stops reads `failed` at the next `serve`. An asset never checked answers 404 `IDS_NOT_CHECKED`. Deleting the asset deletes its IDS file and report
- Every route needs `Authorization: Bearer <jwt>` from the asset's owner or an admin, and another caller gets the same 404 as a missing asset. Any valid token reaches an asset with no owner

### Not implemented

These exist in the code or answer on a route, and do not do what their names say:

| Subsystem | State |
|-----------|-------|
| Implicit tiling | `tiletopia_core::implicit_tiling` has no caller. Tilesets are written with explicit children |
| Catalog add | `POST /api/v1/catalog/{id}/add` queues a job whose input is the dataset URL, and the job queue only reads local files, so the job fails. `POST /api/v1/assets/{id}/tile` on that asset answers 404 naming the missing input |
| Splat re-tile | `POST /api/v1/assets/{id}/tile` on a Gaussian splat capture queues a job without the upload's longitude and latitude, so the job fails naming them, as it does for a mesh |
| Splat CLI | `tiletopia tile` reads a Gaussian splat PLY as a point cloud and writes its centres as black points |
| DXF solids | A DXF upload with a known unit answers 201 whatever entities it holds. Its job fails on 3DSOLID, BODY or REGION, and skips 2D entities extruded by a thickness |
| Global terrain below level 7 | `/api/v1/terrain/global/` answers levels 0 to 6 with the geoid surface and no relief |

The `gui/` globe extrudes OpenStreetMap building footprints in the browser from Overpass. That path does not use the server.

---

## Quick Start

### Build

```bash
git clone https://github.com/GeoLang/tiletopia.git
cd tiletopia
cargo build --release                      # target/release/tiletopia
cargo build --release --bin tiletopia --features martin    # adds vector tilesets and /martin
cargo build --release --bin tiletopia --features jp2       # adds JPEG 2000 uploads, needs Grok
```

The `jp2` feature links [Grok](https://github.com/GrokImageCompression/grok) through pkg-config, and bindgen needs libclang. Build Grok at the `rev` in `Cargo.toml` with cmake and g++ 12 or clang 16, install it with `-DGRK_BUILD_CODEC=OFF`, set `PKG_CONFIG_PATH` to its `lib/pkgconfig`, and put its `lib` on `LD_LIBRARY_PATH` to run the server. The Docker image has it.

### Tile a point cloud

```bash
tiletopia tile --input scan.las --output ./tileset --max-error 1.0
```

`tile` reads point clouds only. Meshes, IFC and vector files tile through the server's job queue.

### Start the server

```bash
TILETOPIA_ION_BASE_URL=http://localhost:3000 TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) tiletopia serve --data-dir ./data --port 3000
```

- `TILETOPIA_MAGO_JAR`: path to the mago-3d-tiler jar that tiles vector files. The Docker image sets it. Outside the image, download `mago-3d-tiler-1.16.2.jar` from the Gaia3D releases, point this at it, and put a JDK 21 on `PATH`. Unset, vector jobs fail naming the variable. Meshes never need it.
- `TILETOPIA_CESIUM_DIR`: a CesiumJS `Build/Cesium` directory copied into every `offline_viewer` export. The Docker image sets it. Outside the image, unzip `Build/Cesium` from a [CesiumJS release](https://github.com/CesiumGS/cesium/releases), or run `pnpm --dir gui build` and use `gui/dist/cesium`.
- `TILETOPIA_NODEODM_URL`: base URL of the NodeODM server that reconstructs photo sets, see [Photo sets](#photo-sets). Unset, a zip upload answers 400 naming the variable. A value that is not a URL refuses startup.
- `TILETOPIA_IDS_CHECK_URL`: base URL of the `sidecars/ids-check` service that checks IFC models against IDS files, see [Model coordination](#model-coordination). Unset, `POST /v1/assets/{id}/ids` answers 503 naming the variable. A value that is not a URL refuses startup, and so does a missing or empty `TILETOPIA_IDS_CHECK_TOKEN`. The sidecar mounts this server's data directory read-only at `/data` and listens on port 8000, and `docker compose up` runs both.
- `TILETOPIA_IDS_CHECK_TOKEN`: the shared secret sent to the IDS check sidecar as `Authorization: Bearer <token>`, required when `TILETOPIA_IDS_CHECK_URL` is set. Set the sidecar's `IDS_CHECK_TOKEN` to the same value, such as the output of `openssl rand -hex 32`. The sidecar does not start without `IDS_CHECK_TOKEN` and answers 401 to a `POST /check` without the matching header, so other containers on its network cannot read IDS reports of the files under `/data`. Its `GET /health` needs no token. `docker-compose.yml` passes `TILETOPIA_IDS_CHECK_TOKEN` from the environment to both, with a fixed local default.
- `TILETOPIA_PHOTOGRAMMETRY_TIMEOUT_SECS`: how long one reconstruction may run before its task is cancelled, 86400 by default. Time the task spends queued on the node does not count. A value that is not a whole number above 0 refuses startup.
- `TILETOPIA_PMTILES_DIR`: directory of PMTiles archives to serve under `/martin`. Each `*.pmtiles` file directly in it, subdirectories excluded, is registered under its filename stem: `basemap.pmtiles` answers at `/martin/basemap/{z}/{x}/{y}`. Unset serves nothing. A directory that cannot be read refuses startup, and a single archive that fails to open is logged and skipped. Needs the `martin` feature. These routes need a JWT like the rest of the API, so a tile client has to send a token.
- `TILETOPIA_ION_BASE_URL`: required. The origin the Ion-compat, WMTS and TMS endpoints write into the URLs they return. Set it to the address clients reach this server at, such as `https://tiles.example.com`. `http://localhost:3000` only works for clients on the same machine. A value that is unset, is not http or https, or has a path, query or trailing slash refuses startup, and the error names the value to use. A path breaks S3 Signature V4 on the Ion upload route.
- `TILETOPIA_MAX_USERS`: the most accounts the users table may hold before `/api/v1/auth/signup` answers 403 with `{"error": ...}`. Existing accounts log in as before. Unset leaves signup open.
- `TILETOPIA_SIGNUPS_PER_HOUR`: signups accepted in the last hour across everyone, counted from account creation times. Past it signup answers 429. Unset is no limit.
- `TILETOPIA_SIGNUPS_PER_ADDRESS_PER_HOUR`: signups accepted in the last hour from one client address, the same address the login lockout uses. Checked before `TILETOPIA_SIGNUPS_PER_HOUR`, and past it signup answers 429. Unset is no limit. Behind a proxy set `TILETOPIA_TRUSTED_PROXY_HOPS`, or every signup counts against the proxy's address. The address is stored with the account and cleared by the first signup after it is an hour old.
- `TILETOPIA_LOGIN_LOCKOUT_FAILURES`: consecutive failed logins from one client address that lock an account for that address, 10 by default, `0` for no lockout. A locked address answers 429 to every login for that account, the right password included, until `TILETOPIA_LOGIN_LOCKOUT_MINUTES` pass, 15 by default. Logins from other addresses are not affected. The account locks for every address after 20 times that many failures, so a stranger has to fail from 20 addresses before the owner is refused. A successful login resets both counts for that address.
- `TILETOPIA_TRUSTED_PROXY_HOPS`: how many proxies in front of tiletopia append to `X-Forwarded-For`. The client address is the entry that many places from the right, the one the outermost trusted proxy appended. Unset or `0` uses the TCP peer. Behind CloudFront, the load balancer and the platform proxy it is 3.

The signup and lockout limits are counted from the users table, so a restart resets none of them. A value that is not a whole number refuses startup.

### Docker

Tagged releases publish `ghcr.io/geolang/tiletopia`, built with the `martin` feature and carrying tippecanoe, mago-3d-tiler and CesiumJS:

```bash
docker run -p 3000:3000 -e TILETOPIA_ION_BASE_URL=http://localhost:3000 -e TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) -v /path/to/data:/data ghcr.io/geolang/tiletopia
```

The same releases publish the IDS check sidecar as `ghcr.io/geolang/tiletopia-ids-check`, under the same tag.

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

### Imagery tiles

`POST /api/v1/assets` turns a georeferenced GeoTIFF, COG, JPEG, PNG or JPEG 2000 file into a tile layer. A `.jp2` or `.j2k` upload needs the `jp2` feature, and answers 400 without it.

- `GET /api/v1/assets/{id}/imagery/{z}/{x}/{y}.png` answers a 256 px web mercator tile. Ground the image does not cover is transparent. Zooms run to 22, and past the image's own detail the tiles enlarge it
- `GET /api/v1/assets/{id}/imagery/tilejson.json` answers TileJSON 3.0 with the bounds, `minzoom` 0, a `maxzoom` matching the image's pixel size, and a root-relative tile URL
- `GET /v1/assets/{id}/endpoint` answers `externalType: URL_TEMPLATE` for an imagery asset, which `IonImageryProvider.fromAssetId` loads with the `Authorization` header described under [Cesium Ion compatibility](#cesium-ion-compatibility). Set `TILETOPIA_ION_BASE_URL` to the address clients reach, since the URL in that answer is absolute
- `GET /api/v1/assets/{id}/imagery/wmts?SERVICE=WMTS&REQUEST=GetCapabilities` and `GET /api/v1/assets/{id}/imagery/wmts/1.0.0/WMTSCapabilities.xml` answer OGC WMTS 1.0.0 capabilities for QGIS, ArcGIS Pro and other WMTS clients. The one layer is named after the asset id, in the `GoogleMapsCompatible` tile matrix set from zoom 0 to the TileJSON `maxzoom`, with `TileMatrixSetLimits` around the image. `REQUEST=GetTile` on the same URL answers the XYZ tile, and the RESTful `ResourceURL` is the XYZ route itself
- `GET /api/v1/assets/{id}/imagery/tms/tilemapresource.xml` answers an OSGeo TMS 1.0 TileMap with the `global-mercator` profile, and `tms/{z}/{x}/{y}.png` answers the XYZ tile with the row counted from the south. The `tms/` URL is meant for Cesium's `TileMapServiceImageryProvider`, and no CesiumJS run has loaded it yet

The WMTS and TMS documents carry absolute URLs built from `TILETOPIA_ION_BASE_URL` too. A WMTS `GetTile` outside the limits answers 400 with an OWS `TileOutOfRange` exception report, and a TMS tile outside them answers 404 with a `TileMapServerError`. The XYZ route still answers a transparent tile there.

All of these are anonymous, like every other tile read. A tiling request on one of these assets still fails, since it asks for 3D Tiles.

A TIFF is placed by its GeoTIFF tags, and any CRS with an EPSG code that projicio knows is reprojected. A JPEG or PNG carries no georeferencing, so the upload is refused unless it has a `crs` field such as `EPSG:32632` and one of these:

- `bounds`: `west,south,east,north` in `crs` units
- `world_file`: the text of a `.jgw`, `.pgw` or `.tfw` file, north up and not rotated

The same fields place a TIFF with no usable tags, and they override tags it has. Every text field of `POST /api/v1/assets` holds 64 KiB of UTF-8 at most, and a longer one answers 400 naming it. On that route and `POST /api/v1/tilesets`, 4 MiB of a multipart body with no next field answers 400. That counts a preamble, field headers with no blank line, and a field name the route ignores.

A `.jp2` is placed by its GeoJP2 box, or else its GMLJP2 box, and the same fields override both. Grok reads boxes only up to the codestream, so boxes written after it count as absent, and so does a rotated grid. A `.j2k` has no boxes and always needs the fields. A file placed by none of these is refused.

Three bands or more are red, green and blue. One or two bands are grey. The next band is alpha when the TIFF's ExtraSamples tag or the JP2 channel definition box says so, and any other extra band is ignored. 8 bit samples are shown as they are, and other sample types stretch from the lowest to the highest value in the file's coarsest overview. `GDAL_NODATA` and NaN are transparent. A colour pixel is transparent only when every colour band is nodata, so a dark pixel with one band at a nodata of 0 stays visible.

A `.tif` that holds several bands becomes an `imagery` asset, and one that cannot be tiled is refused with the reason. A single band `.tif` is a `terrain` asset with a quantized-mesh job and no imagery tiles, unless the upload sends `asset_type=imagery`. The `asset_type` field, `imagery` or `terrain`, overrides the band count. `world_file` or `bounds` on a `.tif` that ends up `terrain` answers 400, since the terrain job ignores them. A JPEG, PNG or JPEG 2000 file is always `imagery`, and any other `asset_type` on one answers 400. A file that is not a TIFF at all stays a `terrain` asset whose job fails naming the reason.

A tiled little-endian COG with deflate or no compression, and with a level no wider than 4096 px, is read in place. Anything else is decoded once at upload into a deflate COG under `<data-dir>/{id}/imagery/`, in its own sample type. TIFFs decode one strip or tile at a time and PNGs one row at a time into a raw scratch file in the same directory, which is deleted once the COG is written, so memory stays flat as the image grows and the disk briefly holds about three times the decoded size. The limits are 65,535 pixels a side, 2 GiB of decoded samples, and 64 MiB for one TIFF strip or tile. JPEG 2000 decodes through Grok in windows of whole tile rows, each at most 64 MiB of 32 bit samples, under the same 65,535 pixel and 2 GiB limits. 8 bit unsigned JPEG 2000 samples stay 8 bit, and any other precision up to 16 bits is stored as 16 bit and stretched. Palettes, subsampled components, mixed sample types and the sYCC, eYCC, CMYK and CIE Lab colour spaces are refused. JPEG and interlaced PNG decode whole, so they are limited to 119,304,647 decoded bytes, about 6300x6300 RGB. The decode takes one of the two slots the SRTM terrain builds share, each a third of the 1 GiB task, and waits while both are in use. Past a limit the upload is refused, naming the `gdal_translate` command that makes a COG this server can read. Tiles render on demand from the COG's overviews, through a geoplumb graph with a 64 MiB chunk cache for each of the 8 most recently opened layers, under the same render slots as the analysis tiles.

A COG read in place has its internal mask applied. Any other TIFF with an internal transparency mask answers 400 naming the mask, because the decode path does not apply one. `gdal_translate -b 1 -b 2 -b 3 -b mask -colorinterp red,green,blue,alpha` turns it into an alpha band, which is served. An image that crosses the antimeridian is refused, so split it at 180 degrees first. An image around a pole is served as far as web mercator reaches, about 85 degrees.

### Analysis tiles

`/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders from the server's own DEM by default, the same stores the elevation routes read. Two variables switch it to Copernicus GLO-30:

- `TILETOPIA_ANALYSIS_DEM_BBOX`: `west,south,east,north` in degrees. Setting it switches the analysis tiles to Copernicus GLO-30 COGs, read over STAC as each tile needs them.
- `TILETOPIA_ANALYSIS_STAC_API`: STAC API root, `https://earth-search.aws.element84.com/v1` by default.

The bbox only anchors the raster grid on its most recent item. Tiles anywhere resolve through per-window STAC searches, cached in two-degree blocks. Each source keeps at most 256 searches and 8192 item references, and evicts the least recently used search past either limit. A tile that needs more than 32 uncached block searches fails, so below about zoom 6 the layer answers 500. A malformed bbox refuses startup, and a failed search answers 500 rather than serving other terrain.

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

The `/api/v1/terrain/` routes read DEM files under `<data-dir>/dem` first and fall back to SRTM tiles downloaded from `https://elevation-tiles-prod.s3.amazonaws.com/skadi`, which `TILETOPIA_SRTM_BASE_URL` overrides. A failed download answers 503 naming the tile. One request fetches only as many SRTM tiles as fit in a third of the 1 GiB task, which is four, and at most two such requests or imagery decodes build at once. A wider terrain tile renders flat from SRTM, as the low zooms always have.

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

A terrain asset in the Ion-compat layer resolves to a bundle too. Name the bundle directory after the asset id, and `GET /v1/assets/{id}/endpoint` answers with `/api/v1/terrain/bundles/{id}/`, which `CesiumTerrainProvider` takes as is. An asset with no bundle under `<data-dir>/terrain_bundles/{id}/` gets a 404 naming the directory and the asset's status.

`GET /api/v1/terrain/bundles` treats a directory named after a uuid as an asset's bundle, and lists it only to a caller who may view that asset. A uuid-named directory with no asset row is listed to nobody. Other directories are listed to everyone. Tiles and `layer.json` are served by name either way.

A DEM upload writes that bundle itself. `POST /api/v1/assets` with a tif, tiff, hgt, dt0, dt1 or dt2 file queues a job, and `POST /api/v1/assets/{id}/tile` rebuilds the bundle with the `crs` of the asset's latest job. When the job is done, the asset's endpoint answers as above.

- The tile tree runs from both root tiles down to the first level as dense as the DEM's coarser axis, at most level 23. Tiles are not gzipped.
- Ground the DEM does not cover is flat at height 0, so a tile straddling the DEM's edge has a step there. A void within 100 samples of a valid sample takes the mean of its nearest filled neighbours, before any reprojection, and the fill is logged with no message on the job. A void farther out is flat at height 0 too. Heights pass through unchanged, with no geoid correction.
- HGT takes its location from the file name (`N47E011.hgt`), so keep that name in the upload. DTED takes its location from the file header. A GeoTIFF takes it from its ModelTiepoint or ModelTransformation tags and its GeoKeys, and the upload's `crs` field (an EPSG code) names the CRS for a file with none. The file's own CRS wins over the field. Anything but EPSG:4326 is reprojected through projicio.
- A refused DEM fails its job with one of these reasons at the start of `error`: `dem_too_large`, `dem_too_small`, `dem_no_elevations`, `dem_not_georeferenced`, `dem_rotated`, `dem_no_crs`, `dem_crs_unsupported`, `dem_bounds_invalid`, `dem_crosses_antimeridian`. A file the reader cannot parse, such as an HGT with no tile name, fails with the reader's message instead.
- `dem_too_large` is 22,369,621 samples, a 4,729 by 4,729 grid. That keeps the job's peak at a third of the 1 GiB task, at 16 bytes per sample.
- Deleting the asset deletes its bundle.

### Global terrain

`/api/v1/terrain/global/` is quantized-mesh terrain for the whole Earth, generated on request from Copernicus DEM GLO-30 with no Ion account. A tile reads the one-degree Copernicus cells under it over HTTP `Range`: a header read per cell, then one read per run of adjacent internal tiles its window touches, from the coarsest overview still as fine as the tile's sample spacing. Heights are converted from EGM2008 to height above the WGS84 ellipsoid with a geoid grid, since CesiumJS places quantized-mesh heights on the ellipsoid.

- `TILETOPIA_GLOBAL_TERRAIN_DEM_URL`: root of the GLO-30 COGs, `https://copernicus-dem-30m.s3.amazonaws.com` for the AWS Open Data bucket. A cell is read from `<root>/<name>/<name>.tif`, with names like `Copernicus_DSM_COG_10_N46_00_E007_00_DEM`.
- `TILETOPIA_GLOBAL_TERRAIN_GEOID_URL`: a geoid undulation grid covering the globe, as a pixel-is-point COG, read whole into memory on first use. `https://cdn.proj.org/us_nga_egm96_15.tif` is EGM96 at 15 minutes, 1440x721 samples in 2.7 MB. A grid over 2,000,000 samples is refused, which rules out EGM2008 at 2.5 minutes.
- `TILETOPIA_GLOBAL_TERRAIN_CACHE_DIR`: where generated tiles are written, as `{z}/{x}/{y}.terrain`.
- `TILETOPIA_GLOBAL_TERRAIN_CACHE_MB`: size cap of that directory, 1024 by default. Past it the oldest tiles are deleted. A value that is not a whole number above 0 refuses startup.
- `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID`: an Ion asset id that resolves to this layer. New assets never take it, and startup is refused, naming the variable and the asset, when an asset already holds it. `Cesium.createWorldTerrainAsync()` asks for id 1 in the CesiumJS 1.147 source, so with `1` this layer answers that request once `Cesium.Ion.defaultServer` points at this server. `GET /v1/assets/{that id}/endpoint` is the one Ion asset read that takes no token. No CesiumJS run has loaded it yet.

With the DEM URL, the geoid URL or the cache directory unset, both routes answer 503 naming what is missing. A failed read answers 503 naming the file.

```javascript
const terrain = await Cesium.CesiumTerrainProvider.fromUrl(
    'http://localhost:3000/api/v1/terrain/global/'
)
```

- Levels 7 to 14 read the DEM. Level 14 is the first whose samples are finer than GLO-30's one arc-second, and CesiumJS upsamples past it. Levels 0 to 6 carry the geoid alone, sea level with no relief: a level 6 tile spans up to 16 cells and a level 0 tile 32,400.
- Ocean has no Copernicus cell, and the [AWS registry entry](https://registry.opendata.aws/copernicus-dem/) says to take its height as 0. A cell that answers 404 is read that way. Cells Copernicus has not released to the public answer 404 too, so they read as sea level, N40 E044 over Yerevan among them.
- Copernicus heights are on EGM2008 and the suggested grid is EGM96, so the difference between the two geoid models stays in the heights.
- One tile build reads at most nine cells, one after another. Counted from its buffers it peaks near 30 MB: the compressed internal tiles of one cell, about 10 MB at full resolution, and one decoded 1024-pixel tile, 16 MB. Fetched bytes are kept in memory up to 96 MiB, so neighbouring tiles share their reads, and the EGM96 grid takes 8 MB. A build takes one of the two slots the SRTM terrain builds share.
- Tiles carry no vertex normals, water mask or metadata extension.

GLO-30 is published under the [Copernicus DEM GLO-30 licence](https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM), free, full and open, which allows reproduction, distribution and adaptation. Article 6(b) requires this notice on adapted data, and layer.json `attribution` and the Ion endpoint carry it: "produced using Copernicus WorldDEM-30 © DLR e.V. 2010-2014 and © Airbus Defence and Space GmbH 2014-2018 provided under COPERNICUS by the European Union and ESA; all rights reserved". Article 6(c) asks a deployment that serves the data to state in its terms: "The organisations in charge of the Copernicus programme by law or by delegation do not incur any liability for any use of the Copernicus WorldDEM-30". The PROJ EGM96 grid is marked "Derived from work by NGA. Public Domain".

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
| Geocoding | [geokode](https://github.com/GeoLang/geokode) at same-origin `/api/geocode/forward`. `pnpm run dev` sends it to this server, which has no such route, so search finds nothing in dev |
| Terrain | this server's `/api/v1/terrain/`, or a flat ellipsoid when it does not answer |
| Photorealistic 3D | Google 3D Tiles, when `VITE_GOOGLE_3D_TILES_KEY` is set |

Log in from the sidebar with an account made through `POST /api/v1/auth/signup`. The dashboard keeps the token in `localStorage` and sends it as `Authorization: Bearer` on every call to this server, and as the websocket subprotocol for collaboration. Uploading needs the editor role, which `tiletopia set-role` grants.

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
| `POST` | `/api/v1/assets/{id}/tile` | Start a tiling job. A job's `input_path` is relative to the asset's directory, so a re-tile survives moving the data directory |
| `GET` | `/api/v1/assets/{id}/jobs` | The asset's tiling jobs, newest first |
| `GET` | `/api/v1/jobs/{id}` | One tiling job, used to poll the `job_id` from an upload |
| `GET` | `/api/v1/assets/{id}/tileset.json` | Tileset |
| `GET` | `/api/v1/assets/{id}/tiles/{path}` | One tile |
| `GET` | `/api/v1/assets/{id}/imagery/{z}/{x}/{y}.png` | One imagery tile |
| `GET` | `/api/v1/assets/{id}/imagery/tilejson.json` | An imagery asset's TileJSON |
| `GET` | `/api/v1/assets/{id}/imagery/wmts` | WMTS 1.0.0 KVP `GetCapabilities` and `GetTile` |
| `GET` | `/api/v1/assets/{id}/imagery/wmts/1.0.0/WMTSCapabilities.xml` | WMTS 1.0.0 RESTful capabilities |
| `GET` | `/api/v1/assets/{id}/imagery/tms/tilemapresource.xml` | TMS 1.0 TileMap |
| `GET` | `/api/v1/assets/{id}/imagery/tms/{z}/{x}/{y}.png` | One imagery tile, rows from the south |
| `GET` | `/api/v1/assets/{id}/czml` | A CZML asset's document, as uploaded |
| `GET` | `/api/v1/photogrammetry/presets` | Photo set presets, with the ODM options each one sets |
| `GET` | `/api/v1/tilesets` | Built vector tilesets |
| `POST` | `/api/v1/tilesets` | Upload a vector file and queue its build (multipart) |
| `GET` | `/api/v1/tilesets/{id}` | One tileset, with build status and the stderr tail on failure |
| `DELETE` | `/api/v1/tilesets/{id}` | Delete the archive, its row and its martin source |
| `GET` | `/martin/catalog` | Sources the caller may see: operator archives plus their own tilesets |
| `GET` | `/martin/{source}` | TileJSON for a PMTiles source |
| `GET` | `/martin/{source}/{z}/{x}/{y}` | Vector tile from a PMTiles source |
| `GET` | `/api/v1/terrain/layer.json` | Quantized-mesh layer metadata, generated from the DEM |
| `GET` | `/api/v1/terrain/{z}/{x}/{y}.terrain` | Quantized-mesh tile, generated from the DEM |
| `GET` | `/api/v1/terrain/global/layer.json` | Global terrain layer metadata |
| `GET` | `/api/v1/terrain/global/{z}/{x}/{y}.terrain` | Global quantized-mesh tile from Copernicus GLO-30 |
| `GET` | `/api/v1/terrain/bundles` | Prebuilt terrain bundles |
| `GET` | `/api/v1/terrain/bundles/{name}/layer.json` | A bundle's layer metadata |
| `GET` | `/api/v1/terrain/bundles/{name}/{z}/{x}/{y}.terrain` | A bundle's quantized-mesh tile |
| `GET` | `/api/v1/terrain/rgb/{z}/{x}/{y}.png` | Terrain-RGB tile for MapLibre |
| `WS` | `/api/v1/realtime/{room}` | Presence, cursors, chat and view sync |
| `GET` | `/api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` | Hillshade, slope or ndvi tile |
| `GET` | `/api/v1/analysis/export/{op}` | One analysis op over a bbox as a web mercator COG |
| `GET` | `/api/v1/audit` | Audit trail, newest first. Instance-admin only |
| `GET` | `/metrics` | Prometheus metrics |

Other routes that do real work on request input: STAC search proxies `TILETOPIA_STAC_API`, COG windows read `TILETOPIA_COG_SOURCES`, `/api/v1/static-map/` renders the DEM to PNG, JPEG, WebP, SVG or PDF, `POST /api/v1/geostatistics/interpolate` runs IDW or kriging over posted samples, `POST /api/v1/geoprocessing/run` runs buffer, simplify and boolean overlays, webhooks deliver signed events, the scheduler runs the jobs it stores, and API keys (`X-Api-Key`, admin-minted, hashed at rest) authenticate read routes. What answers fixed data instead is in [Not implemented](#not-implemented).

### Access

Tile data reads are anonymous, since a map library cannot send a header with them: `tileset.json`, `tiles/{path}`, `data/{path}`, `imagery/`, `czml`, everything under `/api/v1/terrain/` (generated quantized-mesh, prebuilt bundles and their listing, terrain-RGB) and the `/api/v1/analysis/xyz/` tiles. The rest of `/api/v1/analysis/` stays gated.

Also anonymous, which matters before this goes on the internet:

- `/api/v1/auth/signup` and `/api/v1/auth/login`
- `/api/v1/stories/share/{token}`, a story shared by its token
- `GET /v1/assets/{id}/endpoint` for the id in `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID`, and no other id
- `/metrics`

`/tiletopia_ion_uploads/...` takes no JWT. Its requests are signed with the credentials `POST /v1/assets` issued, and those credentials write only under their own asset's prefix.

Everything else needs `Authorization: Bearer <jwt>`, and writes need the editor or admin role.

The `role` claim must be exactly `admin`, `editor` or `viewer`. Any other value is refused, so a token from a service with its own role names gets no access here.

On top of the role tier:

| Route | Rule |
|---|---|
| `DELETE /api/v1/assets/{id}`, `POST /api/v1/assets/{id}/tile` | editor and owner of the asset, or admin |
| `POST /v1/assets/{id}/uploadComplete` | editor and owner of the asset, or admin. Anyone else gets the 404 a missing asset gets |
| `POST /api/v1/scheduler/jobs` with `retile_asset` | editor and owner of the asset, or admin, otherwise 404. The run checks again when it fires, against the creator's stored role, and fails if the check no longer passes |
| webhook deliveries | an event goes only to subscriptions whose creator may view the asset, by the asset's owner when the event is emitted and the creator's stored role. An event for an asset whose row is gone goes to admins only |
| `POST`/`DELETE /api/v1/assets/{id}/annotations` | editor and owner of the asset, or admin. Deletes are scoped to the asset in the path. These serve the `gui/` dashboard. Viewtopia keeps annotations in Agora's live documents |
| `POST /v1/assets/{id}/bcf/import`, `PUT`/`DELETE /v1/assets/{id}/bcf/topics/{guid}`, `POST /v1/assets/compare`, `POST /v1/assets/{id}/ids` | editor and owner of the asset, or admin. Another owner's asset answers 404, as on the reads |
| `GET /api/v1/assets`, `GET /v1/assets` | token required. Lists your own assets, admins see all |
| `GET /api/v1/assets/{id}`, `/jobs`, `/annotations`, `GET /v1/assets/{id}`, `/endpoint`, `GET /api/v1/jobs/{id}` | owner of the asset, or admin. Anyone else gets the 404 a missing asset gets |
| `POST /api/v1/exports` | editor and owner of the asset, or admin |
| `GET /api/v1/terrain/bundles` | no token lists the bundles not named after an asset id. A token adds the bundles of assets you may view, and a bad token answers 401 |
| `POST /api/v1/tilesets` | editor or admin |
| `GET /api/v1/tilesets/{id}` | owner of the tileset, or admin. Anyone else gets the 404 a missing tileset gets |
| `DELETE /api/v1/tilesets/{id}`, `GET /api/v1/tilesets` | owner of the tileset, or admin |
| `POST`/`PUT`/`DELETE /api/v1/plugins/registry/...` | admin, because a plugin runs server-wide |
| `POST /api/v1/stories` | editor or admin, and the story records the caller as its author |
| `PUT`/`DELETE /api/v1/stories/{id}` | editor and author of the story, or admin |
| `GET /api/v1/stories`, `GET /api/v1/stories/{id}` | a public story with any valid token, a private one for its author or an admin. Anyone else gets a 404 |

Assets created before ownership existed have no owner, and only admins can read or modify them. To give one an owner, set `owner_id` on its row in SQLite. Hiding an asset from the list does not hide its tiles, since tile URLs are public, and `imagery/tilejson.json`, WMTS capabilities and the TMS TileMap state the asset's name to anyone who has its uuid. The `/martin` routes need a token but not ownership, so any signed-in caller who knows a source id can read it. `/martin/catalog` lists only the archives from `TILETOPIA_PMTILES_DIR` plus the caller's own tilesets, so nobody can list another owner's source ids. Admins see every source.

The realtime websocket needs any valid JWT. Browsers cannot set the Authorization header on a websocket handshake, so the token goes in as a subprotocol:

```js
new WebSocket(`ws://host/api/v1/realtime/${room}`, ["bearer", jwt])
```

That sends `Sec-WebSocket-Protocol: bearer, <jwt>`, marker first. The 101 response echoes `Sec-WebSocket-Protocol: bearer`, never the token. Non-browser clients can send `Authorization: Bearer <jwt>` instead. Query strings are never credentials.

The server sets `user_id` on every collaboration message to the sender's JWT `sub` before rebroadcasting it. `user_name` stays client-chosen.

---

## TileTopia vs Cesium Ion

| Capability | TileTopia | Cesium Ion |
|------------|-----------|------------|
| Point cloud, mesh and CAD inputs | LAS, LAZ, E57, PLY, glTF, GLB, OBJ, FBX, DAE, DXF, CityGML, IFC | Includes LAS, LAZ, glTF, FBX, DAE, OBJ, CityGML, IFC, RVT, DWG, DXF, DGN |
| Vector inputs | GeoJSON, TopoJSON, GeoPackage, KML, through mago-3d-tiler. CZML hosted as uploaded | Includes GeoJSON, TopoJSON, KML, CZML. KML, CZML and GeoJSON can be hosted as uploaded |
| Gaussian splats | 3DGS PLY upload, tiled to SPZ glTF with coarser levels | PLY upload through the REST API, and splats reconstructed from photos as a tech preview, tiled to SPZ glTF, with LODs for reconstructed ones |
| Outputs | 3D Tiles 1.1, quantized-mesh, terrain-RGB, imagery PNG tiles as XYZ, TMS and WMTS, DEM analysis PNG tiles, PMTiles vector tiles with the `martin` feature | 3D Tiles 1.0 and 1.1, TMS and WMTS, quantized-mesh, GLB |
| Tile compression | meshopt or Draco on mesh geometry, KTX2 on mesh textures | Includes Draco, meshopt, KTX2 |
| Accounts | Organizations, with admin, editor and viewer roles | Teams |
| Stories | Stored on the server, shared by token, played in the dashboard. The dashboard has no story editor | Stories |
| MCP server | None | Hosted, since 2026-10-12 |
| Self-hosting | AGPL-3.0 binary or Docker image | Cesium ion Self-Hosted, for commercial buyers, on Kubernetes |
| Presence, cursors, chat | WebSocket relay | Not found in Ion's docs |
| Global base terrain | Copernicus GLO-30 quantized-mesh generated on request, levels 0 to 14, sea level below level 7, no normals or water mask | Cesium World Terrain, the asset `createWorldTerrainAsync` loads |
| Global buildings | None | Cesium OSM Buildings, the asset `createOsmBuildingsAsync` loads |

TileTopia is one AGPL binary you run. The Cesium Ion column was read on 2026-11-04 from [tiler data formats](https://cesium.com/learn/3d-tiling/tiler-data-formats/), [pricing](https://cesium.com/platform/cesium-ion/pricing/) and the [Cesium blog](https://cesium.com/blog/). The Gaussian splats row was read on 2026-11-05 from [3D reconstruction](https://cesium.com/learn/reality-data/reconstruction/), the [forum answer on PLY uploads](https://community.cesium.com/t/converting-3dgs-ply-to-3d-tiles-via-cesium-ion-rest-api/42003/2) and the tiles in CesiumJS's own `Specs/Data/Cesium3DTiles/GaussianSplats`, which ion wrote.

---

## Tests

```bash
cargo test
cd gui && pnpm run test:all   # vitest unit tests, then Playwright e2e
```

The IDS check sidecar has its own tests: run `uv sync --locked` in `sidecars/ids-check`, then `uv run pytest` there.

Feature-gated tests need their feature, for example `cargo test -p tiletopia-server --features martin`. The JPEG 2000 tests write their fixtures through Grok, so `--features jp2` needs Grok installed as in [Build](#build).

`scripts/validate-tiles.sh none` uploads a point cloud, a textured GLB and an IFC to a local server, then runs [3d-tiles-validator](https://github.com/CesiumGS/3d-tiles-validator) on each tileset and exits non-zero on any error. With `none` it also downloads the IFC's `/v1/assets/{id}/dxf` export and runs `scripts/check_dxf.py` on it. It needs cargo, node 22, pnpm, curl, jq, openssl and uv. To check another compression value, add it to the `validate-tiles` matrix in `.github/workflows/ci.yml`: the script sends it as the upload's `mesh_compression` field and, for any value except `none`, requires a glTF compression extension in every GLB tile.

`uv run --with ezdxf==1.4.5 scripts/check_dxf.py <drawing.dxf>` loads a DXF, such as a `/v1/assets/{id}/dxf` export, with ezdxf's `recover.readfile`, runs `audit()` on it, and exits non-zero on any error and on any flaw repaired during the load or the audit.

---

## License

AGPL-3.0-or-later, see [LICENSE](LICENSE).

Copyright (C) 2026 Grok Image Compression Inc.
