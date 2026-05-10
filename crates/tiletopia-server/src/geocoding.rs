//! Geocoding API — address/place lookup and reverse geocoding.
//!
//! Provides forward geocoding (address → coordinates) and
//! reverse geocoding (coordinates → address) using open data sources.
//! Includes both offline demo lookups and async Nominatim API integration.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A geocoding result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeocodingResult {
    pub id: Uuid,
    pub query: String,
    pub results: Vec<GeocodedPlace>,
    pub provider: GeoProvider,
}

/// A geocoded place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeocodedPlace {
    pub place_id: String,
    pub display_name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub place_type: PlaceType,
    pub confidence: f32,
    pub bounding_box: Option<[f64; 4]>, // [south, north, west, east]
    pub address: Address,
}

/// Structured address components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Address {
    pub house_number: Option<String>,
    pub street: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub country_code: Option<String>,
}

/// Place type classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PlaceType {
    Address,
    Street,
    Neighborhood,
    City,
    State,
    Country,
    Poi,
    Airport,
    Station,
}

/// Geocoding data provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GeoProvider {
    /// OpenStreetMap / Nominatim
    Osm,
    /// GeoNames open data
    GeoNames,
    /// Local index (custom)
    Local,
}

/// Forward geocode: address string → coordinates.
pub fn geocode(query: &str) -> GeocodingResult {
    // Demo results for common queries
    let results = match query.to_lowercase() {
        q if q.contains("golden gate") => vec![GeocodedPlace {
            place_id: "osm_way_28757474".into(),
            display_name: "Golden Gate Bridge, San Francisco, CA, USA".into(),
            latitude: 37.8199,
            longitude: -122.4783,
            place_type: PlaceType::Poi,
            confidence: 0.98,
            bounding_box: Some([37.8080, 37.8320, -122.4840, -122.4700]),
            address: Address {
                house_number: None,
                street: Some("Golden Gate Bridge".into()),
                city: Some("San Francisco".into()),
                state: Some("California".into()),
                postal_code: Some("94129".into()),
                country: Some("United States".into()),
                country_code: Some("US".into()),
            },
        }],
        q if q.contains("eiffel") => vec![GeocodedPlace {
            place_id: "osm_way_5013364".into(),
            display_name: "Eiffel Tower, 5 Avenue Anatole France, Paris, France".into(),
            latitude: 48.8584,
            longitude: 2.2945,
            place_type: PlaceType::Poi,
            confidence: 0.99,
            bounding_box: Some([48.8555, 48.8613, 2.2905, 2.2985]),
            address: Address {
                house_number: Some("5".into()),
                street: Some("Avenue Anatole France".into()),
                city: Some("Paris".into()),
                state: Some("Île-de-France".into()),
                postal_code: Some("75007".into()),
                country: Some("France".into()),
                country_code: Some("FR".into()),
            },
        }],
        q if q.contains("times square") => vec![GeocodedPlace {
            place_id: "osm_node_2693465769".into(),
            display_name: "Times Square, Manhattan, New York, NY, USA".into(),
            latitude: 40.7580,
            longitude: -73.9855,
            place_type: PlaceType::Poi,
            confidence: 0.97,
            bounding_box: Some([40.7550, 40.7610, -73.9890, -73.9820]),
            address: Address {
                house_number: None,
                street: Some("Broadway".into()),
                city: Some("New York".into()),
                state: Some("New York".into()),
                postal_code: Some("10036".into()),
                country: Some("United States".into()),
                country_code: Some("US".into()),
            },
        }],
        _ => vec![GeocodedPlace {
            place_id: "fallback_0".into(),
            display_name: format!("Results for: {query}"),
            latitude: 0.0,
            longitude: 0.0,
            place_type: PlaceType::Address,
            confidence: 0.1,
            bounding_box: None,
            address: Address {
                house_number: None,
                street: None,
                city: None,
                state: None,
                postal_code: None,
                country: None,
                country_code: None,
            },
        }],
    };

    GeocodingResult {
        id: Uuid::new_v4(),
        query: query.to_string(),
        results,
        provider: GeoProvider::Osm,
    }
}

/// Reverse geocode: coordinates → nearest address.
pub fn reverse_geocode(latitude: f64, longitude: f64) -> GeocodedPlace {
    // Simple demo: return a descriptive result
    GeocodedPlace {
        place_id: format!("reverse_{latitude:.4}_{longitude:.4}"),
        display_name: format!("{latitude:.6}, {longitude:.6}"),
        latitude,
        longitude,
        place_type: PlaceType::Address,
        confidence: 0.85,
        bounding_box: Some([
            latitude - 0.001,
            latitude + 0.001,
            longitude - 0.001,
            longitude + 0.001,
        ]),
        address: Address {
            house_number: None,
            street: None,
            city: Some("Unknown".into()),
            state: None,
            postal_code: None,
            country: None,
            country_code: None,
        },
    }
}

/// Batch geocode multiple addresses.
pub fn batch_geocode(queries: &[String]) -> Vec<GeocodingResult> {
    queries.iter().map(|q| geocode(q)).collect()
}

/// Geocoding errors for async API calls.
#[derive(Debug, thiserror::Error)]
pub enum GeocodingError {
    #[error("network error: {0}")]
    Network(String),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Nominatim API response item.
#[derive(Debug, Deserialize)]
struct NominatimResult {
    place_id: u64,
    display_name: String,
    lat: String,
    lon: String,
    #[serde(rename = "type")]
    place_type: String,
    #[serde(default)]
    importance: f64,
    boundingbox: Option<Vec<String>>,
    address: Option<NominatimAddress>,
}

/// Address components from Nominatim.
#[derive(Debug, Deserialize)]
struct NominatimAddress {
    house_number: Option<String>,
    road: Option<String>,
    city: Option<String>,
    town: Option<String>,
    village: Option<String>,
    state: Option<String>,
    postcode: Option<String>,
    country: Option<String>,
    country_code: Option<String>,
}

impl NominatimResult {
    fn to_geocoded_place(&self) -> GeocodedPlace {
        let lat: f64 = self.lat.parse().unwrap_or(0.0);
        let lon: f64 = self.lon.parse().unwrap_or(0.0);

        let bbox = self.boundingbox.as_ref().and_then(|bb| {
            if bb.len() == 4 {
                Some([
                    bb[0].parse().unwrap_or(0.0), // south
                    bb[1].parse().unwrap_or(0.0), // north
                    bb[2].parse().unwrap_or(0.0), // west
                    bb[3].parse().unwrap_or(0.0), // east
                ])
            } else {
                None
            }
        });

        let addr = self.address.as_ref();
        let city = addr.and_then(|a| {
            a.city
                .clone()
                .or_else(|| a.town.clone())
                .or_else(|| a.village.clone())
        });

        GeocodedPlace {
            place_id: format!("osm_{}", self.place_id),
            display_name: self.display_name.clone(),
            latitude: lat,
            longitude: lon,
            place_type: classify_place_type(&self.place_type),
            confidence: self.importance as f32,
            bounding_box: bbox,
            address: Address {
                house_number: addr.and_then(|a| a.house_number.clone()),
                street: addr.and_then(|a| a.road.clone()),
                city,
                state: addr.and_then(|a| a.state.clone()),
                postal_code: addr.and_then(|a| a.postcode.clone()),
                country: addr.and_then(|a| a.country.clone()),
                country_code: addr.and_then(|a| a.country_code.clone()),
            },
        }
    }
}

fn classify_place_type(osm_type: &str) -> PlaceType {
    match osm_type {
        "house" | "building" | "residential" => PlaceType::Address,
        "road" | "street" | "highway" | "path" => PlaceType::Street,
        "suburb" | "neighbourhood" | "quarter" => PlaceType::Neighborhood,
        "city" | "town" | "village" | "hamlet" => PlaceType::City,
        "state" | "province" | "region" => PlaceType::State,
        "country" => PlaceType::Country,
        "aerodrome" | "aeroway" => PlaceType::Airport,
        "station" | "halt" | "stop" => PlaceType::Station,
        _ => PlaceType::Poi,
    }
}

/// Forward geocode using the Nominatim OpenStreetMap API.
pub async fn geocode_nominatim(query: &str) -> Result<GeocodingResult, GeocodingError> {
    let client = reqwest::Client::builder()
        .user_agent("tiletopia/0.3.0")
        .build()
        .map_err(|e| GeocodingError::Network(e.to_string()))?;

    let resp = client
        .get("https://nominatim.openstreetmap.org/search")
        .query(&[
            ("q", query),
            ("format", "jsonv2"),
            ("limit", "5"),
            ("addressdetails", "1"),
        ])
        .send()
        .await
        .map_err(|e| GeocodingError::Network(e.to_string()))?;

    let items: Vec<NominatimResult> = resp
        .json()
        .await
        .map_err(|e| GeocodingError::Parse(e.to_string()))?;

    let results: Vec<GeocodedPlace> = items.iter().map(|r| r.to_geocoded_place()).collect();

    Ok(GeocodingResult {
        id: Uuid::new_v4(),
        query: query.to_string(),
        results,
        provider: GeoProvider::Osm,
    })
}

/// Reverse geocode using the Nominatim OpenStreetMap API.
pub async fn reverse_geocode_nominatim(
    latitude: f64,
    longitude: f64,
) -> Result<GeocodedPlace, GeocodingError> {
    let client = reqwest::Client::builder()
        .user_agent("tiletopia/0.3.0")
        .build()
        .map_err(|e| GeocodingError::Network(e.to_string()))?;

    let lat_str = latitude.to_string();
    let lon_str = longitude.to_string();

    let resp = client
        .get("https://nominatim.openstreetmap.org/reverse")
        .query(&[
            ("lat", lat_str.as_str()),
            ("lon", lon_str.as_str()),
            ("format", "jsonv2"),
            ("addressdetails", "1"),
        ])
        .send()
        .await
        .map_err(|e| GeocodingError::Network(e.to_string()))?;

    let item: NominatimResult = resp
        .json()
        .await
        .map_err(|e| GeocodingError::Parse(e.to_string()))?;

    Ok(item.to_geocoded_place())
}

/// Parse a Nominatim JSON response into geocoded places (useful for testing with mock data).
pub fn parse_nominatim_response(json: &str) -> Result<Vec<GeocodedPlace>, GeocodingError> {
    let items: Vec<NominatimResult> =
        serde_json::from_str(json).map_err(|e| GeocodingError::Parse(e.to_string()))?;
    Ok(items.iter().map(|r| r.to_geocoded_place()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forward_geocode() {
        let result = geocode("Golden Gate Bridge");
        assert_eq!(result.results.len(), 1);
        assert!(result.results[0].confidence > 0.9);
        assert!((result.results[0].latitude - 37.8199).abs() < 0.01);
    }

    #[test]
    fn test_reverse_geocode() {
        let place = reverse_geocode(37.7749, -122.4194);
        assert!((place.latitude - 37.7749).abs() < 0.0001);
    }

    #[test]
    fn test_batch_geocode() {
        let queries = vec!["Eiffel Tower".into(), "Times Square".into()];
        let results = batch_geocode(&queries);
        assert_eq!(results.len(), 2);
    }

    /// Parse a mock Nominatim JSON response without making network calls.
    #[test]
    fn test_parse_nominatim_response() {
        let json = r#"[
            {
                "place_id": 12345,
                "display_name": "Golden Gate Bridge, San Francisco, CA, USA",
                "lat": "37.8199",
                "lon": "-122.4783",
                "type": "attraction",
                "importance": 0.85,
                "boundingbox": ["37.8080", "37.8320", "-122.4840", "-122.4700"],
                "address": {
                    "road": "Golden Gate Bridge",
                    "city": "San Francisco",
                    "state": "California",
                    "postcode": "94129",
                    "country": "United States",
                    "country_code": "us"
                }
            }
        ]"#;

        let places = parse_nominatim_response(json).unwrap();
        assert_eq!(places.len(), 1);
        let p = &places[0];
        assert_eq!(p.place_id, "osm_12345");
        assert!((p.latitude - 37.8199).abs() < 0.001);
        assert!((p.longitude - (-122.4783)).abs() < 0.001);
        assert_eq!(p.address.city.as_deref(), Some("San Francisco"));
        assert_eq!(p.address.country_code.as_deref(), Some("us"));
        assert_eq!(p.place_type, PlaceType::Poi);
    }

    #[test]
    fn test_parse_nominatim_empty() {
        let places = parse_nominatim_response("[]").unwrap();
        assert!(places.is_empty());
    }

    #[test]
    fn test_classify_place_types() {
        assert_eq!(classify_place_type("city"), PlaceType::City);
        assert_eq!(classify_place_type("road"), PlaceType::Street);
        assert_eq!(classify_place_type("country"), PlaceType::Country);
        assert_eq!(classify_place_type("aerodrome"), PlaceType::Airport);
        assert_eq!(classify_place_type("something_else"), PlaceType::Poi);
    }
}
