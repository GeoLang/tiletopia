//! STAC catalog — SpatioTemporal Asset Catalog (OGC standard).
//!
//! Implements the STAC specification for geospatial metadata:
//! - Catalog (root container)
//! - Collections (grouped items)
//! - Items (individual assets with spatiotemporal extent)
//! - Extensions (eo, sar, pointcloud, etc.)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// STAC Catalog (root).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacCatalog {
    #[serde(rename = "type")]
    pub catalog_type: String, // "Catalog"
    pub id: String,
    pub title: String,
    pub description: String,
    pub stac_version: String,
    pub links: Vec<StacLink>,
    #[serde(rename = "conformsTo")]
    pub conforms_to: Vec<String>,
}

/// STAC Collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacCollection {
    #[serde(rename = "type")]
    pub collection_type: String, // "Collection"
    pub id: String,
    pub title: String,
    pub description: String,
    pub license: String,
    pub extent: Extent,
    pub providers: Vec<Provider>,
    pub summaries: serde_json::Value,
    pub links: Vec<StacLink>,
    pub item_count: u32,
}

/// STAC Item (individual asset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacItem {
    #[serde(rename = "type")]
    pub item_type: String, // "Feature"
    pub stac_version: String,
    pub id: String,
    pub geometry: GeoJsonGeometry,
    pub bbox: [f64; 4],
    pub properties: StacProperties,
    pub assets: std::collections::HashMap<String, StacAsset>,
    pub links: Vec<StacLink>,
    pub collection: String,
}

/// GeoJSON geometry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoJsonGeometry {
    #[serde(rename = "type")]
    pub geom_type: String,
    pub coordinates: serde_json::Value,
}

/// STAC item properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacProperties {
    pub datetime: Option<DateTime<Utc>>,
    pub start_datetime: Option<DateTime<Utc>>,
    pub end_datetime: Option<DateTime<Utc>>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub title: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// STAC asset (file reference).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacAsset {
    pub href: String,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub media_type: Option<String>,
    pub roles: Vec<String>,
}

/// Spatial and temporal extent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extent {
    pub spatial: SpatialExtent,
    pub temporal: TemporalExtent,
}

/// Spatial extent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialExtent {
    pub bbox: Vec<[f64; 4]>,
}

/// Temporal extent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalExtent {
    pub interval: Vec<[Option<DateTime<Utc>>; 2]>,
}

/// STAC link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacLink {
    pub rel: String,
    pub href: String,
    #[serde(rename = "type")]
    pub link_type: Option<String>,
    pub title: Option<String>,
}

/// Data provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub roles: Vec<String>,
    pub url: Option<String>,
}

/// Generate the TileTopia STAC catalog.
pub fn root_catalog() -> StacCatalog {
    StacCatalog {
        catalog_type: "Catalog".into(),
        id: "tiletopia".into(),
        title: "TileTopia STAC Catalog".into(),
        description: "SpatioTemporal Asset Catalog for all managed geospatial datasets".into(),
        stac_version: "1.0.0".into(),
        links: vec![
            StacLink {
                rel: "self".into(),
                href: "/api/v1/stac".into(),
                link_type: Some("application/json".into()),
                title: None,
            },
            StacLink {
                rel: "root".into(),
                href: "/api/v1/stac".into(),
                link_type: Some("application/json".into()),
                title: None,
            },
            StacLink {
                rel: "child".into(),
                href: "/api/v1/stac/collections/point-clouds".into(),
                link_type: Some("application/json".into()),
                title: Some("Point Clouds".into()),
            },
            StacLink {
                rel: "child".into(),
                href: "/api/v1/stac/collections/terrain".into(),
                link_type: Some("application/json".into()),
                title: Some("Terrain DEMs".into()),
            },
            StacLink {
                rel: "child".into(),
                href: "/api/v1/stac/collections/bim-models".into(),
                link_type: Some("application/json".into()),
                title: Some("BIM Models".into()),
            },
        ],
        conforms_to: vec![
            "https://api.stacspec.org/v1.0.0/core".into(),
            "https://api.stacspec.org/v1.0.0/collections".into(),
            "https://api.stacspec.org/v1.0.0/item-search".into(),
        ],
    }
}

/// Demo collections.
pub fn collections() -> Vec<StacCollection> {
    vec![
        StacCollection {
            collection_type: "Collection".into(),
            id: "point-clouds".into(),
            title: "Point Cloud Datasets".into(),
            description: "LiDAR and photogrammetry point clouds managed in TileTopia".into(),
            license: "proprietary".into(),
            extent: Extent {
                spatial: SpatialExtent {
                    bbox: vec![[-180.0, -90.0, 180.0, 90.0]],
                },
                temporal: TemporalExtent {
                    interval: vec![[Some(Utc::now() - chrono::Duration::days(365)), None]],
                },
            },
            providers: vec![Provider {
                name: "TileTopia".into(),
                roles: vec!["host".into(), "processor".into()],
                url: Some("https://tiletopia.dev".into()),
            }],
            summaries: serde_json::json!({
                "pc:type": ["lidar", "photogrammetry"],
                "pc:encoding": ["LAS", "LAZ"],
                "pc:count": { "minimum": 1000000, "maximum": 500000000 }
            }),
            links: vec![],
            item_count: 47,
        },
        StacCollection {
            collection_type: "Collection".into(),
            id: "terrain".into(),
            title: "Terrain / DEM Datasets".into(),
            description: "Digital Elevation Models and generated terrain tiles".into(),
            license: "various".into(),
            extent: Extent {
                spatial: SpatialExtent {
                    bbox: vec![[-180.0, -90.0, 180.0, 90.0]],
                },
                temporal: TemporalExtent {
                    interval: vec![[Some(Utc::now() - chrono::Duration::days(730)), None]],
                },
            },
            providers: vec![
                Provider {
                    name: "Copernicus".into(),
                    roles: vec!["producer".into()],
                    url: Some("https://spacedata.copernicus.eu".into()),
                },
                Provider {
                    name: "TileTopia".into(),
                    roles: vec!["host".into()],
                    url: None,
                },
            ],
            summaries: serde_json::json!({
                "gsd": [30, 10, 1],
                "eo:bands": [{"name": "elevation", "common_name": "dem"}]
            }),
            links: vec![],
            item_count: 16,
        },
        StacCollection {
            collection_type: "Collection".into(),
            id: "bim-models".into(),
            title: "BIM / 3D Models".into(),
            description: "IFC, glTF, and CityGML models with construction metadata".into(),
            license: "proprietary".into(),
            extent: Extent {
                spatial: SpatialExtent {
                    bbox: vec![[-122.5, 37.7, -122.3, 37.9]],
                },
                temporal: TemporalExtent {
                    interval: vec![[Some(Utc::now() - chrono::Duration::days(180)), None]],
                },
            },
            providers: vec![Provider {
                name: "TileTopia".into(),
                roles: vec!["host".into()],
                url: None,
            }],
            summaries: serde_json::json!({
                "formats": ["IFC4", "glTF", "CityGML"],
                "lod": [1, 2, 3, 4]
            }),
            links: vec![],
            item_count: 23,
        },
    ]
}

/// Search items by bounding box and datetime.
pub fn search_items(
    _bbox: Option<[f64; 4]>,
    _datetime: Option<&str>,
    _collections: Option<&[String]>,
    _limit: usize,
) -> Vec<StacItem> {
    // Demo: return a sample item
    let mut assets = std::collections::HashMap::new();
    assets.insert(
        "data".into(),
        StacAsset {
            href: "/api/v1/assets/abc123/tileset.json".into(),
            title: Some("3D Tileset".into()),
            media_type: Some("application/json".into()),
            roles: vec!["data".into()],
        },
    );
    assets.insert(
        "thumbnail".into(),
        StacAsset {
            href: "/api/v1/assets/abc123/thumbnail.png".into(),
            title: Some("Thumbnail".into()),
            media_type: Some("image/png".into()),
            roles: vec!["thumbnail".into()],
        },
    );

    vec![StacItem {
        item_type: "Feature".into(),
        stac_version: "1.0.0".into(),
        id: Uuid::new_v4().to_string(),
        geometry: GeoJsonGeometry {
            geom_type: "Polygon".into(),
            coordinates: serde_json::json!([[
                [-122.42, 37.77],
                [-122.41, 37.77],
                [-122.41, 37.78],
                [-122.42, 37.78],
                [-122.42, 37.77]
            ]]),
        },
        bbox: [-122.42, 37.77, -122.41, 37.78],
        properties: StacProperties {
            datetime: Some(Utc::now() - chrono::Duration::days(7)),
            start_datetime: None,
            end_datetime: None,
            created: Utc::now() - chrono::Duration::days(7),
            updated: Utc::now(),
            title: Some("Highway 101 Bridge LiDAR Scan".into()),
            extra: serde_json::json!({
                "pc:count": 187000000,
                "pc:type": "lidar",
                "pc:encoding": "LAZ",
                "pc:schemas": [
                    {"name": "X", "size": 8, "type": "floating"},
                    {"name": "Y", "size": 8, "type": "floating"},
                    {"name": "Z", "size": 8, "type": "floating"},
                    {"name": "Intensity", "size": 2, "type": "unsigned"},
                    {"name": "Classification", "size": 1, "type": "unsigned"}
                ]
            }),
        },
        assets,
        links: vec![],
        collection: "point-clouds".into(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_catalog() {
        let catalog = root_catalog();
        assert_eq!(catalog.stac_version, "1.0.0");
        assert_eq!(catalog.links.len(), 5);
    }

    #[test]
    fn test_collections() {
        let colls = collections();
        assert_eq!(colls.len(), 3);
        assert!(colls.iter().any(|c| c.id == "point-clouds"));
    }

    #[test]
    fn test_search_items() {
        let items = search_items(None, None, None, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_type, "Feature");
    }
}
