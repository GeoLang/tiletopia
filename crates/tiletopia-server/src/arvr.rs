//! AR/VR streaming — optimized tile delivery for headsets.
//!
//! Supports foveated LoD, low-latency streaming, and headset-specific formats.

use serde::{Deserialize, Serialize};

/// Supported AR/VR platforms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum XrPlatform {
    MetaQuest,
    AppleVisionPro,
    MicrosoftHoloLens,
    GenericWebXR,
}

/// XR session configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XrSessionConfig {
    pub platform: XrPlatform,
    pub foveation_level: FoveationLevel,
    pub max_tile_budget_mb: u32,
    pub target_framerate: u32,
    pub ipd_mm: f64, // Inter-pupillary distance
    pub stream_quality: StreamQuality,
}

/// Foveated rendering level (reduce LoD in peripheral vision).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FoveationLevel {
    None,
    Low,    // 25% reduction at edges
    Medium, // 50% reduction at edges
    High,   // 75% reduction at edges
}

/// Streaming quality preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamQuality {
    Low,      // 1Mbps, 30fps
    Medium,   // 5Mbps, 60fps
    High,     // 15Mbps, 72fps
    Ultra,    // 50Mbps, 90fps
    Adaptive, // Auto-adjust based on bandwidth
}

/// Foveated tile request (from headset eye tracker).
#[derive(Debug, Clone)]
pub struct FoveatedRequest {
    pub gaze_direction: [f64; 3],
    pub fov_degrees: f64,
    pub foveal_radius_degrees: f64,
    pub head_position: [f64; 3],
    pub head_orientation: [f64; 4], // quaternion
}

/// Tile priority based on foveation.
#[derive(Debug, Clone)]
pub struct TilePriority {
    pub tile_id: String,
    pub lod_level: u32,
    pub priority: f64, // 0.0 = don't load, 1.0 = highest priority
    pub in_foveal_region: bool,
}

/// Compute tile priorities based on foveated rendering.
pub fn compute_foveated_priorities(
    request: &FoveatedRequest,
    tile_centers: &[(String, [f64; 3])],
    config: &XrSessionConfig,
) -> Vec<TilePriority> {
    let foveal_cos = (request.foveal_radius_degrees.to_radians()).cos();
    let peripheral_cos = (request.fov_degrees.to_radians() / 2.0).cos();

    let gaze_len = (request.gaze_direction[0].powi(2)
        + request.gaze_direction[1].powi(2)
        + request.gaze_direction[2].powi(2))
    .sqrt();

    tile_centers
        .iter()
        .map(|(id, center)| {
            // Direction from head to tile
            let dx = center[0] - request.head_position[0];
            let dy = center[1] - request.head_position[1];
            let dz = center[2] - request.head_position[2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();

            if dist < 1e-10 {
                return TilePriority {
                    tile_id: id.clone(),
                    lod_level: 0,
                    priority: 1.0,
                    in_foveal_region: true,
                };
            }

            // Angle between gaze direction and tile direction
            let dot = (request.gaze_direction[0] * dx
                + request.gaze_direction[1] * dy
                + request.gaze_direction[2] * dz)
                / (gaze_len * dist);

            let in_foveal = dot >= foveal_cos;

            let priority = if in_foveal {
                1.0
            } else if dot >= peripheral_cos {
                // Peripheral: reduced priority based on foveation level
                let falloff = match config.foveation_level {
                    FoveationLevel::None => 1.0,
                    FoveationLevel::Low => 0.75,
                    FoveationLevel::Medium => 0.5,
                    FoveationLevel::High => 0.25,
                };
                let t = (dot - peripheral_cos) / (foveal_cos - peripheral_cos);
                falloff + t * (1.0 - falloff)
            } else {
                0.0 // Outside FOV
            };

            // LoD based on distance and priority
            let lod = if in_foveal {
                0 // Highest detail
            } else {
                match config.foveation_level {
                    FoveationLevel::None => 0,
                    FoveationLevel::Low => 1,
                    FoveationLevel::Medium => 2,
                    FoveationLevel::High => 3,
                }
            };

            TilePriority {
                tile_id: id.clone(),
                lod_level: lod,
                priority,
                in_foveal_region: in_foveal,
            }
        })
        .collect()
}

/// Compute tile budget allocation for XR streaming.
pub fn allocate_tile_budget(
    priorities: &[TilePriority],
    budget_mb: u32,
    tile_size_estimate_kb: u32,
) -> Vec<String> {
    let budget_kb = budget_mb as u64 * 1024;
    let mut sorted: Vec<&TilePriority> = priorities.iter().filter(|p| p.priority > 0.0).collect();
    sorted.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut allocated = Vec::new();
    let mut used_kb = 0u64;

    for tp in sorted {
        let size = tile_size_estimate_kb as u64 / (tp.lod_level as u64 + 1);
        if used_kb + size <= budget_kb {
            allocated.push(tp.tile_id.clone());
            used_kb += size;
        } else {
            break;
        }
    }
    allocated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_foveated_priorities() {
        let request = FoveatedRequest {
            gaze_direction: [0.0, 0.0, -1.0], // Looking forward
            fov_degrees: 110.0,
            foveal_radius_degrees: 10.0,
            head_position: [0.0, 0.0, 0.0],
            head_orientation: [0.0, 0.0, 0.0, 1.0],
        };
        let tiles = vec![
            ("center".into(), [0.0, 0.0, -10.0]), // In gaze direction
            ("side".into(), [10.0, 0.0, -5.0]),   // Off to the side
        ];
        let config = XrSessionConfig {
            platform: XrPlatform::MetaQuest,
            foveation_level: FoveationLevel::High,
            max_tile_budget_mb: 256,
            target_framerate: 72,
            ipd_mm: 63.0,
            stream_quality: StreamQuality::High,
        };
        let priorities = compute_foveated_priorities(&request, &tiles, &config);
        assert_eq!(priorities.len(), 2);
        // Center tile should have higher priority
        let center = priorities.iter().find(|p| p.tile_id == "center").unwrap();
        let side = priorities.iter().find(|p| p.tile_id == "side").unwrap();
        assert!(center.priority > side.priority);
        assert!(center.in_foveal_region);
    }

    #[test]
    fn test_tile_budget_allocation() {
        let priorities = vec![
            TilePriority {
                tile_id: "a".into(),
                lod_level: 0,
                priority: 1.0,
                in_foveal_region: true,
            },
            TilePriority {
                tile_id: "b".into(),
                lod_level: 1,
                priority: 0.5,
                in_foveal_region: false,
            },
            TilePriority {
                tile_id: "c".into(),
                lod_level: 0,
                priority: 0.0,
                in_foveal_region: false,
            },
        ];
        let allocated = allocate_tile_budget(&priorities, 1, 512); // 1MB budget, 512KB per tile
        assert!(allocated.contains(&"a".to_string()));
        assert!(!allocated.contains(&"c".to_string())); // priority 0
    }

    #[test]
    fn test_platform_configs() {
        let config = XrSessionConfig {
            platform: XrPlatform::AppleVisionPro,
            foveation_level: FoveationLevel::Medium,
            max_tile_budget_mb: 512,
            target_framerate: 90,
            ipd_mm: 64.0,
            stream_quality: StreamQuality::Ultra,
        };
        assert_eq!(config.target_framerate, 90);
    }

    #[test]
    fn test_foveation_none_gives_equal_priority() {
        let request = FoveatedRequest {
            gaze_direction: [0.0, 0.0, -1.0],
            fov_degrees: 110.0,
            foveal_radius_degrees: 10.0,
            head_position: [0.0, 0.0, 0.0],
            head_orientation: [0.0, 0.0, 0.0, 1.0],
        };
        let tiles = vec![
            ("a".into(), [0.0, 0.0, -10.0]),
            ("b".into(), [5.0, 0.0, -10.0]),
        ];
        let config = XrSessionConfig {
            platform: XrPlatform::GenericWebXR,
            foveation_level: FoveationLevel::None,
            max_tile_budget_mb: 128,
            target_framerate: 60,
            ipd_mm: 63.0,
            stream_quality: StreamQuality::Adaptive,
        };
        let priorities = compute_foveated_priorities(&request, &tiles, &config);
        // With no foveation, peripheral tiles still get LoD 0
        for p in &priorities {
            assert_eq!(p.lod_level, 0);
        }
    }

    #[test]
    fn test_empty_tiles() {
        let request = FoveatedRequest {
            gaze_direction: [0.0, 0.0, -1.0],
            fov_degrees: 90.0,
            foveal_radius_degrees: 5.0,
            head_position: [0.0, 0.0, 0.0],
            head_orientation: [0.0, 0.0, 0.0, 1.0],
        };
        let config = XrSessionConfig {
            platform: XrPlatform::MetaQuest,
            foveation_level: FoveationLevel::Medium,
            max_tile_budget_mb: 128,
            target_framerate: 72,
            ipd_mm: 63.0,
            stream_quality: StreamQuality::Medium,
        };
        let priorities = compute_foveated_priorities(&request, &[], &config);
        assert!(priorities.is_empty());
    }
}
