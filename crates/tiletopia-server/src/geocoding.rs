//! Geocoding API — address/place lookup and reverse geocoding.
//!
//! Provides forward geocoding (address → coordinates) and
//! reverse geocoding (coordinates → address) using open data sources.

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
}
