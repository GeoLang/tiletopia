#[cfg(test)]
mod tests {
    use tiletopia_terrain::{generate_quantized_mesh, generate_terrain, Heightmap};

    fn flat_heightmap(width: u32, height: u32, elevation: f32) -> Heightmap {
        Heightmap {
            width,
            height,
            min_lon: -1.0,
            min_lat: -1.0,
            max_lon: 1.0,
            max_lat: 1.0,
            elevations: vec![elevation; (width * height) as usize],
        }
    }

    fn sloped_heightmap(width: u32, height: u32) -> Heightmap {
        let mut elevations = Vec::with_capacity((width * height) as usize);
        for row in 0..height {
            for col in 0..width {
                elevations.push((col + row) as f32);
            }
        }
        Heightmap {
            width,
            height,
            min_lon: -1.0,
            min_lat: -1.0,
            max_lon: 1.0,
            max_lat: 1.0,
            elevations,
        }
    }

    #[test]
    fn quantized_mesh_produces_bytes() {
        let hm = flat_heightmap(4, 4, 100.0);
        let data = generate_quantized_mesh(&hm);
        assert!(!data.is_empty());
        // Header is 88 bytes + vertex count (4 bytes)
        assert!(data.len() > 92);
    }

    #[test]
    fn terrain_generation_produces_tiles() {
        let hm = sloped_heightmap(16, 16);
        let tiles = generate_terrain(&hm, 2, 10.0);
        // Level 0: 1 tile, level 1: 4 tiles, level 2: 16 tiles = 21
        assert_eq!(tiles.len(), 21);
        assert!(tiles.iter().all(|t| !t.data.is_empty()));
    }

    #[test]
    fn heightmap_sample_bilinear() {
        let hm = sloped_heightmap(3, 3);
        // Corner should be exact
        assert!((hm.sample(0.0, 0.0) - 0.0).abs() < 0.01);
        // Center should interpolate
        let center = hm.sample(0.5, 0.5);
        assert!(center > 0.0);
    }

    #[test]
    fn heightmap_subsample() {
        let hm = sloped_heightmap(10, 10);
        let sub = hm.subsample(5, 5);
        assert_eq!(sub.width, 5);
        assert_eq!(sub.height, 5);
        assert_eq!(sub.elevations.len(), 25);
    }

    #[test]
    fn heightmap_from_ingest() {
        let ingest_hm = tiletopia_ingest::Heightmap {
            width: 4,
            height: 4,
            elevations: vec![100.0; 16],
            bounds: [-1.0, -1.0, 1.0, 1.0],
            nodata: None,
        };
        let hm = Heightmap::from_ingest(&ingest_hm);
        assert_eq!(hm.width, 4);
        assert_eq!(hm.height, 4);
        assert_eq!(hm.elevations.len(), 16);
    }
}
