//! Usage metering and billing — track API usage for monetization.
//!
//! Records every billable event (tile request, upload, terrain generation, etc.)
//! and aggregates into usage reports for billing systems (Stripe, etc.)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A billable usage event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub event_type: UsageEventType,
    pub quantity: u64,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, String>,
}

/// Types of billable events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum UsageEventType {
    /// Tile served from cache or generated
    TileRequest,
    /// Data uploaded (bytes)
    Upload,
    /// Terrain tile generated (CPU-intensive)
    TerrainGeneration,
    /// 3D Tiles processing (point cloud → tiles)
    TileProcessing,
    /// API call (general)
    ApiCall,
    /// Storage used (bytes, measured daily)
    StorageBytes,
    /// Bandwidth out (bytes)
    BandwidthOut,
    /// Export generated
    Export,
}

/// Usage summary for a time period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub tenant_id: Uuid,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub totals: HashMap<UsageEventType, u64>,
    pub estimated_cost_usd: f64,
}

/// Pricing tiers (per unit costs in USD).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingTier {
    pub name: String,
    pub base_monthly_usd: f64,
    pub included_tile_requests: u64,
    pub included_storage_gb: u64,
    pub included_processing_minutes: u64,
    pub overage_per_1k_tiles: f64,
    pub overage_per_gb_storage: f64,
    pub overage_per_gb_bandwidth: f64,
}

impl PricingTier {
    pub fn free() -> Self {
        Self {
            name: "Free".into(),
            base_monthly_usd: 0.0,
            included_tile_requests: 100_000,
            included_storage_gb: 5,
            included_processing_minutes: 10,
            overage_per_1k_tiles: 0.0, // hard cap on free
            overage_per_gb_storage: 0.0,
            overage_per_gb_bandwidth: 0.0,
        }
    }

    pub fn pro() -> Self {
        Self {
            name: "Pro".into(),
            base_monthly_usd: 49.0,
            included_tile_requests: 5_000_000,
            included_storage_gb: 100,
            included_processing_minutes: 120,
            overage_per_1k_tiles: 0.005,
            overage_per_gb_storage: 0.10,
            overage_per_gb_bandwidth: 0.08,
        }
    }

    pub fn enterprise() -> Self {
        Self {
            name: "Enterprise".into(),
            base_monthly_usd: 499.0,
            included_tile_requests: 100_000_000,
            included_storage_gb: 5_000,
            included_processing_minutes: 2400,
            overage_per_1k_tiles: 0.002,
            overage_per_gb_storage: 0.05,
            overage_per_gb_bandwidth: 0.04,
        }
    }

    /// Calculate estimated cost for a usage summary.
    pub fn estimate_cost(&self, totals: &HashMap<UsageEventType, u64>) -> f64 {
        let mut cost = self.base_monthly_usd;

        // Tile requests
        let tile_requests = totals
            .get(&UsageEventType::TileRequest)
            .copied()
            .unwrap_or(0);
        if tile_requests > self.included_tile_requests {
            let overage = tile_requests - self.included_tile_requests;
            cost += (overage as f64 / 1000.0) * self.overage_per_1k_tiles;
        }

        // Storage (in bytes → GB)
        let storage_bytes = totals
            .get(&UsageEventType::StorageBytes)
            .copied()
            .unwrap_or(0);
        let storage_gb = storage_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if storage_gb > self.included_storage_gb as f64 {
            cost += (storage_gb - self.included_storage_gb as f64) * self.overage_per_gb_storage;
        }

        // Bandwidth
        let bandwidth = totals
            .get(&UsageEventType::BandwidthOut)
            .copied()
            .unwrap_or(0);
        let bandwidth_gb = bandwidth as f64 / (1024.0 * 1024.0 * 1024.0);
        cost += bandwidth_gb * self.overage_per_gb_bandwidth;

        cost
    }
}

/// In-memory usage metering store.
pub struct MeteringStore {
    events: Arc<RwLock<Vec<UsageEvent>>>,
}

impl Default for MeteringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MeteringStore {
    pub fn new() -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Record a usage event.
    pub async fn record(&self, tenant_id: Uuid, event_type: UsageEventType, quantity: u64) {
        let event = UsageEvent {
            id: Uuid::new_v4(),
            tenant_id,
            event_type,
            quantity,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
        };
        self.events.write().await.push(event);
    }

    /// Record with metadata.
    pub async fn record_with_metadata(
        &self,
        tenant_id: Uuid,
        event_type: UsageEventType,
        quantity: u64,
        metadata: HashMap<String, String>,
    ) {
        let event = UsageEvent {
            id: Uuid::new_v4(),
            tenant_id,
            event_type,
            quantity,
            timestamp: Utc::now(),
            metadata,
        };
        self.events.write().await.push(event);
    }

    /// Get usage summary for a tenant in the current billing period.
    pub async fn get_summary(
        &self,
        tenant_id: Uuid,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
    ) -> UsageSummary {
        let events = self.events.read().await;
        let mut totals: HashMap<UsageEventType, u64> = HashMap::new();

        for event in events.iter() {
            if event.tenant_id == tenant_id
                && event.timestamp >= period_start
                && event.timestamp <= period_end
            {
                *totals.entry(event.event_type.clone()).or_insert(0) += event.quantity;
            }
        }

        let pricing = PricingTier::pro(); // Default to Pro for estimation
        let estimated_cost = pricing.estimate_cost(&totals);

        UsageSummary {
            tenant_id,
            period_start,
            period_end,
            totals,
            estimated_cost_usd: estimated_cost,
        }
    }

    /// Get total event count (for monitoring).
    pub async fn total_events(&self) -> usize {
        self.events.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_record_and_summarize() {
        let store = MeteringStore::new();
        let tenant = Uuid::new_v4();

        store.record(tenant, UsageEventType::TileRequest, 1).await;
        store.record(tenant, UsageEventType::TileRequest, 1).await;
        store.record(tenant, UsageEventType::Upload, 1024).await;

        let now = Utc::now();
        let summary = store
            .get_summary(
                tenant,
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .await;

        assert_eq!(summary.totals.get(&UsageEventType::TileRequest), Some(&2));
        assert_eq!(summary.totals.get(&UsageEventType::Upload), Some(&1024));
    }

    #[test]
    fn test_pricing_tiers() {
        let free = PricingTier::free();
        let pro = PricingTier::pro();
        let enterprise = PricingTier::enterprise();

        assert_eq!(free.base_monthly_usd, 0.0);
        assert_eq!(pro.base_monthly_usd, 49.0);
        assert_eq!(enterprise.base_monthly_usd, 499.0);
    }

    #[test]
    fn test_cost_estimation_within_included() {
        let pro = PricingTier::pro();
        let mut totals = HashMap::new();
        totals.insert(UsageEventType::TileRequest, 1_000_000); // within 5M included
        let cost = pro.estimate_cost(&totals);
        assert_eq!(cost, 49.0); // Just base cost, no overage
    }

    #[test]
    fn test_cost_estimation_with_overage() {
        let pro = PricingTier::pro();
        let mut totals = HashMap::new();
        totals.insert(UsageEventType::TileRequest, 6_000_000); // 1M over included 5M
        let cost = pro.estimate_cost(&totals);
        // $49 + (1,000,000 / 1000) * $0.005 = $49 + $5 = $54
        assert!((cost - 54.0).abs() < 0.01);
    }
}
