//! Custom dashboard builder — drag-and-drop widget layouts.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Widget type for dashboard panels.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WidgetType {
    CesiumView {
        tileset_ids: Vec<String>,
    },
    Chart {
        chart_type: ChartType,
        data_source: String,
    },
    AlertList {
        max_items: usize,
    },
    KpiCard {
        metric: String,
        label: String,
    },
    SensorFeed {
        sensor_ids: Vec<String>,
    },
    AnnotationPanel {
        layer_id: String,
    },
    TimeSlider {
        from: String,
        to: String,
    },
    Custom {
        component_name: String,
        props: serde_json::Value,
    },
}

/// Chart types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChartType {
    Line,
    Bar,
    Pie,
    Scatter,
    Heatmap,
    TimeSeries,
}

/// Position and size of a widget on the grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WidgetLayout {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A widget instance in a dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Widget {
    pub id: String,
    pub widget_type: WidgetType,
    pub layout: WidgetLayout,
    pub title: String,
    pub refresh_interval_secs: Option<u32>,
}

/// A complete dashboard configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub widgets: Vec<Widget>,
    pub columns: u32,
    pub rows: u32,
    pub shared_with: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Dashboard store.
pub struct DashboardStore {
    dashboards: HashMap<String, Dashboard>,
}

impl DashboardStore {
    pub fn new() -> Self {
        Self {
            dashboards: HashMap::new(),
        }
    }

    /// Create a new dashboard.
    pub fn create(&mut self, dashboard: Dashboard) -> String {
        let id = dashboard.id.clone();
        self.dashboards.insert(id.clone(), dashboard);
        id
    }

    /// Get a dashboard by ID.
    pub fn get(&self, id: &str) -> Option<&Dashboard> {
        self.dashboards.get(id)
    }

    /// Update a dashboard.
    pub fn update(&mut self, dashboard: Dashboard) -> bool {
        if self.dashboards.contains_key(&dashboard.id) {
            self.dashboards.insert(dashboard.id.clone(), dashboard);
            true
        } else {
            false
        }
    }

    /// Delete a dashboard.
    pub fn delete(&mut self, id: &str) -> bool {
        self.dashboards.remove(id).is_some()
    }

    /// List dashboards for a user (owned or shared).
    pub fn list_for_user(&self, user_id: &str) -> Vec<&Dashboard> {
        self.dashboards
            .values()
            .filter(|d| d.owner_id == user_id || d.shared_with.contains(&user_id.to_string()))
            .collect()
    }

    /// Add a widget to a dashboard.
    pub fn add_widget(&mut self, dashboard_id: &str, widget: Widget) -> bool {
        if let Some(dashboard) = self.dashboards.get_mut(dashboard_id) {
            dashboard.widgets.push(widget);
            true
        } else {
            false
        }
    }

    /// Remove a widget from a dashboard.
    pub fn remove_widget(&mut self, dashboard_id: &str, widget_id: &str) -> bool {
        if let Some(dashboard) = self.dashboards.get_mut(dashboard_id) {
            let before = dashboard.widgets.len();
            dashboard.widgets.retain(|w| w.id != widget_id);
            dashboard.widgets.len() < before
        } else {
            false
        }
    }

    /// Validate widget layout (no overlaps).
    pub fn validate_layout(dashboard: &Dashboard) -> Vec<String> {
        let mut errors = Vec::new();
        for (i, w1) in dashboard.widgets.iter().enumerate() {
            // Check bounds
            if w1.layout.x + w1.layout.width > dashboard.columns {
                errors.push(format!("Widget '{}' exceeds column bounds", w1.id));
            }
            if w1.layout.y + w1.layout.height > dashboard.rows {
                errors.push(format!("Widget '{}' exceeds row bounds", w1.id));
            }
            // Check overlaps
            for w2 in dashboard.widgets.iter().skip(i + 1) {
                if widgets_overlap(&w1.layout, &w2.layout) {
                    errors.push(format!("Widgets '{}' and '{}' overlap", w1.id, w2.id));
                }
            }
        }
        errors
    }
}

impl Default for DashboardStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if two widget layouts overlap.
fn widgets_overlap(a: &WidgetLayout, b: &WidgetLayout) -> bool {
    a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dashboard() -> Dashboard {
        Dashboard {
            id: "dash-1".into(),
            name: "Site Overview".into(),
            owner_id: "user-1".into(),
            widgets: vec![Widget {
                id: "w1".into(),
                widget_type: WidgetType::CesiumView {
                    tileset_ids: vec!["ts-1".into()],
                },
                layout: WidgetLayout {
                    x: 0,
                    y: 0,
                    width: 6,
                    height: 4,
                },
                title: "3D View".into(),
                refresh_interval_secs: None,
            }],
            columns: 12,
            rows: 8,
            shared_with: vec!["user-2".into()],
            created_at: "2024-01-01".into(),
            updated_at: "2024-01-01".into(),
        }
    }

    #[test]
    fn test_create_and_get() {
        let mut store = DashboardStore::new();
        store.create(sample_dashboard());
        let d = store.get("dash-1").unwrap();
        assert_eq!(d.name, "Site Overview");
    }

    #[test]
    fn test_add_widget() {
        let mut store = DashboardStore::new();
        store.create(sample_dashboard());
        store.add_widget(
            "dash-1",
            Widget {
                id: "w2".into(),
                widget_type: WidgetType::KpiCard {
                    metric: "tile_count".into(),
                    label: "Total Tiles".into(),
                },
                layout: WidgetLayout {
                    x: 6,
                    y: 0,
                    width: 3,
                    height: 2,
                },
                title: "Tile Count".into(),
                refresh_interval_secs: Some(30),
            },
        );
        let d = store.get("dash-1").unwrap();
        assert_eq!(d.widgets.len(), 2);
    }

    #[test]
    fn test_validate_no_overlap() {
        let dashboard = sample_dashboard();
        let errors = DashboardStore::validate_layout(&dashboard);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validate_overlap_detected() {
        let dashboard = Dashboard {
            id: "d".into(),
            name: "t".into(),
            owner_id: "u".into(),
            widgets: vec![
                Widget {
                    id: "a".into(),
                    widget_type: WidgetType::AlertList { max_items: 5 },
                    layout: WidgetLayout {
                        x: 0,
                        y: 0,
                        width: 4,
                        height: 4,
                    },
                    title: "".into(),
                    refresh_interval_secs: None,
                },
                Widget {
                    id: "b".into(),
                    widget_type: WidgetType::AlertList { max_items: 5 },
                    layout: WidgetLayout {
                        x: 2,
                        y: 2,
                        width: 4,
                        height: 4,
                    },
                    title: "".into(),
                    refresh_interval_secs: None,
                },
            ],
            columns: 12,
            rows: 8,
            shared_with: vec![],
            created_at: "".into(),
            updated_at: "".into(),
        };
        let errors = DashboardStore::validate_layout(&dashboard);
        assert!(!errors.is_empty());
        assert!(errors[0].contains("overlap"));
    }

    #[test]
    fn test_list_for_user() {
        let mut store = DashboardStore::new();
        store.create(sample_dashboard());
        let user1_dashboards = store.list_for_user("user-1");
        assert_eq!(user1_dashboards.len(), 1);
        let user2_dashboards = store.list_for_user("user-2");
        assert_eq!(user2_dashboards.len(), 1); // shared
    }
}
