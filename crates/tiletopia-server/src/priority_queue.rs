//! Priority job queue with SLA-based scheduling.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

/// Priority tier for jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Critical = 4,
    High = 3,
    Normal = 2,
    Low = 1,
    Background = 0,
}

impl Priority {
    pub fn max_wait(&self) -> Duration {
        match self {
            Priority::Critical => Duration::from_secs(5),
            Priority::High => Duration::from_secs(30),
            Priority::Normal => Duration::from_secs(300),
            Priority::Low => Duration::from_secs(3600),
            Priority::Background => Duration::from_secs(86400),
        }
    }
}

/// A job in the priority queue.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub priority: Priority,
    pub tenant_id: String,
    pub job_type: JobType,
    pub submitted_at: Instant,
    pub estimated_duration: Duration,
    pub sla_deadline: Option<Instant>,
}

/// Types of processing jobs.
#[derive(Debug, Clone, PartialEq)]
pub enum JobType {
    TileGeneration { asset_id: String },
    TerrainProcessing { dem_id: String },
    PointCloudClassification { dataset_id: String },
    Photogrammetry { project_id: String },
    Export { tileset_id: String, format: String },
}

/// Wrapper for BinaryHeap ordering.
struct PrioritizedJob {
    job: Job,
    effective_priority: u64,
}

impl PartialEq for PrioritizedJob {
    fn eq(&self, other: &Self) -> bool {
        self.effective_priority == other.effective_priority
    }
}

impl Eq for PrioritizedJob {}

impl PartialOrd for PrioritizedJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedJob {
    fn cmp(&self, other: &Self) -> Ordering {
        self.effective_priority.cmp(&other.effective_priority)
    }
}

/// Priority queue with aging and SLA awareness.
pub struct PriorityQueue {
    heap: BinaryHeap<PrioritizedJob>,
    tenant_quotas: std::collections::HashMap<String, TenantQuota>,
}

/// Per-tenant resource quota.
#[derive(Debug, Clone)]
pub struct TenantQuota {
    pub max_concurrent_jobs: usize,
    pub active_jobs: usize,
    pub priority_boost: u64,
    pub tier: TenantTier,
}

/// Tenant subscription tier.
#[derive(Debug, Clone, PartialEq)]
pub enum TenantTier {
    Free,
    Pro,
    Enterprise,
}

impl PriorityQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            tenant_quotas: std::collections::HashMap::new(),
        }
    }

    /// Set quota for a tenant.
    pub fn set_tenant_quota(&mut self, tenant_id: &str, quota: TenantQuota) {
        self.tenant_quotas.insert(tenant_id.to_string(), quota);
    }

    /// Submit a job to the queue.
    pub fn submit(&mut self, job: Job) {
        let base_priority = job.priority as u64 * 1000;
        let age_bonus = job.submitted_at.elapsed().as_secs();
        let tenant_boost = self
            .tenant_quotas
            .get(&job.tenant_id)
            .map(|q| q.priority_boost)
            .unwrap_or(0);

        // SLA urgency: if approaching deadline, boost priority
        let sla_boost = job.sla_deadline.map_or(0, |deadline| {
            let remaining = deadline.duration_since(Instant::now());
            if remaining < Duration::from_secs(60) {
                500
            } else if remaining < Duration::from_secs(300) {
                200
            } else {
                0
            }
        });

        let effective_priority = base_priority + age_bonus + tenant_boost + sla_boost;

        self.heap.push(PrioritizedJob {
            job,
            effective_priority,
        });
    }

    /// Dequeue the highest priority job.
    pub fn dequeue(&mut self) -> Option<Job> {
        self.heap.pop().map(|pj| pj.job)
    }

    /// Peek at the highest priority job without removing.
    pub fn peek(&self) -> Option<&Job> {
        self.heap.peek().map(|pj| &pj.job)
    }

    /// Number of jobs in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Check if a tenant can submit more jobs (within quota).
    pub fn can_submit(&self, tenant_id: &str) -> bool {
        match self.tenant_quotas.get(tenant_id) {
            Some(quota) => quota.active_jobs < quota.max_concurrent_jobs,
            None => true, // No quota = unlimited
        }
    }

    /// Get queue stats.
    pub fn stats(&self) -> QueueStats {
        let mut by_priority = [0usize; 5];
        for pj in self.heap.iter() {
            let idx = pj.job.priority as usize;
            if idx < 5 {
                by_priority[idx] += 1;
            }
        }
        QueueStats {
            total: self.heap.len(),
            by_priority,
        }
    }
}

impl Default for PriorityQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Queue statistics.
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub total: usize,
    pub by_priority: [usize; 5],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_submit_and_dequeue() {
        let mut q = PriorityQueue::new();
        q.submit(Job {
            id: "low-1".into(),
            priority: Priority::Low,
            tenant_id: "t1".into(),
            job_type: JobType::Export {
                tileset_id: "ts1".into(),
                format: "3dtiles".into(),
            },
            submitted_at: Instant::now(),
            estimated_duration: Duration::from_secs(60),
            sla_deadline: None,
        });
        q.submit(Job {
            id: "high-1".into(),
            priority: Priority::High,
            tenant_id: "t1".into(),
            job_type: JobType::TileGeneration {
                asset_id: "a1".into(),
            },
            submitted_at: Instant::now(),
            estimated_duration: Duration::from_secs(120),
            sla_deadline: None,
        });
        assert_eq!(q.len(), 2);
        let next = q.dequeue().unwrap();
        assert_eq!(next.id, "high-1"); // Higher priority first
    }

    #[test]
    fn test_tenant_quota() {
        let mut q = PriorityQueue::new();
        q.set_tenant_quota(
            "free-tenant",
            TenantQuota {
                max_concurrent_jobs: 2,
                active_jobs: 2,
                priority_boost: 0,
                tier: TenantTier::Free,
            },
        );
        assert!(!q.can_submit("free-tenant"));
    }

    #[test]
    fn test_enterprise_boost() {
        let mut q = PriorityQueue::new();
        q.set_tenant_quota(
            "enterprise",
            TenantQuota {
                max_concurrent_jobs: 100,
                active_jobs: 0,
                priority_boost: 500,
                tier: TenantTier::Enterprise,
            },
        );
        q.submit(Job {
            id: "ent-job".into(),
            priority: Priority::Normal,
            tenant_id: "enterprise".into(),
            job_type: JobType::TileGeneration {
                asset_id: "x".into(),
            },
            submitted_at: Instant::now(),
            estimated_duration: Duration::from_secs(60),
            sla_deadline: None,
        });
        q.submit(Job {
            id: "free-job".into(),
            priority: Priority::Normal,
            tenant_id: "free".into(),
            job_type: JobType::TileGeneration {
                asset_id: "y".into(),
            },
            submitted_at: Instant::now(),
            estimated_duration: Duration::from_secs(60),
            sla_deadline: None,
        });
        let first = q.dequeue().unwrap();
        assert_eq!(first.id, "ent-job"); // Enterprise gets priority boost
    }

    #[test]
    fn test_queue_stats() {
        let mut q = PriorityQueue::new();
        q.submit(Job {
            id: "j1".into(),
            priority: Priority::Critical,
            tenant_id: "t".into(),
            job_type: JobType::TileGeneration {
                asset_id: "a".into(),
            },
            submitted_at: Instant::now(),
            estimated_duration: Duration::from_secs(10),
            sla_deadline: None,
        });
        let stats = q.stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.by_priority[4], 1); // Critical = index 4
    }

    #[test]
    fn test_empty_queue() {
        let mut q = PriorityQueue::new();
        assert!(q.is_empty());
        assert!(q.dequeue().is_none());
    }
}
