//! STAC catalog — SpatioTemporal Asset Catalog (OGC standard).
//!
//! The catalog root and the collection list describe what this server holds.
//! Item search is a proxy: `TILETOPIA_STAC_API` names an upstream STAC API and
//! [`search`] forwards bbox, datetime, collections and limit to its
//! `/search`, answering the upstream FeatureCollection unchanged so every
//! extension field a client reads survives. Unset, search refuses.

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;

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

/// Root of the STAC API item search is forwarded to, without a trailing slash,
/// as in `https://example.org/stac/v1`. Unset, `/api/v1/stac/search` refuses.
pub const UPSTREAM_API_ENV: &str = "TILETOPIA_STAC_API";

/// Items per page when the caller names no limit, matching the STAC API default.
pub const DEFAULT_LIMIT: u32 = 10;

/// Most items one search may ask for. A STAC API caps its own pages too, so
/// this only keeps a caller from asking the upstream for a page it will not send.
pub const MAX_LIMIT: u32 = 500;

/// How long a search waits on the upstream. A viewer pans while it waits, so a
/// hung catalog has to become an error rather than a held connection.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// The item-search parameters this proxy forwards.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchParams {
    pub bbox: Option<[f64; 4]>,
    /// A STAC datetime or interval, passed through as written.
    pub datetime: Option<String>,
    pub collections: Vec<String>,
    pub limit: u32,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            bbox: None,
            datetime: None,
            collections: Vec::new(),
            limit: DEFAULT_LIMIT,
        }
    }
}

impl SearchParams {
    /// Read the parameters out of their query-string form. The error is the
    /// message the 400 carries, so it names the parameter it read.
    pub fn from_query(
        bbox: Option<&str>,
        datetime: Option<&str>,
        collections: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Self, String> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 || limit > MAX_LIMIT {
            return Err(format!("limit={limit} is outside 1..={MAX_LIMIT}"));
        }
        Ok(Self {
            bbox: bbox
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(parse_bbox)
                .transpose()?,
            datetime: datetime
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string),
            collections: collections
                .into_iter()
                .flat_map(|list| list.split(','))
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect(),
            limit,
        })
    }

    /// The query the upstream `/search` is called with.
    fn query(&self) -> Vec<(&'static str, String)> {
        let mut query = vec![("limit", self.limit.to_string())];
        if let Some([west, south, east, north]) = self.bbox {
            query.push(("bbox", format!("{west},{south},{east},{north}")));
        }
        if !self.collections.is_empty() {
            query.push(("collections", self.collections.join(",")));
        }
        if let Some(datetime) = &self.datetime {
            query.push(("datetime", datetime.clone()));
        }
        query
    }
}

/// Four finite numbers is all this checks. Degree ranges and a west past east
/// are the upstream's to judge, since a bbox crossing the antimeridian is
/// written west greater than east and is a legal search.
fn parse_bbox(raw: &str) -> Result<[f64; 4], String> {
    let bad = |detail: String| format!("bbox {detail}, expected west,south,east,north in degrees");
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 4 {
        return Err(bad(format!("has {} values", parts.len())));
    }
    let mut bbox = [0.0; 4];
    for (slot, part) in bbox.iter_mut().zip(&parts) {
        *slot = part
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| bad(format!("has {part:?} where a number belongs")))?;
    }
    Ok(bbox)
}

/// Why a search could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("no STAC upstream is configured, set {UPSTREAM_API_ENV} to a STAC API root")]
    NoUpstream,
    #[error("{0}")]
    BadRequest(String),
    #[error("STAC upstream {api} could not be reached: {reason}")]
    Unreachable { api: String, reason: String },
    #[error("STAC upstream {api} answered {status}")]
    Rejected { api: String, status: u16 },
    #[error("STAC upstream {api} answered no item collection: {reason}")]
    Malformed { api: String, reason: String },
}

impl SearchError {
    pub fn status(&self) -> StatusCode {
        match self {
            SearchError::NoUpstream => StatusCode::SERVICE_UNAVAILABLE,
            SearchError::BadRequest(_) => StatusCode::BAD_REQUEST,
            SearchError::Unreachable { .. }
            | SearchError::Rejected { .. }
            | SearchError::Malformed { .. } => StatusCode::BAD_GATEWAY,
        }
    }
}

/// The upstream STAC API root, or `None` when none is configured.
pub fn upstream_api() -> Option<String> {
    std::env::var(UPSTREAM_API_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("tiletopia/", env!("CARGO_PKG_VERSION")))
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .expect("rustls http client")
    })
}

/// Forward an item search to the upstream STAC API and answer its item
/// collection unchanged.
pub async fn search(api: &str, params: &SearchParams) -> Result<serde_json::Value, SearchError> {
    let unreachable = |reason: String| SearchError::Unreachable {
        api: api.to_string(),
        reason,
    };
    let response = client()
        .get(format!("{api}/search"))
        .query(&params.query())
        .header(reqwest::header::ACCEPT, "application/geo+json")
        .send()
        .await
        .map_err(|e| unreachable(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(SearchError::Rejected {
            api: api.to_string(),
            status: status.as_u16(),
        });
    }

    let body: serde_json::Value = response.json().await.map_err(|e| SearchError::Malformed {
        api: api.to_string(),
        reason: e.to_string(),
    })?;

    // a catalog behind a captive portal answers 200 and html, and passing that
    // through as a FeatureCollection is what makes an empty map look like an
    // empty search
    if !body
        .get("features")
        .is_some_and(serde_json::Value::is_array)
    {
        return Err(SearchError::Malformed {
            api: api.to_string(),
            reason: "no features array".into(),
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::RawQuery;
    use axum::{Json, Router, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn root_catalog_conforms_to_item_search() {
        let catalog = root_catalog();
        assert_eq!(catalog.stac_version, "1.0.0");
        assert!(
            catalog
                .conforms_to
                .iter()
                .any(|c| c.ends_with("/item-search"))
        );
    }

    #[test]
    fn collections_are_listed() {
        let colls = collections();
        assert_eq!(colls.len(), 3);
        assert!(colls.iter().any(|c| c.id == "point-clouds"));
    }

    #[test]
    fn query_carries_every_parameter_given() {
        let params = SearchParams::from_query(
            Some("-122.5,37.7,-122.3,37.9"),
            Some("2026-01-01T00:00:00Z/.."),
            Some("sentinel-2-l2a, cop-dem-glo-30"),
            Some(25),
        )
        .unwrap();
        assert_eq!(params.bbox, Some([-122.5, 37.7, -122.3, 37.9]));
        assert_eq!(
            params.collections,
            vec!["sentinel-2-l2a".to_string(), "cop-dem-glo-30".to_string()]
        );
        let query = params.query();
        assert!(query.contains(&("bbox", "-122.5,37.7,-122.3,37.9".to_string())));
        assert!(query.contains(&("collections", "sentinel-2-l2a,cop-dem-glo-30".to_string())));
        assert!(query.contains(&("datetime", "2026-01-01T00:00:00Z/..".to_string())));
        assert!(query.contains(&("limit", "25".to_string())));
    }

    #[test]
    fn empty_query_asks_for_the_default_page() {
        let params = SearchParams::from_query(None, None, None, None).unwrap();
        assert_eq!(params, SearchParams::default());
        assert_eq!(params.query(), vec![("limit", DEFAULT_LIMIT.to_string())]);
    }

    #[test]
    fn a_malformed_bbox_is_refused() {
        for raw in ["1,2,3", "1,2,3,four", "1,2,3,4,5"] {
            let err = SearchParams::from_query(Some(raw), None, None, None).unwrap_err();
            assert!(err.starts_with("bbox "), "{raw}: {err}");
        }
    }

    #[test]
    fn a_limit_past_the_cap_is_refused() {
        for limit in [0, MAX_LIMIT + 1] {
            let err = SearchParams::from_query(None, None, None, Some(limit)).unwrap_err();
            assert!(err.contains("limit"), "{limit}: {err}");
        }
    }

    #[test]
    fn no_upstream_configured_is_a_503() {
        assert_eq!(
            SearchError::NoUpstream.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            SearchError::NoUpstream
                .to_string()
                .contains(UPSTREAM_API_ENV)
        );
    }

    /// Every query string the upstream was called with.
    type SeenQueries = Arc<Mutex<Vec<String>>>;

    /// A STAC API on loopback that records the query string it was called with
    /// and answers one item, so a search can be proven to reach the wire.
    async fn upstream(body: serde_json::Value, status: StatusCode) -> (String, SeenQueries) {
        let seen: SeenQueries = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().route(
            "/search",
            get(move |RawQuery(query): RawQuery| {
                let recorder = Arc::clone(&recorder);
                let body = body.clone();
                async move {
                    recorder.lock().unwrap().push(query.unwrap_or_default());
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn one_item_collection() -> serde_json::Value {
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "S2B_10SEG_20260801",
                "collection": "sentinel-2-l2a",
                "assets": { "red": { "href": "https://example.org/red.tif" } }
            }],
            "links": [{ "rel": "next", "href": "https://example.org/next" }]
        })
    }

    #[tokio::test]
    async fn search_forwards_every_parameter_and_answers_the_upstream_collection() {
        let (api, seen) = upstream(one_item_collection(), StatusCode::OK).await;
        let params = SearchParams::from_query(
            Some("-122.5,37.7,-122.3,37.9"),
            Some("2026-08-01T00:00:00Z/.."),
            Some("sentinel-2-l2a"),
            Some(3),
        )
        .unwrap();

        let body = search(&api, &params).await.unwrap();

        let query = seen.lock().unwrap()[0].clone();
        assert!(
            query.contains("bbox=-122.5%2C37.7%2C-122.3%2C37.9"),
            "{query}"
        );
        assert!(query.contains("collections=sentinel-2-l2a"), "{query}");
        assert!(
            query.contains("datetime=2026-08-01T00%3A00%3A00Z%2F.."),
            "{query}"
        );
        assert!(query.contains("limit=3"), "{query}");
        // the upstream body reaches the caller whole, links and all
        assert_eq!(body, one_item_collection());
    }

    #[tokio::test]
    async fn an_upstream_error_is_passed_on_as_a_bad_gateway() {
        let (api, _) = upstream(serde_json::json!({}), StatusCode::INTERNAL_SERVER_ERROR).await;
        let err = search(&api, &SearchParams::default()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[tokio::test]
    async fn an_upstream_that_answers_no_items_is_not_passed_through() {
        // 200 and json, but not an item collection: an empty map would be a lie
        let (api, _) = upstream(serde_json::json!({"message": "hello"}), StatusCode::OK).await;
        let err = search(&api, &SearchParams::default()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("no item collection"), "{err}");
    }

    #[tokio::test]
    async fn an_upstream_that_is_not_listening_is_an_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = search(&format!("http://{addr}"), &SearchParams::default())
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("could not be reached"), "{err}");
    }
}
