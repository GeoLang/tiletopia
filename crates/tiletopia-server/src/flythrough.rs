//! Cinematic flythrough — render camera paths to video.

use serde::{Deserialize, Serialize};

/// A keyframe in a camera path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub time_secs: f64,
    pub position: [f64; 3],
    pub look_at: [f64; 3],
    pub fov_degrees: f64,
    pub easing: EasingFunction,
}

/// Easing function for interpolation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EasingFunction {
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
    CubicBezier { x1: f64, y1: f64, x2: f64, y2: f64 },
}

/// Flythrough export configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlythroughConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub output_format: VideoFormat,
    pub quality: VideoQuality,
    pub antialiasing: bool,
    pub motion_blur: bool,
}

impl Default for FlythroughConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            output_format: VideoFormat::Mp4,
            quality: VideoQuality::High,
            antialiasing: true,
            motion_blur: false,
        }
    }
}

/// Output video format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VideoFormat {
    Mp4,
    WebM,
    Gif,
    ImageSequence,
}

/// Video quality preset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VideoQuality {
    Draft, // Fast render, low quality
    Medium,
    High,
    Ultra, // Slow render, maximum quality
}

/// A complete flythrough definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flythrough {
    pub id: String,
    pub name: String,
    pub keyframes: Vec<Keyframe>,
    pub config: FlythroughConfig,
    pub tileset_ids: Vec<String>,
    pub total_duration_secs: f64,
}

/// Interpolate between two keyframes.
pub fn interpolate_keyframe(a: &Keyframe, b: &Keyframe, t: f64) -> Keyframe {
    let t_eased = apply_easing(t, &b.easing);

    Keyframe {
        time_secs: a.time_secs + (b.time_secs - a.time_secs) * t_eased,
        position: [
            a.position[0] + (b.position[0] - a.position[0]) * t_eased,
            a.position[1] + (b.position[1] - a.position[1]) * t_eased,
            a.position[2] + (b.position[2] - a.position[2]) * t_eased,
        ],
        look_at: [
            a.look_at[0] + (b.look_at[0] - a.look_at[0]) * t_eased,
            a.look_at[1] + (b.look_at[1] - a.look_at[1]) * t_eased,
            a.look_at[2] + (b.look_at[2] - a.look_at[2]) * t_eased,
        ],
        fov_degrees: a.fov_degrees + (b.fov_degrees - a.fov_degrees) * t_eased,
        easing: b.easing.clone(),
    }
}

/// Apply easing function to a t value (0.0 to 1.0).
pub fn apply_easing(t: f64, easing: &EasingFunction) -> f64 {
    match easing {
        EasingFunction::Linear => t,
        EasingFunction::EaseIn => t * t,
        EasingFunction::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
        EasingFunction::EaseInOut => {
            if t < 0.5 {
                2.0 * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
            }
        }
        EasingFunction::CubicBezier { x1, y1, x2, y2 } => {
            // Simplified cubic bezier (Newton's method for X, then evaluate Y)
            cubic_bezier_y(t, *x1, *y1, *x2, *y2)
        }
    }
}

fn cubic_bezier_y(t: f64, _x1: f64, y1: f64, _x2: f64, y2: f64) -> f64 {
    // Simplified: assume t maps linearly to bezier parameter
    let mt = 1.0 - t;
    3.0 * mt * mt * t * y1 + 3.0 * mt * t * t * y2 + t * t * t
}

/// Generate all frame camera positions for a flythrough.
pub fn generate_frame_cameras(flythrough: &Flythrough) -> Vec<Keyframe> {
    if flythrough.keyframes.is_empty() {
        return Vec::new();
    }
    if flythrough.keyframes.len() == 1 {
        let total_frames = (flythrough.total_duration_secs * flythrough.config.fps as f64) as usize;
        return vec![flythrough.keyframes[0].clone(); total_frames.max(1)];
    }

    let total_frames =
        (flythrough.total_duration_secs * flythrough.config.fps as f64).ceil() as usize;
    let mut frames = Vec::with_capacity(total_frames);

    for frame_idx in 0..total_frames {
        let time = frame_idx as f64 / flythrough.config.fps as f64;

        // Find surrounding keyframes
        let mut seg_start = 0;
        for i in 0..flythrough.keyframes.len() - 1 {
            if time >= flythrough.keyframes[i].time_secs
                && time <= flythrough.keyframes[i + 1].time_secs
            {
                seg_start = i;
                break;
            }
            if i == flythrough.keyframes.len() - 2 {
                seg_start = i;
            }
        }

        let a = &flythrough.keyframes[seg_start];
        let b = &flythrough.keyframes[(seg_start + 1).min(flythrough.keyframes.len() - 1)];

        let seg_duration = b.time_secs - a.time_secs;
        let t = if seg_duration > 0.0 {
            ((time - a.time_secs) / seg_duration).clamp(0.0, 1.0)
        } else {
            0.0
        };

        frames.push(interpolate_keyframe(a, b, t));
    }

    frames
}

/// Estimate render time for a flythrough.
pub fn estimate_render_time_secs(flythrough: &Flythrough) -> f64 {
    let total_frames = flythrough.total_duration_secs * flythrough.config.fps as f64;
    let frame_time = match flythrough.config.quality {
        VideoQuality::Draft => 0.1,
        VideoQuality::Medium => 0.5,
        VideoQuality::High => 2.0,
        VideoQuality::Ultra => 8.0,
    };
    total_frames * frame_time
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_interpolation() {
        let a = Keyframe {
            time_secs: 0.0,
            position: [0.0, 0.0, 0.0],
            look_at: [0.0, 0.0, -1.0],
            fov_degrees: 60.0,
            easing: EasingFunction::Linear,
        };
        let b = Keyframe {
            time_secs: 1.0,
            position: [10.0, 0.0, 0.0],
            look_at: [10.0, 0.0, -1.0],
            fov_degrees: 90.0,
            easing: EasingFunction::Linear,
        };
        let mid = interpolate_keyframe(&a, &b, 0.5);
        assert!((mid.position[0] - 5.0).abs() < 1e-10);
        assert!((mid.fov_degrees - 75.0).abs() < 1e-10);
    }

    #[test]
    fn test_ease_in() {
        let t = apply_easing(0.5, &EasingFunction::EaseIn);
        assert!((t - 0.25).abs() < 1e-10); // t^2
    }

    #[test]
    fn test_generate_frames() {
        let flythrough = Flythrough {
            id: "ft-1".into(),
            name: "Test".into(),
            keyframes: vec![
                Keyframe {
                    time_secs: 0.0,
                    position: [0.0; 3],
                    look_at: [0.0, 0.0, -1.0],
                    fov_degrees: 60.0,
                    easing: EasingFunction::Linear,
                },
                Keyframe {
                    time_secs: 1.0,
                    position: [10.0, 0.0, 0.0],
                    look_at: [10.0, 0.0, -1.0],
                    fov_degrees: 60.0,
                    easing: EasingFunction::Linear,
                },
            ],
            config: FlythroughConfig {
                fps: 10,
                ..Default::default()
            },
            tileset_ids: vec![],
            total_duration_secs: 1.0,
        };
        let frames = generate_frame_cameras(&flythrough);
        assert_eq!(frames.len(), 10);
        // First frame at origin
        assert!((frames[0].position[0]).abs() < 1e-5);
        // Last frame near destination
        assert!((frames[9].position[0] - 9.0).abs() < 2.0);
    }

    #[test]
    fn test_estimate_render_time() {
        let flythrough = Flythrough {
            id: "ft-2".into(),
            name: "Test".into(),
            keyframes: vec![],
            config: FlythroughConfig {
                fps: 30,
                ..Default::default()
            },
            tileset_ids: vec![],
            total_duration_secs: 10.0,
        };
        let time = estimate_render_time_secs(&flythrough);
        // 300 frames * 2.0s per frame (High quality) = 600s
        assert!((time - 600.0).abs() < 1e-5);
    }

    #[test]
    fn test_ease_in_out() {
        let t0 = apply_easing(0.0, &EasingFunction::EaseInOut);
        let t1 = apply_easing(1.0, &EasingFunction::EaseInOut);
        assert!(t0.abs() < 1e-10);
        assert!((t1 - 1.0).abs() < 1e-10);
    }
}
