use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use geoplumb::tile::{TILE_PX, XyzTile};
use geoplumb::window::Bbox;
use quick_xml::escape::escape;
use uuid::Uuid;

use crate::analysis_tiles::WEB_MERCATOR_EXTENT;
use crate::http_cache::CachePolicy;
use crate::imagery::{self, ImageryManifest};
use crate::terrain_api::Refusal;
use crate::{AppState, Asset};

const WMTS_VERSION: &str = "1.0.0";
const TILE_MATRIX_SET: &str = "GoogleMapsCompatible";
const STYLE: &str = "default";
const PNG: &str = "image/png";

pub fn imagery_service_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/assets/{id}/imagery/wmts", get(wmts_kvp))
        .route(
            "/api/v1/assets/{id}/imagery/wmts/1.0.0/WMTSCapabilities.xml",
            get(wmts_capabilities),
        )
        .route(
            "/api/v1/assets/{id}/imagery/tms/tilemapresource.xml",
            get(tms_tile_map),
        )
        .route("/api/v1/assets/{id}/imagery/tms/{z}/{x}/{y}", get(tms_tile))
}

fn imagery_url(ion_base_url: &str, id: Uuid) -> String {
    format!("{ion_base_url}/api/v1/assets/{id}/imagery")
}

async fn imagery_asset(state: &AppState, id: Uuid) -> Result<(Asset, ImageryManifest), Refusal> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|e| imagery::server_error("imagery asset lookup failed", e))?
        .ok_or_else(|| imagery::no_imagery(id))?;
    let manifest = imagery::read_manifest(&state.data_dir, id)
        .await
        .map_err(|e| imagery::server_error("imagery manifest read failed", e))?
        .ok_or_else(|| imagery::no_imagery(id))?;
    Ok((asset, manifest))
}

fn xml_response(content_type: &str, body: String) -> Response {
    let headers = [
        (header::CONTENT_TYPE, content_type.to_string()),
        (
            header::CACHE_CONTROL,
            CachePolicy::metadata().cache_control_header(),
        ),
    ];
    (headers, body).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TileLimits {
    min_row: u32,
    max_row: u32,
    min_col: u32,
    max_col: u32,
}

impl TileLimits {
    fn contains(&self, x: u32, y: u32) -> bool {
        (self.min_col..=self.max_col).contains(&x) && (self.min_row..=self.max_row).contains(&y)
    }
}

fn tile_limits(footprint: &Bbox, z: u8) -> TileLimits {
    let tiles = 1u32 << z;
    let tile_side = 2.0 * WEB_MERCATOR_EXTENT / f64::from(tiles);
    let last = f64::from(tiles - 1);
    let first_index = |distance: f64| (distance / tile_side).floor().clamp(0.0, last) as u32;
    let last_index = |distance: f64| ((distance / tile_side).ceil() - 1.0).clamp(0.0, last) as u32;
    TileLimits {
        min_row: first_index(WEB_MERCATOR_EXTENT - footprint.max_y),
        max_row: last_index(WEB_MERCATOR_EXTENT - footprint.min_y),
        min_col: first_index(footprint.min_x + WEB_MERCATOR_EXTENT),
        max_col: last_index(footprint.max_x + WEB_MERCATOR_EXTENT),
    }
}

fn level_resolution(z: u8) -> f64 {
    XyzTile { z, x: 0, y: 0 }.resolution()
}

#[derive(Debug, Clone, Copy)]
enum ExceptionCode {
    MissingParameterValue,
    InvalidParameterValue,
    OperationNotSupported,
    VersionNegotiationFailed,
    TileOutOfRange,
}

impl ExceptionCode {
    fn name(self) -> &'static str {
        match self {
            ExceptionCode::MissingParameterValue => "MissingParameterValue",
            ExceptionCode::InvalidParameterValue => "InvalidParameterValue",
            ExceptionCode::OperationNotSupported => "OperationNotSupported",
            ExceptionCode::VersionNegotiationFailed => "VersionNegotiationFailed",
            ExceptionCode::TileOutOfRange => "TileOutOfRange",
        }
    }

    fn status(self) -> StatusCode {
        // ogc 07-057r7 tables 21 and 24 give 501 to OperationNotSupported and 400 to the rest
        match self {
            ExceptionCode::OperationNotSupported => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

struct WmtsException {
    code: ExceptionCode,
    locator: Option<&'static str>,
    text: String,
}

impl IntoResponse for WmtsException {
    fn into_response(self) -> Response {
        let locator = self
            .locator
            .map(|locator| format!(r#" locator="{locator}""#))
            .unwrap_or_default();
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<ExceptionReport xmlns="http://www.opengis.net/ows/1.1" version="{WMTS_VERSION}" xml:lang="en">
  <Exception exceptionCode="{code}"{locator}>
    <ExceptionText>{text}</ExceptionText>
  </Exception>
</ExceptionReport>
"#,
            code = self.code.name(),
            text = escape(self.text.as_str()),
        );
        (
            self.code.status(),
            [(header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response()
    }
}

impl From<WmtsException> for Refusal {
    fn from(exception: WmtsException) -> Self {
        Refusal::from(exception.into_response())
    }
}

struct KvpRequest {
    parameters: HashMap<String, String>,
}

impl KvpRequest {
    fn new(query: Vec<(String, String)>) -> Self {
        // kvp parameter names are case insensitive under ows common 11.5.2
        let parameters = query
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .collect();
        KvpRequest { parameters }
    }

    fn optional(&self, name: &'static str) -> Option<&str> {
        self.parameters
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    fn value(&self, name: &'static str) -> Result<&str, WmtsException> {
        self.optional(name).ok_or_else(|| WmtsException {
            code: ExceptionCode::MissingParameterValue,
            locator: Some(name),
            text: format!("{name} is required"),
        })
    }

    fn require(&self, name: &'static str, expected: &str) -> Result<(), WmtsException> {
        let value = self.value(name)?;
        if value != expected {
            return Err(WmtsException {
                code: ExceptionCode::InvalidParameterValue,
                locator: Some(name),
                text: format!("{name} {value}: expected {expected}"),
            });
        }
        Ok(())
    }

    fn number<T: FromStr>(&self, name: &'static str) -> Result<T, WmtsException> {
        let value = self.value(name)?;
        value.parse().map_err(|_| WmtsException {
            code: ExceptionCode::InvalidParameterValue,
            locator: Some(name),
            text: format!("{name} {value}: expected a non-negative integer"),
        })
    }

    fn check_accepted_versions(&self) -> Result<(), WmtsException> {
        let Some(versions) = self.optional("AcceptVersions") else {
            return Ok(());
        };
        if versions
            .split(',')
            .any(|version| version.trim() == WMTS_VERSION)
        {
            return Ok(());
        }
        Err(WmtsException {
            code: ExceptionCode::VersionNegotiationFailed,
            locator: None,
            text: format!("AcceptVersions {versions}: this server answers {WMTS_VERSION} only"),
        })
    }

    fn requested_tile(
        &self,
        id: Uuid,
        manifest: &ImageryManifest,
    ) -> Result<XyzTile, WmtsException> {
        self.require("Version", WMTS_VERSION)?;
        self.require("Layer", &id.to_string())?;
        self.require("Style", STYLE)?;
        self.require("Format", PNG)?;
        self.require("TileMatrixSet", TILE_MATRIX_SET)?;
        let z: u8 = self.number("TileMatrix")?;
        if z > manifest.max_zoom {
            return Err(WmtsException {
                code: ExceptionCode::InvalidParameterValue,
                locator: Some("TileMatrix"),
                text: format!("TileMatrix {z}: this layer has 0 to {}", manifest.max_zoom),
            });
        }
        let y: u32 = self.number("TileRow")?;
        let x: u32 = self.number("TileCol")?;
        let limits = tile_limits(&manifest.mercator_footprint(), z);
        let out_of_range = |name: &'static str, index: u32, first: u32, last: u32| WmtsException {
            code: ExceptionCode::TileOutOfRange,
            locator: Some(name),
            text: format!("{name} {index}: TileMatrix {z} of this layer has {first} to {last}"),
        };
        if !(limits.min_row..=limits.max_row).contains(&y) {
            return Err(out_of_range("TileRow", y, limits.min_row, limits.max_row));
        }
        if !(limits.min_col..=limits.max_col).contains(&x) {
            return Err(out_of_range("TileCol", x, limits.min_col, limits.max_col));
        }
        Ok(XyzTile { z, x, y })
    }
}

async fn wmts_kvp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(query): Query<Vec<(String, String)>>,
) -> Result<Response, Refusal> {
    let (asset, manifest) = imagery_asset(&state, id).await?;
    let request = KvpRequest::new(query);
    request.require("Service", "WMTS")?;
    match request.value("Request")? {
        "GetCapabilities" => {
            request.check_accepted_versions()?;
            Ok(xml_response(
                "application/xml",
                capabilities_document(&state.ion_base_url, id, &asset.name, &manifest),
            ))
        }
        "GetTile" => {
            let tile = request.requested_tile(id, &manifest)?;
            imagery::tile_png_response(&state, id, tile).await
        }
        other => Err(WmtsException {
            code: ExceptionCode::OperationNotSupported,
            locator: Some("Request"),
            text: format!("{other}: this server answers GetCapabilities and GetTile"),
        }
        .into()),
    }
}

async fn wmts_capabilities(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, Refusal> {
    let (asset, manifest) = imagery_asset(&state, id).await?;
    Ok(xml_response(
        "application/xml",
        capabilities_document(&state.ion_base_url, id, &asset.name, &manifest),
    ))
}

fn kvp_operation(name: &str, service_url: &str) -> String {
    format!(
        r#"    <ows:Operation name="{name}">
      <ows:DCP>
        <ows:HTTP>
          <ows:Get xlink:href="{service_url}?">
            <ows:Constraint name="GetEncoding">
              <ows:AllowedValues>
                <ows:Value>KVP</ows:Value>
              </ows:AllowedValues>
            </ows:Constraint>
          </ows:Get>
        </ows:HTTP>
      </ows:DCP>
    </ows:Operation>
"#
    )
}

fn capabilities_document(
    ion_base_url: &str,
    id: Uuid,
    name: &str,
    manifest: &ImageryManifest,
) -> String {
    // ogc 07-057r7 section 6.1 defines scale against a 0.28 mm rendering pixel
    const RENDERING_PIXEL_METERS: f64 = 0.00028;
    let title = escape(name);
    let imagery_url = imagery_url(ion_base_url, id);
    let service_url = format!("{imagery_url}/wmts");
    let tile_template = format!("{ion_base_url}{}", imagery::tile_template(id))
        .replace("{z}", "{TileMatrix}")
        .replace("{x}", "{TileCol}")
        .replace("{y}", "{TileRow}");
    let [west, south, east, north] = manifest.bounds_degrees;
    let footprint = manifest.mercator_footprint();
    let mut matrix_limits = String::new();
    let mut matrices = String::new();
    for z in 0..=manifest.max_zoom {
        let limits = tile_limits(&footprint, z);
        let tiles = 1u32 << z;
        let scale_denominator = level_resolution(z) / RENDERING_PIXEL_METERS;
        matrix_limits.push_str(&format!(
            r#"          <TileMatrixLimits>
            <TileMatrix>{z}</TileMatrix>
            <MinTileRow>{}</MinTileRow>
            <MaxTileRow>{}</MaxTileRow>
            <MinTileCol>{}</MinTileCol>
            <MaxTileCol>{}</MaxTileCol>
          </TileMatrixLimits>
"#,
            limits.min_row, limits.max_row, limits.min_col, limits.max_col
        ));
        matrices.push_str(&format!(
            r#"      <TileMatrix>
        <ows:Identifier>{z}</ows:Identifier>
        <ScaleDenominator>{scale_denominator}</ScaleDenominator>
        <TopLeftCorner>{} {WEB_MERCATOR_EXTENT}</TopLeftCorner>
        <TileWidth>{TILE_PX}</TileWidth>
        <TileHeight>{TILE_PX}</TileHeight>
        <MatrixWidth>{tiles}</MatrixWidth>
        <MatrixHeight>{tiles}</MatrixHeight>
      </TileMatrix>
"#,
            -WEB_MERCATOR_EXTENT
        ));
    }
    let get_capabilities = kvp_operation("GetCapabilities", &service_url);
    let get_tile = kvp_operation("GetTile", &service_url);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Capabilities xmlns="http://www.opengis.net/wmts/1.0" xmlns:ows="http://www.opengis.net/ows/1.1" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.opengis.net/wmts/1.0 http://schemas.opengis.net/wmts/1.0/wmtsGetCapabilities_response.xsd" version="{WMTS_VERSION}">
  <ows:ServiceIdentification>
    <ows:Title>{title}</ows:Title>
    <ows:ServiceType>OGC WMTS</ows:ServiceType>
    <ows:ServiceTypeVersion>{WMTS_VERSION}</ows:ServiceTypeVersion>
  </ows:ServiceIdentification>
  <ows:OperationsMetadata>
{get_capabilities}{get_tile}  </ows:OperationsMetadata>
  <Contents>
    <Layer>
      <ows:Title>{title}</ows:Title>
      <ows:WGS84BoundingBox>
        <ows:LowerCorner>{west} {south}</ows:LowerCorner>
        <ows:UpperCorner>{east} {north}</ows:UpperCorner>
      </ows:WGS84BoundingBox>
      <ows:Identifier>{id}</ows:Identifier>
      <Style isDefault="true">
        <ows:Identifier>{STYLE}</ows:Identifier>
      </Style>
      <Format>{PNG}</Format>
      <TileMatrixSetLink>
        <TileMatrixSet>{TILE_MATRIX_SET}</TileMatrixSet>
        <TileMatrixSetLimits>
{matrix_limits}        </TileMatrixSetLimits>
      </TileMatrixSetLink>
      <ResourceURL format="{PNG}" resourceType="tile" template="{tile_template}"/>
    </Layer>
    <TileMatrixSet>
      <ows:Identifier>{TILE_MATRIX_SET}</ows:Identifier>
      <ows:SupportedCRS>urn:ogc:def:crs:EPSG::3857</ows:SupportedCRS>
      <WellKnownScaleSet>urn:ogc:def:wkss:OGC:1.0:GoogleMapsCompatible</WellKnownScaleSet>
{matrices}    </TileMatrixSet>
  </Contents>
  <ServiceMetadataURL xlink:href="{service_url}/{WMTS_VERSION}/WMTSCapabilities.xml"/>
</Capabilities>
"#
    )
}

async fn tms_tile_map(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, Refusal> {
    let (asset, manifest) = imagery_asset(&state, id).await?;
    Ok(xml_response(
        "text/xml",
        tile_map_document(&state.ion_base_url, id, &asset.name, &manifest),
    ))
}

fn tile_map_document(
    ion_base_url: &str,
    id: Uuid,
    name: &str,
    manifest: &ImageryManifest,
) -> String {
    let title = escape(name);
    let tms_url = format!("{}/tms", imagery_url(ion_base_url, id));
    let footprint = manifest.mercator_footprint();
    let tile_sets: String = (0..=manifest.max_zoom)
        .map(|z| {
            format!(
                r#"    <TileSet href="{tms_url}/{z}" units-per-pixel="{}" order="{z}"/>
"#,
                level_resolution(z)
            )
        })
        .collect();
    // cesium's TileMapServiceImageryProvider unprojects global-mercator BoundingBox values from metres
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<TileMap version="1.0.0">
  <Title>{title}</Title>
  <SRS>EPSG:3857</SRS>
  <BoundingBox minx="{}" miny="{}" maxx="{}" maxy="{}"/>
  <Origin x="{}" y="{}"/>
  <TileFormat width="{TILE_PX}" height="{TILE_PX}" mime-type="{PNG}" extension="png"/>
  <TileSets profile="global-mercator">
{tile_sets}  </TileSets>
</TileMap>
"#,
        footprint.min_x,
        footprint.min_y,
        footprint.max_x,
        footprint.max_y,
        -WEB_MERCATOR_EXTENT,
        -WEB_MERCATOR_EXTENT,
    )
}

fn tms_tile_inside_limits(
    manifest: &ImageryManifest,
    z: &str,
    x: &str,
    tms_y: &str,
) -> Option<XyzTile> {
    let z: u8 = z.parse().ok().filter(|z| *z <= manifest.max_zoom)?;
    let x: u32 = x.parse().ok()?;
    let tms_y: u32 = tms_y.strip_suffix(".png")?.parse().ok()?;
    // tms rows count up from the south edge, xyz rows down from the north edge
    let y = ((1u32 << z) - 1).checked_sub(tms_y)?;
    tile_limits(&manifest.mercator_footprint(), z)
        .contains(x, y)
        .then_some(XyzTile { z, x, y })
}

async fn tms_tile(
    State(state): State<Arc<AppState>>,
    Path((id, z, x, tms_y)): Path<(Uuid, String, String, String)>,
) -> Result<Response, Refusal> {
    let (_, manifest) = imagery_asset(&state, id).await?;
    let Some(tile) = tms_tile_inside_limits(&manifest, &z, &x, &tms_y) else {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<TileMapServerError>
  <Message>The requested tile is outside the zoom levels or the bounding box that tilemapresource.xml lists.</Message>
</TileMapServerError>
"#;
        return Err((
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/xml")],
            body,
        )
            .into_response()
            .into());
    };
    imagery::tile_png_response(&state, id, tile).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_limits_cover_every_tile_the_footprint_touches() {
        let quarter = WEB_MERCATOR_EXTENT / 2.0;
        let footprint = Bbox::new(-quarter - 1.0, -1.0, 1.0, quarter);
        assert_eq!(
            tile_limits(&footprint, 2),
            TileLimits {
                min_row: 1,
                max_row: 2,
                min_col: 0,
                max_col: 2,
            }
        );
    }

    #[test]
    fn footprints_on_the_globe_edge_stay_inside_the_matrix() {
        let world = Bbox::new(
            -WEB_MERCATOR_EXTENT,
            -WEB_MERCATOR_EXTENT - 1.0,
            WEB_MERCATOR_EXTENT,
            WEB_MERCATOR_EXTENT + 1.0,
        );
        for z in [0, 1, 5] {
            let last = (1u32 << z) - 1;
            assert_eq!(
                tile_limits(&world, z),
                TileLimits {
                    min_row: 0,
                    max_row: last,
                    min_col: 0,
                    max_col: last,
                }
            );
        }
    }
}
