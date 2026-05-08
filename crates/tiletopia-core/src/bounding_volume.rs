//! Axis-aligned bounding box operations.

/// Axis-aligned bounding box in 3D space.
#[derive(Debug, Clone, Copy)]
pub struct Aabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb {
    /// Create from min/max corners.
    pub fn new(min: [f64; 3], max: [f64; 3]) -> Self {
        Self { min, max }
    }

    /// Create an empty (inverted) AABB suitable for expansion.
    pub fn empty() -> Self {
        Self {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
        }
    }

    /// Expand to include a point.
    pub fn expand_point(&mut self, p: [f64; 3]) {
        for i in 0..3 {
            self.min[i] = self.min[i].min(p[i]);
            self.max[i] = self.max[i].max(p[i]);
        }
    }

    /// Center of the AABB.
    pub fn center(&self) -> [f64; 3] {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// Half-extents along each axis.
    pub fn half_extents(&self) -> [f64; 3] {
        [
            (self.max[0] - self.min[0]) * 0.5,
            (self.max[1] - self.min[1]) * 0.5,
            (self.max[2] - self.min[2]) * 0.5,
        ]
    }

    /// Maximum extent (longest axis length).
    pub fn max_extent(&self) -> f64 {
        let h = self.half_extents();
        h[0].max(h[1]).max(h[2]) * 2.0
    }

    /// Split into 8 octants.
    pub fn octants(&self) -> [Aabb; 8] {
        let c = self.center();
        let [mx, my, mz] = self.min;
        let [cx, cy, cz] = c;
        let [ax, ay, az] = self.max;

        [
            Aabb::new([mx, my, mz], [cx, cy, cz]),
            Aabb::new([cx, my, mz], [ax, cy, cz]),
            Aabb::new([mx, cy, mz], [cx, ay, cz]),
            Aabb::new([cx, cy, mz], [ax, ay, cz]),
            Aabb::new([mx, my, cz], [cx, cy, az]),
            Aabb::new([cx, my, cz], [ax, cy, az]),
            Aabb::new([mx, cy, cz], [cx, ay, az]),
            Aabb::new([cx, cy, cz], [ax, ay, az]),
        ]
    }

    /// Check if a point is inside this AABB.
    pub fn contains_point(&self, p: [f64; 3]) -> bool {
        p[0] >= self.min[0]
            && p[0] <= self.max[0]
            && p[1] >= self.min[1]
            && p[1] <= self.max[1]
            && p[2] >= self.min[2]
            && p[2] <= self.max[2]
    }

    /// Convert to 3D Tiles oriented bounding box (12 floats: center + 3 half-axis vectors).
    pub fn to_3dtiles_box(&self) -> [f64; 12] {
        let c = self.center();
        let h = self.half_extents();
        [
            c[0], c[1], c[2], // center
            h[0], 0.0, 0.0, // x half-axis
            0.0, h[1], 0.0, // y half-axis
            0.0, 0.0, h[2], // z half-axis
        ]
    }
}
