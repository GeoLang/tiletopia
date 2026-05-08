//! 4D BIM — construction scheduling tied to 3D model phases.
//!
//! Links IFC/BIM elements to construction schedule tasks,
//! enabling animated timeline playback showing build progression.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A 4D BIM project (3D model + schedule).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bim4DProject {
    pub id: Uuid,
    pub name: String,
    pub model_asset_id: Uuid,
    pub schedule: ConstructionSchedule,
    pub phases: Vec<ConstructionPhase>,
    pub current_date: NaiveDate,
    pub progress_percent: f32,
    pub created_at: DateTime<Utc>,
}

/// Construction schedule (Gantt chart data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstructionSchedule {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub total_tasks: u32,
    pub completed_tasks: u32,
    pub critical_path_days: u32,
    pub delay_days: i32,
}

/// A construction phase (group of tasks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstructionPhase {
    pub id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: PhaseStatus,
    pub tasks: Vec<ScheduleTask>,
    pub element_ids: Vec<String>, // IFC GlobalId references
    pub color: String,
}

/// Task status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PhaseStatus {
    NotStarted,
    InProgress,
    Completed,
    Delayed,
    OnHold,
}

/// A schedule task tied to BIM elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub duration_days: u32,
    pub predecessors: Vec<Uuid>,
    pub status: PhaseStatus,
    pub progress_percent: f32,
    pub element_ids: Vec<String>,
    pub resource: String,
    pub is_critical: bool,
}

/// Timeline keyframe for animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineKeyframe {
    pub date: NaiveDate,
    pub visible_elements: Vec<String>,
    pub in_progress_elements: Vec<String>,
    pub highlight_color: Option<String>,
    pub camera_position: Option<[f64; 3]>,
}

/// 4D BIM engine.
pub struct Bim4DEngine {
    projects: Arc<RwLock<Vec<Bim4DProject>>>,
}

impl Default for Bim4DEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Bim4DEngine {
    pub fn new() -> Self {
        Self {
            projects: Arc::new(RwLock::new(vec![Self::demo_project()])),
        }
    }

    /// List 4D BIM projects.
    pub async fn list_projects(&self) -> Vec<Bim4DProject> {
        self.projects.read().await.clone()
    }

    /// Get timeline keyframes for animation.
    pub async fn get_timeline(&self, project_id: Uuid) -> Option<Vec<TimelineKeyframe>> {
        let projects = self.projects.read().await;
        let project = projects.iter().find(|p| p.id == project_id)?;

        let mut keyframes = Vec::new();
        for phase in &project.phases {
            keyframes.push(TimelineKeyframe {
                date: phase.start_date,
                visible_elements: phase.element_ids.clone(),
                in_progress_elements: phase
                    .tasks
                    .iter()
                    .flat_map(|t| t.element_ids.clone())
                    .collect(),
                highlight_color: Some(phase.color.clone()),
                camera_position: None,
            });
        }
        Some(keyframes)
    }

    fn demo_project() -> Bim4DProject {
        let project_id = Uuid::new_v4();
        let phase1_id = Uuid::new_v4();
        let phase2_id = Uuid::new_v4();
        let phase3_id = Uuid::new_v4();

        Bim4DProject {
            id: project_id,
            name: "Downtown Office Tower — 42 Floors".into(),
            model_asset_id: Uuid::new_v4(),
            schedule: ConstructionSchedule {
                start_date: NaiveDate::from_ymd_opt(2024, 3, 1).unwrap(),
                end_date: NaiveDate::from_ymd_opt(2026, 9, 30).unwrap(),
                total_tasks: 847,
                completed_tasks: 312,
                critical_path_days: 912,
                delay_days: -5, // ahead of schedule
            },
            phases: vec![
                ConstructionPhase {
                    id: phase1_id,
                    name: "Foundation & Substructure".into(),
                    start_date: NaiveDate::from_ymd_opt(2024, 3, 1).unwrap(),
                    end_date: NaiveDate::from_ymd_opt(2024, 9, 15).unwrap(),
                    status: PhaseStatus::Completed,
                    tasks: vec![
                        ScheduleTask {
                            id: Uuid::new_v4(),
                            name: "Excavation".into(),
                            start_date: NaiveDate::from_ymd_opt(2024, 3, 1).unwrap(),
                            end_date: NaiveDate::from_ymd_opt(2024, 5, 15).unwrap(),
                            duration_days: 75,
                            predecessors: vec![],
                            status: PhaseStatus::Completed,
                            progress_percent: 100.0,
                            element_ids: vec!["2O2Fr$t4X7Zf8NOew3FLOH".into()],
                            resource: "Earthworks Crew A".into(),
                            is_critical: true,
                        },
                        ScheduleTask {
                            id: Uuid::new_v4(),
                            name: "Pile Driving".into(),
                            start_date: NaiveDate::from_ymd_opt(2024, 4, 15).unwrap(),
                            end_date: NaiveDate::from_ymd_opt(2024, 7, 30).unwrap(),
                            duration_days: 106,
                            predecessors: vec![],
                            status: PhaseStatus::Completed,
                            progress_percent: 100.0,
                            element_ids: vec!["3$7_s8vfX3UAQRTNm1BKWR".into()],
                            resource: "Foundation Crew B".into(),
                            is_critical: true,
                        },
                    ],
                    element_ids: vec![
                        "2O2Fr$t4X7Zf8NOew3FLOH".into(),
                        "3$7_s8vfX3UAQRTNm1BKWR".into(),
                    ],
                    color: "#8b5e3c".into(),
                },
                ConstructionPhase {
                    id: phase2_id,
                    name: "Core & Superstructure (Floors 1-20)".into(),
                    start_date: NaiveDate::from_ymd_opt(2024, 8, 1).unwrap(),
                    end_date: NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
                    status: PhaseStatus::InProgress,
                    tasks: vec![
                        ScheduleTask {
                            id: Uuid::new_v4(),
                            name: "Core walls (B1-L10)".into(),
                            start_date: NaiveDate::from_ymd_opt(2024, 8, 1).unwrap(),
                            end_date: NaiveDate::from_ymd_opt(2025, 1, 15).unwrap(),
                            duration_days: 168,
                            predecessors: vec![phase1_id],
                            status: PhaseStatus::Completed,
                            progress_percent: 100.0,
                            element_ids: vec!["1kTvXnbbzEmRQ2L1lCsa8v".into()],
                            resource: "Concrete Crew C".into(),
                            is_critical: true,
                        },
                        ScheduleTask {
                            id: Uuid::new_v4(),
                            name: "Steel frame (L11-L20)".into(),
                            start_date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
                            end_date: NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
                            duration_days: 180,
                            predecessors: vec![],
                            status: PhaseStatus::InProgress,
                            progress_percent: 65.0,
                            element_ids: vec!["0btBFw6f90Nfh9rP1dlXr2".into()],
                            resource: "Steel Erection D".into(),
                            is_critical: true,
                        },
                    ],
                    element_ids: vec![
                        "1kTvXnbbzEmRQ2L1lCsa8v".into(),
                        "0btBFw6f90Nfh9rP1dlXr2".into(),
                    ],
                    color: "#4a90d9".into(),
                },
                ConstructionPhase {
                    id: phase3_id,
                    name: "Superstructure (Floors 21-42) & Facade".into(),
                    start_date: NaiveDate::from_ymd_opt(2025, 5, 1).unwrap(),
                    end_date: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
                    status: PhaseStatus::NotStarted,
                    tasks: vec![],
                    element_ids: vec!["5Wt8Ps2v9CKe7m0x3QfYhJ".into()],
                    color: "#50c878".into(),
                },
            ],
            current_date: NaiveDate::from_ymd_opt(2025, 5, 8).unwrap(),
            progress_percent: 36.8,
            created_at: Utc::now() - chrono::Duration::days(430),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_projects() {
        let engine = Bim4DEngine::new();
        let projects = engine.list_projects().await;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].phases.len(), 3);
    }

    #[tokio::test]
    async fn test_get_timeline() {
        let engine = Bim4DEngine::new();
        let projects = engine.list_projects().await;
        let timeline = engine.get_timeline(projects[0].id).await.unwrap();
        assert_eq!(timeline.len(), 3);
    }
}
