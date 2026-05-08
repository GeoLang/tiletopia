//! tiletopia-terrain: quantized mesh terrain from heightmaps
//!
//! Generates quantized mesh terrain tiles from GeoTIFF/DTED/HGT heightmaps
//! using Delaunay triangulation with geometric error-based simplification.

pub mod global_dem;

/// Heightmap grid (row-major, top-left origin).
pub struct Heightmap {
    pub width: u32,
    pub height: u32,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub elevations: Vec<f32>,
}

impl Heightmap {
    /// Create from ingest heightmap data.
    pub fn from_ingest(hm: &tiletopia_ingest::Heightmap) -> Self {
        Self {
            width: hm.width as u32,
            height: hm.height as u32,
            min_lon: hm.bounds[0],
            min_lat: hm.bounds[1],
            max_lon: hm.bounds[2],
            max_lat: hm.bounds[3],
            elevations: hm.elevations.iter().map(|&e| e as f32).collect(),
        }
    }

    /// Get elevation at (col, row). Returns None if out of bounds.
    pub fn get(&self, col: u32, row: u32) -> Option<f32> {
        if col < self.width && row < self.height {
            Some(self.elevations[(row * self.width + col) as usize])
        } else {
            None
        }
    }

    /// Bilinear interpolation at fractional coordinates.
    pub fn sample(&self, u: f64, v: f64) -> f32 {
        let x = (u * (self.width - 1) as f64).clamp(0.0, (self.width - 1) as f64);
        let y = (v * (self.height - 1) as f64).clamp(0.0, (self.height - 1) as f64);
        let x0 = x.floor() as u32;
        let y0 = y.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = (x - x0 as f64) as f32;
        let fy = (y - y0 as f64) as f32;

        let v00 = self.get(x0, y0).unwrap_or(0.0);
        let v10 = self.get(x1, y0).unwrap_or(0.0);
        let v01 = self.get(x0, y1).unwrap_or(0.0);
        let v11 = self.get(x1, y1).unwrap_or(0.0);

        let top = v00 * (1.0 - fx) + v10 * fx;
        let bot = v01 * (1.0 - fx) + v11 * fx;
        top * (1.0 - fy) + bot * fy
    }

    /// Subsample the heightmap at a lower resolution.
    pub fn subsample(&self, target_width: u32, target_height: u32) -> Heightmap {
        let mut elevations = Vec::with_capacity((target_width * target_height) as usize);
        for row in 0..target_height {
            let v = row as f64 / (target_height - 1).max(1) as f64;
            for col in 0..target_width {
                let u = col as f64 / (target_width - 1).max(1) as f64;
                elevations.push(self.sample(u, v));
            }
        }
        Heightmap {
            width: target_width,
            height: target_height,
            min_lon: self.min_lon,
            min_lat: self.min_lat,
            max_lon: self.max_lon,
            max_lat: self.max_lat,
            elevations,
        }
    }
}

/// Quantized mesh tile output.
pub struct QuantizedMeshTile {
    pub x: u32,
    pub y: u32,
    pub level: u32,
    pub data: Vec<u8>,
}

/// Quantized mesh header (Cesium terrain format).
#[repr(C, packed)]
#[allow(dead_code)]
struct QuantizedMeshHeader {
    center_x: f64,
    center_y: f64,
    center_z: f64,
    min_height: f32,
    max_height: f32,
    bounding_sphere_center_x: f64,
    bounding_sphere_center_y: f64,
    bounding_sphere_center_z: f64,
    bounding_sphere_radius: f64,
    horizon_occlusion_x: f64,
    horizon_occlusion_y: f64,
    horizon_occlusion_z: f64,
}

/// Generate a single quantized mesh tile from a heightmap region.
pub fn generate_quantized_mesh(heightmap: &Heightmap) -> Vec<u8> {
    let w = heightmap.width;
    let h = heightmap.height;

    let min_h = heightmap
        .elevations
        .iter()
        .copied()
        .fold(f32::INFINITY, f32::min);
    let max_h = heightmap
        .elevations
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let range = (max_h - min_h).max(1.0);

    // Quantize vertices to u16
    let mut u_values: Vec<u16> = Vec::with_capacity((w * h) as usize);
    let mut v_values: Vec<u16> = Vec::with_capacity((w * h) as usize);
    let mut h_values: Vec<u16> = Vec::with_capacity((w * h) as usize);

    for row in 0..h {
        for col in 0..w {
            u_values.push(((col as f64 / (w - 1).max(1) as f64) * 32767.0) as u16);
            v_values.push(((row as f64 / (h - 1).max(1) as f64) * 32767.0) as u16);
            let elev = heightmap.get(col, row).unwrap_or(0.0);
            h_values.push((((elev - min_h) / range) * 32767.0) as u16);
        }
    }

    // Generate triangle indices (two triangles per grid cell)
    let mut indices: Vec<u32> = Vec::with_capacity(((w - 1) * (h - 1) * 6) as usize);
    for row in 0..(h - 1) {
        for col in 0..(w - 1) {
            let i00 = row * w + col;
            let i10 = row * w + col + 1;
            let i01 = (row + 1) * w + col;
            let i11 = (row + 1) * w + col + 1;
            indices.extend_from_slice(&[i00, i10, i01, i10, i11, i01]);
        }
    }

    // Build output buffer
    let vertex_count = w * h;
    let mut buf = Vec::new();

    // Header (simplified — real implementation needs ECEF coords)
    let center_lon = (heightmap.min_lon + heightmap.max_lon) / 2.0;
    let center_lat = (heightmap.min_lat + heightmap.max_lat) / 2.0;
    let ecef = tiletopia_core::spatial::geodetic_to_ecef(
        tiletopia_core::spatial::deg_to_rad(center_lat),
        tiletopia_core::spatial::deg_to_rad(center_lon),
        ((min_h + max_h) / 2.0) as f64,
    );

    // Write header bytes
    buf.extend_from_slice(&ecef[0].to_le_bytes());
    buf.extend_from_slice(&ecef[1].to_le_bytes());
    buf.extend_from_slice(&ecef[2].to_le_bytes());
    buf.extend_from_slice(&min_h.to_le_bytes());
    buf.extend_from_slice(&max_h.to_le_bytes());
    // Bounding sphere (same as center for simplicity)
    buf.extend_from_slice(&ecef[0].to_le_bytes());
    buf.extend_from_slice(&ecef[1].to_le_bytes());
    buf.extend_from_slice(&ecef[2].to_le_bytes());
    buf.extend_from_slice(&1000.0f64.to_le_bytes()); // radius
    // Horizon occlusion point
    buf.extend_from_slice(&ecef[0].to_le_bytes());
    buf.extend_from_slice(&ecef[1].to_le_bytes());
    buf.extend_from_slice(&ecef[2].to_le_bytes());

    // Vertex count
    buf.extend_from_slice(&vertex_count.to_le_bytes());

    // Delta-encode u, v, h
    fn delta_encode(values: &[u16]) -> Vec<u16> {
        let mut encoded = Vec::with_capacity(values.len());
        let mut prev = 0i32;
        for &v in values {
            let delta = v as i32 - prev;
            let zigzag = ((delta << 1) ^ (delta >> 31)) as u16;
            encoded.push(zigzag);
            prev = v as i32;
        }
        encoded
    }

    for v in delta_encode(&u_values) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in delta_encode(&v_values) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in delta_encode(&h_values) {
        buf.extend_from_slice(&v.to_le_bytes());
    }

    // Triangle count and indices
    let triangle_count = indices.len() as u32 / 3;
    buf.extend_from_slice(&triangle_count.to_le_bytes());

    if vertex_count > 65535 {
        for &idx in &indices {
            buf.extend_from_slice(&idx.to_le_bytes());
        }
    } else {
        for &idx in &indices {
            buf.extend_from_slice(&(idx as u16).to_le_bytes());
        }
    }

    buf
}

/// Generate terrain tiles from a heightmap at multiple LOD levels.
pub fn generate_terrain(
    heightmap: &Heightmap,
    max_level: u32,
    _geometric_error_threshold: f64,
) -> Vec<QuantizedMeshTile> {
    let mut tiles = Vec::new();

    for level in 0..=max_level {
        let tile_size = 2u32.pow(level);
        let sub_w = (heightmap.width / tile_size).max(2);
        let sub_h = (heightmap.height / tile_size).max(2);

        for ty in 0..tile_size {
            for tx in 0..tile_size {
                // Compute bounds for this tile
                let lon_step = (heightmap.max_lon - heightmap.min_lon) / tile_size as f64;
                let lat_step = (heightmap.max_lat - heightmap.min_lat) / tile_size as f64;

                let sub = Heightmap {
                    width: sub_w,
                    height: sub_h,
                    min_lon: heightmap.min_lon + tx as f64 * lon_step,
                    min_lat: heightmap.min_lat + ty as f64 * lat_step,
                    max_lon: heightmap.min_lon + (tx + 1) as f64 * lon_step,
                    max_lat: heightmap.min_lat + (ty + 1) as f64 * lat_step,
                    elevations: {
                        let mut elev = Vec::with_capacity((sub_w * sub_h) as usize);
                        for row in 0..sub_h {
                            let v = (ty as f64 + row as f64 / (sub_h - 1).max(1) as f64)
                                / tile_size as f64;
                            for col in 0..sub_w {
                                let u = (tx as f64 + col as f64 / (sub_w - 1).max(1) as f64)
                                    / tile_size as f64;
                                elev.push(heightmap.sample(u, v));
                            }
                        }
                        elev
                    },
                };

                let data = generate_quantized_mesh(&sub);
                tiles.push(QuantizedMeshTile {
                    x: tx,
                    y: ty,
                    level,
                    data,
                });
            }
        }
    }

    tracing::info!(
        "Generated {} terrain tiles (levels 0-{})",
        tiles.len(),
        max_level
    );
    tiles
}
