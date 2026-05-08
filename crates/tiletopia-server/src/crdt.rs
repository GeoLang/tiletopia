//! CRDT-based collaborative annotations — conflict-free concurrent editing.
//!
//! Multiple users can edit annotations simultaneously with automatic
//! merge using Last-Writer-Wins Register (LWW) and Observed-Remove Set (OR-Set).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A logical timestamp for CRDT ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HLC {
    /// Wall clock (milliseconds since epoch).
    pub wall: u64,
    /// Logical counter for same-wall-clock events.
    pub counter: u32,
    /// Node ID (unique per client).
    pub node: u32,
}

impl HLC {
    pub fn new(node: u32) -> Self {
        Self {
            wall: current_millis(),
            counter: 0,
            node,
        }
    }

    /// Advance the clock (local event).
    pub fn tick(&mut self) -> Self {
        let now = current_millis();
        if now > self.wall {
            self.wall = now;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        *self
    }

    /// Merge with a remote timestamp (receive event).
    pub fn merge(&mut self, remote: HLC) -> Self {
        let now = current_millis();
        if now > self.wall && now > remote.wall {
            self.wall = now;
            self.counter = 0;
        } else if self.wall == remote.wall {
            self.counter = self.counter.max(remote.counter) + 1;
        } else if remote.wall > self.wall {
            self.wall = remote.wall;
            self.counter = remote.counter + 1;
        } else {
            self.counter += 1;
        }
        *self
    }
}

/// A Last-Writer-Wins Register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LWWRegister<T: Clone> {
    pub value: T,
    pub timestamp: HLC,
}

impl<T: Clone> LWWRegister<T> {
    pub fn new(value: T, timestamp: HLC) -> Self {
        Self { value, timestamp }
    }

    /// Update the register (only succeeds if timestamp is newer).
    pub fn update(&mut self, value: T, timestamp: HLC) -> bool {
        if timestamp > self.timestamp {
            self.value = value;
            self.timestamp = timestamp;
            true
        } else {
            false
        }
    }

    /// Merge with a remote register.
    pub fn merge(&mut self, other: &LWWRegister<T>) {
        if other.timestamp > self.timestamp {
            self.value = other.value.clone();
            self.timestamp = other.timestamp;
        }
    }
}

/// An element in an OR-Set (Observed-Remove Set).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ORSetElement<T: Clone> {
    value: T,
    /// Unique add-tags (each add gets a unique tag).
    add_tags: Vec<Uuid>,
    /// Removed tags.
    remove_tags: Vec<Uuid>,
}

/// Observed-Remove Set — supports concurrent add/remove without conflicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ORSet<T: Clone + PartialEq> {
    elements: Vec<ORSetElement<T>>,
}

impl<T: Clone + PartialEq> ORSet<T> {
    pub fn new() -> Self {
        Self {
            elements: Vec::new(),
        }
    }

    /// Add an element to the set.
    pub fn add(&mut self, value: T) -> Uuid {
        let tag = Uuid::new_v4();
        if let Some(elem) = self.elements.iter_mut().find(|e| e.value == value) {
            elem.add_tags.push(tag);
        } else {
            self.elements.push(ORSetElement {
                value,
                add_tags: vec![tag],
                remove_tags: Vec::new(),
            });
        }
        tag
    }

    /// Remove an element (marks all current add-tags as removed).
    pub fn remove(&mut self, value: &T) -> bool {
        if let Some(elem) = self.elements.iter_mut().find(|e| e.value == *value) {
            elem.remove_tags.extend(elem.add_tags.iter().copied());
            true
        } else {
            false
        }
    }

    /// Get all currently-present elements.
    pub fn values(&self) -> Vec<&T> {
        self.elements
            .iter()
            .filter(|e| {
                // Element is present if it has add-tags not in remove-tags
                e.add_tags.iter().any(|t| !e.remove_tags.contains(t))
            })
            .map(|e| &e.value)
            .collect()
    }

    /// Merge with a remote OR-Set.
    pub fn merge(&mut self, other: &ORSet<T>) {
        for remote_elem in &other.elements {
            if let Some(local_elem) = self
                .elements
                .iter_mut()
                .find(|e| e.value == remote_elem.value)
            {
                // Union add-tags, union remove-tags
                for tag in &remote_elem.add_tags {
                    if !local_elem.add_tags.contains(tag) {
                        local_elem.add_tags.push(*tag);
                    }
                }
                for tag in &remote_elem.remove_tags {
                    if !local_elem.remove_tags.contains(tag) {
                        local_elem.remove_tags.push(*tag);
                    }
                }
            } else {
                self.elements.push(remote_elem.clone());
            }
        }
    }

    pub fn len(&self) -> usize {
        self.values().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T: Clone + PartialEq> Default for ORSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A collaborative annotation using CRDTs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtAnnotation {
    pub id: Uuid,
    pub text: LWWRegister<String>,
    pub position: LWWRegister<[f64; 3]>,
    pub color: LWWRegister<[u8; 4]>,
    pub tags: ORSet<String>,
}

/// A collaborative annotation layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtAnnotationLayer {
    pub id: Uuid,
    pub name: LWWRegister<String>,
    pub annotations: HashMap<Uuid, CrdtAnnotation>,
    clock: HLC,
}

impl CrdtAnnotationLayer {
    pub fn new(name: String, node_id: u32) -> Self {
        let mut clock = HLC::new(node_id);
        let ts = clock.tick();
        Self {
            id: Uuid::new_v4(),
            name: LWWRegister::new(name, ts),
            annotations: HashMap::new(),
            clock,
        }
    }

    /// Add a new annotation.
    pub fn add_annotation(&mut self, text: String, position: [f64; 3]) -> Uuid {
        let ts = self.clock.tick();
        let id = Uuid::new_v4();
        self.annotations.insert(
            id,
            CrdtAnnotation {
                id,
                text: LWWRegister::new(text, ts),
                position: LWWRegister::new(position, ts),
                color: LWWRegister::new([255, 255, 0, 255], ts),
                tags: ORSet::new(),
            },
        );
        id
    }

    /// Update annotation text.
    pub fn update_text(&mut self, annotation_id: Uuid, text: String) -> bool {
        let ts = self.clock.tick();
        if let Some(ann) = self.annotations.get_mut(&annotation_id) {
            ann.text.update(text, ts)
        } else {
            false
        }
    }

    /// Merge with a remote layer state.
    pub fn merge(&mut self, remote: &CrdtAnnotationLayer) {
        self.clock.merge(remote.clock);
        self.name.merge(&remote.name);

        for (id, remote_ann) in &remote.annotations {
            if let Some(local_ann) = self.annotations.get_mut(id) {
                local_ann.text.merge(&remote_ann.text);
                local_ann.position.merge(&remote_ann.position);
                local_ann.color.merge(&remote_ann.color);
                local_ann.tags.merge(&remote_ann.tags);
            } else {
                self.annotations.insert(*id, remote_ann.clone());
            }
        }
    }

    pub fn annotation_count(&self) -> usize {
        self.annotations.len()
    }
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hlc_ordering() {
        let mut clock1 = HLC::new(1);
        let mut clock2 = HLC::new(2);
        let t1 = clock1.tick();
        let t2 = clock2.tick();
        // Both should have valid timestamps
        assert!(t1.wall > 0);
        assert!(t2.wall > 0);
    }

    #[test]
    fn test_lww_register() {
        let mut clock = HLC::new(1);
        let t1 = clock.tick();
        let t2 = clock.tick();
        let mut reg = LWWRegister::new("hello".to_string(), t1);
        assert!(reg.update("world".to_string(), t2));
        assert_eq!(reg.value, "world");
        // Older timestamp should not update
        assert!(!reg.update("old".to_string(), t1));
        assert_eq!(reg.value, "world");
    }

    #[test]
    fn test_or_set_add_remove() {
        let mut set: ORSet<String> = ORSet::new();
        set.add("apple".to_string());
        set.add("banana".to_string());
        assert_eq!(set.len(), 2);
        set.remove(&"apple".to_string());
        assert_eq!(set.len(), 1);
        assert_eq!(set.values()[0], "banana");
    }

    #[test]
    fn test_or_set_concurrent_add_remove() {
        // Simulate concurrent operations
        let mut set_a: ORSet<String> = ORSet::new();
        let mut set_b: ORSet<String> = ORSet::new();

        // A adds "item"
        set_a.add("item".to_string());
        // B also adds "item" (concurrent)
        set_b.add("item".to_string());
        // A removes "item"
        set_a.remove(&"item".to_string());

        // Merge: B's add should survive A's remove (add-wins semantics)
        set_a.merge(&set_b);
        assert_eq!(set_a.len(), 1); // B's add tag is not in A's remove set
    }

    #[test]
    fn test_crdt_annotation_layer_merge() {
        let mut layer1 = CrdtAnnotationLayer::new("Layer 1".to_string(), 1);
        let mut layer2 = CrdtAnnotationLayer::new("Layer 1".to_string(), 2);

        let id1 = layer1.add_annotation("Point A".to_string(), [1.0, 2.0, 3.0]);
        let _id2 = layer2.add_annotation("Point B".to_string(), [4.0, 5.0, 6.0]);

        // User 1 updates their annotation
        layer1.update_text(id1, "Point A (updated)".to_string());

        // Merge: both annotations should exist
        layer1.merge(&layer2);
        assert_eq!(layer1.annotation_count(), 2);
        assert_eq!(layer1.annotations[&id1].text.value, "Point A (updated)");
    }
}
