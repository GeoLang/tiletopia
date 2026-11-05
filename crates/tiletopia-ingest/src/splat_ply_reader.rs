use crate::IngestError;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::path::Path;
use tiletopia_core::splat_tiler::{GaussianSplat, HIGHER_DEGREE_COEFFICIENT_COUNTS, SplatCloud};

const POSITION_PROPERTIES: [&str; 3] = ["x", "y", "z"];
// 3dgs training writes these vertex properties besides position
const SPLAT_PROPERTIES: [&str; 11] = [
    "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1", "scale_2", "rot_0", "rot_1",
    "rot_2", "rot_3",
];
const REQUIRED_PROPERTY_COUNT: usize = POSITION_PROPERTIES.len() + SPLAT_PROPERTIES.len();
const HIGHER_DEGREE_PREFIX: &str = "f_rest_";
const LONGEST_HEADER_LINE: u64 = 1024;

struct PlyProperty {
    name: String,
    type_name: String,
    is_list: bool,
}

struct PlyHeader {
    format: String,
    vertex_count: usize,
    vertex_is_first_element: bool,
    vertex_properties: Vec<PlyProperty>,
}

fn parse_error(path: &Path, message: impl std::fmt::Display) -> IngestError {
    IngestError::ParseError(format!("{}: {message}", path.display()))
}

fn read_header(path: &Path, reader: &mut BufReader<File>) -> Result<PlyHeader, IngestError> {
    let mut header = PlyHeader {
        format: String::new(),
        vertex_count: 0,
        vertex_is_first_element: false,
        vertex_properties: Vec::new(),
    };
    let mut element_count = 0;
    let mut in_vertex_element = false;
    let mut line = String::new();
    for line_number in 0.. {
        line.clear();
        let line_bytes = reader
            .by_ref()
            .take(LONGEST_HEADER_LINE)
            .read_line(&mut line)?;
        if line_bytes == 0 {
            return Err(parse_error(path, "the file ends before end_header"));
        }
        if !line.ends_with('\n') {
            return Err(parse_error(
                path,
                format!("a header line is longer than {LONGEST_HEADER_LINE} bytes"),
            ));
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if line_number == 0 {
            if words != ["ply"] {
                return Err(parse_error(path, "not a PLY file"));
            }
            continue;
        }
        match words.as_slice() {
            ["end_header"] => break,
            ["format", format, _version] => header.format = format.to_string(),
            ["element", name, count] => {
                in_vertex_element = *name == "vertex";
                if in_vertex_element {
                    header.vertex_count = count
                        .parse()
                        .map_err(|_| parse_error(path, format!("vertex count {count}")))?;
                    header.vertex_is_first_element = element_count == 0;
                }
                element_count += 1;
            }
            ["property", "list", _, _, name] if in_vertex_element => {
                header.vertex_properties.push(PlyProperty {
                    name: name.to_string(),
                    type_name: String::new(),
                    is_list: true,
                })
            }
            ["property", type_name, name] if in_vertex_element => {
                header.vertex_properties.push(PlyProperty {
                    name: name.to_string(),
                    type_name: type_name.to_string(),
                    is_list: false,
                })
            }
            _ => {}
        }
    }
    Ok(header)
}

fn open_header(path: &Path) -> Result<(PlyHeader, BufReader<File>), IngestError> {
    let mut reader = BufReader::new(File::open(path)?);
    let header = read_header(path, &mut reader)?;
    Ok((header, reader))
}

pub fn is_splat_ply(path: &Path) -> Result<bool, IngestError> {
    let (header, _) = open_header(path)?;
    Ok(header.vertex_properties.iter().any(|property| {
        SPLAT_PROPERTIES.contains(&property.name.as_str())
            || property.name.starts_with(HIGHER_DEGREE_PREFIX)
    }))
}

fn scalar_bytes(type_name: &str) -> Option<usize> {
    match type_name {
        "char" | "int8" | "uchar" | "uint8" => Some(1),
        "short" | "int16" | "ushort" | "uint16" => Some(2),
        "int" | "int32" | "uint" | "uint32" | "float" | "float32" => Some(4),
        "double" | "float64" => Some(8),
        _ => None,
    }
}

fn float_offset(
    path: &Path,
    header: &PlyHeader,
    offsets: &[usize],
    name: &str,
) -> Result<Option<usize>, IngestError> {
    let Some(position) = header.vertex_properties.iter().position(|p| p.name == name) else {
        return Ok(None);
    };
    let type_name = &header.vertex_properties[position].type_name;
    if !matches!(type_name.as_str(), "float" | "float32") {
        return Err(parse_error(
            path,
            format!("{name} is {type_name}, where a splat PLY property is float"),
        ));
    }
    Ok(Some(offsets[position]))
}

fn float_at(record: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(record[offset..offset + 4].try_into().expect("four bytes"))
}

fn not_finite(path: &Path, splat_index: usize, name: &str, value: f32) -> IngestError {
    parse_error(path, format!("splat {splat_index}: {name} is {value}"))
}

pub fn read(path: &Path, max_bytes_held: u64) -> Result<SplatCloud, IngestError> {
    let (header, mut reader) = open_header(path)?;
    if header.format != "binary_little_endian" {
        return Err(parse_error(
            path,
            format!(
                "a splat PLY must be binary_little_endian, this one is {}",
                header.format
            ),
        ));
    }
    let mut offsets = Vec::with_capacity(header.vertex_properties.len());
    let mut stride = 0;
    for property in &header.vertex_properties {
        if property.is_list {
            return Err(parse_error(
                path,
                format!(
                    "{} is a list, which a splat PLY vertex does not hold",
                    property.name
                ),
            ));
        }
        let bytes = scalar_bytes(&property.type_name).ok_or_else(|| {
            parse_error(
                path,
                format!(
                    "{} has the unknown type {}",
                    property.name, property.type_name
                ),
            )
        })?;
        offsets.push(stride);
        stride += bytes;
    }

    let required_names: Vec<&str> = POSITION_PROPERTIES
        .into_iter()
        .chain(SPLAT_PROPERTIES)
        .collect();
    let mut required_offsets = Vec::with_capacity(required_names.len());
    let mut missing = Vec::new();
    for name in &required_names {
        match float_offset(path, &header, &offsets, name)? {
            Some(offset) => required_offsets.push(offset),
            None => missing.push(*name),
        }
    }
    if !missing.is_empty() {
        return Err(parse_error(
            path,
            format!("the splat PLY has no {} property", missing.join(", ")),
        ));
    }
    if !header.vertex_is_first_element {
        return Err(parse_error(
            path,
            "the vertex element is not the first element",
        ));
    }

    let higher_degree_count = header
        .vertex_properties
        .iter()
        .filter(|property| property.name.starts_with(HIGHER_DEGREE_PREFIX))
        .count();
    let degree = HIGHER_DEGREE_COEFFICIENT_COUNTS
        .iter()
        .position(|&count| count == higher_degree_count)
        .ok_or_else(|| {
            parse_error(
                path,
                format!(
                    "{higher_degree_count} {HIGHER_DEGREE_PREFIX} properties, where a splat PLY \
                     has 0, 9, 24 or 45"
                ),
            )
        })?;
    let mut higher_degree_offsets = Vec::with_capacity(higher_degree_count);
    for coefficient in 0..higher_degree_count {
        let name = format!("{HIGHER_DEGREE_PREFIX}{coefficient}");
        let offset = float_offset(path, &header, &offsets, &name)?
            .ok_or_else(|| parse_error(path, format!("the splat PLY has no {name} property")))?;
        higher_degree_offsets.push(offset);
    }
    let coefficients_per_channel = higher_degree_count / 3;

    let mut cloud = SplatCloud::with_capacity(header.vertex_count, degree as u8, max_bytes_held)
        .map_err(|reason| IngestError::SplatRefused(format!("{}: {reason}", path.display())))?;
    let mut record = vec![0u8; stride];
    let mut required = [0f32; REQUIRED_PROPERTY_COUNT];
    for splat_index in 0..header.vertex_count {
        reader
            .read_exact(&mut record)
            .map_err(|error| match error.kind() {
                ErrorKind::UnexpectedEof => parse_error(
                    path,
                    format!(
                        "the file ends at splat {splat_index} of the {} its header names",
                        header.vertex_count
                    ),
                ),
                _ => IngestError::Io(error),
            })?;
        for ((value, offset), name) in required
            .iter_mut()
            .zip(&required_offsets)
            .zip(&required_names)
        {
            *value = float_at(&record, *offset);
            if !value.is_finite() {
                return Err(not_finite(path, splat_index, name, *value));
            }
        }
        let mut higher_degree_coefficients = [0f32; 45];
        // 3dgs writes f_rest channel major, every red coefficient first
        for (property, offset) in higher_degree_offsets.iter().enumerate() {
            let channel = property / coefficients_per_channel;
            let coefficient = property % coefficients_per_channel;
            let value = float_at(&record, *offset);
            if !value.is_finite() {
                let name = format!("{HIGHER_DEGREE_PREFIX}{property}");
                return Err(not_finite(path, splat_index, &name, value));
            }
            higher_degree_coefficients[coefficient * 3 + channel] = value;
        }
        let [
            x,
            y,
            z,
            dc_red,
            dc_green,
            dc_blue,
            opacity,
            scale_0,
            scale_1,
            scale_2,
            rot_0,
            rot_1,
            rot_2,
            rot_3,
        ] = required;
        cloud
            .push(&GaussianSplat {
                position: [x, y, z],
                log_scale: [scale_0, scale_1, scale_2],
                // 3dgs writes the quaternion scalar first, as rot_0
                rotation_xyzw: [rot_1, rot_2, rot_3, rot_0],
                opacity_logit: opacity,
                color_dc: [dc_red, dc_green, dc_blue],
                higher_degree_coefficients,
            })
            .map_err(|reason| parse_error(path, reason))?;
    }

    tracing::info!("Read {} splats from {}", cloud.len(), path.display());
    Ok(cloud)
}
