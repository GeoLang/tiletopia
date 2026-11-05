use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::elevation::JOB_BUDGET_BYTES;

// arcs held as 32-byte positions are up to 5.3 times their text, and the largest feature's stitched copy as much again
const BYTES_HELD_PER_TOPOJSON_BYTE: u64 = 12;

#[derive(Deserialize)]
struct Topology {
    #[serde(rename = "type")]
    kind: String,
    objects: BTreeMap<String, TopologyGeometry>,
    arcs: Vec<Vec<Position>>,
    transform: Option<Transform>,
}

#[derive(Deserialize)]
struct Transform {
    scale: [f64; 2],
    translate: [f64; 2],
}

#[derive(Deserialize)]
struct TopologyGeometry {
    #[serde(rename = "type")]
    kind: String,
    id: Option<Box<RawValue>>,
    properties: Option<Box<RawValue>>,
    coordinates: Option<Box<RawValue>>,
    arcs: Option<Box<RawValue>>,
    geometries: Option<Vec<TopologyGeometry>>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(try_from = "Vec<f64>")]
struct Position {
    x: f64,
    y: f64,
    z: Option<f64>,
}

impl TryFrom<Vec<f64>> for Position {
    type Error = String;

    fn try_from(numbers: Vec<f64>) -> Result<Self, String> {
        match numbers.as_slice() {
            [x, y] => Ok(Self {
                x: *x,
                y: *y,
                z: None,
            }),
            [x, y, z] => Ok(Self {
                x: *x,
                y: *y,
                z: Some(*z),
            }),
            _ => Err(format!(
                "a position has two or three numbers, this one has {}",
                numbers.len()
            )),
        }
    }
}

impl Serialize for Position {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.z {
            Some(z) => [self.x, self.y, z].serialize(serializer),
            None => [self.x, self.y].serialize(serializer),
        }
    }
}

#[derive(Serialize)]
struct Feature<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a RawValue>,
    properties: Option<&'a RawValue>,
    geometry: Geometry,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Geometry {
    Point {
        coordinates: Position,
    },
    MultiPoint {
        coordinates: Vec<Position>,
    },
    LineString {
        coordinates: Vec<Position>,
    },
    MultiLineString {
        coordinates: Vec<Vec<Position>>,
    },
    Polygon {
        coordinates: Vec<Vec<Position>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Position>>>,
    },
    #[serde(rename = "GeometryCollection")]
    Collection {
        geometries: Vec<Geometry>,
    },
}

pub(crate) fn write_geojson(topojson_path: &Path, geojson_path: &Path) -> Result<(), String> {
    let size = std::fs::metadata(topojson_path)
        .map_err(|e| format!("could not read the TopoJSON: {e}"))?
        .len();
    let max_size = JOB_BUDGET_BYTES / BYTES_HELD_PER_TOPOJSON_BYTE;
    if size > max_size {
        return Err(format!(
            "the TopoJSON is {size} bytes, past the {max_size} bytes a job decodes"
        ));
    }
    let text = std::fs::read_to_string(topojson_path)
        .map_err(|e| format!("could not read the TopoJSON: {e}"))?;
    let mut topology: Topology =
        serde_json::from_str(&text).map_err(|e| format!("not a TopoJSON topology: {e}"))?;
    drop(text);
    if topology.kind != "Topology" {
        return Err(format!(
            "TopoJSON type {}: the top object of a TopoJSON is a Topology",
            topology.kind
        ));
    }
    if let Some(transform) = &topology.transform {
        decode_quantized_arcs(&mut topology.arcs, transform);
    }

    let write_error = |e: std::io::Error| format!("could not write the decoded GeoJSON: {e}");
    let mut output = BufWriter::new(std::fs::File::create(geojson_path).map_err(write_error)?);
    output
        .write_all(br#"{"type":"FeatureCollection","features":["#)
        .map_err(write_error)?;
    let mut written_features = 0;
    for object in topology.objects.values() {
        // a GeometryCollection object becomes one feature per member, as topojson-client's feature() does
        let members: Vec<&TopologyGeometry> = match (&object.geometries, object.kind.as_str()) {
            (Some(geometries), "GeometryCollection") => geometries.iter().collect(),
            _ => vec![object],
        };
        for member in members {
            let feature = Feature {
                kind: "Feature",
                id: member.id.as_deref(),
                properties: member.properties.as_deref(),
                geometry: to_geojson(member, &topology)?,
            };
            if written_features > 0 {
                output.write_all(b",").map_err(write_error)?;
            }
            serde_json::to_writer(&mut output, &feature)
                .map_err(|e| format!("could not write the decoded GeoJSON: {e}"))?;
            written_features += 1;
        }
    }
    output.write_all(b"]}").map_err(write_error)?;
    output.flush().map_err(write_error)
}

fn decode_quantized_arcs(arcs: &mut [Vec<Position>], transform: &Transform) {
    // quantized arcs hold each position after the first as a delta from the one before
    for arc in arcs {
        let (mut x, mut y) = (0.0, 0.0);
        for position in arc {
            x += position.x;
            y += position.y;
            position.x = x * transform.scale[0] + transform.translate[0];
            position.y = y * transform.scale[1] + transform.translate[1];
        }
    }
}

fn absolute_point(position: Position, transform: Option<&Transform>) -> Position {
    let Some(transform) = transform else {
        return position;
    };
    Position {
        x: position.x * transform.scale[0] + transform.translate[0],
        y: position.y * transform.scale[1] + transform.translate[1],
        z: position.z,
    }
}

fn stitch(arcs: &[Vec<Position>], indexes: &[i32]) -> Result<Vec<Position>, String> {
    let mut line: Vec<Position> = Vec::new();
    for &index in indexes {
        // a negative index names the reversed arc at its ones' complement
        let arc_number = (if index < 0 { !index } else { index }) as usize;
        let arc = arcs.get(arc_number).ok_or_else(|| {
            format!(
                "arc index {index} names arc {arc_number}, and the topology has {}",
                arcs.len()
            )
        })?;
        // each arc after the first starts on the position the one before ends on
        let repeated = usize::from(!line.is_empty());
        if index < 0 {
            line.extend(arc.iter().rev().skip(repeated).copied());
        } else {
            line.extend(arc.iter().skip(repeated).copied());
        }
    }
    Ok(line)
}

fn parsed_member<T: DeserializeOwned>(
    geometry: &TopologyGeometry,
    value: &Option<Box<RawValue>>,
    name: &str,
) -> Result<T, String> {
    let raw = value
        .as_ref()
        .ok_or_else(|| format!("a {} has no {name}", geometry.kind))?;
    serde_json::from_str(raw.get()).map_err(|e| format!("{} {name}: {e}", geometry.kind))
}

fn to_geojson(geometry: &TopologyGeometry, topology: &Topology) -> Result<Geometry, String> {
    let arcs = &topology.arcs;
    let transform = topology.transform.as_ref();
    let converted = match geometry.kind.as_str() {
        "Point" => Geometry::Point {
            coordinates: absolute_point(
                parsed_member(geometry, &geometry.coordinates, "coordinates")?,
                transform,
            ),
        },
        "MultiPoint" => {
            let points: Vec<Position> =
                parsed_member(geometry, &geometry.coordinates, "coordinates")?;
            Geometry::MultiPoint {
                coordinates: points
                    .into_iter()
                    .map(|point| absolute_point(point, transform))
                    .collect(),
            }
        }
        "LineString" => {
            let indexes: Vec<i32> = parsed_member(geometry, &geometry.arcs, "arcs")?;
            Geometry::LineString {
                coordinates: stitch(arcs, &indexes)?,
            }
        }
        "MultiLineString" => {
            let lines: Vec<Vec<i32>> = parsed_member(geometry, &geometry.arcs, "arcs")?;
            Geometry::MultiLineString {
                coordinates: lines
                    .iter()
                    .map(|line| stitch(arcs, line))
                    .collect::<Result<_, _>>()?,
            }
        }
        "Polygon" => {
            let rings: Vec<Vec<i32>> = parsed_member(geometry, &geometry.arcs, "arcs")?;
            Geometry::Polygon {
                coordinates: rings
                    .iter()
                    .map(|ring| stitch(arcs, ring))
                    .collect::<Result<_, _>>()?,
            }
        }
        "MultiPolygon" => {
            let polygons: Vec<Vec<Vec<i32>>> = parsed_member(geometry, &geometry.arcs, "arcs")?;
            Geometry::MultiPolygon {
                coordinates: polygons
                    .iter()
                    .map(|rings| {
                        rings
                            .iter()
                            .map(|ring| stitch(arcs, ring))
                            .collect::<Result<Vec<_>, String>>()
                    })
                    .collect::<Result<_, _>>()?,
            }
        }
        "GeometryCollection" => {
            let members = geometry
                .geometries
                .as_ref()
                .ok_or("a GeometryCollection has no geometries")?;
            Geometry::Collection {
                geometries: members
                    .iter()
                    .map(|member| to_geojson(member, topology))
                    .collect::<Result<_, _>>()?,
            }
        }
        other => return Err(format!("{other} is not a TopoJSON geometry type")),
    };
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // two one-degree squares sharing an edge, quantized to steps of half a degree east and a quarter degree north
    const SHARED_EDGE_TOPOLOGY: &str = r#"{
      "type": "Topology",
      "transform": {"scale": [0.5, 0.25], "translate": [100, 10]},
      "objects": {
        "fields": {"type": "GeometryCollection", "geometries": [
          {"type": "Polygon", "id": "west", "properties": {"crop": "wheat"}, "arcs": [[0, 1]]},
          {"type": "Polygon", "id": "east", "properties": {"crop": "barley"}, "arcs": [[2, -1]]}
        ]},
        "boundary": {"type": "LineString", "arcs": [-1]},
        "well": {"type": "Point", "coordinates": [4, 4]}
      },
      "arcs": [
        [[2, 0], [0, 4]],
        [[2, 4], [-2, 0], [0, -4], [2, 0]],
        [[2, 0], [2, 0], [0, 4], [-2, 0]]
      ]
    }"#;

    #[test]
    fn shared_arcs_decode_into_the_coordinates_of_each_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let topojson_path = dir.path().join("fields.topojson");
        std::fs::write(&topojson_path, SHARED_EDGE_TOPOLOGY).unwrap();
        let geojson_path = dir.path().join("fields.geojson");

        write_geojson(&topojson_path, &geojson_path).unwrap();

        let collection: Value =
            serde_json::from_str(&std::fs::read_to_string(&geojson_path).unwrap()).unwrap();
        assert_eq!(collection["type"], "FeatureCollection");
        let features = collection["features"].as_array().unwrap();
        let decoded: Vec<(Value, Value, Value)> = features
            .iter()
            .map(|feature| {
                (
                    feature["id"].clone(),
                    feature["geometry"]["type"].clone(),
                    feature["geometry"]["coordinates"].clone(),
                )
            })
            .collect();
        assert_eq!(
            decoded,
            vec![
                (
                    Value::Null,
                    json!("LineString"),
                    json!([[101.0, 11.0], [101.0, 10.0]]),
                ),
                (
                    json!("west"),
                    json!("Polygon"),
                    json!([[
                        [101.0, 10.0],
                        [101.0, 11.0],
                        [100.0, 11.0],
                        [100.0, 10.0],
                        [101.0, 10.0]
                    ]]),
                ),
                (
                    json!("east"),
                    json!("Polygon"),
                    json!([[
                        [101.0, 10.0],
                        [102.0, 10.0],
                        [102.0, 11.0],
                        [101.0, 11.0],
                        [101.0, 10.0]
                    ]]),
                ),
                (Value::Null, json!("Point"), json!([102.0, 11.0])),
            ]
        );
        assert_eq!(features[1]["properties"], json!({"crop": "wheat"}));
    }
}
