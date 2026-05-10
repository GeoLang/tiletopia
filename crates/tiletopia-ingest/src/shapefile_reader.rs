//! ESRI Shapefile vector reader.

use crate::{IngestError, VectorFeature, VectorGeometry};
use shapefile::dbase::FieldValue;
use std::collections::HashMap;
use std::path::Path;

/// Read an ESRI Shapefile into vector features.
pub fn read(path: &Path) -> Result<Vec<VectorFeature>, IngestError> {
    let mut reader = shapefile::Reader::from_path(path)
        .map_err(|e| IngestError::ParseError(format!("Shapefile open error: {e}")))?;

    let mut result = Vec::new();

    for record in reader.iter_shapes_and_records() {
        let (shape, record) =
            record.map_err(|e| IngestError::ParseError(format!("Shapefile record error: {e}")))?;

        let geometry = convert_shape(shape)?;

        let properties: HashMap<String, String> = record
            .into_iter()
            .filter_map(|(name, value)| {
                let s = match value {
                    FieldValue::Character(Some(s)) => s,
                    FieldValue::Numeric(Some(n)) => n.to_string(),
                    FieldValue::Float(Some(f)) => f.to_string(),
                    FieldValue::Double(d) => d.to_string(),
                    FieldValue::Integer(i) => i.to_string(),
                    FieldValue::Logical(Some(b)) => b.to_string(),
                    _ => return None,
                };
                Some((name, s))
            })
            .collect();

        result.push(VectorFeature {
            geometry,
            properties,
        });
    }

    tracing::info!("Read {} features from {}", result.len(), path.display());
    Ok(result)
}

fn convert_shape(shape: shapefile::Shape) -> Result<VectorGeometry, IngestError> {
    match shape {
        shapefile::Shape::Point(p) => Ok(VectorGeometry::Point(p.x, p.y)),
        shapefile::Shape::PointZ(p) => Ok(VectorGeometry::Point(p.x, p.y)),
        shapefile::Shape::PointM(p) => Ok(VectorGeometry::Point(p.x, p.y)),
        shapefile::Shape::Multipoint(mp) => Ok(VectorGeometry::MultiPoint(
            mp.points().iter().map(|p| (p.x, p.y)).collect(),
        )),
        shapefile::Shape::MultipointZ(mp) => Ok(VectorGeometry::MultiPoint(
            mp.points().iter().map(|p| (p.x, p.y)).collect(),
        )),
        shapefile::Shape::Polyline(pl) => Ok(VectorGeometry::MultiLineString(
            pl.parts().iter().map(|part| part.iter().map(|p| (p.x, p.y)).collect()).collect(),
        )),
        shapefile::Shape::PolylineZ(pl) => Ok(VectorGeometry::MultiLineString(
            pl.parts().iter().map(|part| part.iter().map(|p| (p.x, p.y)).collect()).collect(),
        )),
        shapefile::Shape::Polygon(pg) => Ok(VectorGeometry::Polygon(
            pg.rings()
                .iter()
                .map(|ring| ring.points().iter().map(|p| (p.x, p.y)).collect())
                .collect(),
        )),
        shapefile::Shape::PolygonZ(pg) => Ok(VectorGeometry::Polygon(
            pg.rings()
                .iter()
                .map(|ring| ring.points().iter().map(|p| (p.x, p.y)).collect())
                .collect(),
        )),
        other => Err(IngestError::UnsupportedFormat(format!(
            "unsupported shape type: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_fn_signature() {
        let _: fn(&Path) -> Result<Vec<VectorFeature>, IngestError> = read;
    }

    #[test]
    fn test_read_nonexistent_shapefile() {
        let result = read(Path::new("/tmp/nonexistent_shapefile.shp"));
        assert!(result.is_err());
    }
}
