//! Mobile SDK support — endpoints optimized for mobile clients.
//!
//! Provides:
//! - Bandwidth-efficient tile protocols (vector tiles, progressive loading)
//! - Offline tile package downloads
//! - Device capability negotiation
//! - SDK configuration endpoints (React Native, Flutter, Swift, Kotlin)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Supported mobile platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Platform {
    #[serde(rename = "ios")]
    Ios,
    #[serde(rename = "android")]
    Android,
    #[serde(rename = "react-native")]
    ReactNative,
    #[serde(rename = "flutter")]
    Flutter,
}

/// Device capability report (sent by SDK on init).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub platform: Platform,
    pub sdk_version: String,
    pub screen_density: f32,
    pub gpu_tier: GpuTier,
    pub available_memory_mb: u32,
    pub network_type: NetworkType,
    pub supports_webgl2: bool,
    pub supports_3d_tiles: bool,
    pub max_texture_size: u32,
}

/// GPU capability tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GpuTier {
    Low,
    Medium,
    High,
}

/// Network connection type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NetworkType {
    Wifi,
    Cellular4G,
    Cellular5G,
    Offline,
}

/// SDK configuration returned to mobile clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkConfig {
    pub tile_endpoint: String,
    pub terrain_endpoint: String,
    pub max_concurrent_requests: u8,
    pub tile_cache_size_mb: u32,
    pub preferred_tile_size: u16,
    pub use_compressed_textures: bool,
    pub progressive_loading: bool,
    pub offline_enabled: bool,
    pub analytics_endpoint: Option<String>,
}

/// Offline tile package metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflinePackage {
    pub id: Uuid,
    pub name: String,
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub tile_count: u32,
    pub size_bytes: u64,
    pub format: TileFormat,
    pub created_at: DateTime<Utc>,
    pub download_url: String,
}

/// Tile format for offline packages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TileFormat {
    Pbf,
    Png,
    Webp,
    QuantizedMesh,
}

/// Generate SDK config based on device capabilities.
pub fn generate_sdk_config(caps: &DeviceCapabilities) -> SdkConfig {
    let (max_requests, cache_size, tile_size, compressed) = match caps.gpu_tier {
        GpuTier::High => (8, 512, 512, true),
        GpuTier::Medium => (4, 256, 256, true),
        GpuTier::Low => (2, 128, 256, false),
    };

    let progressive = matches!(
        caps.network_type,
        NetworkType::Cellular4G | NetworkType::Offline
    );

    SdkConfig {
        tile_endpoint: "/api/v1/tiles".into(),
        terrain_endpoint: "/api/v1/terrain".into(),
        max_concurrent_requests: max_requests,
        tile_cache_size_mb: cache_size,
        preferred_tile_size: tile_size,
        use_compressed_textures: compressed,
        progressive_loading: progressive,
        offline_enabled: true,
        analytics_endpoint: Some("/api/v1/analytics/mobile".into()),
    }
}

/// Available offline packages (demo data).
pub fn available_offline_packages() -> Vec<OfflinePackage> {
    vec![
        OfflinePackage {
            id: Uuid::new_v4(),
            name: "San Francisco Metro".into(),
            bounds: [-122.6, 37.6, -122.2, 37.9],
            min_zoom: 0,
            max_zoom: 14,
            tile_count: 48_562,
            size_bytes: 127 * 1024 * 1024,
            format: TileFormat::Pbf,
            created_at: Utc::now() - chrono::Duration::days(7),
            download_url: "/api/v1/mobile/offline/sf-metro".into(),
        },
        OfflinePackage {
            id: Uuid::new_v4(),
            name: "NYC Manhattan Terrain".into(),
            bounds: [-74.02, 40.70, -73.93, 40.80],
            min_zoom: 0,
            max_zoom: 12,
            tile_count: 12_400,
            size_bytes: 89 * 1024 * 1024,
            format: TileFormat::QuantizedMesh,
            created_at: Utc::now() - chrono::Duration::days(3),
            download_url: "/api/v1/mobile/offline/nyc-terrain".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sdk_config_high_end() {
        let caps = DeviceCapabilities {
            platform: Platform::Ios,
            sdk_version: "1.0.0".into(),
            screen_density: 3.0,
            gpu_tier: GpuTier::High,
            available_memory_mb: 4096,
            network_type: NetworkType::Wifi,
            supports_webgl2: true,
            supports_3d_tiles: true,
            max_texture_size: 4096,
        };
        let config = generate_sdk_config(&caps);
        assert_eq!(config.max_concurrent_requests, 8);
        assert_eq!(config.tile_cache_size_mb, 512);
        assert!(!config.progressive_loading);
    }

    #[test]
    fn test_sdk_config_low_end() {
        let caps = DeviceCapabilities {
            platform: Platform::Android,
            sdk_version: "1.0.0".into(),
            screen_density: 1.5,
            gpu_tier: GpuTier::Low,
            available_memory_mb: 1024,
            network_type: NetworkType::Cellular4G,
            supports_webgl2: false,
            supports_3d_tiles: false,
            max_texture_size: 2048,
        };
        let config = generate_sdk_config(&caps);
        assert_eq!(config.max_concurrent_requests, 2);
        assert_eq!(config.tile_cache_size_mb, 128);
        assert!(config.progressive_loading);
    }

    #[test]
    fn test_offline_packages() {
        let packages = available_offline_packages();
        assert_eq!(packages.len(), 2);
        assert!(packages[0].tile_count > 0);
    }
}
