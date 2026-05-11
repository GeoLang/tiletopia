//! Entity linking — maps external entity IDs (buildings, sensors, IoT devices)
//! to 3D Tiles assets with spatial queries.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A link between an external entity and a 3D Tiles asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityLink {
    pub id: Uuid,
    pub entity_id: String,
    pub entity_type: EntityType,
    pub asset_id: Uuid,
    pub position: Option<[f64; 3]>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Type of external entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    Building,
    Sensor,
    Device,
    Infrastructure,
    Vehicle,
    Custom(String),
}

/// Store for entity links with spatial indexing.
pub struct EntityLinkStore {
    links: Vec<EntityLink>,
    by_entity: HashMap<String, Vec<usize>>,
}

impl EntityLinkStore {
    pub fn new() -> Self {
        Self {
            links: Vec::new(),
            by_entity: HashMap::new(),
        }
    }

    /// Create a new entity link.
    pub fn create(&mut self, link: EntityLink) -> &EntityLink {
        let idx = self.links.len();
        self.by_entity
            .entry(link.entity_id.clone())
            .or_default()
            .push(idx);
        self.links.push(link);
        &self.links[idx]
    }

    /// Get a link by its ID.
    pub fn get(&self, id: Uuid) -> Option<&EntityLink> {
        self.links.iter().find(|l| l.id == id)
    }

    /// Update an existing link's metadata and position.
    pub fn update(
        &mut self,
        id: Uuid,
        position: Option<[f64; 3]>,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> Option<&EntityLink> {
        let link = self.links.iter_mut().find(|l| l.id == id)?;
        if let Some(pos) = position {
            link.position = Some(pos);
        }
        if let Some(meta) = metadata {
            link.metadata.extend(meta);
        }
        link.updated_at = chrono::Utc::now();
        Some(link)
    }

    /// Delete a link by ID. Returns true if found and removed.
    pub fn delete(&mut self, id: Uuid) -> bool {
        if let Some(pos) = self.links.iter().position(|l| l.id == id) {
            let entity_id = self.links[pos].entity_id.clone();
            self.links.remove(pos);
            // Rebuild index for this entity
            self.by_entity.remove(&entity_id);
            for (i, link) in self.links.iter().enumerate() {
                if link.entity_id == entity_id {
                    self.by_entity.entry(entity_id.clone()).or_default().push(i);
                }
            }
            true
        } else {
            false
        }
    }

    /// Find all 3D tile assets linked to a given entity ID.
    pub fn query_by_entity(&self, entity_id: &str) -> Vec<&EntityLink> {
        match self.by_entity.get(entity_id) {
            Some(indices) => indices.iter().filter_map(|&i| self.links.get(i)).collect(),
            None => Vec::new(),
        }
    }

    /// Find entities within `radius` meters of a 3D position.
    pub fn query_by_position(&self, center: [f64; 3], radius: f64) -> Vec<&EntityLink> {
        let r2 = radius * radius;
        self.links
            .iter()
            .filter(|l| {
                if let Some(pos) = l.position {
                    let dx = pos[0] - center[0];
                    let dy = pos[1] - center[1];
                    let dz = pos[2] - center[2];
                    dx * dx + dy * dy + dz * dz <= r2
                } else {
                    false
                }
            })
            .collect()
    }

    /// List all links, optionally filtered by entity type.
    pub fn list(&self, entity_type: Option<&EntityType>) -> Vec<&EntityLink> {
        match entity_type {
            Some(et) => self.links.iter().filter(|l| &l.entity_type == et).collect(),
            None => self.links.iter().collect(),
        }
    }
}

impl Default for EntityLinkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_link(entity_id: &str, pos: [f64; 3]) -> EntityLink {
        EntityLink {
            id: Uuid::new_v4(),
            entity_id: entity_id.into(),
            entity_type: EntityType::Building,
            asset_id: Uuid::new_v4(),
            position: Some(pos),
            metadata: HashMap::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_crud() {
        let mut store = EntityLinkStore::new();
        let link = make_link("building-1", [1.0, 2.0, 3.0]);
        let id = link.id;
        store.create(link);
        assert!(store.get(id).is_some());
        store.update(id, Some([4.0, 5.0, 6.0]), None);
        assert_eq!(store.get(id).unwrap().position.unwrap(), [4.0, 5.0, 6.0]);
        assert!(store.delete(id));
        assert!(store.get(id).is_none());
    }

    #[test]
    fn test_query_by_entity() {
        let mut store = EntityLinkStore::new();
        store.create(make_link("sensor-42", [0.0, 0.0, 0.0]));
        store.create(make_link("sensor-42", [1.0, 0.0, 0.0]));
        store.create(make_link("sensor-99", [2.0, 0.0, 0.0]));
        assert_eq!(store.query_by_entity("sensor-42").len(), 2);
        assert_eq!(store.query_by_entity("sensor-99").len(), 1);
        assert_eq!(store.query_by_entity("missing").len(), 0);
    }

    #[test]
    fn test_query_by_position() {
        let mut store = EntityLinkStore::new();
        store.create(make_link("a", [0.0, 0.0, 0.0]));
        store.create(make_link("b", [10.0, 0.0, 0.0]));
        store.create(make_link("c", [100.0, 0.0, 0.0]));
        let nearby = store.query_by_position([0.0, 0.0, 0.0], 15.0);
        assert_eq!(nearby.len(), 2);
    }
}
