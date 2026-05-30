//! Geocoding API — address/place lookup and reverse geocoding.
//!
//! Uses `geokode_core` for offline forward/reverse geocoding with
//! FST text index and R-tree spatial index. Falls back to Nominatim
//! OSM API for online queries.

use geokode_core::address::{GeoResult as GeokodeResult, parse_address};
use geokode_core::geocode::{Geocoder, GeocoderBuilder};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
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
    pub bounding_box: Option<[f64; 4]>,
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
    Osm,
    GeoNames,
    Local,
}

/// Demo geocoder with well-known places.
static DEMO_GEOCODER: LazyLock<Geocoder> = LazyLock::new(|| {
    let mut builder = GeocoderBuilder::new();
    // Famous landmarks
    builder.add(
        parse_address("Golden Gate Bridge, San Francisco, CA, USA"),
        37.8199,
        -122.4783,
    );
    builder.add(
        parse_address("Eiffel Tower, 5 Avenue Anatole France, Paris, France"),
        48.8584,
        2.2945,
    );
    builder.add(
        parse_address("Times Square, Manhattan, New York, NY, USA"),
        40.7580,
        -73.9855,
    );
    builder.add(
        parse_address("Sydney Opera House, Bennelong Point, Sydney, Australia"),
        -33.8568,
        151.2153,
    );
    builder.add(
        parse_address("Big Ben, Westminster, London, UK"),
        51.5007,
        -0.1246,
    );
    builder.add(
        parse_address("Colosseum, Piazza del Colosseo, Rome, Italy"),
        41.8902,
        12.4922,
    );
    builder.add(
        parse_address("Statue of Liberty, Liberty Island, New York, NY, USA"),
        40.6892,
        -74.0445,
    );
    builder.build().expect("demo geocoder build")
});

/// Forward geocode: address string → coordinates (offline via geokode-core).
pub fn geocode(query: &str) -> GeocodingResult {
    let geokode_results = DEMO_GEOCODER.forward(query);
    let results: Vec<GeocodedPlace> = geokode_results.iter().map(convert_geokode_result).collect();

    GeocodingResult {
        id: Uuid::new_v4(),
        query: query.to_string(),
        results,
        provider: GeoProvider::Local,
    }
}

/// Reverse geocode: coordinates → nearest address (offline via geokode-core).
pub fn reverse_geocode(latitude: f64, longitude: f64) -> GeocodedPlace {
    let results = DEMO_GEOCODER.reverse(longitude, latitude, 1);
    results
        .first()
        .map(convert_geokode_result)
        .unwrap_or_else(|| GeocodedPlace {
            place_id: format!("reverse_{latitude:.4}_{longitude:.4}"),
            display_name: format!("{latitude:.6}, {longitude:.6}"),
            latitude,
            longitude,
            place_type: PlaceType::Address,
            confidence: 0.0,
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
        })
}

/// Batch geocode multiple addresses.
pub fn batch_geocode(queries: &[String]) -> Vec<GeocodingResult> {
    queries.iter().map(|q| geocode(q)).collect()
}

/// Convert a geokode result to tiletopia's geocoded place.
fn convert_geokode_result(r: &GeokodeResult) -> GeocodedPlace {
    GeocodedPlace {
        place_id: format!("local_{:.4}_{:.4}", r.lat, r.lon),
        display_name: r.address.full.clone(),
        latitude: r.lat,
        longitude: r.lon,
        place_type: PlaceType::Poi,
        confidence: r.confidence as f32,
        bounding_box: None,
        address: Address {
            house_number: r.address.house_number.clone(),
            street: r.address.street.clone(),
            city: r.address.city.clone(),
            state: r.address.state.clone(),
            postal_code: r.address.postcode.clone(),
            country: r.address.country.clone(),
            country_code: None,
        },
    }
}

// ─── Online Nominatim API ───────────────────────────────────────────────────

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
                    bb[0].parse().unwrap_or(0.0),
                    bb[1].parse().unwrap_or(0.0),
                    bb[2].parse().unwrap_or(0.0),
                    bb[3].parse().unwrap_or(0.0),
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

/// Parse a Nominatim JSON response into geocoded places.
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
        assert!(!result.results.is_empty());
        assert!(result.results[0].confidence > 0.5);
    }

    #[test]
    fn test_reverse_geocode() {
        // Near Golden Gate Bridge
        let place = reverse_geocode(37.8199, -122.4783);
        assert!((place.latitude - 37.8199).abs() < 0.01);
    }

    #[test]
    fn test_batch_geocode() {
        let queries = vec!["Eiffel Tower".into(), "Times Square".into()];
        let results = batch_geocode(&queries);
        assert_eq!(results.len(), 2);
    }

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
        assert_eq!(p.address.city.as_deref(), Some("San Francisco"));
        assert_eq!(p.place_type, PlaceType::Poi);
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
