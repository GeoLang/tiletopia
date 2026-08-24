//! STAC catalog — SpatioTemporal Asset Catalog (OGC standard).
//!
//! The catalog root is this server's own. Everything under it is a proxy:
//! `TILETOPIA_STAC_API` names an upstream STAC API, [`search`] forwards bbox,
//! datetime, collections and limit to its `/search` and [`collections`] asks it
//! for `/collections`, each answering the upstream body unchanged so every
//! extension field a client reads survives. Unset, both refuse and the root
//! advertises neither.

use axum::http::StatusCode;
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

/// STAC link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacLink {
    pub rel: String,
    pub href: String,
    #[serde(rename = "type")]
    pub link_type: Option<String>,
    pub title: Option<String>,
}

/// The catalog root. With no upstream configured neither the collection list
/// nor item search can answer, so the root links to neither and claims only the
/// core conformance class.
///
/// The collections class is never claimed: it asks for a `/collections/{id}`
/// route this server does not have.
pub fn root_catalog(has_upstream: bool) -> StacCatalog {
    let link = |rel: &str, href: &str| StacLink {
        rel: rel.into(),
        href: href.into(),
        link_type: Some("application/json".into()),
        title: None,
    };
    let mut links = vec![link("self", "/api/v1/stac"), link("root", "/api/v1/stac")];
    let mut conforms_to = vec!["https://api.stacspec.org/v1.0.0/core".to_string()];
    if has_upstream {
        links.push(link("data", "/api/v1/stac/collections"));
        links.push(link("search", "/api/v1/stac/search"));
        conforms_to.push("https://api.stacspec.org/v1.0.0/item-search".to_string());
    }
    StacCatalog {
        catalog_type: "Catalog".into(),
        id: "tiletopia".into(),
        title: "TileTopia STAC Catalog".into(),
        description: "SpatioTemporal Asset Catalog for all managed geospatial datasets".into(),
        stac_version: "1.0.0".into(),
        links,
        conforms_to,
    }
}

/// Root of the STAC API calls are forwarded to, without a trailing slash, as in
/// `https://example.org/stac/v1`. Unset, `/api/v1/stac/search` and
/// `/api/v1/stac/collections` refuse.
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

/// Why an upstream call could not be answered.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("no STAC upstream is configured, set {UPSTREAM_API_ENV} to a STAC API root")]
    NoUpstream,
    #[error("{0}")]
    BadRequest(String),
    #[error("STAC upstream {api} could not be reached: {reason}")]
    Unreachable { api: String, reason: String },
    #[error("STAC upstream {api} answered {status}")]
    Rejected { api: String, status: u16 },
    #[error("STAC upstream {api} answered no {expected}: {reason}")]
    Malformed {
        api: String,
        expected: &'static str,
        reason: String,
    },
}

impl UpstreamError {
    pub fn status(&self) -> StatusCode {
        match self {
            UpstreamError::NoUpstream => StatusCode::SERVICE_UNAVAILABLE,
            UpstreamError::BadRequest(_) => StatusCode::BAD_REQUEST,
            UpstreamError::Unreachable { .. }
            | UpstreamError::Rejected { .. }
            | UpstreamError::Malformed { .. } => StatusCode::BAD_GATEWAY,
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

/// One call this proxy makes on the upstream: what it asks for, and the array a
/// client reads out of the answer.
struct UpstreamCall {
    /// Path under the API root, as in `/search`.
    path: &'static str,
    accept: &'static str,
    array: &'static str,
    /// What the answer is called in an error message.
    expected: &'static str,
}

const ITEM_SEARCH: UpstreamCall = UpstreamCall {
    path: "/search",
    accept: "application/geo+json",
    array: "features",
    expected: "item collection",
};

const COLLECTION_LIST: UpstreamCall = UpstreamCall {
    path: "/collections",
    accept: "application/json",
    array: "collections",
    expected: "collection list",
};

async fn proxy(
    api: &str,
    call: &UpstreamCall,
    query: &[(&'static str, String)],
) -> Result<serde_json::Value, UpstreamError> {
    let malformed = |reason: String| UpstreamError::Malformed {
        api: api.to_string(),
        expected: call.expected,
        reason,
    };
    let response = client()
        .get(format!("{api}{}", call.path))
        .query(query)
        .header(reqwest::header::ACCEPT, call.accept)
        .send()
        .await
        .map_err(|e| UpstreamError::Unreachable {
            api: api.to_string(),
            reason: e.to_string(),
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(UpstreamError::Rejected {
            api: api.to_string(),
            status: status.as_u16(),
        });
    }

    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| malformed(e.to_string()))?;

    // a catalog behind a captive portal answers 200 and html, and passing that
    // through is what makes an empty map look like an empty catalog
    if !body
        .get(call.array)
        .is_some_and(serde_json::Value::is_array)
    {
        return Err(malformed(format!("no {} array", call.array)));
    }
    Ok(body)
}

/// Forward an item search to the upstream STAC API and answer its item
/// collection unchanged.
pub async fn search(api: &str, params: &SearchParams) -> Result<serde_json::Value, UpstreamError> {
    proxy(api, &ITEM_SEARCH, &params.query()).await
}

/// Ask the upstream STAC API for its collections and answer the list unchanged.
pub async fn collections(api: &str) -> Result<serde_json::Value, UpstreamError> {
    proxy(api, &COLLECTION_LIST, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::RawQuery;
    use axum::{Json, Router, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_root_advertises_search_and_collections_with_an_upstream() {
        let catalog = root_catalog(true);
        assert_eq!(catalog.stac_version, "1.0.0");
        assert!(
            catalog
                .conforms_to
                .iter()
                .any(|c| c.ends_with("/item-search"))
        );
        let hrefs: Vec<&str> = catalog.links.iter().map(|l| l.href.as_str()).collect();
        assert!(hrefs.contains(&"/api/v1/stac/collections"), "{hrefs:?}");
        assert!(hrefs.contains(&"/api/v1/stac/search"), "{hrefs:?}");
    }

    #[test]
    fn the_root_advertises_only_core_without_an_upstream() {
        let catalog = root_catalog(false);
        assert_eq!(
            catalog.conforms_to,
            vec!["https://api.stacspec.org/v1.0.0/core".to_string()]
        );
        // no link to a list or a search that would refuse the click
        let rels: Vec<&str> = catalog.links.iter().map(|l| l.rel.as_str()).collect();
        assert_eq!(rels, vec!["self", "root"]);
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
            UpstreamError::NoUpstream.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            UpstreamError::NoUpstream
                .to_string()
                .contains(UPSTREAM_API_ENV)
        );
    }

    /// Every query string the upstream was called with.
    type SeenQueries = Arc<Mutex<Vec<String>>>;

    /// A STAC API on loopback that records the query string it was called with
    /// and answers this body, so a call can be proven to reach the wire.
    async fn upstream(
        path: &'static str,
        body: serde_json::Value,
        status: StatusCode,
    ) -> (String, SeenQueries) {
        let seen: SeenQueries = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().route(
            path,
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

    fn two_collections() -> serde_json::Value {
        serde_json::json!({
            "collections": [
                {
                    "type": "Collection",
                    "id": "sentinel-2-l2a",
                    "stac_version": "1.0.0",
                    "license": "proprietary",
                    "summaries": { "eo:bands": [{ "name": "B04" }] }
                },
                { "type": "Collection", "id": "cop-dem-glo-30", "stac_version": "1.0.0" }
            ],
            "links": [{ "rel": "self", "href": "https://example.org/collections" }]
        })
    }

    #[tokio::test]
    async fn collections_answers_the_upstream_list_unchanged() {
        let (api, seen) = upstream("/collections", two_collections(), StatusCode::OK).await;

        let body = collections(&api).await.unwrap();

        // no parameters, and the list reaches the caller whole: summaries,
        // links and every id the upstream named
        assert_eq!(seen.lock().unwrap()[0], "");
        assert_eq!(body, two_collections());
    }

    #[tokio::test]
    async fn an_upstream_collection_list_error_is_passed_on_as_a_bad_gateway() {
        let (api, _) = upstream(
            "/collections",
            serde_json::json!({}),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        let err = collections(&api).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[tokio::test]
    async fn an_upstream_that_answers_no_collection_list_is_not_passed_through() {
        let (api, _) = upstream(
            "/collections",
            serde_json::json!({"message": "hello"}),
            StatusCode::OK,
        )
        .await;
        let err = collections(&api).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("no collection list"), "{err}");
    }

    #[tokio::test]
    async fn search_forwards_every_parameter_and_answers_the_upstream_collection() {
        let (api, seen) = upstream("/search", one_item_collection(), StatusCode::OK).await;
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
        let (api, _) = upstream(
            "/search",
            serde_json::json!({}),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        let err = search(&api, &SearchParams::default()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[tokio::test]
    async fn an_upstream_that_answers_no_items_is_not_passed_through() {
        // 200 and json, but not an item collection: an empty map would be a lie
        let (api, _) = upstream(
            "/search",
            serde_json::json!({"message": "hello"}),
            StatusCode::OK,
        )
        .await;
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
