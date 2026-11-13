use ifc_lite_core::{AttributeValue, EntityDecoder, EntityScanner, IfcType, ifc_type_from_keyword};

const MAX_STATIONS: usize = 2000;
const MAX_VISITED: usize = 8_000;
const ARC_STEP_RADIANS: f64 = std::f64::consts::PI / 16.0;

#[derive(Debug, Clone, PartialEq)]
pub struct AlignmentStation {
    pub distance: f64,
    pub position: [f64; 3],
    pub tangent: [f64; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlignmentCurve {
    pub id: String,
    pub name: String,
    pub length: f64,
    pub approximate: bool,
    pub stations: Vec<AlignmentStation>,
}

struct Record {
    ifc_type: IfcType,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct FileUnits {
    metres: f64,
    radians: f64,
}

pub fn alignments_of(source: &[u8]) -> Vec<AlignmentCurve> {
    let records = index_records(source);
    let children = child_lists(source, &records);
    let mut decoder = EntityDecoder::new(source);
    let units = FileUnits {
        metres: decoder.length_unit_scale(),
        radians: decoder.plane_angle_to_radians(),
    };
    let mut curves = Vec::new();
    for (id, record) in &records {
        if record.ifc_type != IfcType::IfcAlignment {
            continue;
        }
        let Ok(alignment) = decoder.decode_at(record.start, record.end) else {
            continue;
        };
        let global_id = alignment.get_string(0).unwrap_or("alignment").to_string();
        let name = alignment
            .get_string(2)
            .filter(|name| !name.is_empty())
            .unwrap_or(&global_id)
            .to_string();
        let (stations, approximate) = curve_of(*id, &records, &children, &mut decoder, units);
        let length = stations
            .last()
            .map(|station| station.distance)
            .unwrap_or(0.0);
        curves.push(AlignmentCurve {
            id: global_id,
            name,
            length,
            approximate,
            stations,
        });
    }
    curves
}

fn index_records(source: &[u8]) -> std::collections::BTreeMap<u32, Record> {
    let mut records = std::collections::BTreeMap::new();
    let mut scanner = EntityScanner::new(source);
    while let Some((id, keyword, start, end)) = scanner.next_entity() {
        records.insert(
            id,
            Record {
                ifc_type: ifc_type_from_keyword(keyword),
                start,
                end,
            },
        );
    }
    records
}

fn child_lists(
    source: &[u8],
    records: &std::collections::BTreeMap<u32, Record>,
) -> std::collections::HashMap<u32, Vec<u32>> {
    let mut children = std::collections::HashMap::<u32, Vec<u32>>::new();
    let mut decoder = EntityDecoder::new(source);
    for record in records.values() {
        if record.ifc_type != IfcType::IfcRelNests && record.ifc_type != IfcType::IfcRelAggregates {
            continue;
        }
        let Ok(relation) = decoder.decode_at(record.start, record.end) else {
            continue;
        };
        let Some(parent) = relation.get_ref(4) else {
            continue;
        };
        let Some(related) = relation.get_refs(5) else {
            continue;
        };
        children.entry(parent).or_default().extend(related);
    }
    children
}

fn curve_of(
    alignment: u32,
    records: &std::collections::BTreeMap<u32, Record>,
    children: &std::collections::HashMap<u32, Vec<u32>>,
    decoder: &mut EntityDecoder<'_>,
    units: FileUnits,
) -> (Vec<AlignmentStation>, bool) {
    let mut segments = Vec::new();
    walk_children(
        alignment,
        records,
        children,
        decoder,
        &mut segments,
        &mut std::collections::HashSet::new(),
    );
    if !segments.is_empty() {
        return sample_segments(&segments, records, decoder, units);
    }
    let mut polylines = Vec::new();
    walk_refs(
        alignment,
        records,
        decoder,
        &mut polylines,
        &mut std::collections::HashSet::new(),
    );
    if let Some(polyline) = polylines.first() {
        return (sample_polyline(*polyline, records, decoder, units), false);
    }
    (Vec::new(), false)
}

fn walk_children(
    id: u32,
    records: &std::collections::BTreeMap<u32, Record>,
    children: &std::collections::HashMap<u32, Vec<u32>>,
    decoder: &mut EntityDecoder<'_>,
    segments: &mut Vec<u32>,
    seen: &mut std::collections::HashSet<u32>,
) {
    if !seen.insert(id) || seen.len() > MAX_VISITED {
        return;
    }
    if let Some(record) = records.get(&id) {
        match record.ifc_type {
            IfcType::IfcAlignmentHorizontalSegment => segments.push(id),
            IfcType::IfcAlignmentSegment => {
                if let Ok(segment) = decoder.decode_at(record.start, record.end)
                    && let Some(parameters) = segment.get_ref(7)
                {
                    walk_children(parameters, records, children, decoder, segments, seen);
                }
            }
            _ => {}
        }
    }
    if let Some(nested) = children.get(&id) {
        for child in nested {
            walk_children(*child, records, children, decoder, segments, seen);
        }
    }
}

fn walk_refs(
    id: u32,
    records: &std::collections::BTreeMap<u32, Record>,
    decoder: &mut EntityDecoder<'_>,
    polylines: &mut Vec<u32>,
    seen: &mut std::collections::HashSet<u32>,
) {
    if !seen.insert(id) || seen.len() > MAX_VISITED {
        return;
    }
    let Some(record) = records.get(&id) else {
        return;
    };
    if record.ifc_type == IfcType::IfcPolyline {
        polylines.push(id);
    }
    let Ok(entity) = decoder.decode_at(record.start, record.end) else {
        return;
    };
    let mut refs = Vec::new();
    for attribute in entity.attributes.iter() {
        collect_refs(attribute, &mut refs);
    }
    for reference in refs {
        walk_refs(reference, records, decoder, polylines, seen);
    }
}

fn collect_refs(value: &AttributeValue, out: &mut Vec<u32>) {
    match value {
        AttributeValue::EntityRef(id) => out.push(*id),
        AttributeValue::List(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        AttributeValue::Typed { value, .. } => collect_refs(value, out),
        _ => {}
    }
}

fn sample_segments(
    segments: &[u32],
    records: &std::collections::BTreeMap<u32, Record>,
    decoder: &mut EntityDecoder<'_>,
    units: FileUnits,
) -> (Vec<AlignmentStation>, bool) {
    let mut runs = Vec::new();
    let mut approximate = false;
    for id in segments {
        let Some(record) = records.get(id) else {
            continue;
        };
        let Ok(segment) = decoder.decode_at(record.start, record.end) else {
            continue;
        };
        let Some(start_id) = segment.get_ref(2) else {
            continue;
        };
        let Some(start) = point_of(decoder, start_id, units) else {
            continue;
        };
        let direction = segment.get_float(3).unwrap_or(0.0) * units.radians;
        let length = segment.get_float(6).unwrap_or(0.0) * units.metres;
        if !length.is_finite() || length <= 0.0 || !direction.is_finite() {
            continue;
        }
        let kind = segment
            .get(8)
            .and_then(AttributeValue::as_enum)
            .unwrap_or("LINE");
        let radius = segment
            .get_float(4)
            .filter(|radius| radius.is_finite() && *radius != 0.0)
            .map(|radius| radius * units.metres);
        let mut run = Vec::new();
        match (kind, radius) {
            ("LINE", _) => push_line(&mut run, start, direction, length),
            ("CIRCULARARC", Some(radius)) => {
                push_arc(&mut run, start, direction, length, radius);
            }
            _ => {
                approximate = true;
                push_line(&mut run, start, direction, length);
            }
        }
        runs.push(run);
    }
    (stations_from_runs(&runs), approximate)
}

fn sample_polyline(
    id: u32,
    records: &std::collections::BTreeMap<u32, Record>,
    decoder: &mut EntityDecoder<'_>,
    units: FileUnits,
) -> Vec<AlignmentStation> {
    let Some(record) = records.get(&id) else {
        return Vec::new();
    };
    let Ok(polyline) = decoder.decode_at(record.start, record.end) else {
        return Vec::new();
    };
    let Some(points) = polyline.get_refs(0) else {
        return Vec::new();
    };
    let positions: Vec<[f64; 3]> = points
        .iter()
        .filter_map(|point| point_of(decoder, *point, units))
        .collect();
    stations_from_runs(&[positions])
}

fn point_of(decoder: &mut EntityDecoder<'_>, id: u32, units: FileUnits) -> Option<[f64; 3]> {
    let (x, y, z) = decoder.get_cartesian_point_fast(id)?;
    (x.is_finite() && y.is_finite() && z.is_finite()).then_some([
        x * units.metres,
        y * units.metres,
        z * units.metres,
    ])
}

fn push_line(points: &mut Vec<[f64; 3]>, start: [f64; 3], direction: f64, length: f64) {
    let (sin, cos) = direction.sin_cos();
    points.push(start);
    points.push([start[0] + length * cos, start[1] + length * sin, start[2]]);
}

fn push_arc(points: &mut Vec<[f64; 3]>, start: [f64; 3], direction: f64, length: f64, radius: f64) {
    let turn = length / radius;
    let steps = ((turn.abs() / ARC_STEP_RADIANS).ceil() as usize).clamp(1, 64);
    let (sin, cos) = direction.sin_cos();
    let left = [-sin, cos];
    let center = [start[0] + left[0] * radius, start[1] + left[1] * radius];
    let from_center = [start[0] - center[0], start[1] - center[1]];
    points.push(start);
    for step in 1..=steps {
        let angle = turn * (step as f64) / (steps as f64);
        let (sin_a, cos_a) = angle.sin_cos();
        let rotated = [
            from_center[0] * cos_a - from_center[1] * sin_a,
            from_center[0] * sin_a + from_center[1] * cos_a,
        ];
        points.push([center[0] + rotated[0], center[1] + rotated[1], start[2]]);
    }
}

fn stations_from_runs(runs: &[Vec<[f64; 3]>]) -> Vec<AlignmentStation> {
    let mut paths: Vec<Vec<[f64; 3]>> = Vec::new();
    for run in runs {
        let continues = paths
            .last()
            .and_then(|path| path.last())
            .zip(run.first())
            .is_some_and(|(end, start)| unit(sub(*start, *end)).is_none());
        match paths.last_mut() {
            Some(path) if continues => path.extend(run),
            _ => paths.push(run.clone()),
        }
    }
    let mut raw = Vec::new();
    for path in &paths {
        let start_distance = raw
            .last()
            .map_or(0.0, |station: &AlignmentStation| station.distance);
        raw.extend(path_stations(path, start_distance));
    }
    if raw.len() <= MAX_STATIONS {
        return raw;
    }
    let last = raw.len() - 1;
    let mut kept = Vec::with_capacity(MAX_STATIONS);
    for slot in 0..MAX_STATIONS {
        let index = if slot + 1 == MAX_STATIONS {
            last
        } else {
            slot * last / (MAX_STATIONS - 1)
        };
        if kept
            .last()
            .is_none_or(|station: &AlignmentStation| station.distance != raw[index].distance)
        {
            kept.push(raw[index].clone());
        }
    }
    kept
}

fn path_stations(path: &[[f64; 3]], start_distance: f64) -> Vec<AlignmentStation> {
    let Some((first, rest)) = path.split_first() else {
        return Vec::new();
    };
    let mut stations = Vec::new();
    let mut at = *first;
    let mut distance = start_distance;
    let mut heading = None;
    for point in rest {
        let step = sub(*point, at);
        let Some(direction) = unit(step) else {
            continue;
        };
        stations.push(AlignmentStation {
            distance,
            position: at,
            tangent: direction,
        });
        distance += length(step);
        at = *point;
        heading = Some(direction);
    }
    if let Some(tangent) = heading {
        stations.push(AlignmentStation {
            distance,
            position: at,
            tangent,
        });
    }
    stations
}

fn sub(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| left[axis] - right[axis])
}

fn length(value: [f64; 3]) -> f64 {
    value.iter().map(|axis| axis * axis).sum::<f64>().sqrt()
}

fn unit(value: [f64; 3]) -> Option<[f64; 3]> {
    let span = length(value);
    (span > 1.0e-9).then(|| std::array::from_fn(|axis| value[axis] / span))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\nFILE_NAME('','',(''),(''),'','','');\nFILE_SCHEMA(('IFC4X3'));\nENDSEC;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    #[test]
    fn a_polyline_alignment_keeps_its_stations_and_length() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Road',$,$,$,#7);\n\
             #2=IFCPOLYLINE((#3,#4,#5));\n\
             #3=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #4=IFCCARTESIANPOINT((10.,0.,0.));\n\
             #5=IFCCARTESIANPOINT((10.,10.,0.));\n\
             #6=IFCSHAPEREPRESENTATION($,'Axis','Curve',(#2));\n\
             #7=IFCPRODUCTDEFINITIONSHAPE($,$,(#6));\n",
        );
        let curves = alignments_of(source.as_bytes());
        assert_eq!(curves.len(), 1);
        assert_eq!(curves[0].id, "aligngid");
        assert_eq!(curves[0].name, "Road");
        assert!(!curves[0].approximate);
        assert!((curves[0].length - 20.0).abs() < 1.0e-6);
        assert_eq!(curves[0].stations.len(), 3);
        assert_eq!(curves[0].stations[2].position, [10.0, 10.0, 0.0]);
        assert!(curves[0].stations[1].tangent[0] < 1.0e-6);
        assert!((curves[0].stations[1].tangent[1] - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn a_line_and_a_quarter_circle_meet_and_a_clothoid_is_straight() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Arc road',$,$,$,$);\n\
             #10=IFCALIGNMENTSEGMENT('seg',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,0.,10.,10.,15.708,$,.CIRCULARARC.);\n\
             #12=IFCCARTESIANPOINT((0.,0.));\n\
             #13=IFCALIGNMENTSEGMENT('cloth',$,$,$,$,$,$,#14);\n\
             #14=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#15,0.,$,$,5.,$,.CLOTHOID.);\n\
             #15=IFCCARTESIANPOINT((0.,0.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10,#13));\n",
        );
        let curves = alignments_of(source.as_bytes());
        let arc_end = curves[0]
            .stations
            .iter()
            .find(|station| (station.distance - 15.708).abs() < 0.05)
            .expect("quarter circle");
        assert!(
            (arc_end.position[0] - 10.0).abs() < 0.05,
            "{:?}",
            arc_end.position
        );
        assert!(
            (arc_end.position[1] - 10.0).abs() < 0.05,
            "{:?}",
            arc_end.position
        );
        assert!(curves[0].approximate);
        let end = curves[0].stations.last().unwrap();
        assert!((end.position[0] - 5.0).abs() < 1.0e-6);
        assert!(end.position[1].abs() < 1.0e-6);
    }

    #[test]
    fn a_long_polyline_is_capped_and_keeps_its_ends() {
        let mut points = String::new();
        let mut refs = String::new();
        for index in 0..3_000 {
            let id = 10 + index;
            points.push_str(&format!("#{id}=IFCCARTESIANPOINT(({index}.,0.,0.));\n"));
            if index > 0 {
                refs.push(',');
            }
            refs.push_str(&format!("#{id}"));
        }
        let source = file(&format!(
            "#1=IFCALIGNMENT('long',$,'Long',$,$,$,#2);\n#2=IFCPOLYLINE(({refs}));\n{points}"
        ));
        let curves = alignments_of(source.as_bytes());
        assert!(curves[0].stations.len() <= MAX_STATIONS);
        assert_eq!(curves[0].stations[0].position[0], 0.0);
        assert!((curves[0].stations.last().unwrap().position[0] - 2999.0).abs() < 1.0e-6);
        assert!((curves[0].length - 2999.0).abs() < 1.0e-6);
    }

    #[test]
    fn an_empty_file_and_an_alignment_with_no_curve_have_no_stations() {
        assert!(alignments_of(b"").is_empty());
        let source = file("#1=IFCALIGNMENT('bare',$,'Bare',$,$,$,$);\n");
        let curves = alignments_of(source.as_bytes());
        assert_eq!(curves.len(), 1);
        assert_eq!(curves[0].length, 0.0);
        assert!(curves[0].stations.is_empty());
        assert!(!curves[0].approximate);
    }

    #[test]
    fn nest_order_is_kept_and_a_negative_radius_curves_the_other_way() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Bend',$,$,$,$);\n\
             #10=IFCALIGNMENTSEGMENT('first',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,0.,$,$,5.,$,.LINE.);\n\
             #12=IFCCARTESIANPOINT((0.,0.));\n\
             #13=IFCALIGNMENTSEGMENT('second',$,$,$,$,$,$,#14);\n\
             #14=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#15,0.,-10.,-10.,15.708,$,.CIRCULARARC.);\n\
             #15=IFCCARTESIANPOINT((100.,0.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10,#13));\n",
        );
        let curves = alignments_of(source.as_bytes());
        assert!(
            (curves[0].stations[0].position[0]).abs() < 1.0e-6,
            "{:?}",
            curves[0].stations[0]
        );
        let end = curves[0].stations.last().unwrap();
        assert!((end.position[0] - 110.0).abs() < 0.05, "{:?}", end.position);
        assert!((end.position[1] + 10.0).abs() < 0.05, "{:?}", end.position);
        assert!(!curves[0].approximate);
    }

    #[test]
    fn segments_that_meet_share_one_station_heading_along_the_road() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'North',$,$,$,$);\n\
             #10=IFCALIGNMENTSEGMENT('first',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,1.5707963267948966,$,$,10.,$,.LINE.);\n\
             #12=IFCCARTESIANPOINT((0.,0.));\n\
             #13=IFCALIGNMENTSEGMENT('second',$,$,$,$,$,$,#14);\n\
             #14=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#15,1.5707963267948966,$,$,10.,$,.LINE.);\n\
             #15=IFCCARTESIANPOINT((0.,10.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10,#13));\n",
        );
        let stations = &alignments_of(source.as_bytes())[0].stations;
        let distances: Vec<f64> = stations.iter().map(|station| station.distance).collect();
        assert_eq!(distances.len(), 3, "{stations:?}");
        assert!((distances[1] - 10.0).abs() < 1.0e-6, "{stations:?}");
        assert!((distances[2] - 20.0).abs() < 1.0e-6, "{stations:?}");
        for station in stations {
            assert!(
                station.tangent[0].abs() < 1.0e-6 && (station.tangent[1] - 1.0).abs() < 1.0e-6,
                "{station:?}"
            );
        }
    }

    #[test]
    fn a_gap_between_segments_adds_no_distance() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Gap',$,$,$,$);\n\
             #10=IFCALIGNMENTSEGMENT('first',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,0.,$,$,5.,$,.LINE.);\n\
             #12=IFCCARTESIANPOINT((0.,0.));\n\
             #13=IFCALIGNMENTSEGMENT('second',$,$,$,$,$,$,#14);\n\
             #14=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#15,0.,$,$,5.,$,.LINE.);\n\
             #15=IFCCARTESIANPOINT((100.,0.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10,#13));\n",
        );
        let curve = &alignments_of(source.as_bytes())[0];
        assert!((curve.length - 10.0).abs() < 1.0e-6, "{curve:?}");
        assert_eq!(curve.stations.last().unwrap().position, [105.0, 0.0, 0.0]);
    }

    #[test]
    fn millimetres_and_degrees_become_metres_and_radians() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Units',$,$,$,$);\n\
             #2=IFCPROJECT('project',$,'Project',$,$,$,$,$,#3);\n\
             #3=IFCUNITASSIGNMENT((#4,#5));\n\
             #4=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);\n\
             #5=IFCCONVERSIONBASEDUNIT(#6,.PLANEANGLEUNIT.,'DEGREE',#7);\n\
             #6=IFCDIMENSIONALEXPONENTS(0,0,0,0,0,0,0);\n\
             #7=IFCMEASUREWITHUNIT(IFCPLANEANGLEMEASURE(0.0174532925199433),#8);\n\
             #8=IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.);\n\
             #10=IFCALIGNMENTSEGMENT('seg',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,90.,$,$,10000.,$,.LINE.);\n\
             #12=IFCCARTESIANPOINT((2000.,0.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10));\n",
        );
        let curve = &alignments_of(source.as_bytes())[0];
        assert!((curve.length - 10.0).abs() < 1.0e-6, "{curve:?}");
        let end = curve.stations.last().unwrap().position;
        assert!(
            (end[0] - 2.0).abs() < 1.0e-6 && (end[1] - 10.0).abs() < 1.0e-6,
            "{end:?}"
        );
    }

    #[test]
    fn a_zero_length_segment_adds_no_station() {
        let source = file(
            "#1=IFCALIGNMENT('aligngid',$,'Flat',$,$,$,$);\n\
             #10=IFCALIGNMENTSEGMENT('seg',$,$,$,$,$,$,#11);\n\
             #11=IFCALIGNMENTHORIZONTALSEGMENT('s','e',#12,0.,$,$,0.,$,.LINE.);\n\
             #12=IFCCARTESIANPOINT((3.,4.));\n\
             #20=IFCRELNESTS('nest',$,$,$,#1,(#10));\n",
        );
        let curves = alignments_of(source.as_bytes());
        assert!(curves[0].stations.is_empty());
        assert!(!curves[0].approximate);
    }
}
