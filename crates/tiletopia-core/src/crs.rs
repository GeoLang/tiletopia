//! Custom CRS reprojection support.
//!
//! Transforms coordinates between arbitrary coordinate reference systems.
//! Supports EPSG codes, WKT, and PROJ strings.

use projicio_core::{
    Ellipsoid, GeocentricCoord, Transform, epsg, geocentric_to_geodetic, geodetic_to_geocentric,
};

const WGS84_GEODETIC_EPSG: u32 = 4326;
const EARTH_CENTERED_EARTH_FIXED_EPSG: u32 = 4978;

/// Supported CRS definitions.
#[derive(Debug, Clone)]
pub enum CrsDef {
    /// EPSG code (e.g., 4326 for WGS84, 32632 for UTM zone 32N)
    Epsg(u32),
    /// PROJ.4 string
    Proj(String),
    /// WKT definition
    Wkt(String),
}

const US_SURVEY_FOOT_METRES: f64 = 1200.0 / 3937.0;

fn metres_per_proj4_axis_unit(definition: &str) -> Option<f64> {
    let parameter = |name: &str| {
        definition
            .split_whitespace()
            .find_map(|token| token.strip_prefix(name))
    };
    if parameter("+proj=").is_none_or(|method| matches!(method, "longlat" | "latlong")) {
        return None;
    }
    if let Some(to_meter) = parameter("+to_meter=") {
        return to_meter.parse().ok().filter(|metres: &f64| *metres > 0.0);
    }
    match parameter("+units=") {
        // proj takes a projected definition with no unit as metres
        None | Some("m") => Some(1.0),
        Some("ft") => Some(0.3048),
        Some("us-ft") => Some(US_SURVEY_FOOT_METRES),
        Some(_) => None,
    }
}

impl CrsDef {
    pub fn metres_per_axis_unit(&self) -> Option<f64> {
        let code = match self {
            Self::Epsg(code) => *code,
            Self::Proj(definition) => return metres_per_proj4_axis_unit(definition),
            Self::Wkt(definition) => epsg::parse_wkt_epsg(definition)?,
        };
        metres_per_proj4_axis_unit(epsg::proj4_definition(code)?)
    }

    fn definition(&self) -> String {
        match self {
            Self::Epsg(code) => format!("EPSG:{code}"),
            Self::Proj(text) | Self::Wkt(text) => text.clone(),
        }
    }
}

/// A coordinate reprojection transformer.
#[derive(Debug, Clone)]
pub struct Transformer {
    pub source: CrsDef,
    pub target: CrsDef,
}

/// A 3D coordinate.
#[derive(Debug, Clone, Copy)]
pub struct Coord3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// How a source/target pair is transformed.
pub enum Route {
    Identity,
    GeodeticToGeocentric,
    GeocentricToGeodetic,
    /// projicio transforms x and y, z is carried through untouched.
    HorizontalOnly(Transform),
}

impl Transformer {
    /// Create a new transformer from source to target CRS.
    pub fn new(source: CrsDef, target: CrsDef) -> Self {
        Self { source, target }
    }

    /// Transform a single coordinate from source to target CRS.
    pub fn transform(&self, coord: Coord3D) -> Result<Coord3D, ReprojError> {
        self.route()?.apply(coord)
    }

    /// Transform a batch of coordinates.
    pub fn transform_batch(&self, coords: &[Coord3D]) -> Result<Vec<Coord3D>, ReprojError> {
        let route = self.route()?;
        coords.iter().map(|c| route.apply(*c)).collect()
    }

    pub fn route(&self) -> Result<Route, ReprojError> {
        match (&self.source, &self.target) {
            (CrsDef::Epsg(source), CrsDef::Epsg(target)) if source == target => Ok(Route::Identity),
            (CrsDef::Epsg(WGS84_GEODETIC_EPSG), CrsDef::Epsg(EARTH_CENTERED_EARTH_FIXED_EPSG)) => {
                Ok(Route::GeodeticToGeocentric)
            }
            (CrsDef::Epsg(EARTH_CENTERED_EARTH_FIXED_EPSG), CrsDef::Epsg(WGS84_GEODETIC_EPSG)) => {
                Ok(Route::GeocentricToGeodetic)
            }
            // any other geocentric pairing would run 3D coordinates through a 2D transform
            (CrsDef::Epsg(EARTH_CENTERED_EARTH_FIXED_EPSG), _)
            | (_, CrsDef::Epsg(EARTH_CENTERED_EARTH_FIXED_EPSG)) => {
                Err(ReprojError::UnsupportedTransform)
            }
            (source, target) => Ok(Route::HorizontalOnly(build_transform(source, target)?)),
        }
    }
}

impl Route {
    pub fn apply(&self, coord: Coord3D) -> Result<Coord3D, ReprojError> {
        match self {
            Self::Identity => Ok(coord),
            Self::GeodeticToGeocentric => {
                let geocentric = geodetic_to_geocentric(
                    coord.y.to_radians(),
                    coord.x.to_radians(),
                    coord.z,
                    &Ellipsoid::WGS84,
                );
                Ok(Coord3D {
                    x: geocentric.x,
                    y: geocentric.y,
                    z: geocentric.z,
                })
            }
            Self::GeocentricToGeodetic => {
                let geocentric = GeocentricCoord {
                    x: coord.x,
                    y: coord.y,
                    z: coord.z,
                };
                let (latitude, longitude, height) =
                    geocentric_to_geodetic(&geocentric, &Ellipsoid::WGS84);
                Ok(Coord3D {
                    x: longitude.to_degrees(),
                    y: latitude.to_degrees(),
                    z: height,
                })
            }
            Self::HorizontalOnly(transform) => {
                let (x, y) = transform
                    .convert(coord.x, coord.y)
                    .map_err(|error| ReprojError::Projicio(error.to_string()))?;
                Ok(Coord3D { x, y, z: coord.z })
            }
        }
    }
}

const MOST_DEFINITION_BYTES: usize = 64 * 1024;
const MOST_DEFINITION_NESTING: usize = 32;

fn refuse_oversized_definition(definition: &str) -> Result<(), ReprojError> {
    if definition.len() > MOST_DEFINITION_BYTES {
        return Err(ReprojError::DefinitionRefused(format!(
            "the CRS definition is {} bytes, past the {MOST_DEFINITION_BYTES} byte limit",
            definition.len()
        )));
    }
    let mut nesting = 0usize;
    let mut in_quotes = false;
    for byte in definition.bytes() {
        match byte {
            b'"' => in_quotes = !in_quotes,
            // proj4wkt skips quoted names, so a `]` in one closes nothing and must not lower the count
            _ if in_quotes => {}
            b'[' | b'(' => nesting += 1,
            b']' | b')' => nesting = nesting.saturating_sub(1),
            _ => {}
        }
        if nesting > MOST_DEFINITION_NESTING {
            return Err(ReprojError::DefinitionRefused(format!(
                "the CRS definition has nesting past the {MOST_DEFINITION_NESTING} level limit"
            )));
        }
    }
    Ok(())
}

fn build_transform(source: &CrsDef, target: &CrsDef) -> Result<Transform, ReprojError> {
    let source = source.definition();
    let target = target.definition();
    // proj4wkt 0.1.1 recurses once per `[` with no limit, and a stack overflow aborts the process
    refuse_oversized_definition(&source)?;
    refuse_oversized_definition(&target)?;
    Transform::new(&source, &target).map_err(|error| ReprojError::Projicio(error.to_string()))
}

/// Reprojection errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ReprojError {
    #[error("unsupported CRS transform")]
    UnsupportedTransform,
    #[error("projicio error: {0}")]
    Projicio(String),
    #[error("{0}")]
    DefinitionRefused(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // the content of a typical ESRI .prj: NAD83 UTM zone 18N
    const UTM_18N_WKT: &str = r#"PROJCS["NAD_1983_UTM_Zone_18N",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-75.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

    #[test]
    fn axis_units_come_from_the_crs_definition() {
        let metres = |crs: CrsDef| crs.metres_per_axis_unit();
        assert_eq!(metres(CrsDef::Epsg(32632)), Some(1.0));
        assert_eq!(metres(CrsDef::Epsg(2248)), Some(US_SURVEY_FOOT_METRES));
        assert_eq!(metres(CrsDef::Epsg(4326)), None);
        assert_eq!(metres(CrsDef::Epsg(99999)), None);
        assert_eq!(
            metres(CrsDef::Proj("+proj=tmerc +to_meter=0.5".to_string())),
            Some(0.5)
        );
        assert_eq!(
            metres(CrsDef::Proj("+proj=tmerc +units=ft".to_string())),
            Some(0.3048)
        );
        assert_eq!(
            metres(CrsDef::Proj("+proj=tmerc +units=link".to_string())),
            None
        );
        assert_eq!(metres(CrsDef::Wkt(UTM_18N_WKT.to_string())), None);
    }

    #[test]
    fn test_identity_transform() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(4326));
        let c = Coord3D {
            x: 10.0,
            y: 48.0,
            z: 100.0,
        };
        let r = t.transform(c).unwrap();
        assert!((r.x - c.x).abs() < 1e-10);
    }

    #[test]
    fn test_wgs84_to_utm() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632));
        let c = Coord3D {
            x: 9.0,
            y: 48.0,
            z: 0.0,
        }; // lon=9, lat=48
        let r = t.transform(c).unwrap();
        assert!((r.x - 500_000.0).abs() < 1.0); // central meridian → 500km easting
    }

    #[test]
    fn test_roundtrip_utm() {
        let t_fwd = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632));
        let t_inv = Transformer::new(CrsDef::Epsg(32632), CrsDef::Epsg(4326));
        let c = Coord3D {
            x: 11.5,
            y: 47.3,
            z: 500.0,
        };
        let utm = t_fwd.transform(c).unwrap();
        let back = t_inv.transform(utm).unwrap();
        assert!((back.x - c.x).abs() < 1e-6);
        assert!((back.y - c.y).abs() < 1e-6);
    }

    #[test]
    fn test_wgs84_to_ecef() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(4978));
        let c = Coord3D {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }; // null island
        let r = t.transform(c).unwrap();
        assert!((r.x - Ellipsoid::WGS84.a).abs() < 1.0);
        assert!(r.y.abs() < 1.0);
        assert!(r.z.abs() < 1.0);
    }

    #[test]
    fn test_transform_batch_projects_to_utm_and_keeps_z() {
        let coords = vec![Coord3D {
            x: 9.0,
            y: 48.0,
            z: 250.0,
        }];
        let result = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632))
            .transform_batch(&coords)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!((result[0].x - 500_000.0).abs() < 1.0, "{}", result[0].x);
        assert!((result[0].z - 250.0).abs() < 1e-9);
    }

    #[test]
    fn test_transform_from_wkt_definition() {
        let t = Transformer::new(
            CrsDef::Wkt(UTM_18N_WKT.to_string()),
            CrsDef::Epsg(WGS84_GEODETIC_EPSG),
        );
        let c = Coord3D {
            x: 500_000.0,
            y: 4_510_000.0,
            z: 12.0,
        };
        let r = t.transform(c).unwrap();
        // false easting on a central meridian of -75 degrees
        assert!((r.x - (-75.0)).abs() < 1e-6, "lon={}", r.x);
        assert!((r.y - 40.74).abs() < 0.05, "lat={}", r.y);
        assert!((r.z - 12.0).abs() < 1e-9);
    }

    fn refusal_of(definition: String) -> String {
        let origin = Coord3D {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        Transformer::new(CrsDef::Wkt(definition), CrsDef::Epsg(WGS84_GEODETIC_EPSG))
            .transform(origin)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn nesting_past_the_limit_is_refused_before_any_parse() {
        let error = refusal_of("A[".repeat(MOST_DEFINITION_NESTING + 1));
        assert!(error.contains("nesting"), "{error}");

        assert!(refuse_oversized_definition(&"A[".repeat(MOST_DEFINITION_NESTING)).is_ok());
    }

    #[test]
    fn a_closing_bracket_inside_a_quoted_name_does_not_hide_nesting() {
        let error = refusal_of("A[\"]\",".repeat(MOST_DEFINITION_NESTING + 1));
        assert!(error.contains("nesting"), "{error}");
    }

    #[test]
    fn parentheses_count_as_nesting() {
        let error = refusal_of("A(".repeat(MOST_DEFINITION_NESTING + 1));
        assert!(error.contains("nesting"), "{error}");
    }

    #[test]
    fn a_definition_past_the_byte_limit_is_refused() {
        let error = refusal_of(" ".repeat(MOST_DEFINITION_BYTES + 1));
        assert!(error.contains("byte limit"), "{error}");

        assert!(refuse_oversized_definition(&" ".repeat(MOST_DEFINITION_BYTES)).is_ok());
    }

    #[test]
    fn test_transformer_utm_roundtrip() {
        let fwd = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632));
        let inv = Transformer::new(CrsDef::Epsg(32632), CrsDef::Epsg(4326));
        let c = Coord3D {
            x: 9.0,
            y: 48.0,
            z: 100.0,
        };
        let utm = fwd.transform(c).unwrap();
        assert!((utm.x - 500_000.0).abs() < 1.0);
        let back = inv.transform(utm).unwrap();
        assert!((back.x - 9.0).abs() < 1e-6);
        assert!((back.y - 48.0).abs() < 1e-6);
        assert!((back.z - 100.0).abs() < 1e-6);
    }
}
