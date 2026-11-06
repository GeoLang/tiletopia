# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- 2026-11-06: a mesh tileset's parent tile carries the error of its own content:
  the largest half diagonal among the parts it leaves out, or the simplification
  limit of a part it keeps, and never less than a child's. It was half the
  diagonal of the tile's box, so Cesium refined every branch to its leaves. The
  first view of the 262 MB `balconies-a2` IFC fetches 78 MB in 44 requests
  instead of 622 MB in 452, holds 188 MB of geometry instead of 984 MB, and
  draws at 378 fps instead of 76 on an RTX 2080 Ti. Tilesets written before this
  need a re-tile.
- 2026-11-05: `GET /v1/assets/{id}/section` and `GET /v1/assets/{id}/dxf` read
  an IFC in the tileset's frame for every model. A map-converted IFC that is
  tiled with each vertex moved into the east-north-up frame was cut and
  exported in its IFC coordinates, so a section 20 km from the map origin
  missed the tiled element. Tiling, section and DXF export call one function
  for that move.
- 2026-11-05: a storey's `baseHeight` on a model tiled with each vertex moved
  goes through the same map conversion as its meshes. It was too high by about
  d²/2R, 31 m at 20 km from the map origin. Stores built before this need a
  re-tile.
- 2026-11-05: a storey with no placement whose building placement does not
  resolve counts under `unresolved_building_placement` instead of
  `unresolved_storey_placement`.
- 2026-11-05: a section plane level with a solid's bottom face cuts that face's
  outline, as one level with its top face did. Vertices within 0.1 mm of the
  plane count as on it, and an edge in the plane is cut once.
- 2026-11-05: IDS checks run one at a time in the sidecar across all assets,
  and the rest wait in order. Each held a sidecar thread loading a whole model.
- 2026-11-05: the IDS check sidecar answers 401 to a `POST /check` without
  `Authorization: Bearer` matching its `IDS_CHECK_TOKEN`, and does not start
  without one. tiletopia sends `TILETOPIA_IDS_CHECK_TOKEN`, which startup
  requires when `TILETOPIA_IDS_CHECK_URL` is set.
- 2026-11-05: `scripts/validate-tiles.sh none` runs `scripts/check_dxf.py` with
  ezdxf 1.4.5 on the DXF export of a tiled IFC, so CI checks the export with a
  reader other than tiletopia's.

- 2026-11-05: CI runs the IDS check sidecar's tests with uv and builds its
  Docker image. Tagged releases publish it as
  `ghcr.io/geolang/tiletopia-ids-check` under the same tag as
  `ghcr.io/geolang/tiletopia`.

- 2026-11-05: a storey's `baseHeight` is the height of its `ObjectPlacement`,
  resolved through the building and site placements in the project length
  unit, instead of its `Elevation` above the model's z of 0. `Elevation` and
  `ElevationOfRefHeight` no longer move it. A storey with no placement takes
  its `Elevation` above its building's placement, and the store counts it
  under `storey_height_from_elevation`. `storey_without_height` and
  `unresolved_storey_placement` count the storeys left without one. Stores
  built before this need a re-tile.

- 2026-11-05: `GET /v1/assets/{id}/dxf` writes DXF R2000 (`AC1015`) instead of
  R12, whose readers ignore `$INSUNITS`, and its coordinates are declared metres.
  The file carries `$HANDSEED`, the CLASSES, TABLES, BLOCKS and OBJECTS sections
  R2000 needs, and a handle, an owner and the `AcDbEntity` and `AcDbFace`
  subclass markers on every `3DFACE`. `scripts/check_dxf.py` checks a DXF with
  ezdxf's recover and audit.
- 2026-11-05: meshopt and Draco IFC tiles carry CAD edges too. The merged
  primitive's indices use the meshopt `INDICES` codec instead of `TRIANGLES`,
  and Draco sequential connectivity instead of EdgeBreaker, since both of those
  move triangle corners. Instanced geometry still carries none.

- 2026-11-05: IFC tiles written with `mesh_compression` `none` carry CAD edges
  through `EXT_mesh_primitive_edge_visibility` on the merged primitive, beside
  `EXT_mesh_features`. Meshopt and Draco tiles and instanced geometry carry
  none. The IFC reader maps the file with `memmap2` instead of reading it into
  a string.

- 2026-11-05: a mesh tile writes geometry that repeats within it once, through
  `EXT_mesh_gpu_instancing` with per-instance ids in `EXT_instance_features`, when
  that saves at least 4 KB of the tile. Copies are found by comparing every
  reader's meshes after the fact: same vertex order and triangles, placed by a
  rigid translation and rotation to within 1 mm, with matching normals. Mirrored
  copies, scaled copies and meshes with texcoords are written as before.
- 2026-11-05: the mesh tiler assigns whole source meshes to child tiles by their
  centre. Only a mesh with more triangles than `max_triangles_per_tile` is split
  by triangle. Parent tiles simplify each source mesh alone, with an error limit
  of 1% of that mesh's longest side, and hold the largest meshes until they reach
  `max_triangles_per_tile`, which used to apply to leaves only.
- 2026-11-05: each mesh tile's property table lists only the source meshes with
  vertices in that tile, with `_FEATURE_ID_0` and `featureCount` numbered per
  tile, instead of repeating the whole material group's table. An element's
  fragments in several tiles each carry its `asset_id`. A tile where no mesh has
  a column value carries no table and no feature ids, because glTF refuses the
  zero-length buffer view such a column needs.
- 2026-11-05: `MeshData` carries optional `class` and `storey` strings, written
  as property table columns only in tiles where a mesh has them. `name` stays
  out of tiles.
- 2026-11-05: mesh positions are `f64` through both `MeshData` types, and each
  GLB holds `f32` offsets from the centre of its tile under a glTF node
  `translation`. Simplification runs on `f32` offsets from the material group's
  centre. Tileset bounding volumes and `root_transform` are unchanged. OBJ, FBX,
  DXF, CityGML and CityJSON coordinates stay `f64` from the file, and glTF and
  COLLADA stay `f32` at the source because their parsers hold `f32`. tobj now
  builds with `use_f64`.

### Added

- 2026-11-05: `GET /v1/assets/{id}/section?axis=&position=` answers each IFC
  element's cross-section on the plane where `x`, `y` or `z` equals `position`,
  as polygons of rings in the tileset's local frame, outer ring first and holes
  after it. Cut segments are joined into rings by welding ends within 0.1 mm,
  and chains that do not close are counted in `openChainsDropped`. Each request
  reads the IFC again.
- 2026-11-05: `POST /v1/assets/{id}/ids` checks a ready IFC model against an
  IDS file in the background, and `GET /v1/assets/{id}/ids` answers the last
  check with ifctester's JSON report. The check runs in the new
  `sidecars/ids-check` Python service, which `TILETOPIA_IDS_CHECK_URL` names.
- 2026-11-05: storeys carry `baseHeight` in `tree/children` and
  `elements/{elementId}`, their IFC `Elevation` in metres above the ellipsoid,
  placed like `location.center.height`. A storey with no `Elevation` has none.
  The store gains a `base_height` column, so a store built before this needs a
  re-tile.
- 2026-11-05: ifc-lite moves from 2.4.2 to 28.2, whose CSG kernel is pure Rust,
  and IFC tiling follows it. Openings cut their walls, and `IfcOpeningElement`,
  `IfcSpace` and `IfcSpatialZone` are no longer tiled as solids. A finished job
  reports the IFC elements that lost geometry in `error`, with a count and the
  first five reasons. `.ifczip` uploads unpack to their single `.ifc`. An IFC
  with no upload coordinates is placed by `IfcMapConversion` or
  `ePSet_MapConversion` through projicio, converting map units to the CRS's unit,
  and then by `IfcSite`. A CRS of unknown unit fails the job naming it. An
  `IfcMapConversionScaled` factor off by more than 0.1 percent resizes the model
  however it is placed, and a negative factor or a `Scale` that does not convert
  the project unit to the map unit fails the job naming it. `RefElevation`
  now uses the project length unit, so a millimetre project is no longer placed
  1000 times too high. The root transform carries the map conversion's rotation
  plus the grid convergence, or `TrueNorth`. An IFC is read once per job, and a
  file that is not UTF-8 reads.
- 2026-11-05: a tiling job on an `.ifc` or `.ifczip` writes each element's class, name, type
  object, storey, spatial parent, property sets, quantities with units, materials,
  `Tag` and location to `<asset_dir>/elements.sqlite`, with an FTS5 index. Ion's
  element routes serve it under `/v1/assets/{id}/elements` and `/tree`: list with
  filter, get, search, aggregate with an added `groupBy`, properties and
  property-index. They need a JWT from the asset's owner or an admin, where the
  other `GET /v1/assets/...` reads stay open. Extraction fails past budgets on
  property rows, object and property set pairs, text bytes and property visits,
  and after 10 minutes, and it cuts stored text at 64 KiB and lists at 10000
  entries with a warning. IFC meshes count against the job's memory budget, a
  WKT past 64 KiB or nested past 32 levels is refused before it is parsed, and
  tiling jobs run on ifc-lite's 256 MiB stack thread pool.
- 2026-11-05: `.jp2` and `.j2k` uploads serve imagery tiles through the new `jp2`
  cargo feature, off by default, which binds Grok through `grokj2k-sys`. The
  Docker image turns it on. Without the feature the upload answers 400 naming it.
  A `.jp2` is placed by its GeoJP2 box or else its GMLJP2 box, and `world_file`
  or `bounds` with `crs` override both and place a `.j2k`. The decode runs in
  windows of whole tile rows into the scratch file the other imagery uses.
- 2026-11-05: model coordination. A re-upload with `base_asset_id`, or Ion's
  `options.baseAssetId` with `existingFileBehavior: KEEP` under `BIM_CAD`, is a
  new asset and the next version of a model, listed by
  `GET /v1/assets/{id}/tileset-revisions`. A model keeps its newest 5 ready
  versions and at most 20 GiB. `POST /v1/assets/compare` queues a comparison of
  two versions' element stores into a stored `CHANGE_DETECTION` report of added,
  removed and changed elements with property keys and geometry hashes, read at
  `/v1/change-reports/{id}`. The element store gains `mesh_hash`, so a store
  built before this has no hashes and is rebuilt by a re-tile.
  `POST /v1/assets/{id}/bcf/import` and `GET .../bcf/export` round-trip BCF 3.0
  topics, comments and viewpoints with camera, selection, visibility, colouring,
  lines and clipping through a parser on `quick-xml`, and report element ids the
  store lacks by count. Documents, internal BIM snippets and bitmaps are counted
  and dropped. `GET /v1/assets/{id}/dxf` writes a model's meshes as `3DFACE`
  entities in ASCII DXF, clipped below `cutHeight` when it is sent.
- 2026-11-05: Ion's `uploadComplete` takes one zip or several files under the
  asset's prefix, so an OBJ with materials and textures or a `.gltf` with
  external buffers tiles. The main file is the one file whose extension the
  source type takes, and none or several answer 400 naming them. A zip may
  unpack to 100 times its size, at most 4 GiB, and hold 10,000 entries, and
  symbolic links, absolute paths and `..` steps are refused. A file named
  `zip` with no dot is now refused at upload instead of being treated as a
  photo set.
- 2026-11-05: imagery assets serve OGC WMTS 1.0.0 at
  `/api/v1/assets/{id}/imagery/wmts`, KVP `GetCapabilities` and `GetTile`, and
  at `/imagery/wmts/1.0.0/WMTSCapabilities.xml`, in the `GoogleMapsCompatible`
  tile matrix set with `TileMatrixSetLimits` around the image. They also serve
  an OSGeo TMS 1.0 TileMap at `/imagery/tms/tilemapresource.xml` with tiles at
  `/imagery/tms/{z}/{x}/{y}.png`, rows counted from the south. Both read the
  same PNGs as the XYZ route, anonymously, and their absolute URLs come from
  `TILETOPIA_ION_BASE_URL`.
- 2026-11-05: a PLY upload with 3DGS splat properties (`f_dc_*`, `opacity`,
  `scale_*`, `rot_*`, `f_rest_*`) tiles to GLBs with `KHR_gaussian_splatting`
  and `KHR_gaussian_splatting_compression_spz_2`, which CesiumJS 1.147 draws as
  splats. It is placed by `longitude` and `latitude` like a mesh, keeps
  spherical harmonics up to degree 3, and gets coarser tiles holding the 32,768
  splats with the largest opacity times area. Tile boxes hold each splat out to
  3 times its largest scale. The job fails naming a missing
  property, and refuses more than 4,805,821 splats at degree 3 or 12,979,364 at
  degree 0. Other PLY uploads still tile as point clouds.
- 2026-11-05: ASCII DXF uploads tile through the native mesh tiler: 3DFACE,
  polyface and polygon mesh POLYLINE, MESH, and INSERT blocks with their
  transforms, arrays and nesting, scaled by `$INSUNITS`. 2D entities are skipped
  with a warning. A drawing with nothing 3D, an ACIS solid (3DSOLID, BODY,
  REGION) fails the job naming it. A drawing fails past 5,592,405 stored and
  placed vertices.
- 2026-11-05: DXF uploads take a `units` field naming the unit of a drawing whose
  `$INSUNITS` is missing or 0. Such a drawing without the field, a field that
  disagrees with a nonzero `$INSUNITS`, an unknown unit name and binary DXF answer
  400 at upload. The job stores the field and a scheduled re-tile reuses it.
- 2026-11-05: TopoJSON uploads are decoded to GeoJSON and tiled by mago-3d-tiler,
  up to 29,826,161 bytes.
- 2026-11-05: CZML uploads are checked for a leading document packet with a
  version 1 `version`, stored, and served as uploaded at
  `GET /api/v1/assets/{id}/czml` with no token. `GET /v1/assets/{id}/endpoint`
  answers type `CZML` with that url. Anything else with a `.czml` name answers
  400.
- 2026-11-05: global terrain at `/api/v1/terrain/global/`, generated on demand
  from Copernicus DEM GLO-30 COGs and a geoid grid, both read over HTTP `Range`,
  with heights above the WGS84 ellipsoid. Levels 7 to 14 read the DEM, levels 0
  to 6 carry the geoid alone. Generated tiles go to a disk cache, 1 GiB by
  default. `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID` makes
  `GET /v1/assets/{id}/endpoint` answer with this layer and the Copernicus
  notice, so `CesiumTerrainProvider.fromIonAssetId` loads it. Configured with
  `TILETOPIA_GLOBAL_TERRAIN_DEM_URL`, `TILETOPIA_GLOBAL_TERRAIN_GEOID_URL`,
  `TILETOPIA_GLOBAL_TERRAIN_CACHE_DIR` and `TILETOPIA_GLOBAL_TERRAIN_CACHE_MB`.
- 2026-11-05: a `.zip` of geotagged JPEG photos uploaded to `POST /api/v1/assets`
  is reconstructed by OpenDroneMap through the NodeODM server named by
  `TILETOPIA_NODEODM_URL`, and its textured mesh goes through the native mesh
  tiler, placed by ODM's `coords.txt`. Without the variable the upload answers 400
  naming it. New upload field `photogrammetry_preset`, new variable
  `TILETOPIA_PHOTOGRAMMETRY_TIMEOUT_SECS`, and new dependency `kamadak-exif`.
  `GET /api/v1/photogrammetry/presets` lists `fast`, `default` and `high_quality`
  with their ODM options, where it used to answer four fixed names.
- 2026-11-05: Cesium ion's upload flow. `POST /v1/assets` needs `type` and
  `options.sourceType`, answers 200 where it answered 201, and returns an S3
  location on this server with credentials that last 12 hours. `POST
  /v1/assets/{id}/uploadComplete` queues the tiling job. Status now tells
  `AWAITING_FILES` from `NOT_STARTED`. Types and options it cannot apply answer
  400, which replaces the 501 for `IMAGERY` and the stored `uploading` row for
  other types. An upload not completed in 12 hours goes to `error` and its files
  are deleted. At startup, an asset still `uploading` with no job and no upload
  credentials goes to `error`. Each asset takes one source file.
- 2026-11-04: mesh uploads take `mesh_compression`, `meshopt` by default, or
  `draco` or `none`. The job stores it and a scheduled re-tile reuses it.
  `meshopt` writes `EXT_meshopt_compression`. `draco` writes
  `KHR_draco_mesh_compression` through draco-gltf with 14 position, 10 normal
  and 12 UV bits.
- 2026-11-04: mesh uploads take `texture_compression`, `none` by default, or
  `ktx2`, which writes KTX2 textures with ETC1S and BasisLZ through
  `KHR_texture_basisu`. Builds now need libclang for basisu_c_sys.
- 2026-11-04: georeferenced GeoTIFF, COG, JPEG and PNG uploads serve web mercator
  PNG tiles at `GET /api/v1/assets/{id}/imagery/{z}/{x}/{y}.png` and TileJSON at
  `/imagery/tilejson.json`. `GET /v1/assets/{id}/endpoint` answers `URL_TEMPLATE`
  for them. New upload fields: `world_file`, `bounds` and `asset_type`. A file
  that is not served as is decodes up to 14,913,080 samples, 1930x1930 pixels of
  four bands, in one of the two slots the SRTM terrain builds share.
- 2026-11-04: DEM uploads (tif, tiff, hgt, dt0, dt1, dt2) queue a job that writes
  a quantized-mesh bundle under `<data-dir>/terrain_bundles/<asset id>/`.
  GeoTIFFs not in EPSG:4326 are reprojected through projicio. Refused DEMs fail
  the job with a named reason, and the cap is 22,369,621 samples.
- 2026-11-04: the native mesh tiler takes COLLADA 1.4.1 DAE uploads, with scene
  node transforms, `up_axis`, `unit`, UVs, diffuse colours and textures. A scene
  fails when its texture copies plus 64 bytes a vertex add up past a third of
  the 1 GiB task.
- 2026-11-04: CI runs CesiumGS 3d-tiles-validator on a tiled point cloud, a
  textured GLB and an IFC, once per mesh compression value.

### Changed

- 2026-11-05: breaking: asset reads need a token and pass the owner check.
  `GET /v1/assets`, `/v1/assets/{id}` and `/v1/assets/{id}/endpoint` answer 401
  with no token, and the list holds only the caller's assets. These two and
  `GET /api/v1/assets/{id}`, `/jobs`, `/annotations` and `GET /api/v1/jobs/{id}`
  answer another owner the 404 of a missing asset, and `POST /api/v1/exports`
  does the same for the asset it names. `GET /api/v1/stories` and
  `/stories/{id}` keep private stories to their author and admins. An asset with
  no owner is now readable by admins alone. The endpoint of the id in
  `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID` stays open, since
  `createWorldTerrainAsync` cannot send a header, and its 409 no longer names the
  asset holding the id. `GET /api/v1/terrain/bundles` lists a bundle named after
  an asset id only to a caller who may view that asset, and tiles stay anonymous.
- 2026-11-05: breaking: an asset with no owner can be modified by admins alone.
  Webhook events go only to subscriptions whose creator may view the asset. A
  `retile_asset` scheduled job needs a creator who may modify the asset, when it
  is created (404 otherwise) and each time it fires (the run fails otherwise).
  `POST /v1/assets/{id}/uploadComplete` and `GET /api/v1/tilesets/{id}` answer
  another owner the 404 of a missing id instead of 403.
- 2026-11-05: the `TILETOPIA_AUTH_DISABLED=true` startup warning says routes that
  check a role or an owner still need a token.
- 2026-11-05: breaking: `tiletopia serve` refuses to start unless
  `TILETOPIA_ION_BASE_URL` is an http or https origin with no path, query or
  trailing slash. The `http://localhost:3000` default is gone, so every
  deployment has to set the variable.
- 2026-11-05: imagery uploads that are not served as is decode in a fixed amount
  of memory however large the image, so the 1930x1930 cap is gone. TIFFs decode
  one strip or tile at a time and PNGs one row at a time into a raw file beside
  the asset, overviews are built from that file two rows at a time, and the COG
  is written one tile at a time through terrano's `write_cog_tiles`. Samples stay
  in their own type instead of f64. The limits are 65,535 pixels a side, 2 GiB
  of decoded samples, and 64 MiB for one TIFF strip or tile. JPEG and interlaced
  PNG still decode whole and are limited to a third of the build budget,
  119,304,647 bytes decoded, about 6300x6300 RGB. Overviews of integer images
  now leave nodata out of the average. Images over 16,384 px a side get more
  than five overviews, down to 512 px. Needs terrano with `write_cog_tiles`.
- 2026-11-04: a JPEG or PNG upload with no `crs` and no `world_file` or `bounds`
  answers 400 naming the reason, where it used to be stored, and so does one with
  an `asset_type` other than `imagery`. A `.tif` with several bands becomes an
  `imagery` asset and answers 400 if it cannot be tiled, unless the upload sets
  `asset_type=terrain`.
- 2026-11-04: a single band `.tif` stays a `terrain` asset with a quantized-mesh
  job and no imagery tiles, unless the upload names `asset_type=imagery`. With
  `world_file` or `bounds` and no `asset_type=imagery` it answers 400.
- 2026-11-04: a jp2 upload answers 400, where it used to be stored as `uploading`
  with no job.
- 2026-11-04: `_FEATURE_ID_0` is written as FLOAT because glTF forbids
  UNSIGNED_INT attributes, ids above 2^24 raise an error, and
  `EXT_structural_metadata` schemas have an id.
- 2026-11-04: geoprocessing simplify, point in polygon, boolean overlay and buffer
  run on `topoi-core`. Buffer coordinates change because arcs have 32 sides per
  circle.
- 2026-11-04: meshopt moved to 0.7.
- 2026-11-04: token checks and webhook signing come from `platform-auth` and
  `platform-webhook`. Webhooks to loopback, private and link-local addresses are
  refused unless `TILETOPIA_WEBHOOK_ALLOW_PRIVATE_HOSTS=true`, delivery uses no
  proxy, and the refusal texts for a bad URL changed. A platform token with an
  empty `sub`, `agora_use` or `geolang_use` is refused.
- 2026-10-28: COG reads share the reader without a lock (f254d5a).

### Fixed

- 2026-11-05: an IFC placed by a map conversion to a projected CRS, too wide for
  one flat east-north-up frame, has each vertex taken through the map CRS,
  projicio and ECEF into the east-north-up frame at the conversion origin. The
  flat frame had elements 20 km out about 31 m above their map position.
  Models whose flat frame is within half the placement tolerance at every
  bounding box corner tile as before.
- 2026-11-05: `percentComplete` on the Ion asset routes is the running job's
  progress instead of a constant 50.
- 2026-11-05: breaking: new assets never take the id in
  `TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID`, so id 1 keeps resolving to the global
  terrain after the first upload. Startup is refused, naming the variable and the
  asset, when an asset already holds the id. The endpoint's 409 is gone.
- 2026-11-05: a failed status write during a tiling job fails the job naming the
  write. A job that cannot record `done` records `failed` instead, and an asset
  that cannot record `ready` goes to `error`. A job whose failure cannot be written
  either is logged and reads as running until the next restart, with no webhook.
- 2026-11-05: deleting an asset stops its running reconstruction between stages
  and during the download of the result. A tiler still running at the delete runs
  to its end, then the job removes what it wrote and fails with `job.failed`
  instead of reporting `job.completed`.
- 2026-11-05: a void in an uploaded DEM takes the mean of its nearest valid
  samples, out to 100 samples, before any reprojection, and the fill is logged.
  It used to become height 0. Ground outside the DEM stays flat at 0.
- 2026-11-05: `tiletopia edge` prints the target triple and the `tiletopia` binary
  name. It used to print `target/{}/release/tiletopia-cli`.
- 2026-11-05: `POST /api/v1/assets/{id}/tile` on an asset with no input directory,
  such as a catalog asset, answers 404 naming the missing input. It used to answer
  202 and fail the job on the dataset URL.
- 2026-11-05: a job's `input_path` is stored relative to the asset's directory, and
  a re-tile rebases an older absolute path, so it survives moving the data directory.
- 2026-11-05: Ion's upload route takes `.ifczip` under `BIM_CAD`, `.jp2` under
  `RASTER_IMAGERY`, a `.dxf` that names its unit under `BIM_CAD`, `.topojson` under
  `VECTOR_DATA`, and `position` under `POINT_CLOUD` for a Gaussian splat PLY. A
  unitless DXF, a splat PLY without `position` and another point cloud with it
  answer 400 at `uploadComplete`.
- 2026-11-05: a `crs` field on a file the native mesh tiler takes answers 400. It
  used to be ignored.
- 2026-11-05: a TIFF with an internal transparency mask answers 400 naming the mask.
  It used to serve masked pixels as opaque data.
- 2026-11-05: a DXF export reads its source on ifc-lite's 256 MiB stack thread
  pool, as tiling does, so a deeply nested IFC no longer overflows the stack of
  a blocking thread and aborts the server.
- 2026-11-05: element search answers `total` as the number of matches, where it
  was the number of hits on the page.
- 2026-11-05: a BCF import copies its snapshots aside before it writes topics,
  and moves them into place inside the topic transaction, so a snapshot that
  cannot be stored answers 500 and leaves the stored topics and snapshots as
  they were.
- 2026-11-05: `POST /v1/assets/compare` refuses a version whose element store
  has no `mesh_hash` column with 409 `ELEMENT_STORE_OUTDATED` naming a re-tile,
  where the report used to fail later with SQLite's error text.
- 2026-11-05: a change report whose status write fails goes to `ERROR` naming
  the failed write, so `compare` replaces it. It used to stay `NOT_STARTED` or
  `IN_PROGRESS` and answer 409 for the pair until a restart. `percentComplete`
  is left out while a report is `IN_PROGRESS`, where it was always 50.
- 2026-11-05: a malformed Guid in the path of `PUT .../bcf/topics/{guid}`
  answers `INVALID_BCF`, and a version of an asset with no owner has no owner,
  as the coordination contract says.
- 2026-11-05: element extraction counts undecodable materials, layers, profiles
  and constituents, quantity set members that are not quantities and quantities
  that are not finite as `property-index` warnings.
- 2026-11-05: an IFC is charged each mesh's struct sizes in the reader and the
  mesh tiler on top of its bytes per vertex, so a model of many small elements
  is refused before it passes the job's memory budget.
- 2026-11-05: `POST /api/v1/assets/{id}/tile` keeps the last job's placement,
  units, mesh compression and texture compression. It and the scheduled
  `retile_asset` now both call `submit_retile_job`. A re-tile of a placed mesh
  used to fail for lack of coordinates.
- 2026-11-05: a photo set zip skips `__MACOSX` entries with the same check Ion
  zip uploads use, so a zip from macOS Archive Utility is no longer refused.
- 2026-11-05: Ion S3 uploads no longer fill the disk past the 4 GiB cap.
  `.incoming` files are removed when a request ends any way at all, including a
  disconnect, and by the first scan of an asset after a restart.
  `CompleteMultipartUpload` reserves the joined size and closes the upload before
  it joins, so a second complete or a late part answers `NoSuchUpload`, and the
  join and every rename run past a disconnect.
- 2026-11-05: one set of Ion upload credentials holds 20,000 files and parts and
  819 open multipart uploads, and one editor has 16 assets awaiting files. The
  restart scan runs outside the lock every upload takes, and `uploadComplete`
  names the first 20 files of a refusal and counts the rest.
- 2026-11-05: text fields of `POST /api/v1/assets` hold 64 KiB of UTF-8 at most and
  are counted as they arrive. They used to buffer up to the 4 GiB body limit. A
  field that is not UTF-8 is now refused instead of decoded by its charset.
- 2026-11-05: `POST /api/v1/assets` and `POST /api/v1/tilesets` refuse 4 MiB of
  multipart body with no next field. multer held a preamble or unterminated field
  headers in memory up to the 4 GiB body limit. An ignored field past 4 MiB is
  refused too, and a refused tileset upload removes its scratch directory.
- 2026-11-05: one set of Ion upload credentials stages at most `MAX_UPLOAD_BYTES`
  across its files and multipart parts, counted per frame while a request
  streams. A request past that answers `EntityTooLarge` and stores nothing. The
  limit used to apply per file only.
- 2026-11-05: `POST /v1/assets/{id}/bcf/import`, `PUT` and `DELETE
  /v1/assets/{id}/bcf/topics/{guid}` and `POST /v1/assets/compare` need the
  editor or admin role and pass `may_modify_asset`, the same as deleting or
  re-tiling an asset. A viewer-role token gets 403. Their reads stay on
  `may_view_asset`.
- 2026-11-05: a DEM sample beside a void keeps its value. `Heightmap::elevation_at` multiplied the void by a zero weight, which made the sample a void too.

- 2026-11-05: terrano's `CogReader` moves PixelIsPoint origins to the pixel
  corner, sizes untagged overviews from the full image and refuses masked COGs.
  `/api/v1/terrain/global/` takes the corner from it instead of moving the origin
  itself. Registered COG sources report PixelIsPoint bounds from the pixel corner
  and the real pixel size of GDAL overviews. Masked uploads still decode to a COG.
- 2026-11-05: a photo set reconstruction no longer blocks the job queue. Other
  jobs run while NodeODM works, tiling the result still takes its turn one job
  at a time, and `TILETOPIA_PHOTOGRAMMETRY_TIMEOUT_SECS` no longer counts time
  a task spends queued on the node.
- 2026-11-05: `serve` startup fails every job left `running` by an earlier
  process, with the restart in its `error`, and puts its asset in `error` the
  way a failed job does. These jobs used to stay `running` forever.
- 2026-11-05: a `TILETOPIA_JWT_SECRET` that is set but not valid UTF-8 refuses
  startup naming the variable, and the auth middleware answers 500 for it
  instead of letting every request through.
- 2026-11-05: `POST /v1/assets` with type `IMAGERY` answers 501 naming
  `POST /api/v1/assets`, where it used to store an asset as `uploading` that no
  upload or completion step could fill. `POST /api/v1/catalog/{id}/add` of an
  imagery dataset answers 501, where it used to queue a job that always failed.
- 2026-11-05: each mesh tile carries only the vertices its triangles use, with
  its indices renumbered. Tiles written with `mesh_compression` `none` or
  `draco` used to hold every vertex of their material in memory, and `none`
  tiles also wrote them all to the GLB.
- 2026-11-05: the mesh tiler writes each tile GLB, KTX2 texture included, as
  soon as it is built, so it holds the tiles of one root to leaf path at a
  time instead of the whole tree. LOD simplification works on the node's own
  vertices through meshopt's sparse mode, with its error limit still 1% of the
  material's longest side. `build_mesh_tree`, `write_mesh_tileset` and
  `MeshTileNode` are gone from `tiletopia_core::mesh_tiler`, and `tile_meshes`
  is the one entry point.
- 2026-11-05: glTF and GLB meshes are placed by their node transforms in the
  default scene, or the first scene when `scene` is unset, and a mesh used by two
  nodes is tiled twice. They used to sit at their own origin, once each. A
  mirroring node keeps triangles front-facing. A file with meshes but no scene,
  or a node with two parents or in a cycle, fails. Triangle strips and fans
  become triangle lists, non-indexed triangles tile, and line and point
  primitives are skipped with a warning. A strip or fan under three vertices, or
  a triangle list that is not whole triangles, fails.
- 2026-11-05: an FBX mesh under a mirroring node transform, or in a file whose
  axis settings are left-handed, keeps its triangles front-facing. A polygon
  naming a control point the mesh does not have fails the file naming the
  polygon, where it used to drop that corner and misalign every later triangle.
- 2026-11-04: an uploaded model could name any file the server can read. A `..`
  or absolute texture path in a DAE, FBX or OBJ pulled in any readable image, an
  OBJ `mtllib` read any file as materials, and a `.gltf` buffer or image URI read
  any file. Texture and `mtllib` paths now stay inside the mesh directory, and a
  `.gltf` naming a file outside it fails.
- 2026-11-04: DTED origin latitude parsing and signed magnitude elevations, which
  also changes the terrain bundle export of DTED assets. GeoTIFF reads the GDAL
  nodata tag. A non-ASCII HGT file name is a parse error rather than a panic.

### Removed

- 2026-11-05: breaking: `GET /v1/tokens` and `POST /v1/tokens`. The first answered
  a fixed token and the second a random string that authenticated nothing. Ion's
  scopes name asset routes that API keys cannot reach, so `/api/v1/api-keys` stays
  the one route that mints a credential.
- 2026-11-05: `tiletopia_terrain::global_dem::GlobalTerrainConfig`, which nothing used.
- 2026-11-05: `tiletopia_server::annotations`, an in-memory annotation store
  nothing called. The persisted `/api/v1/assets/{id}/annotations` routes stay for
  the `gui/` dashboard.
- 2026-11-04: routes that answered canned data: `GET /api/v1/photogrammetry/projects`,
  `/api/v1/bim4d/projects`, `/api/v1/indoor/buildings`, `/api/v1/metering/summary`
  and `/api/v1/metering/pricing`, with `petgraph`. Removed modules, each at the
  last commit that held it:
  - `crates/tiletopia-server/src/photogrammetry.rs` at 9ace651
  - `crates/tiletopia-server/src/bim4d.rs` at 9ace651
  - `crates/tiletopia-server/src/indoor.rs` at 9ace651
  - `crates/tiletopia-server/src/metering.rs` at 9ace651
- 2026-11-04: draco-rs, `patches/draco-rs`, the `draco` cargo feature and the
  ubuntu-24.04 CI pins (added in 9ace651).
- 2026-10-19: `GET /api/v1/terrain-analysis/operations`,
  `/api/v1/multispectral/indices`, `/api/v1/multispectral/sensors` and
  `/api/v1/classification/models`. Each answered a static list of operations,
  indices, sensors or models that no route runs. The dashboard terrain panel's
  list of analyses went with the first. `geostatistics::morans_i`, called only
  from its test, and the core `measurement`, `anomaly` and `clash_detection`
  modules, which nothing called, went too. Removed modules, each at the last
  commit that held it:
  - `crates/tiletopia-server/src/terrain_analysis.rs` at 483ed94
  - `crates/tiletopia-server/src/multispectral.rs` at 483ed94
  - `crates/tiletopia-server/src/classification.rs` at 483ed94
  - `crates/tiletopia-core/src/measurement.rs` at 483ed94
  - `crates/tiletopia-core/src/anomaly.rs` at 483ed94
  - `crates/tiletopia-core/src/clash_detection.rs` at 483ed94
- 2026-09-24: `/api/v1/geocoding/search` and `/api/v1/geocoding/reverse`.
  They asked public Nominatim and fell back to seven landmarks compiled into
  the binary. The geokode-core dependency went with them. Removed module, at
  the last commit that held it:
  - `crates/tiletopia-server/src/geocoding.rs` at 4a9a069
- 2026-09-24: the dashboard parts that called a route removed on 2026-09-23
  and got a 404. The measure, anomaly and clash panels, the admin panel's
  users table, the stories panel's `/demo/stories` fallback and
  `gui/osm-buildings-demo.html`.
- 2026-09-23: the routes that answered rows or input compiled into the
  binary. `/api/v1/demo/*`, `/features/*`, `/issues/*`, `/workspaces/*`,
  `/mobile/*`, `/classification/classes`, `/collaboration/sessions`,
  `/plugins` and `/plugins/pipelines`, `/routing/*` on a built-in San
  Francisco graph, `/isochrone/*` on a uniform grid, `/map-matching/match` on
  a fixed trace, `/flight-planning/*`, `/scan-registration/*`,
  `/osm-buildings/*`, and the `/geoprocessing`, `/terrain-analysis`,
  `/geostatistics` and `/multispectral` demo routes. The seeded
  `ClassificationEngine` jobs went with `/classification/classes`, the itinera
  crates with routing, and the `gpu` feature, wgpu and pollster with
  `tiletopia_core::gpu`, which nothing called. Removed modules, each at the
  last commit that held it:
  - `crates/tiletopia-server/src/demo.rs` at d5d3b86
  - `crates/tiletopia-server/src/stories.rs` at 4927f17
  - `crates/tiletopia-server/src/feature_service.rs` at ea1ca0f
  - `crates/tiletopia-server/src/issue_tracking.rs` at 4927f17
  - `crates/tiletopia-server/src/workspaces.rs` at 32bdd18
  - `crates/tiletopia-server/src/mobile.rs` at fbc65bd
  - `crates/tiletopia-server/src/collaboration.rs` at dc2ae50
  - `crates/tiletopia-server/src/plugins.rs` at f8ad746
  - `crates/tiletopia-server/src/flight_planning.rs` at 5904175
  - `crates/tiletopia-server/src/scan_registration.rs` at ea1ca0f
  - `crates/tiletopia-server/src/osm_buildings.rs` at a80ad72
  - `crates/tiletopia-server/src/routing.rs` at a3d4683
  - `crates/tiletopia-server/src/isochrone.rs` at 2fcb152
  - `crates/tiletopia-server/src/map_matching.rs` at befcb54
  - `crates/tiletopia-core/src/gpu.rs` at 3e25739
- The 2D map tile metadata routes and the demo engine behind them
  (2026-09-16): `GET /api/v1/tiles/sources`, `/styles`, `/layers`,
  `/{source_id}/tilejson` and `/cache/stats`. The sources and the style were
  compiled into the binary, the TileJSON named a `{z}/{x}/{y}` route that was
  never mounted, and the cache stats were fixed numbers. Vector tiles from
  built tilesets are served under `/martin`. Last present at 79ba8fb.
- The `tiletopia-cache` crate (2026-09-02). It held a `TileCache` trait with
  Redis, Memcached and LRU backends that nothing built or called, and with it
  went the redis, memcache and lru dependencies. The XYZ tile cache the server
  actually uses is in `map_tiles.rs`.
- `GET /api/v1/assets/{id}/thumbnail` (2026-09-02). Nothing ever wrote
  `<data-dir>/<id>/thumbnail.png`, so the route only ever answered 404. Point
  cloud thumbnails are still generated for the `png` export format.
- 17 parked modules with no callers, per the 2026-08-31 owner triage:
  scripting, cluster, cloud_store, marketplace, arvr, whitelabel, federation,
  model_zoo, prediction, onnx_inference, priority_queue, flythrough,
  dynamic_raster, reports, retention, temporal, encryption. With them went
  their orphaned dependencies (printpdf, aes-gcm, video-rs, ort, the server's
  own aws-sdk-s3 and aws-config) and the `onnx` and `video` features. cicd
  stayed: the CLI's validate subcommand calls it.
- The `crdt`, `geofence` and `tenant` modules (2026-09-07). No route, CLI
  path or render loop called them, only their own unit tests. Last present at
  commit 71910e3.
- The s3, gcs, azure and hybrid store backends (2026-09-07). Nothing built them,
  and out went aws-sdk-s3, aws-config, cloud-storage, the two azure_storage
  crates, the deploy tile bucket and the MinIO compose service. Last at 818e00f.

### Fixed

- 2026-11-03: a realtime message from a client is capped at 16 KiB less 255
  bytes, down from 16 KiB. Stamping `user_id` took a relayed chat line up to 255
  bytes over 16 KiB.
- 2026-10-01: a realtime room takes at most 23 connections and an account at
  most 16 across all rooms, refused with close code 4029 and the reason
  `room full` or `too many connections`. A full room's presence broadcast fits
  in 16 KiB. A token whose `sub` is over 255 bytes as JSON gets 400 before the
  upgrade, and a failed upgrade releases its room.
- 2026-09-24: a realtime message over 16 KiB closes its connection.
  A room keeps at most 32 unread messages and an account opens at most 8
  rooms, so one account holds about 4 MiB.
- 2026-09-24: the server holds at most 512 realtime rooms, 256 MiB of full
  queues, and refuses a new room past that with the per-account close code.
- 2026-09-24: a realtime Join whose `user_name` is over 64 characters gets the
  same close, so a presence broadcast stays small.
- 2026-09-24: the dashboard's collaboration panel shows member names as text,
  where markup in a name used to run in every other member's browser.
- 2026-09-24: failed logins lock an account only for the client address they
  came from, so a stranger can no longer keep the owner locked out. The account
  locks for every address after 20 times `TILETOPIA_LOGIN_LOCKOUT_FAILURES`, and
  `TILETOPIA_TRUSTED_PROXY_HOPS` says which `X-Forwarded-For` entry is the client.
- 2026-09-24: argon2 hashing and verification run on the blocking pool, two at
  a time, so a login flood queues instead of stalling tiles, terrain and
  realtime.
- 2026-09-24: a terrain or elevation request fetches only the SRTM tiles that
  fit a byte budget of a third of the 1 GiB task, and at most two such builds
  run at once. A zoom 6 terrain tile used to decode sixteen SRTM tiles, about
  830 MB, and two at once ran the task out of memory.
- The dashboard works against a server with auth on (2026-09-23). It sent no
  token, so the asset list, upload and annotation calls answered 401. A login
  form in the sidebar stores the token from `POST /api/v1/auth/login`, and
  every call to this server sends it as `Authorization: Bearer`, the
  collaboration websocket as its subprotocol. Upload now posts the file as
  multipart, the body the route reads, where it posted JSON with no file. The
  chat panel is gone: it posted to `/agent/chat/stream`, which nothing serves.
- A LAS with no GeoKey record is placed by the upload's `crs` field
  (2026-09-23). An upload carries no `.prj`, so such a scan was tiled in its
  own coordinates. The job queue and `tiletopia tile --crs` now take an EPSG
  code as the source CRS when the file names none, and a `crs` that is not an
  EPSG code fails the job naming it.
- Point cloud `.pnts` tiles carry `RTC_CENTER` (2026-09-23). Positions were
  stored relative to each tile's first point with no centre to add back, so
  every tile rendered near the centre of the earth. The feature table binary
  now also starts and ends on 8 bytes from the start of the tile, as the
  spec asks.
- Point clouds in a projected CRS, such as UTM, are tiled where they are on
  the globe (2026-09-23). The job queue and `tiletopia tile` read the CRS from
  the LAS GeoKey record or a `.prj` beside the file and convert to ECEF, the
  frame the tileset's bounding boxes are read in. Before, UTM metres were written as
  ECEF, so a scan landed near the centre of the earth. A file with no CRS is
  tiled unchanged, with a warning naming it.
- The public docs say what the code does (2026-09-16). The 2D map tile
  section advertised an XYZ proxy with a TTL cache, but no `{z}/{x}/{y}` route
  is mounted, `fetch_tile` has no caller, the on-disk cache never expires, and
  `GET /api/v1/tiles/cache/stats` answers numbers compiled into the binary.
  "Draco/meshopt compression for tile delivery" covered a `draco_encode_mesh`
  nothing calls, only meshopt simplification runs, for mesh LODs. The
  anonymous-route list left out the 2D tile metadata, the Ion-compat `/v1/*`
  reads, `/api/v1/stories/share/{token}` and `/metrics`. Test counts were 904,
  892 and 917 across the docs, `cargo test -- --list` gives 873 on default
  features. The GPU table listed a CUDA path and a browser target that do not
  exist. `ION_PARITY_ROADMAP.md` called seven shipped pieces unbuilt.

- The `aarch64-unknown-linux-gnu` release build links (2026-09-16).
  `manifold-csg-sys` shells out to `cmake` instead of using the cmake crate,
  so the cross image's `CMAKE_TOOLCHAIN_FILE_aarch64_unknown_linux_gnu` never
  reached it and `manifoldc.cpp` was compiled for x86_64, which the aarch64
  linker rejected. A new `Cross.toml` passes `CMAKE_TOOLCHAIN_FILE` in
  unsuffixed.
- The `x86_64-apple-darwin` release build links (2026-09-16). `macos-latest` is
  an arm64 runner, and the same raw `cmake` call built manifold for the runner
  rather than the target, so the link found arm64 objects. That matrix row now
  sets `CMAKE_OSX_ARCHITECTURES=x86_64` on the build step.
- The server builds against current geoplumb (2026-09-16). `WindowReq` grew a
  `time` field that overrides a source's own interval for one pull. The DEM
  sources read the same ground whenever they are pulled, so all three analysis
  tile requests pass `None`.
- The `martin` feature builds under a refreshed lockfile (2026-09-16). pmtiles
  0.23.1 moved to object_store 0.14 in a patch release, which martin-core 0.5
  cannot take. martin-core is now 0.11, object_store 0.14 and the pmtiles dev
  dependency 0.24, so one object_store version crosses martin's API.
- `a_subscription_receives_a_signed_payload_for_the_event_it_asked_for` waits
  for the delivery to be recorded (2026-09-16). It aborted the delivery worker
  as soon as the receiver had the POST, which is before the worker writes the
  outcome into the history the test reads next.
- FBX meshes sit where their model node puts them (2026-08-29). The reader
  flattens positions and normals through the node hierarchy with the FBX
  SDK's composition (translation, pivots and offsets, pre and post rotation,
  `RotationOrder`, scaling) plus the node's own geometric offset. A mesh
  offset or scaled through its node used to land at the origin unscaled.
- FBX positions come out in metres (2026-08-29). `UnitScaleFactor` is
  centimetres per file unit. A default file (1) was read 100 times too large,
  and the 2026-08-28 change multiplied by the factor without dividing by 100,
  which made a metre file (100) 100 times too large as well.
- FBX mesh ingest reads `UnitScaleFactor`, `FrontAxis` and `CoordAxis`, and
  writes normals (from the file, or from the faces when the layer is missing)
  (2026-08-28).
- glTF 16- and 32-bit textures convert to RGBA8 instead of leaving the mesh
  untextured (2026-08-28). Tile crops keep the source mime type instead of re-encoding
  JPEG at quality 90. Texture decode refuses a raster past 8192 on a side
  or 4096² pixels.

- A mesh tileset whose root is a single tile drew nothing in Cesium
  (2026-08-25). The tileset-level `geometricError` copied the root tile's,
  which is zero for a leaf, and Cesium never visits the root of a tileset
  whose own error is zero. The tileset now carries the root bounds' error and
  the leaf root keeps zero.

### Changed

- 2026-10-28: sqlx 0.8 to 0.9 in the server, the version mbtiles already
  used, so the binary links one sqlite. Queries built at runtime pass
  through `AssertSqlSafe`. The lock takes geoplumb with its GeoPackage source
  and terrano 0.3 with range reads by shared reference.
- 2026-09-24: the globe's search bar asks geokode at same-origin
  `/api/geocode/forward` where it asked public Nominatim. A result with a
  bbox flies to that rectangle. A miss or an error shows no result.
- jsonwebtoken 9 to 11 on the `aws_lc_rs` backend (2026-08-25), a crate the
  lockfile already resolved. Only HS256 `encode`, `decode`, `from_secret` and
  `Validation::default()` are used, and 11 keeps `validate_aud` on by default,
  so a token carrying `aud` is still refused. `cloud-storage` still pins its
  own jsonwebtoken 7.

### Added

- 2026-09-24: `TILETOPIA_SIGNUPS_PER_ADDRESS_PER_HOUR` answers 429 past that
  many signups from one client address in the last hour, off when unset.
- 2026-09-23: signup and login limits. `TILETOPIA_MAX_USERS` closes signup
  with a 403 once the users table holds that many accounts, and
  `TILETOPIA_SIGNUPS_PER_HOUR` answers 429 past that many signups in the last
  hour across everyone. Both are off when unset. Consecutive failed logins lock
  the account with a 429 for `TILETOPIA_LOGIN_LOCKOUT_MINUTES`, after
  `TILETOPIA_LOGIN_LOCKOUT_FAILURES` of them, 10 and 15 minutes by default.
  The count is taken before the password is checked, so a burst of parallel
  guesses gets no more tries than the limit. Every count lives in the users
  table and survives a restart.
- IFC element ids reach the tiles (2026-08-25). The IFC reader keeps each
  element's GlobalId and the mesh tiler makes every source mesh one feature:
  each vertex carries its feature id as `_FEATURE_ID_0` under
  `EXT_mesh_features`, and the tile's `EXT_structural_metadata` property table
  holds an `asset_id` string per feature, empty for a mesh with no id. A format
  that carries no element identity, OBJ, glTF or FBX, tiles exactly as before,
  with neither extension written. Draco does not touch this: the GLB writer
  never compresses its output, so `draco_encode_mesh` is not on the tiling path
  and the feature id attribute is written uncompressed either way.
- `PUT /api/v1/admin/users/{id}/org` (2026-08-25). Admin only, next to the role
  route. The body is `{"org_id": "<uuid>"}` to put a user in an organization or
  `{"org_id": null}` to take them out. An organization that does not exist
  answers 404. The change lands an audit row, and the user's later writes carry
  the organization on theirs.
- The offline viewer export is offline out of the image (2026-08-25). The
  Docker image unzips `Build/Cesium` from the pinned CesiumJS 1.119 release,
  checked against its SHA-256, into `/opt/tiletopia/cesium` and sets
  `TILETOPIA_CESIUM_DIR` to it, so an `offline_viewer` bundle from the published
  image carries the library and the Apache-2.0 license it is redistributed
  under, and its page names no remote host. That is 20 MB more image. Outside
  the image the variable is still unset by default, and the exported page falls
  back to cesium.com behind the banner that says it needs the network.
- Audit rows name the caller's organization (2026-08-25). The middleware reads
  `org_id` off the caller's row in the users table when it records, so the row
  says which organization the user was in at the time of the write. A subject
  with no user row, an API key or a tool token, records none. Nothing sets a
  user's `org_id` over the API yet: `POST /api/v1/orgs` creates an organization
  and no route puts a user in one.
- Story writes are gated (2026-08-25). `POST /api/v1/stories` and the `PUT`
  and `DELETE` on a story take the Edit tier through `require_editor`, so a
  viewer token is refused with 403. A story records the caller's token subject
  as `author_id`, and only that author or an admin updates or deletes it:
  anyone else gets 403. Deleting an id that does not exist answers 404 rather
  than 204. Reads are unchanged: any valid token lists and gets every story,
  and the share route stays public.
- Audit rows carry the address and the created id (2026-08-25). The server is
  served with connect info, so a row records the direct peer's IP, which is the
  proxy when one fronts the server. No proxy header is read: `X-Forwarded-For`
  is caller-controlled. Every create handler returns the id it chose as an
  `AuditedResource` response extension, which the middleware uses when the path
  names no id, so a create no longer records an empty `resource_id`. Newly
  audited: story create, update and delete, portal item create and delete,
  `POST /api/v1/catalog/{dataset_id}/add`, and `PUT /api/v1/users/me`, whose row
  names the caller since its path names nothing. `org_id` stays empty, since no
  token this server issues carries an organization.
- Export expiry is real (2026-08-25). A finished export is downloadable for 7
  days; past that `ExportStatus::Expired` is what `GET /api/v1/exports` and
  `GET /api/v1/exports/{id}` report, and the download answers 410 naming the
  time it expired rather than 404. The `prune_export_files` scheduled action
  retires expired jobs' directories whatever their files' age, alongside the
  age rule that catches files no job record covers any more, and its outcome
  message counts both.

- The audit log is real (2026-08-24). `audit_middleware` records successful
  mutations that pass the editor/admin gates into a SQLite `audit_entries`
  table: assets, annotations, tilesets, plugins, webhooks, scheduler,
  orgs, admin user role changes, API key mint/revoke, exports, and the
  Ion-compat writes (`POST /v1/assets`, `POST /v1/tokens`, which reach the
  same effects as the native routes). `GET /api/v1/audit` reads it back,
  instance-admin only, filterable. Retention sweeps hourly per
  `TILETOPIA_AUDIT_RETENTION_DAYS` (default 30). demo.rs no longer seeds
  fake audit entries and `/api/v1/demo/audit` is gone; the GUI reads the
  real route. `user_id` comes from the JWT, `ip_address` and `org_id` stay
  empty (no connect-info, no org claim), and a create records an empty
  `resource_id` because the id is only known inside the handler.
- Offline viewer export is real (2026-08-24): `offline_viewer` is an export
  format on `POST /api/v1/exports`, producing a downloadable zip with the
  viewer page, `serve.py` and the asset's tiles, pruned like every export.
  The page is only fully offline when `TILETOPIA_CESIUM_DIR` points at a
  CesiumJS build to bundle; otherwise it says so in a visible banner
  instead of silently loading from the CDN.


- Mesh formats (glTF, GLB, OBJ, FBX, CityGML, IFC) tile natively with textures
  and materials (2026-08-24), so `TILETOPIA_MAGO_JAR` matters only to vector
  files. The readers carry UVs, diffuse textures and diffuse colours into the
  tile GLBs, a tile holding part of a textured mesh gets the crop of the
  texture its triangles reach, and a texture that cannot be found or decoded
  degrades to untextured with a warning. The FBX reader honours
  `GlobalSettings` `UpAxis`/`UpAxisSign` instead of assuming y up, and reads
  textures embedded in `Video` `Content` or by `RelativeFilename`. Two writer
  bugs fixed on the way: a GLB with both texcoords and a texture pointed its
  image at the texcoord buffer view, and `split_texture` could ask for a
  zero-width crop at a UV of exactly 1.0. Tile filenames under a node with
  more than ten children are `0-1.glb` style now, not `01.glb`.

- The scheduler runs real jobs (2026-08-24). The old module is deleted whole:
  its eight-name `JobType` enum, its priority tier, its `SchedulerStats`, its
  `spawn()` that nothing called, its `create_job` that answered "an hour from
  now" for every cron expression, and the three seeded jobs carrying run counts
  of 28, 720 and 4. Nothing is seeded now, and `/api/v1/scheduler/stats` and
  `/api/v1/scheduler/runs`, which answered counts of the seeds, are gone.
- Three actions, which are the three the worker runs. `retile_asset` submits a
  tiling job through `JobQueue::submit`, carrying the placement the asset's last
  job carried so a mesh re-tile does not fail for want of coordinates.
  `prune_export_files` removes export directories under the data directory whose
  newest file is past an age, which nothing else removed: an export's job record
  is process memory, so a restart left the files with nothing to delete them. A
  directory holding no file yet is an export still encoding and is left alone.
  `prune_finished_jobs` deletes settled rows from the `jobs` table past an age.
  `GET /api/v1/scheduler/actions` lists exactly these three, and a test schedules
  one job per advertised kind and fails unless every one of them executes.
- Three schedules: `interval` in seconds, `cron`, and `one_shot` at a time in the
  future. Cron is the `cron` crate over five standard fields, minute hour
  day-of-month month day-of-week, at second 0 in UTC. The field count is checked
  before parsing, because four fields padded for the crate's seconds and year is
  a valid six-field expression meaning something else. An expression the server
  cannot read, an interval under a second and a one-shot already past are each a
  400 naming what was wrong.
- `POST /api/v1/scheduler/jobs` stores a job in the new `scheduled_jobs` table:
  the action, the schedule, an enabled flag, the next and last run, the last
  outcome with its detail, a run count of finished runs only, and the failures in
  a row. `PUT /api/v1/scheduler/jobs/{id}` enables or disables one, and enabling
  recomputes the next run from now so a job that sat disabled past its time does
  not fire on the way back. `DELETE /api/v1/scheduler/jobs/{id}` removes it. All
  three take the Edit tier, and a job somebody else created is a 404 rather than
  a 403. `GET /api/v1/scheduler/jobs` answers the caller's own jobs and an admin
  every one, out of the table rather than out of memory.
- A worker starts with the server, wakes every second, runs what the table says
  is due, and writes the outcome back. A one-shot disables itself once it has
  run. A failed run keeps its error on the row and comes back on the next tick;
  three failures in a row disable the job. The next run is on the row, so a
  scheduler built fresh over the same database picks up what is due and leaves
  what is not.
- Webhooks deliver real events (2026-08-24). One module is left: `webhook.rs`,
  whose signature was a `DefaultHasher` of the payload and the secret, is
  deleted, and `webhooks.rs` now holds subscriptions, delivery and the event
  set. The two demo subscriptions carrying `whsec_demo_secret_1` and `_2` are
  gone with it, as is the eight-name event list nothing emitted.
- `POST /api/v1/webhooks` registers a target URL and an event set and answers a
  freshly generated `whsec_` secret once, the way `POST /api/v1/api-keys` mints a
  key; a caller cannot supply one. `PUT /api/v1/webhooks/{id}` points a
  subscription elsewhere or pauses it and `DELETE /api/v1/webhooks/{id}` removes
  it. All three take the Edit tier, and a subscription somebody else created is a
  404 rather than a 403. `GET /api/v1/webhooks` answers the caller's own
  subscriptions and an admin every one, out of the new
  `webhook_subscriptions` table, and `GET /api/v1/webhooks/deliveries` answers
  the deliveries this process has finished attempting. No listing carries the
  signing secret. An unknown event name, an empty event list, a URL that is not
  absolute http or https, and a URL carrying a username or password are each a
  400 naming what was wrong.
- `GET /api/v1/webhooks/events` lists the three events the server emits:
  `job.completed` and `job.failed` when a tiling job settles in
  `JobQueue::finish`, and `asset.deleted` when the delete route removes an
  asset. The payload carries the job or asset it is about. A test subscribes to
  every advertised event, drives all three paths for real, and fails if the
  advertised set and the delivered set differ.
- A delivery worker starts with the server and posts each queued event with
  `X-TileTopia-Signature: sha256=<hex>`, the HMAC-SHA256 of the request body
  under the subscription's secret, plus the event name and a delivery id that
  stays the same across retries. A failed attempt is retried three times, 30
  seconds apart and doubling, then dropped; the failure stays in the delivery
  history the route reports. Deliveries do not follow redirects, so a receiver
  cannot bounce a signed request at another host, and the response body is never
  read. Subscriptions are in SQLite, pending deliveries are in process memory: a
  restart drops what has not gone out.
- Geoprocessing operates on real geometry (2026-08-24). Buffer, union,
  intersection and difference are the `geo` crate's offset and boolean overlay,
  so a buffered square gains rounded corners with the area of the analytic
  rounded square, the union of two overlapping squares is the L they cover
  rather than a hull around both, the union of two disjoint squares is a
  MultiPolygon with two parts, and a difference that encloses its second
  geometry cuts a hole. Intersection no longer clips against the second
  polygon's edges as half-planes, which answered the unit square instead of the
  whole L for a concave clip. Centroid is the area-weighted polygon centroid,
  not the mean of the vertices, so an L answers (1.1, 1.1) and not (4/3, 4/3).
  Buffering projects to meters with a local equirectangular frame about the
  geometry's centroid latitude, and is refused past 89 degrees of latitude where
  that frame collapses. Convex hull (Graham scan), Douglas-Peucker simplify and
  the ray-casting point-in-polygon test are unchanged.
- `POST /api/v1/geoprocessing/run` takes an `operation`, a GeoJSON-shaped
  `geometry`, a second `other` geometry for the binary operations, and
  `distance_m` or `tolerance` where the operation needs one. It answers the
  result geometry with GeoJSON nesting, plus the geodesic `area_m2` and
  `length_m` where the result has them. Boolean overlay and buffer always answer
  a MultiPolygon so a caller reads one shape whether the result split or not. An
  unknown operation, a missing `distance_m` or `tolerance`, a missing second
  geometry, a non-finite coordinate, a ring under four positions and a
  non-polygon input to an overlay are each a 400 naming what was wrong.
  `/operations` now lists only the seven operations `run` accepts: Dissolve,
  Clip and Voronoi were advertised and unimplemented, and are gone.
- Kriging solves a linear system (2026-08-24). Ordinary kriging builds the
  (n+1)x(n+1) semivariogram matrix with a Lagrange row and reads the kriging
  variance off the solution as the weights dotted with the right-hand side.
  Simple kriging builds the n x n covariance matrix, `sill - semivariance`,
  around the caller's `known_mean` and answers `sill - w'c`. Universal kriging
  adds constant, x and y drift rows, and reproduces a plane the samples lie on
  exactly. All three are factored once by an in-file Gaussian elimination with
  partial pivoting, so a grid cell costs one substitution, and a pivot below
  1e-12 of the largest matrix entry is refused as singular instead of filling
  the grid with NaN. Coordinates are centred on the sample centroid for the
  drift rows. `semivariance` honours all five variogram models the methods
  endpoint lists, each capped at the sill so simple kriging has a covariance
  for every one.
- `POST /api/v1/geostatistics/interpolate` takes `samples`, `bounds`,
  `resolution` and `method` and answers the `InterpolationResult`, with one
  kriging variance per cell for the three kriging methods. Empty samples, bounds
  with no extent, a non-positive resolution, a non-finite coordinate or value, a
  repeated sample location, an IDW power at or below zero, and a variogram with
  no sill are each a 400 naming what was wrong; a singular system is a 422.
  Samples are capped at 500 because the solve is dense, and a grid at 1,000,000
  cells. Duplicate sample locations are refused rather than averaged: two
  samples at one place give the matrix two identical rows, and averaging them
  would answer a question the caller did not ask.
- Static maps answer image bytes (2026-08-24). `GET
  /api/v1/static-map/render?bbox=west,south,east,north&width=&height=&format=`
  and a `POST` of the same request as JSON, which also carries `markers` and
  `overlays`, answer the rendered image with its own content type: `image/png`,
  `image/jpeg`, `image/webp`, `image/svg+xml` or `application/pdf`. The route
  used to answer JSON metadata with the image bytes marked `#[serde(skip)]`, so
  no caller ever received an image; WebP was encoded as JPEG and PDF and SVG as
  PNG. WebP is now the image crate's lossless WebP, the SVG is real markup with
  a `<circle>` per marker, a `<polyline>` or `<polygon>` per overlay and the base
  layer as a base64 PNG data URI so nothing is fetched from outside the
  document, and the PDF is a one-page document placing the render as a
  `/DCTDecode` image over the whole MediaBox, sized in points from the pixel
  dimensions and the `dpi`.
- The static map base layer is a hillshade of the DEM this server holds, from
  the same stores the elevation and analysis routes read. Where no DEM covers
  the box the image gets a flat background instead, and the
  `x-static-map-base-layer` response header says which of the two it is. A DEM
  that should be readable but is not is a 503, as elsewhere. The shading is
  computed on at most 1024 DEM samples per side and scaled onto the canvas, so a
  4096-pixel image costs the same DEM reads as a 1024-pixel one.
- Polygon overlays are filled, by an even-odd scanline over `fill_color` blended
  at `fill_opacity`, and `stroke_width` is drawn as a stroke that wide rather
  than a one-pixel line. `GET /api/v1/static-map/formats` lists the five formats
  with the content type each answers and the two base layers, and validation is
  honest: a side of 0 or past 4096, a format nothing encodes, a box that is off
  the globe or covers no ground, a `dpi` outside 72, 150 and 300, a colour that
  is not six hex digits, and a request naming neither a box nor a center are
  each a 400 saying what was wrong.

- `GET /api/v1/stac/search` searches a real catalog. `TILETOPIA_STAC_API` names
  an upstream STAC API root and the route forwards `bbox`, `datetime`,
  `collections` and `limit` to its `/search`, answering the upstream item
  collection unchanged so the extension fields a client reads survive. With no
  upstream configured it answers 503 naming the variable, an unreachable or
  failing upstream is a 502, and a 200 that carries no `features` array is a 502
  too rather than an empty map. `limit` is capped at 500.
- `GET /api/v1/cog/datasets/{id}/window?level&col&row&cols&rows` reads real
  pixels out of a registered COG through terrano's windowed `CogReader`, one
  row-major plane per band with nodata as null. Local sources are read by seek
  and remote ones by HTTP `Range`, so a window costs the internal tiles it
  touches. A window is capped at 512x512 pixels.
- `TILETOPIA_COG_SOURCES` is the COG registry: one href per comma-separated
  entry, each a local path or an http(s) URL, keyed under its filename stem the
  way a PMTiles source is. Every entry is opened at startup and `GET
  /api/v1/cog/datasets` reports the size, dimensions, band count, EPSG, bounds,
  internal tile size and overview levels the file declares. Unset serves
  nothing. A local path that cannot be opened stops the server; a remote href
  that cannot be opened, or a host that answers 200 to a `Range` request, is
  logged and skipped.
- API keys authenticate a request (2026-08-24). A key is `ttk_` plus 32 bytes of
  OS randomness in hex, and what the database holds is its SHA-256 hex digest,
  so the plaintext exists only in the create response. `X-Api-Key` is resolved
  by one indexed lookup on that digest: a string not shaped like a key is
  refused before any lookup, which is also what makes presenting the stored
  digest itself a 401. Revoked and expired keys are 401 naming which of the two
  it was, a key outside the route's class is 403, and a key past its budget is
  429 with `Retry-After` in seconds and `retry_after_ms` in the body. Last use is
  recorded off the request path, so a failed write cannot fail a request. A
  request carrying `X-Api-Key` is authenticated by that key alone: a bad key is
  refused rather than falling back to a bearer token that came with it, and a
  good key never inherits that token's reach. `is_public_read` paths are decided
  before any key is looked at, so tile reads stay anonymous.
- `POST /api/v1/api-keys` mints a key from a `name`, a `permissions` list, a
  `tier` of free, pro or enterprise, and an optional RFC 3339 `expires_at` in
  the future. It answers the plaintext key once. `GET /api/v1/api-keys` lists
  every key as metadata: no plaintext and no digest, since `key_hash` cannot
  serialize. `POST /api/v1/api-keys/{id}/revoke` kills a key and keeps the row,
  `DELETE /api/v1/api-keys/{id}` drops it. All four sit behind `require_admin`,
  which reads JWTs only, so key management is admin-only and no key reaches it:
  there is no self-service key management, and no Admin permission for a key to
  carry. An unknown permission, an unknown tier, an empty permission list and an
  expiry in the past are each a 400 naming what was wrong.
- A key's permission maps to a route class in `auth::route_access`: Read reaches
  the catalog, STAC, COG, feature and geocoding GETs; Terrain the elevation and
  terrain-analysis routes; Analytics the analysis, geostatistics and
  geoprocessing compute; Export the static-map render and the analysis exports.
  Anything not listed refuses every key, including `/api/v1/admin/`, org
  management, the tile cache stats, and the routes whose handler scopes its
  answer to a platform user (assets, exports, portal items, tilesets,
  `/api/v1/users/me`).
- `GET /api/v1/api-keys/usage` is the one route a key reads about itself, and
  answers real counts out of the rate limiter: today's requests, when they
  reset, and the tier's per-second and per-day budgets. A key sees its own row,
  an admin sees every key, anyone else is refused.
- `GET /api/v1/stac/collections` asks the upstream named by `TILETOPIA_STAC_API`
  for its `/collections` and answers that list unchanged, so a collection's
  summaries, links and extension fields reach the client whole. Unset, it answers
  the same 503 naming the variable that search answers. An upstream that cannot
  be reached, that refuses the call, or that answers 200 with no `collections`
  array is a 502.

### Removed

- Two facade modules (2026-08-24): `versioning.rs` (asset version control,
  one route serving demo data, ptolemy is the platform's versioned
  backbone) and `dashboard.rs` (no routes, the viewer owns dashboards).
  `/api/v1/versioning/assets` is gone. `temporal.rs` is a different module
  and still exists, uncalled and disclosed.
- The `tiletopia-worker` crate (2026-08-24). It was a second job runner over
  `read_point_cloud`, `read_heightmap` and `read_mesh` that no code called:
  `tiletopia-server` and `tiletopia-cli` listed it as a dependency and never
  named `tiletopia_worker`. `JobQueue` does this work.
- The inverse-variogram weighting that stood in for kriging. It solved no
  system, its weights were `sill / gamma` normalised to sum to one, its
  "variance" was those weights dotted with the same semivariances, and ordinary,
  simple and universal kriging all ran it, so `known_mean` was ignored and no
  drift was ever fitted.
- The five static map basemap styles, Streets, Satellite, Terrain, Dark and
  Blueprint. Nothing rendered them: they were a list of names carrying a fresh
  UUID per call, and the image was a flat grey buffer. The request's `style_id`
  goes with them, along with the `StaticMapResult` metadata shape, its per-call
  UUID and its `render_time_ms`, since the route answers bytes now. A marker's
  `label` is gone because no format drew text, and the `Circle` overlay type
  because it drew the same open path as a polyline.
- The demo STAC item and the two demo COG datasets, along with the local TIFF
  tag parsing that terrano's reader replaces. `/api/v1/stac/search` and
  `/api/v1/cog/datasets` answered invented data before this, and the COG tile
  index fabricated byte offsets from the tile grid.
- The three seeded demo API keys and the in-memory `ApiKeyStore` that held them.
  `GET /api/v1/api-keys` served those seeds, and the store's `get_by_hash` had no
  caller, so no key had ever authenticated anything. The `api_keys` table's
  plaintext `token` column goes with them, along with the `create_api_key`,
  `get_api_key` and `delete_api_key` that nothing called.
- The `Write` and `Admin` key permissions. Every write route sits behind
  `require_editor` or `require_admin`, which read JWTs only, so neither
  permission reached a route: what a key may do is now exactly the four classes
  `route_access` lists.
- The three fabricated STAC collections, point-clouds, terrain and bim-models,
  with their item counts of 47, 16 and 23 and the extents, providers and
  summaries around them. The catalog root's `child` links to them go too: there
  was no `/stac/collections/{id}` route to follow, and the collections
  conformance class goes with them for the same reason.
- The rate limiter's upload-bytes and tile-request counters, and the
  `upload_bytes_per_day` and `tile_requests_per_day` limits beside them. Nothing
  fed them, and the tile reads they were meant to count are anonymous, so there
  is no key to count them against. The limiter enforces requests per second and
  requests per day, which is what it measured.

### Changed

- `interpolate_grid` and `kriging_estimate` return a `Result`, and
  `interpolate_grid` fits its variogram over the samples' own extent rather than
  the requested bounds' width, so the lag bins cover the distances the solve
  asks about. `GET /api/v1/geostatistics/demo` says in its payload that its five
  samples are invented, and points at the interpolate route.
- The `api_keys` table is recreated with `key_hash`, `permissions`, `tier`,
  `created_by`, `last_used_at` and `revoked` columns, dropping the old plaintext
  shape on a database that already has it. Nothing is lost: the table had no
  writer, so no row was ever stored in it.
- `AppState` carries an `api_key_rate_limiter` where it carried an
  `api_key_store`; the keys themselves live in the database. `auth_middleware` is
  layered with `from_fn_with_state` so it can reach both.
- A COG window read goes through the `CogReader` its source was registered with
  instead of reopening the source, so only startup pays for the header reads.
  Each reader sits behind its own lock, and a local source holds its file handle
  for as long as the server runs, one per entry in `TILETOPIA_COG_SOURCES`.
- The STAC catalog root advertises only what the configured upstream lets it
  answer. With `TILETOPIA_STAC_API` set it links to the collection list and the
  search and claims item-search conformance; unset it carries `self` and `root`
  and the core class alone, so nothing in it points at a route that would refuse.
- A `CogDataset` is keyed by a string id rather than a per-boot UUID, and
  reports only what a COG header carries: `bounds` in the file's own CRS units
  replaces the old `bbox`, `band_count` replaces the per-band type, colour
  interpretation and statistics, and `levels` replaces `overviews`. Compression
  and nodata are gone from the shape since the reader does not expose them.

## [Unreleased] - 2026-08-23

### Added

- Elevation reads real DEM (2026-08-24). `GET /api/v1/elevation/point?lat=&lon=`
  and `GET /api/v1/elevation/profile?path=lon,lat;lon,lat` answer from the
  stores the terrain routes already read: a grid loaded into the DEM store, a
  one-degree tile staged under `<data-dir>/dem/`, then the SRTM cache. The
  answer names which one it came from and the sample spacing that store
  actually has. Ground none of them covers is a 404 saying no elevation data is
  staged for the location, and a tile that should be there but cannot be
  fetched is a 503 naming it. The sine field the routes used to serve, labelled
  `source: Srtm30m`, is gone, along with the `Copernicus30m` and `Lidar1m`
  labels nothing ever produced. The profile walks the path it is given, up to
  512 points, instead of a hardcoded one in San Francisco.

  The analysis endpoints and the analysis XYZ tiles read the same field, so a
  point, a profile, a hillshade and a tile of the same ground agree. A one-shot
  analysis over ground no DEM covers is refused the same way; an XYZ tile is
  transparent there, which is what a map library wants. An empty
  `TILETOPIA_SRTM_BASE_URL` turns the download fallback off, so an air-gapped
  server answers the explicit gap instead of a fetch failure.

- Watershed, flow direction and flow accumulation (2026-08-24) join slope,
  aspect, hillshade and contours on `POST /api/v1/analysis/terrain`, as PNG
  rasters like the other raster ops. All three run over a depression-free copy
  of the DEM, since a raw pit swallows every path that reaches it. Flow
  directions are painted one hue per D8 code with pits grey, accumulation on a
  log ramp, and basins by cycling hue so neighbours are told apart.

- `POST /api/v1/analysis/viewshed` casts rays (2026-08-24). It runs terrano's
  new `viewshed` over the DEM around the observer and answers one square per
  visible cell, so a ridge's shadow is a hole in the result. It used to sweep
  the terrain profile radially and return a star polygon of the farthest
  visible point per azimuth, which could not express a hole. The request takes
  `resolution` in cells per side, where it used to take `rays`.

- Vector tilesets. `POST /api/v1/tilesets` takes a `.geojson`, `.geojson.gz`,
  `.fgb` or `.csv` multipart upload, answers 202 with the job id and the
  registry row, and a worker builds it into one PMTiles archive with
  tippecanoe. A ready archive registers as a martin source named after the
  tileset id, so it serves at `/martin/{id}/{z}/{x}/{y}` with TileJSON at
  `/martin/{id}`. `GET /api/v1/tilesets` lists the caller's rows with status,
  source id, built_at, size and the stderr tail on failure, `GET
  /api/v1/tilesets/{id}` is one row, and `DELETE /api/v1/tilesets/{id}` removes
  the archive, the row and the source together. Uploads and deletes take the
  Edit tier plus ownership, admins see everything.
- A delete that lands while a build is running leaves nothing behind: the
  worker finds its row gone and drops the archive it just wrote along with the
  source it registered.
- The tileset upload streams the file straight to disk and raises the route's
  body limit to 4 GiB, since axum's own default of 2 MB is smaller than any
  file worth building a tileset from.
- The tileset registry lives in the SQLite database beside assets and jobs, and
  every ready row re-registers at startup, so a restart serves what the last
  run built. A build that was running when the server stopped is queued again.
- The build runs tippecanoe as a subprocess with `-zg`,
  `--drop-densest-as-needed`, a layer name from the uploaded filename and its
  own work directory, and records the argv it ran. The child gets a timeout, a
  capped address space and a capped file size, set between fork and exec.
  `TILETOPIA_TILESET_DIR`, `TILETOPIA_TILESET_TIMEOUT_SECS`,
  `TILETOPIA_TILESET_MEMORY_MB` and `TILETOPIA_TILESET_DISK_MB` configure them.
  The Docker image builds tippecanoe 2.79.0 from source in its own stage, and
  the Linux CI row installs the same tag so the build tests run there.
- `TILETOPIA_PMTILES_DIR` serves PMTiles archives under `/martin`. Every
  `*.pmtiles` file directly in the directory is registered under its filename
  stem, so `basemap.pmtiles` answers at `/martin/basemap/{z}/{x}/{y}` and
  appears in `/martin/catalog`. Unset serves nothing, a directory that cannot
  be read refuses startup, and one archive that fails to open is logged and
  skipped. An unregistered source is a 404 on both the TileJSON and the tile
  route. The routes sit behind the same JWT as the rest of the API. Needs the
  `martin` cargo feature, which the Docker image and every CI row now build
  with.

### Fixed

- `POST /api/v1/assets` takes an upload over 2 MB. It streams the file straight
  to disk and the route's body limit is the same 4 GiB the tileset upload
  takes, rather than axum's 2 MB default. A `name` field arriving after the
  file still names the asset, and a refused upload takes the streamed file with
  it. A multipart filename or `name` field that is a path rather than one file
  name is refused with 400, so an upload can no longer write outside the
  asset's own directory.
- Martin tile responses carry `Content-Encoding` when the archive stores
  compressed tiles. tippecanoe writes gzipped MVT, and without the header no
  browser client could decode the body.
- The build's address-space cap applies only on Linux. macOS rejects
  `RLIMIT_AS` with `EINVAL`, which surfaced as a failed spawn once setrlimit
  errors stopped being ignored, and builds only run in Linux images.
- `DELETE /api/v1/tilesets/{id}` kills the tippecanoe building that tileset.
  The run used to finish, holding CPU and disk for minutes to write an archive
  the delete had already accounted for.

- A build the server was restarted in the middle of finishes when it is queued
  again. tippecanoe exits rather than write over an archive, so the half-built
  one and its journal are removed before the run.
- `{id}.pmtiles-journal` no longer piles up in the tileset directory: a failed
  build and a delete both take the journal along with the archive.
- The `name` field of a tileset upload is capped at 200 characters and counted
  as it arrives, so an oversized one is a 400 rather than a row in the registry.
- `TILETOPIA_TILESET_MEMORY_MB` and `TILETOPIA_TILESET_DISK_MB` are converted to
  bytes with checked arithmetic, and a value too large to convert refuses
  startup instead of panicking or wrapping to a tiny limit.
- A `setrlimit` that fails now fails the spawn, rather than running the build
  with no limit at all. The build also runs with `RLIMIT_CORE` at 0, so one
  aborted by the memory cap cannot dump a core file that large.
- `GET /martin/{id}` answers with TileJSON a client can use: the `tiles` array
  carries the `/martin/{source}/{z}/{x}/{y}` template instead of being empty,
  `name` is the tileset's stored name, and the fields tippecanoe fills with the
  build's absolute paths are gone. `vector_layers` is kept.
- A tile coordinate outside the zoom's grid is a 404 rather than a 500.
- `GET /martin/catalog` lists the archives from `TILETOPIA_PMTILES_DIR` to every
  signed-in caller, but a built tileset only to its owner, so the catalog cannot
  be used to enumerate another owner's source ids. Admins see everything. Tile
  and TileJSON reads are unchanged: any valid token may read a source it knows
  the id of.

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
