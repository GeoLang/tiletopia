//! Geoprocessing / spatial analysis — buffer, boolean overlay, hull, centroid, simplify.
//!
//! Input and output geometry is GeoJSON-shaped lon/lat degrees. Buffer distances
//! are meters. The buffer and the boolean overlay come from the `geo` crate, so
//! a buffered square gains rounded corners and a union of two overlapping
//! squares is the L-shaped region rather than a hull around both.

use geo::geometry::{Coord, LineString, MultiPolygon, Point, Polygon};
use geo::{BooleanOps, Buffer, Centroid, CoordsIter, Geodesic, GeodesicArea, Length, MapCoords};
use serde::{Deserialize, Serialize};

/// Meters per degree of latitude, and of longitude at the equator.
pub const METERS_PER_DEGREE: f64 = 111320.0;

/// A GeoJSON linear ring needs four positions: three corners and a repeat of the first.
const MIN_RING_POSITIONS: usize = 4;

/// A line needs two positions to have a direction.
const MIN_LINE_POSITIONS: usize = 2;

/// Past this latitude the local projection's degree of longitude shrinks to
/// almost nothing and the unprojection blows up.
const MAX_BUFFER_LATITUDE: f64 = 89.0;

/// Which of a request's two geometries an error is about.
const INPUT: &str = "input";
const OTHER: &str = "other";

const BUFFER: &str = "Buffer";
const UNION: &str = "Union";
const INTERSECTION: &str = "Intersection";
const DIFFERENCE: &str = "Difference";
const CONVEX_HULL: &str = "ConvexHull";
const CENTROID: &str = "Centroid";
const SIMPLIFY: &str = "Simplify";

/// Every operation [`run`] accepts, in the order `/operations` advertises them.
pub const OPERATIONS: [&str; 7] = [
    BUFFER,
    UNION,
    INTERSECTION,
    DIFFERENCE,
    CONVEX_HULL,
    CENTROID,
    SIMPLIFY,
];

/// Why an operation could not be run.
#[derive(Debug, thiserror::Error)]
pub enum GeoprocessingError {
    #[error("'{name}' is not a geoprocessing operation, accepted: {}", OPERATIONS.join(", "))]
    UnknownOperation { name: String },
    #[error("buffer needs a distance_m that is a finite number")]
    MissingDistance,
    #[error("simplify needs a tolerance in degrees that is a finite number above zero")]
    MissingTolerance,
    #[error("{operation} works on two geometries and needs a second one in `other`")]
    MissingSecondGeometry { operation: &'static str },
    #[error("the {role} geometry has a coordinate that is not a finite number")]
    NonFiniteCoordinate { role: &'static str },
    #[error("the {role} geometry needs at least {needed} positions where it has {count}")]
    NotEnoughPositions {
        role: &'static str,
        count: usize,
        needed: usize,
    },
    #[error("{operation} needs a Polygon or MultiPolygon, not a {given}")]
    NotAPolygon {
        operation: &'static str,
        given: &'static str,
    },
    #[error("simplify needs a LineString, Polygon or MultiPolygon, not a {given}")]
    CannotSimplify { given: &'static str },
    #[error(
        "buffering is refused at latitude {latitude}, past {MAX_BUFFER_LATITUDE} degrees the local projection collapses"
    )]
    BufferNearPole { latitude: f64 },
    #[error("the {role} geometry has no centroid, every position is the same point")]
    NoCentroid { role: &'static str },
    #[error("the input positions are collinear, so their convex hull has no area")]
    CollinearHull,
}

/// A GeoJSON-shaped geometry: `{"type": ..., "coordinates": ...}` with GeoJSON's
/// nesting, so a `Polygon` carries rings and a `MultiPolygon` carries polygons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "coordinates")]
pub enum Geometry {
    Point([f64; 2]),
    LineString(Vec<[f64; 2]>),
    Polygon(Vec<Vec<[f64; 2]>>),
    MultiPolygon(Vec<Vec<Vec<[f64; 2]>>>),
}

impl Geometry {
    pub fn type_name(&self) -> &'static str {
        match self {
            Geometry::Point(_) => "Point",
            Geometry::LineString(_) => "LineString",
            Geometry::Polygon(_) => "Polygon",
            Geometry::MultiPolygon(_) => "MultiPolygon",
        }
    }
}

/// An operation and the parameters it takes.
#[derive(Debug, Clone, PartialEq)]
pub enum GeoOperation {
    /// Expand (or, for a negative distance, shrink) a geometry by a distance in meters.
    Buffer { distance_m: f64 },
    /// The region covered by either geometry.
    Union,
    /// The region covered by both geometries.
    Intersection,
    /// The region of the first geometry not covered by the second.
    Difference,
    /// The smallest convex polygon containing every position.
    ConvexHull,
    /// The area-weighted centre of the geometry.
    Centroid,
    /// Drop vertices within a tolerance in degrees (Douglas-Peucker).
    Simplify { tolerance: f64 },
}

impl GeoOperation {
    /// Read an operation from a request, matching the names `/operations` lists
    /// without regard to case.
    pub fn parse(
        name: &str,
        distance_m: Option<f64>,
        tolerance: Option<f64>,
    ) -> Result<Self, GeoprocessingError> {
        let matches = |canonical: &str| name.eq_ignore_ascii_case(canonical);
        if matches(BUFFER) {
            let distance_m = distance_m
                .filter(|distance| distance.is_finite())
                .ok_or(GeoprocessingError::MissingDistance)?;
            return Ok(Self::Buffer { distance_m });
        }
        if matches(SIMPLIFY) {
            let tolerance = tolerance
                .filter(|tolerance| tolerance.is_finite() && *tolerance > 0.0)
                .ok_or(GeoprocessingError::MissingTolerance)?;
            return Ok(Self::Simplify { tolerance });
        }
        if matches(UNION) {
            return Ok(Self::Union);
        }
        if matches(INTERSECTION) {
            return Ok(Self::Intersection);
        }
        if matches(DIFFERENCE) {
            return Ok(Self::Difference);
        }
        if matches(CONVEX_HULL) {
            return Ok(Self::ConvexHull);
        }
        if matches(CENTROID) {
            return Ok(Self::Centroid);
        }
        Err(GeoprocessingError::UnknownOperation {
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Buffer { .. } => BUFFER,
            Self::Union => UNION,
            Self::Intersection => INTERSECTION,
            Self::Difference => DIFFERENCE,
            Self::ConvexHull => CONVEX_HULL,
            Self::Centroid => CENTROID,
            Self::Simplify { .. } => SIMPLIFY,
        }
    }
}

/// What an operation produced. `area_m2` and `length_m` are geodesic measures on
/// the WGS84 ellipsoid, and are absent where the result has no such measure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoprocessingResult {
    pub operation: String,
    pub geometry: Geometry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area_m2: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_m: Option<f64>,
}

/// Run one operation over one or two geometries.
pub fn run(
    operation: &GeoOperation,
    input: &Geometry,
    other: Option<&Geometry>,
) -> Result<GeoprocessingResult, GeoprocessingError> {
    let (geometry, area_m2, length_m) = match operation {
        GeoOperation::Buffer { distance_m } => multi_polygon_result(buffer(input, *distance_m)?),
        GeoOperation::Union => multi_polygon_result(polygon_union(input, second(other, UNION)?)?),
        GeoOperation::Intersection => {
            multi_polygon_result(polygon_intersection(input, second(other, INTERSECTION)?)?)
        }
        GeoOperation::Difference => {
            multi_polygon_result(polygon_difference(input, second(other, DIFFERENCE)?)?)
        }
        GeoOperation::ConvexHull => polygon_result(convex_hull_polygon(input)?),
        GeoOperation::Centroid => point_result(centroid(input)?),
        GeoOperation::Simplify { tolerance } => simplified(input, *tolerance)?,
    };
    Ok(GeoprocessingResult {
        operation: operation.name().to_string(),
        geometry,
        area_m2,
        length_m,
    })
}

fn second<'a>(
    other: Option<&'a Geometry>,
    operation: &'static str,
) -> Result<&'a Geometry, GeoprocessingError> {
    other.ok_or(GeoprocessingError::MissingSecondGeometry { operation })
}

/// Buffer a geometry by a distance in meters, positive to expand and negative to
/// shrink. Always answers a MultiPolygon, one part per piece of the result.
pub fn buffer(
    geometry: &Geometry,
    distance_m: f64,
) -> Result<MultiPolygon<f64>, GeoprocessingError> {
    if !distance_m.is_finite() {
        return Err(GeoprocessingError::MissingDistance);
    }
    let input = geo_geometry(geometry, INPUT)?;
    let latitude = input
        .centroid()
        .ok_or(GeoprocessingError::NoCentroid { role: INPUT })?
        .y();
    if latitude.abs() > MAX_BUFFER_LATITUDE {
        return Err(GeoprocessingError::BufferNearPole { latitude });
    }

    // buffering happens in a local equirectangular frame about the centroid
    // latitude: fine for local-scale geometry, wrong near the poles and for
    // continent-scale shapes
    let longitude_scale = METERS_PER_DEGREE * latitude.to_radians().cos();
    let projected = input.map_coords(|coord| Coord {
        x: coord.x * longitude_scale,
        y: coord.y * METERS_PER_DEGREE,
    });
    Ok(projected.buffer(distance_m).map_coords(|coord| Coord {
        x: coord.x / longitude_scale,
        y: coord.y / METERS_PER_DEGREE,
    }))
}

/// The region covered by either polygon.
pub fn polygon_union(a: &Geometry, b: &Geometry) -> Result<MultiPolygon<f64>, GeoprocessingError> {
    let (left, right) = overlay_inputs(a, b, UNION)?;
    Ok(left.union(&right))
}

/// The region covered by both polygons. Correct for concave polygons, unlike a
/// half-plane clip.
pub fn polygon_intersection(
    a: &Geometry,
    b: &Geometry,
) -> Result<MultiPolygon<f64>, GeoprocessingError> {
    let (left, right) = overlay_inputs(a, b, INTERSECTION)?;
    Ok(left.intersection(&right))
}

/// The region of `a` not covered by `b`. Cuts holes and notches, and may answer
/// several parts.
pub fn polygon_difference(
    a: &Geometry,
    b: &Geometry,
) -> Result<MultiPolygon<f64>, GeoprocessingError> {
    let (left, right) = overlay_inputs(a, b, DIFFERENCE)?;
    Ok(left.difference(&right))
}

fn overlay_inputs(
    a: &Geometry,
    b: &Geometry,
    operation: &'static str,
) -> Result<(MultiPolygon<f64>, MultiPolygon<f64>), GeoprocessingError> {
    Ok((
        polygons(a, INPUT, operation)?,
        polygons(b, OTHER, operation)?,
    ))
}

/// The area-weighted centroid of a geometry.
pub fn centroid(geometry: &Geometry) -> Result<Point<f64>, GeoprocessingError> {
    geo_geometry(geometry, INPUT)?
        .centroid()
        .ok_or(GeoprocessingError::NoCentroid { role: INPUT })
}

fn convex_hull_polygon(geometry: &Geometry) -> Result<Polygon<f64>, GeoprocessingError> {
    let positions: Vec<[f64; 2]> = geo_geometry(geometry, INPUT)?
        .coords_iter()
        .map(|coord| [coord.x, coord.y])
        .collect();
    let hull = convex_hull(&positions);
    if hull.len() < MIN_RING_POSITIONS {
        return Err(GeoprocessingError::CollinearHull);
    }
    Ok(Polygon::new(line_string_from(&hull), Vec::new()))
}

fn simplified(
    geometry: &Geometry,
    tolerance: f64,
) -> Result<(Geometry, Option<f64>, Option<f64>), GeoprocessingError> {
    match geo_geometry(geometry, INPUT)? {
        geo::Geometry::LineString(line) => Ok(line_string_result(simplify_line(&line, tolerance))),
        geo::Geometry::Polygon(polygon) => {
            Ok(polygon_result(simplify_polygon(&polygon, tolerance)))
        }
        geo::Geometry::MultiPolygon(multi) => Ok(multi_polygon_result(MultiPolygon::new(
            multi
                .iter()
                .map(|polygon| simplify_polygon(polygon, tolerance))
                .collect(),
        ))),
        _ => Err(GeoprocessingError::CannotSimplify {
            given: geometry.type_name(),
        }),
    }
}

fn simplify_line(line: &LineString<f64>, tolerance: f64) -> LineString<f64> {
    line_string_from(&simplify(&positions_of(line), tolerance))
}

fn simplify_polygon(polygon: &Polygon<f64>, tolerance: f64) -> Polygon<f64> {
    Polygon::new(
        simplify_ring(polygon.exterior(), tolerance),
        polygon
            .interiors()
            .iter()
            .map(|ring| simplify_ring(ring, tolerance))
            .collect(),
    )
}

fn simplify_ring(ring: &LineString<f64>, tolerance: f64) -> LineString<f64> {
    let simplified = simplify(&positions_of(ring), tolerance);
    if simplified.len() < MIN_RING_POSITIONS {
        return ring.clone();
    }
    line_string_from(&simplified)
}

/// Compute convex hull of a point set (Graham scan).
pub fn convex_hull(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }

    let mut pts: Vec<[f64; 2]> = points.to_vec();
    // Find lowest point (min y, then min x)
    pts.sort_by(|a, b| {
        a[1].partial_cmp(&b[1])
            .unwrap()
            .then(a[0].partial_cmp(&b[0]).unwrap())
    });
    let pivot = pts[0];

    // Sort by polar angle
    pts[1..].sort_by(|a, b| {
        let angle_a = (a[1] - pivot[1]).atan2(a[0] - pivot[0]);
        let angle_b = (b[1] - pivot[1]).atan2(b[0] - pivot[0]);
        angle_a.partial_cmp(&angle_b).unwrap()
    });

    let mut hull: Vec<[f64; 2]> = Vec::new();
    for p in &pts {
        while hull.len() >= 2 && cross(&hull[hull.len() - 2], &hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(*p);
    }
    hull.push(hull[0]); // close ring
    hull
}

/// Cross product of vectors OA and OB.
fn cross(o: &[f64; 2], a: &[f64; 2], b: &[f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Simplify a polyline using the Douglas-Peucker algorithm. The tolerance is in
/// the units of the coordinates.
pub fn simplify(points: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut max_dist = 0.0;
    let mut index = 0;
    let end = points.len() - 1;

    for i in 1..end {
        let d = perpendicular_distance(&points[i], &points[0], &points[end]);
        if d > max_dist {
            max_dist = d;
            index = i;
        }
    }

    if max_dist > tolerance {
        let mut left = simplify(&points[..=index], tolerance);
        let right = simplify(&points[index..], tolerance);
        left.pop(); // remove duplicate
        left.extend_from_slice(&right);
        left
    } else {
        vec![points[0], points[end]]
    }
}

/// Perpendicular distance from point to line segment.
fn perpendicular_distance(point: &[f64; 2], line_start: &[f64; 2], line_end: &[f64; 2]) -> f64 {
    let dx = line_end[0] - line_start[0];
    let dy = line_end[1] - line_start[1];
    let mag = (dx * dx + dy * dy).sqrt();
    if mag < 1e-10 {
        let pdx = point[0] - line_start[0];
        let pdy = point[1] - line_start[1];
        return (pdx * pdx + pdy * pdy).sqrt();
    }
    ((point[0] - line_start[0]) * dy - (point[1] - line_start[1]) * dx).abs() / mag
}

/// Ray-casting point-in-polygon test against a single ring.
pub fn point_in_polygon(point: &[f64; 2], polygon: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        if ((polygon[i][1] > point[1]) != (polygon[j][1] > point[1]))
            && (point[0]
                < (polygon[j][0] - polygon[i][0]) * (point[1] - polygon[i][1])
                    / (polygon[j][1] - polygon[i][1])
                    + polygon[i][0])
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn point_result(point: Point<f64>) -> (Geometry, Option<f64>, Option<f64>) {
    (Geometry::Point([point.x(), point.y()]), None, None)
}

fn line_string_result(line: LineString<f64>) -> (Geometry, Option<f64>, Option<f64>) {
    let length_m = Geodesic.length(&line);
    (
        Geometry::LineString(positions_of(&line)),
        None,
        Some(length_m),
    )
}

fn polygon_result(polygon: Polygon<f64>) -> (Geometry, Option<f64>, Option<f64>) {
    let area_m2 = polygon.geodesic_area_unsigned();
    let length_m = polygon.geodesic_perimeter();
    (
        Geometry::Polygon(rings_of(&polygon)),
        Some(area_m2),
        Some(length_m),
    )
}

fn multi_polygon_result(multi: MultiPolygon<f64>) -> (Geometry, Option<f64>, Option<f64>) {
    let area_m2 = multi.geodesic_area_unsigned();
    let length_m = multi.geodesic_perimeter();
    (
        Geometry::MultiPolygon(multi.iter().map(rings_of).collect()),
        Some(area_m2),
        Some(length_m),
    )
}

fn rings_of(polygon: &Polygon<f64>) -> Vec<Vec<[f64; 2]>> {
    std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .map(positions_of)
        .collect()
}

fn positions_of(line: &LineString<f64>) -> Vec<[f64; 2]> {
    line.coords().map(|coord| [coord.x, coord.y]).collect()
}

fn line_string_from(positions: &[[f64; 2]]) -> LineString<f64> {
    LineString::new(
        positions
            .iter()
            .map(|position| Coord {
                x: position[0],
                y: position[1],
            })
            .collect(),
    )
}

/// Read a geometry as polygons, refusing anything a boolean overlay cannot take.
fn polygons(
    geometry: &Geometry,
    role: &'static str,
    operation: &'static str,
) -> Result<MultiPolygon<f64>, GeoprocessingError> {
    match geo_geometry(geometry, role)? {
        geo::Geometry::Polygon(polygon) => Ok(MultiPolygon::new(vec![polygon])),
        geo::Geometry::MultiPolygon(multi) => Ok(multi),
        _ => Err(GeoprocessingError::NotAPolygon {
            operation,
            given: geometry.type_name(),
        }),
    }
}

/// Convert to geo's types, checking every coordinate is finite and every ring and
/// line has enough positions.
fn geo_geometry(
    geometry: &Geometry,
    role: &'static str,
) -> Result<geo::Geometry<f64>, GeoprocessingError> {
    match geometry {
        Geometry::Point(position) => {
            let coord = coordinate(position, role)?;
            Ok(geo::Geometry::Point(Point::from(coord)))
        }
        Geometry::LineString(positions) => {
            if positions.len() < MIN_LINE_POSITIONS {
                return Err(GeoprocessingError::NotEnoughPositions {
                    role,
                    count: positions.len(),
                    needed: MIN_LINE_POSITIONS,
                });
            }
            Ok(geo::Geometry::LineString(line_string(positions, role)?))
        }
        Geometry::Polygon(rings) => Ok(geo::Geometry::Polygon(polygon(rings, role)?)),
        Geometry::MultiPolygon(polygons) => Ok(geo::Geometry::MultiPolygon(MultiPolygon::new(
            polygons
                .iter()
                .map(|rings| polygon(rings, role))
                .collect::<Result<Vec<_>, _>>()?,
        ))),
    }
}

fn polygon(
    rings: &[Vec<[f64; 2]>],
    role: &'static str,
) -> Result<Polygon<f64>, GeoprocessingError> {
    let mut rings = rings.iter();
    let exterior = rings.next().ok_or(GeoprocessingError::NotEnoughPositions {
        role,
        count: 0,
        needed: MIN_RING_POSITIONS,
    })?;
    Ok(Polygon::new(
        ring(exterior, role)?,
        rings
            .map(|interior| ring(interior, role))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn ring(positions: &[[f64; 2]], role: &'static str) -> Result<LineString<f64>, GeoprocessingError> {
    if positions.len() < MIN_RING_POSITIONS {
        return Err(GeoprocessingError::NotEnoughPositions {
            role,
            count: positions.len(),
            needed: MIN_RING_POSITIONS,
        });
    }
    line_string(positions, role)
}

fn line_string(
    positions: &[[f64; 2]],
    role: &'static str,
) -> Result<LineString<f64>, GeoprocessingError> {
    Ok(LineString::new(
        positions
            .iter()
            .map(|position| coordinate(position, role))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn coordinate(position: &[f64; 2], role: &'static str) -> Result<Coord<f64>, GeoprocessingError> {
    if !position[0].is_finite() || !position[1].is_finite() {
        return Err(GeoprocessingError::NonFiniteCoordinate { role });
    }
    Ok(Coord {
        x: position[0],
        y: position[1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Area, Contains};

    /// A closed square ring, counter-clockwise from its lower-left corner.
    fn square(min_x: f64, min_y: f64, side: f64) -> Geometry {
        Geometry::Polygon(vec![vec![
            [min_x, min_y],
            [min_x + side, min_y],
            [min_x + side, min_y + side],
            [min_x, min_y + side],
            [min_x, min_y],
        ]])
    }

    /// An L covering (0,0)-(3,1) and (0,1)-(1,3): area 5, concave at (1,1).
    fn l_shape() -> Geometry {
        Geometry::Polygon(vec![vec![
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 3.0],
            [0.0, 3.0],
            [0.0, 0.0],
        ]])
    }

    fn only_part(multi: &MultiPolygon<f64>) -> &Polygon<f64> {
        assert_eq!(multi.0.len(), 1, "expected one part, got {}", multi.0.len());
        &multi.0[0]
    }

    #[test]
    fn buffer_replaces_the_square_corners_with_an_arc() {
        // at latitude zero the local frame's degree of longitude and of latitude
        // are both METERS_PER_DEGREE, so a planar area converts back exactly
        const SIDE_M: f64 = 1000.0;
        const DISTANCE_M: f64 = 100.0;
        let side_degrees = SIDE_M / METERS_PER_DEGREE;

        let buffered = buffer(&square(0.0, 0.0, side_degrees), DISTANCE_M).unwrap();
        let part = only_part(&buffered);

        assert!(
            part.exterior().0.len() > 5,
            "a buffered square is not a square, it has rounded corners, got {} positions",
            part.exterior().0.len()
        );

        let area_m2 = buffered.unsigned_area() * METERS_PER_DEGREE * METERS_PER_DEGREE;
        let rounded_square = SIDE_M * SIDE_M
            + 4.0 * SIDE_M * DISTANCE_M
            + std::f64::consts::PI * DISTANCE_M * DISTANCE_M;
        assert!(
            (area_m2 - rounded_square).abs() / rounded_square < 0.01,
            "buffered area {area_m2} is not the rounded-corner area {rounded_square}"
        );
    }

    #[test]
    fn buffer_reaches_the_requested_distance_past_each_edge() {
        const SIDE_M: f64 = 1000.0;
        const DISTANCE_M: f64 = 100.0;
        let side = SIDE_M / METERS_PER_DEGREE;
        let distance = DISTANCE_M / METERS_PER_DEGREE;
        let half = side / 2.0;

        let buffered = buffer(&square(0.0, 0.0, side), DISTANCE_M).unwrap();

        let edge_midpoints_and_outward = [
            ([half, 0.0], [0.0, -1.0]),
            ([side, half], [1.0, 0.0]),
            ([half, side], [0.0, 1.0]),
            ([0.0, half], [-1.0, 0.0]),
        ];
        for (midpoint, outward) in edge_midpoints_and_outward {
            for (fraction, expected_inside) in [(0.9, true), (1.1, false)] {
                let probe = Point::new(
                    midpoint[0] + outward[0] * distance * fraction,
                    midpoint[1] + outward[1] * distance * fraction,
                );
                assert_eq!(
                    buffered.contains(&probe),
                    expected_inside,
                    "{probe:?} at {fraction} of the buffer distance past {midpoint:?}"
                );
            }
        }
    }

    #[test]
    fn buffer_of_a_point_is_a_circle_of_the_requested_radius() {
        const RADIUS_M: f64 = 500.0;
        let buffered = buffer(&Geometry::Point([0.0, 0.0]), RADIUS_M).unwrap();

        let area_m2 = buffered.unsigned_area() * METERS_PER_DEGREE * METERS_PER_DEGREE;
        let circle = std::f64::consts::PI * RADIUS_M * RADIUS_M;
        assert!(
            (area_m2 - circle).abs() / circle < 0.01,
            "buffered point covers {area_m2}, a circle of radius {RADIUS_M} covers {circle}"
        );
    }

    #[test]
    fn buffer_is_refused_at_the_pole() {
        let error = buffer(&square(0.0, 89.5, 0.1), 100.0).unwrap_err();
        assert!(
            matches!(error, GeoprocessingError::BufferNearPole { .. }),
            "{error}"
        );
    }

    #[test]
    fn union_of_overlapping_squares_is_not_their_convex_hull() {
        let a = square(0.0, 0.0, 1.0);
        let b = square(0.5, 0.5, 1.0);

        let union = polygon_union(&a, &b).unwrap();
        let part = only_part(&union);

        // 1 + 1 minus the 0.5 by 0.5 overlap
        assert!(
            (part.unsigned_area() - 1.75).abs() < 1e-9,
            "union covers {}, the two squares less their overlap is 1.75",
            part.unsigned_area()
        );

        // this point sits inside the hull of both squares but outside the L they cover
        let outside_the_union = Point::new(0.25, 1.2);
        let hull = convex_hull(&[
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
            [0.5, 0.5],
            [1.5, 0.5],
            [1.5, 1.5],
            [0.5, 1.5],
        ]);
        assert!(point_in_polygon(&[0.25, 1.2], &hull));
        assert!(!union.contains(&outside_the_union));
    }

    #[test]
    fn union_of_disjoint_squares_answers_two_parts() {
        let union = polygon_union(&square(0.0, 0.0, 1.0), &square(2.0, 0.0, 1.0)).unwrap();

        assert_eq!(union.0.len(), 2);
        assert!((union.unsigned_area() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn difference_cuts_the_overlap_out_of_the_first_square() {
        let difference =
            polygon_difference(&square(0.0, 0.0, 2.0), &square(1.0, 1.0, 2.0)).unwrap();
        let part = only_part(&difference);

        assert!(
            (part.unsigned_area() - 3.0).abs() < 1e-9,
            "difference covers {}, four less the one they share is 3",
            part.unsigned_area()
        );
        assert!(!difference.contains(&Point::new(1.5, 1.5)));
        assert!(difference.contains(&Point::new(0.5, 0.5)));
    }

    #[test]
    fn difference_of_an_enclosed_square_leaves_a_hole() {
        let outer = square(0.0, 0.0, 4.0);
        let inner = square(1.0, 1.0, 2.0);

        let difference = polygon_difference(&outer, &inner).unwrap();
        let part = only_part(&difference);

        assert_eq!(part.interiors().len(), 1, "the inner square cuts a hole");
        assert!((part.unsigned_area() - 12.0).abs() < 1e-9);
        assert!(!difference.contains(&Point::new(2.0, 2.0)));
    }

    #[test]
    fn intersection_of_overlapping_squares_has_the_overlap_area() {
        let intersection =
            polygon_intersection(&square(0.0, 0.0, 2.0), &square(1.0, 1.0, 2.0)).unwrap();

        assert!((only_part(&intersection).unsigned_area() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn intersection_with_a_concave_clip_keeps_the_whole_concave_overlap() {
        // clipping the square against the L's edges as half-planes would answer
        // the unit square, area 1, instead of the L itself
        let intersection = polygon_intersection(&square(0.0, 0.0, 3.0), &l_shape()).unwrap();
        let part = only_part(&intersection);

        assert!(
            (part.unsigned_area() - 5.0).abs() < 1e-9,
            "intersection covers {}, the L covers 5",
            part.unsigned_area()
        );
        assert!(intersection.contains(&Point::new(2.5, 0.5)));
    }

    #[test]
    fn intersection_of_disjoint_squares_answers_no_parts() {
        let intersection =
            polygon_intersection(&square(0.0, 0.0, 1.0), &square(5.0, 5.0, 1.0)).unwrap();

        assert!(intersection.0.is_empty());
    }

    #[test]
    fn centroid_of_an_l_shape_is_area_weighted_not_the_vertex_mean() {
        let centroid = centroid(&l_shape()).unwrap();

        // the 3 by 1 arm weighs its (1.5, 0.5) against the 1 by 2 arm's (0.5, 2)
        assert!((centroid.x() - 1.1).abs() < 1e-9, "{centroid:?}");
        assert!((centroid.y() - 1.1).abs() < 1e-9, "{centroid:?}");
        // the mean of the six corners is (4/3, 4/3)
        assert!((centroid.x() - 4.0 / 3.0).abs() > 0.2);
    }

    #[test]
    fn convex_hull_drops_an_interior_point() {
        let points = vec![[0.0, 0.0], [1.0, 0.0], [0.5, 0.5], [1.0, 1.0], [0.0, 1.0]];

        let hull = convex_hull(&points);

        assert_eq!(hull.len(), 5, "four corners and the closing position");
        assert!(!hull.contains(&[0.5, 0.5]));
    }

    #[test]
    fn simplify_drops_a_nearly_collinear_vertex() {
        let points = vec![[0.0, 0.0], [0.1, 0.001], [0.2, 0.0], [0.3, 0.5], [0.4, 0.0]];

        let simplified = simplify(&points, 0.01);

        assert!(simplified.len() < points.len());
        assert!(!simplified.contains(&[0.1, 0.001]));
    }

    #[test]
    fn simplify_keeps_a_ring_it_would_flatten() {
        let result = run(
            &GeoOperation::Simplify { tolerance: 10.0 },
            &square(0.0, 0.0, 1.0),
            None,
        )
        .unwrap();

        let Geometry::Polygon(rings) = &result.geometry else {
            panic!("expected a Polygon, got {}", result.geometry.type_name());
        };
        assert_eq!(
            rings[0].len(),
            5,
            "a ring never simplifies below a triangle"
        );
    }

    #[test]
    fn point_in_polygon_answers_inside_and_outside() {
        let ring = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];

        assert!(point_in_polygon(&[2.0, 2.0], &ring));
        assert!(!point_in_polygon(&[5.0, 5.0], &ring));
    }

    #[test]
    fn every_advertised_operation_parses() {
        for name in OPERATIONS {
            let operation =
                GeoOperation::parse(name, Some(100.0), Some(0.5)).unwrap_or_else(|error| {
                    panic!("/operations lists '{name}' but parse said {error}")
                });
            assert_eq!(operation.name(), name);
        }
    }

    #[test]
    fn parse_refuses_an_unknown_operation_and_names_the_accepted_set() {
        let error = GeoOperation::parse("Voronoi", None, None).unwrap_err();

        let reason = error.to_string();
        assert!(reason.contains("Voronoi"), "{reason}");
        for name in OPERATIONS {
            assert!(reason.contains(name), "{reason} does not name {name}");
        }
    }

    #[test]
    fn parse_refuses_buffer_without_a_distance_and_simplify_without_a_tolerance() {
        assert!(matches!(
            GeoOperation::parse("Buffer", None, None).unwrap_err(),
            GeoprocessingError::MissingDistance
        ));
        assert!(matches!(
            GeoOperation::parse("Simplify", None, Some(0.0)).unwrap_err(),
            GeoprocessingError::MissingTolerance
        ));
    }

    #[test]
    fn run_refuses_a_non_finite_coordinate() {
        let broken = Geometry::Polygon(vec![vec![
            [0.0, 0.0],
            [f64::NAN, 0.0],
            [1.0, 1.0],
            [0.0, 0.0],
        ]]);

        let error = run(&GeoOperation::Centroid, &broken, None).unwrap_err();

        assert!(
            matches!(error, GeoprocessingError::NonFiniteCoordinate { .. }),
            "{error}"
        );
    }

    #[test]
    fn run_refuses_a_ring_with_three_positions() {
        let sliver = Geometry::Polygon(vec![vec![[0.0, 0.0], [1.0, 0.0], [0.0, 0.0]]]);

        let error = run(&GeoOperation::Union, &sliver, Some(&square(0.0, 0.0, 1.0))).unwrap_err();

        assert!(
            matches!(
                error,
                GeoprocessingError::NotEnoughPositions { count: 3, .. }
            ),
            "{error}"
        );
    }

    #[test]
    fn run_refuses_a_binary_operation_without_a_second_geometry() {
        for operation in [
            GeoOperation::Union,
            GeoOperation::Intersection,
            GeoOperation::Difference,
        ] {
            let error = run(&operation, &square(0.0, 0.0, 1.0), None).unwrap_err();
            assert!(
                matches!(error, GeoprocessingError::MissingSecondGeometry { .. }),
                "{operation:?}: {error}"
            );
        }
    }

    #[test]
    fn run_refuses_an_overlay_on_a_line() {
        let line = Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0]]);

        let error = run(&GeoOperation::Union, &line, Some(&square(0.0, 0.0, 1.0))).unwrap_err();

        assert!(
            matches!(error, GeoprocessingError::NotAPolygon { .. }),
            "{error}"
        );
    }

    #[test]
    fn run_measures_a_polygon_result_and_not_a_point_result() {
        let hull = run(&GeoOperation::ConvexHull, &l_shape(), None).unwrap();
        assert!(hull.area_m2.unwrap() > 0.0);
        assert!(hull.length_m.unwrap() > 0.0);

        let centroid = run(&GeoOperation::Centroid, &l_shape(), None).unwrap();
        assert!(matches!(centroid.geometry, Geometry::Point(_)));
        assert!(centroid.area_m2.is_none());
        assert!(centroid.length_m.is_none());
    }
}
