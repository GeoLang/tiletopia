//! Digital twin scripting engine — rule-based automation for spatial data.
//!
//! Users define rules like "if sensor > threshold → highlight building red".
//! This is a spatial IFTTT / low-code automation layer.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A trigger condition for a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Trigger {
    /// Sensor value exceeds threshold.
    SensorThreshold {
        sensor_id: String,
        property: String,
        operator: CompareOp,
        value: f64,
    },
    /// A spatial event (object enters/exits region).
    SpatialEvent {
        region: [f64; 6], // [min_x, min_y, min_z, max_x, max_y, max_z]
        event_type: SpatialEventType,
    },
    /// Time-based trigger.
    Schedule {
        cron: String, // cron expression
    },
    /// Manual trigger.
    Manual,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CompareOp {
    GreaterThan,
    LessThan,
    Equals,
    NotEquals,
    GreaterOrEqual,
    LessOrEqual,
}

impl CompareOp {
    pub fn evaluate(&self, left: f64, right: f64) -> bool {
        match self {
            Self::GreaterThan => left > right,
            Self::LessThan => left < right,
            Self::Equals => (left - right).abs() < f64::EPSILON,
            Self::NotEquals => (left - right).abs() >= f64::EPSILON,
            Self::GreaterOrEqual => left >= right,
            Self::LessOrEqual => left <= right,
        }
    }
}

/// Spatial event types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SpatialEventType {
    Enter,
    Exit,
    DwellTime,
}

/// An action to execute when a rule fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    /// Change the color/style of a feature.
    Highlight { feature_id: String, color: [u8; 4] },
    /// Send a notification/alert.
    Alert {
        message: String,
        severity: AlertSeverity,
    },
    /// Update a property value.
    SetProperty {
        feature_id: String,
        property: String,
        value: serde_json::Value,
    },
    /// Trigger a webhook.
    Webhook { url: String, method: String },
    /// Log to the event stream.
    Log { message: String },
}

/// Alert severity levels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

/// A complete automation rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub trigger: Trigger,
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
    /// Cooldown in seconds (prevent repeated firing).
    pub cooldown_secs: u64,
    /// Last time this rule fired.
    pub last_fired: Option<String>,
}

/// Additional condition (AND'd with trigger).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Condition {
    /// Time window.
    TimeWindow { start_hour: u8, end_hour: u8 },
    /// Property check.
    PropertyCheck {
        feature_id: String,
        property: String,
        operator: CompareOp,
        value: f64,
    },
}

/// The scripting engine that evaluates rules against incoming data.
pub struct ScriptEngine {
    rules: Vec<Rule>,
    /// Fired actions log.
    action_log: Vec<FiredAction>,
    /// Current sensor values.
    sensor_state: HashMap<String, HashMap<String, f64>>,
}

/// Record of a fired action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiredAction {
    pub rule_id: Uuid,
    pub rule_name: String,
    pub timestamp: String,
    pub actions: Vec<Action>,
}

/// Event that the engine processes.
#[derive(Debug, Clone)]
pub enum Event {
    SensorUpdate {
        sensor_id: String,
        property: String,
        value: f64,
    },
    SpatialMove {
        object_id: String,
        position: [f64; 3],
    },
    TimerTick,
    ManualTrigger {
        rule_id: Uuid,
    },
}

impl ScriptEngine {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            action_log: Vec::new(),
            sensor_state: HashMap::new(),
        }
    }

    /// Add a rule to the engine.
    pub fn add_rule(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Remove a rule.
    pub fn remove_rule(&mut self, rule_id: Uuid) -> bool {
        let len = self.rules.len();
        self.rules.retain(|r| r.id != rule_id);
        self.rules.len() < len
    }

    /// Get all rules.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Get the action log.
    pub fn action_log(&self) -> &[FiredAction] {
        &self.action_log
    }

    /// Process an event and return any fired actions.
    pub fn process_event(&mut self, event: &Event) -> Vec<FiredAction> {
        // Update internal state
        if let Event::SensorUpdate {
            sensor_id,
            property,
            value,
        } = event
        {
            self.sensor_state
                .entry(sensor_id.clone())
                .or_default()
                .insert(property.clone(), *value);
        }

        let mut fired = Vec::new();
        let now = chrono::Utc::now().to_rfc3339();

        for rule in &mut self.rules {
            if !rule.enabled {
                continue;
            }

            // Check cooldown
            if rule.last_fired.as_ref().is_some_and(|last| {
                chrono::DateTime::parse_from_rfc3339(last)
                    .ok()
                    .is_some_and(|last_time| {
                        chrono::Utc::now()
                            .signed_duration_since(last_time)
                            .num_seconds()
                            < rule.cooldown_secs as i64
                    })
            }) {
                continue;
            }

            let triggered = match (&rule.trigger, event) {
                (
                    Trigger::SensorThreshold {
                        sensor_id,
                        property,
                        operator,
                        value,
                    },
                    Event::SensorUpdate {
                        sensor_id: ev_sensor,
                        property: ev_prop,
                        value: ev_val,
                    },
                ) => {
                    sensor_id == ev_sensor
                        && property == ev_prop
                        && operator.evaluate(*ev_val, *value)
                }
                (Trigger::Manual, Event::ManualTrigger { rule_id }) => *rule_id == rule.id,
                _ => false,
            };

            if triggered {
                rule.last_fired = Some(now.clone());
                let action = FiredAction {
                    rule_id: rule.id,
                    rule_name: rule.name.clone(),
                    timestamp: now.clone(),
                    actions: rule.actions.clone(),
                };
                fired.push(action);
            }
        }

        self.action_log.extend(fired.clone());
        fired
    }
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temperature_rule() -> Rule {
        Rule {
            id: Uuid::new_v4(),
            name: "High Temperature Alert".to_string(),
            description: Some("Alert when sensor exceeds 80°C".to_string()),
            enabled: true,
            trigger: Trigger::SensorThreshold {
                sensor_id: "temp-sensor-1".to_string(),
                property: "temperature".to_string(),
                operator: CompareOp::GreaterThan,
                value: 80.0,
            },
            conditions: Vec::new(),
            actions: vec![
                Action::Highlight {
                    feature_id: "building-1".to_string(),
                    color: [255, 0, 0, 255],
                },
                Action::Alert {
                    message: "Temperature exceeds 80°C!".to_string(),
                    severity: AlertSeverity::Critical,
                },
            ],
            cooldown_secs: 60,
            last_fired: None,
        }
    }

    #[test]
    fn test_sensor_trigger_fires() {
        let mut engine = ScriptEngine::new();
        engine.add_rule(temperature_rule());

        let event = Event::SensorUpdate {
            sensor_id: "temp-sensor-1".to_string(),
            property: "temperature".to_string(),
            value: 85.0,
        };
        let fired = engine.process_event(&event);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].actions.len(), 2);
    }

    #[test]
    fn test_sensor_trigger_no_fire_below_threshold() {
        let mut engine = ScriptEngine::new();
        engine.add_rule(temperature_rule());

        let event = Event::SensorUpdate {
            sensor_id: "temp-sensor-1".to_string(),
            property: "temperature".to_string(),
            value: 75.0,
        };
        let fired = engine.process_event(&event);
        assert_eq!(fired.len(), 0);
    }

    #[test]
    fn test_cooldown_prevents_repeated_fire() {
        let mut engine = ScriptEngine::new();
        let mut rule = temperature_rule();
        rule.cooldown_secs = 3600; // 1 hour cooldown
        engine.add_rule(rule);

        let event = Event::SensorUpdate {
            sensor_id: "temp-sensor-1".to_string(),
            property: "temperature".to_string(),
            value: 85.0,
        };
        let fired1 = engine.process_event(&event);
        assert_eq!(fired1.len(), 1);

        // Second event within cooldown should not fire
        let fired2 = engine.process_event(&event);
        assert_eq!(fired2.len(), 0);
    }

    #[test]
    fn test_disabled_rule() {
        let mut engine = ScriptEngine::new();
        let mut rule = temperature_rule();
        rule.enabled = false;
        engine.add_rule(rule);

        let event = Event::SensorUpdate {
            sensor_id: "temp-sensor-1".to_string(),
            property: "temperature".to_string(),
            value: 85.0,
        };
        let fired = engine.process_event(&event);
        assert_eq!(fired.len(), 0);
    }

    #[test]
    fn test_compare_ops() {
        assert!(CompareOp::GreaterThan.evaluate(5.0, 3.0));
        assert!(!CompareOp::GreaterThan.evaluate(3.0, 5.0));
        assert!(CompareOp::LessThan.evaluate(3.0, 5.0));
        assert!(CompareOp::Equals.evaluate(5.0, 5.0));
        assert!(CompareOp::NotEquals.evaluate(5.0, 3.0));
    }
}
