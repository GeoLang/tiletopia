//! Data marketplace — sell/share tilesets between organizations.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A listing in the data marketplace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceListing {
    pub id: String,
    pub title: String,
    pub description: String,
    pub publisher_id: String,
    pub tileset_id: String,
    pub pricing: PricingModel,
    pub license: DataLicense,
    pub tags: Vec<String>,
    pub coverage_bbox: Option<[f64; 4]>, // [min_lon, min_lat, max_lon, max_lat]
    pub preview_url: Option<String>,
    pub published: bool,
    pub downloads: u64,
    pub rating: f64,
}

/// Pricing model for marketplace listings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PricingModel {
    Free,
    OneTime {
        price_cents: u64,
        currency: String,
    },
    Subscription {
        monthly_cents: u64,
        currency: String,
    },
    PerRequest {
        price_per_1k_cents: u64,
        currency: String,
    },
    ContactSales,
}

/// Data license types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DataLicense {
    OpenData,
    CreativeCommons { variant: String },
    Commercial { terms_url: String },
    GovernmentOpen,
    Custom { text: String },
}

/// A purchase/access record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessGrant {
    pub id: String,
    pub listing_id: String,
    pub buyer_id: String,
    pub granted_at: String,
    pub expires_at: Option<String>,
    pub usage_count: u64,
    pub max_usage: Option<u64>,
}

/// Marketplace store.
pub struct Marketplace {
    listings: HashMap<String, MarketplaceListing>,
    grants: Vec<AccessGrant>,
}

impl Marketplace {
    pub fn new() -> Self {
        Self {
            listings: HashMap::new(),
            grants: Vec::new(),
        }
    }

    /// Publish a new listing.
    pub fn publish(&mut self, listing: MarketplaceListing) -> String {
        let id = listing.id.clone();
        self.listings.insert(id.clone(), listing);
        id
    }

    /// Search listings by keyword.
    pub fn search(&self, query: &str) -> Vec<&MarketplaceListing> {
        let query_lower = query.to_lowercase();
        self.listings
            .values()
            .filter(|l| {
                l.published
                    && (l.title.to_lowercase().contains(&query_lower)
                        || l.description.to_lowercase().contains(&query_lower)
                        || l.tags
                            .iter()
                            .any(|t| t.to_lowercase().contains(&query_lower)))
            })
            .collect()
    }

    /// Search by geographic bounding box.
    pub fn search_by_bbox(&self, bbox: &[f64; 4]) -> Vec<&MarketplaceListing> {
        self.listings
            .values()
            .filter(|l| {
                l.published
                    && l.coverage_bbox.is_some_and(|cb| {
                        cb[0] <= bbox[2] && cb[2] >= bbox[0] && cb[1] <= bbox[3] && cb[3] >= bbox[1]
                    })
            })
            .collect()
    }

    /// Grant access to a listing.
    pub fn grant_access(&mut self, grant: AccessGrant) {
        self.grants.push(grant);
    }

    /// Check if a user has access to a listing.
    pub fn has_access(&self, buyer_id: &str, listing_id: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.buyer_id == buyer_id && g.listing_id == listing_id)
    }

    /// Get a listing by ID.
    pub fn get_listing(&self, id: &str) -> Option<&MarketplaceListing> {
        self.listings.get(id)
    }

    /// Record a download/usage.
    pub fn record_usage(&mut self, buyer_id: &str, listing_id: &str) -> bool {
        if let Some(grant) = self
            .grants
            .iter_mut()
            .find(|g| g.buyer_id == buyer_id && g.listing_id == listing_id)
        {
            if grant.max_usage.is_some_and(|max| grant.usage_count >= max) {
                return false;
            }
            grant.usage_count += 1;

            // Increment download count on listing
            if let Some(listing) = self.listings.get_mut(listing_id) {
                listing.downloads += 1;
            }
            true
        } else {
            false
        }
    }

    /// Get total number of published listings.
    pub fn published_count(&self) -> usize {
        self.listings.values().filter(|l| l.published).count()
    }
}

impl Default for Marketplace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_listing(id: &str) -> MarketplaceListing {
        MarketplaceListing {
            id: id.into(),
            title: "Downtown LA Point Cloud".into(),
            description: "High-density LiDAR survey of downtown Los Angeles".into(),
            publisher_id: "pub-1".into(),
            tileset_id: "ts-1".into(),
            pricing: PricingModel::OneTime {
                price_cents: 9900,
                currency: "USD".into(),
            },
            license: DataLicense::Commercial {
                terms_url: "https://example.com/terms".into(),
            },
            tags: vec!["lidar".into(), "los-angeles".into(), "urban".into()],
            coverage_bbox: Some([-118.3, 34.0, -118.2, 34.1]),
            preview_url: None,
            published: true,
            downloads: 0,
            rating: 4.5,
        }
    }

    #[test]
    fn test_publish_and_search() {
        let mut mp = Marketplace::new();
        mp.publish(sample_listing("l-1"));
        let results = mp.search("lidar");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Downtown LA Point Cloud");
    }

    #[test]
    fn test_search_by_bbox() {
        let mut mp = Marketplace::new();
        mp.publish(sample_listing("l-1"));
        // Overlapping bbox
        let results = mp.search_by_bbox(&[-118.25, 34.05, -118.15, 34.15]);
        assert_eq!(results.len(), 1);
        // Non-overlapping bbox
        let results = mp.search_by_bbox(&[0.0, 0.0, 1.0, 1.0]);
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_access_grant() {
        let mut mp = Marketplace::new();
        mp.publish(sample_listing("l-1"));
        assert!(!mp.has_access("buyer-1", "l-1"));
        mp.grant_access(AccessGrant {
            id: "grant-1".into(),
            listing_id: "l-1".into(),
            buyer_id: "buyer-1".into(),
            granted_at: "2024-01-01".into(),
            expires_at: None,
            usage_count: 0,
            max_usage: Some(10),
        });
        assert!(mp.has_access("buyer-1", "l-1"));
    }

    #[test]
    fn test_usage_metering() {
        let mut mp = Marketplace::new();
        mp.publish(sample_listing("l-1"));
        mp.grant_access(AccessGrant {
            id: "g-1".into(),
            listing_id: "l-1".into(),
            buyer_id: "b-1".into(),
            granted_at: "2024-01-01".into(),
            expires_at: None,
            usage_count: 0,
            max_usage: Some(2),
        });
        assert!(mp.record_usage("b-1", "l-1"));
        assert!(mp.record_usage("b-1", "l-1"));
        assert!(!mp.record_usage("b-1", "l-1")); // Exceeded quota
    }

    #[test]
    fn test_free_listing() {
        let mut mp = Marketplace::new();
        let mut listing = sample_listing("free-1");
        listing.pricing = PricingModel::Free;
        listing.license = DataLicense::OpenData;
        mp.publish(listing);
        assert_eq!(mp.published_count(), 1);
    }
}
