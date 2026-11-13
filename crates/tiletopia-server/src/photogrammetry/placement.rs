use tiletopia_core::crs::{Coord3D, CrsDef, Transformer};
use tiletopia_core::spatial::enu_to_ecef_matrix;
use tiletopia_ingest::MeshData;
use tiletopia_ingest::crs_detect::{parse_epsg_code, reproject_to_ecef};

const WGS84_GEODETIC_EPSG: u32 = 4326;
const UTM_NORTH_EPSG_BASE: u32 = 32600;
const UTM_SOUTH_EPSG_BASE: u32 = 32700;
const UTM_ZONES: std::ops::RangeInclusive<u32> = 1..=60;

#[derive(Debug, PartialEq)]
pub(crate) struct Georeference {
    pub(crate) epsg: u32,
    pub(crate) east_offset: f64,
    pub(crate) north_offset: f64,
}

pub(super) fn parse_coords(text: &str) -> Result<Georeference, String> {
    let mut lines = text.lines();
    let crs = lines
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .ok_or("coords.txt names no CRS")?;
    let epsg = crs_epsg(crs)?;
    let offset = lines.next().ok_or("coords.txt has no offset line")?;
    let numbers: Vec<f64> = offset
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| format!("coords.txt offset {offset:?} is not two numbers"))?;
    let [east_offset, north_offset] = numbers[..] else {
        return Err(format!("coords.txt offset {offset:?} is not two numbers"));
    };
    Ok(Georeference {
        epsg,
        east_offset,
        north_offset,
    })
}

fn crs_epsg(crs: &str) -> Result<u32, String> {
    // odm writes "WGS84 UTM <zone><N|S>" for a GPS placed model, and takes EPSG:<code> from a GCP file
    let refused =
        || format!("coords.txt CRS {crs:?}: expected WGS84 UTM <zone><N or S>, or EPSG:<code>");
    if crs
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("EPSG:"))
    {
        return parse_epsg_code(crs).ok_or_else(refused);
    }
    let words: Vec<&str> = crs.split_whitespace().collect();
    let ["WGS84", "UTM", zone] = words[..] else {
        return Err(refused());
    };
    let (zone_number, epsg_base) = if let Some(number) = zone.strip_suffix(['N', 'n']) {
        (number, UTM_NORTH_EPSG_BASE)
    } else if let Some(number) = zone.strip_suffix(['S', 's']) {
        (number, UTM_SOUTH_EPSG_BASE)
    } else {
        return Err(refused());
    };
    let zone_number: u32 = zone_number
        .parse()
        .ok()
        .filter(|number| UTM_ZONES.contains(number))
        .ok_or_else(refused)?;
    Ok(epsg_base + zone_number)
}

fn local_center(meshes: &[MeshData]) -> [f64; 3] {
    let mut low = [f64::INFINITY; 3];
    let mut high = [f64::NEG_INFINITY; 3];
    for position in meshes.iter().flat_map(|mesh| &mesh.positions) {
        for axis in 0..3 {
            low[axis] = low[axis].min(position[axis]);
            high[axis] = high[axis].max(position[axis]);
        }
    }
    [0, 1, 2].map(|axis| (low[axis] + high[axis]) / 2.0)
}

fn to_east_north_up(frame: &[f64; 16], ecef: [f64; 3]) -> [f64; 3] {
    let offset = [
        ecef[0] - frame[12],
        ecef[1] - frame[13],
        ecef[2] - frame[14],
    ];
    // enu_to_ecef_matrix is column major: the east, north and up axes, then the origin
    let along = |column: usize| {
        frame[column] * offset[0] + frame[column + 1] * offset[1] + frame[column + 2] * offset[2]
    };
    [along(0), along(4), along(8)]
}

fn difference(to: [f64; 3], from: [f64; 3]) -> [f64; 3] {
    [to[0] - from[0], to[1] - from[1], to[2] - from[2]]
}

pub(crate) fn place_in_east_north_up(
    meshes: &mut [MeshData],
    georeference: &Georeference,
) -> Result<[f64; 16], String> {
    let epsg = georeference.epsg;
    let absolute = |local: [f64; 3]| {
        [
            local[0] + georeference.east_offset,
            local[1] + georeference.north_offset,
            local[2],
        ]
    };
    let origin = absolute(local_center(meshes));
    let origin_geodetic = Transformer::new(CrsDef::Epsg(epsg), CrsDef::Epsg(WGS84_GEODETIC_EPSG))
        .transform(Coord3D {
            x: origin[0],
            y: origin[1],
            z: origin[2],
        })
        .map_err(|e| format!("EPSG:{epsg} to WGS84: {e}"))?;
    let frame = enu_to_ecef_matrix(
        origin_geodetic.y.to_radians(),
        origin_geodetic.x.to_radians(),
        origin_geodetic.z,
    );

    let mut points: Vec<[f64; 3]> = meshes
        .iter()
        .flat_map(|mesh| &mesh.positions)
        .map(|position| absolute(*position))
        .collect();
    let vertex_count = points.len();
    // normals turn by the same rotation as one metre east and one metre north on the grid
    points.extend([
        origin,
        [origin[0] + 1.0, origin[1], origin[2]],
        [origin[0], origin[1] + 1.0, origin[2]],
    ]);
    reproject_to_ecef(&mut points, epsg).map_err(|e| format!("EPSG:{epsg} to ECEF: {e}"))?;
    let placed: Vec<[f64; 3]> = points
        .into_iter()
        .map(|ecef| to_east_north_up(&frame, ecef))
        .collect();
    let grid_east = difference(placed[vertex_count + 1], placed[vertex_count]);
    let grid_north = difference(placed[vertex_count + 2], placed[vertex_count]);

    let mut next_vertex = 0;
    for mesh in meshes.iter_mut() {
        for position in &mut mesh.positions {
            *position = placed[next_vertex];
            next_vertex += 1;
        }
        for normal in &mut mesh.normals {
            let [x, y, z] = normal.map(f64::from);
            let turned = [
                x * grid_east[0] + y * grid_north[0],
                x * grid_east[1] + y * grid_north[1],
                x * grid_east[2] + y * grid_north[2] + z,
            ];
            let length =
                (turned[0] * turned[0] + turned[1] * turned[1] + turned[2] * turned[2]).sqrt();
            if length > 0.0 {
                *normal = turned.map(|value| (value / length) as f32);
            }
        }
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiletopia_core::spatial::WGS84_A;

    #[test]
    fn coords_name_a_utm_zone_and_an_offset() {
        assert_eq!(
            parse_coords("WGS84 UTM 32N\n500123.5 5316456\n").unwrap(),
            Georeference {
                epsg: 32632,
                east_offset: 500_123.5,
                north_offset: 5_316_456.0,
            }
        );
        assert_eq!(parse_coords("WGS84 UTM 18S\n0 0").unwrap().epsg, 32718);
        assert_eq!(parse_coords("EPSG:32633\n1 2").unwrap().epsg, 32633);
    }

    #[test]
    fn coords_odm_could_write_that_this_cannot_place_are_refused() {
        for (text, named) in [
            ("+proj=utm +zone=32 +datum=WGS84 +units=m\n0 0", "+proj"),
            ("WGS84 UTM 61N\n0 0", "61N"),
            ("WGS84 UTM 32X\n0 0", "32X"),
            ("WGS84 UTM 32N\n", "no offset"),
            ("WGS84 UTM 32N\n500000", "500000"),
            ("", "no CRS"),
        ] {
            let error = parse_coords(text).unwrap_err();
            assert!(error.contains(named), "{text:?}: {error}");
        }
    }

    // utm 32N's central meridian is 9 degrees east, where grid distances are 0.9996 of true ones
    const CENTRAL_MERIDIAN_DEGREES: f64 = 9.0;
    const CENTRAL_SCALE_FACTOR: f64 = 0.9996;
    const HEIGHT: f64 = 15.0;

    fn diamond() -> MeshData {
        MeshData {
            positions: vec![
                [-10.0, 5.0, HEIGHT],
                [10.0, 5.0, HEIGHT],
                [0.0, -5.0, HEIGHT],
                [0.0, 15.0, HEIGHT],
            ],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            texcoords: Vec::new(),
            indices: vec![0, 1, 3, 0, 2, 1],
            name: "diamond".to_string(),
            material: None,
            asset_id: None,
            class: None,
            storey: None,
            element_color: None,
        }
    }

    #[test]
    fn a_model_on_the_central_meridian_is_placed_by_its_offset_and_the_grid_scale() {
        let georeference = Georeference {
            epsg: 32632,
            east_offset: 500_000.0,
            north_offset: 5.0,
        };
        let mut meshes = vec![diamond()];

        let frame = place_in_east_north_up(&mut meshes, &georeference).unwrap();

        // the centre is 10 grid metres north of the equator on the central meridian
        let longitude = CENTRAL_MERIDIAN_DEGREES.to_radians();
        let radius = WGS84_A + HEIGHT;
        let expected = [
            radius * longitude.cos(),
            radius * longitude.sin(),
            10.0 / CENTRAL_SCALE_FACTOR,
        ];
        for axis in 0..3 {
            assert!(
                (frame[12 + axis] - expected[axis]).abs() < 0.01,
                "origin {:?}, expected {expected:?}",
                &frame[12..15]
            );
        }

        let true_ten_metres = 10.0 / CENTRAL_SCALE_FACTOR;
        let expected_vertices = [
            [-true_ten_metres, 0.0, 0.0],
            [true_ten_metres, 0.0, 0.0],
            [0.0, -true_ten_metres, 0.0],
            [0.0, true_ten_metres, 0.0],
        ];
        for (placed, expected) in meshes[0].positions.iter().zip(expected_vertices) {
            for axis in 0..3 {
                assert!(
                    (placed[axis] - expected[axis]).abs() < 0.001,
                    "{placed:?}, expected {expected:?}"
                );
            }
        }
        for normal in &meshes[0].normals {
            assert!((normal[2] - 1.0).abs() < 1e-6, "{normal:?}");
        }
    }
}
