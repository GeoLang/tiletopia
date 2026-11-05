use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use std::fmt;
use std::io::BufReader;
use std::path::Path as FilePath;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, AssetType};

pub fn czml_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/assets/{id}/czml", get(get_document))
}

pub(crate) fn document_path(asset_id: Uuid) -> String {
    format!("/api/v1/assets/{asset_id}/czml")
}

async fn get_document(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, StatusCode> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if asset.asset_type != AssetType::Czml {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = state
        .data_dir
        .join(id.to_string())
        .join("input")
        .join(&asset.name);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    // cesium's own development server and web.config serve .czml as application/json
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[derive(Deserialize)]
struct DocumentPacket {
    id: Option<String>,
    version: Option<String>,
}

struct FirstPacket;

impl<'de> Visitor<'de> for FirstPacket {
    type Value = DocumentPacket;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON array of CZML packets")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut packets: A) -> Result<DocumentPacket, A::Error> {
        let first = packets
            .next_element()?
            .ok_or_else(|| de::Error::custom("the array holds no packets"))?;
        while packets.next_element::<IgnoredAny>()?.is_some() {}
        Ok(first)
    }
}

pub(crate) fn check_document(path: &FilePath) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("could not read the upload: {e}"))?;
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
    let first = serde::Deserializer::deserialize_seq(&mut deserializer, FirstPacket)
        .and_then(|first| deserializer.end().map(|()| first))
        .map_err(|e| format!("not CZML, which is a JSON array of packets: {e}"))?;

    // cesium's CzmlDataSource refuses a document whose first packet is not id "document"
    match first.id.as_deref() {
        Some("document") => {}
        Some(other) => {
            return Err(format!(
                "the first packet has id {other}, and CZML starts with the document packet, id document"
            ));
        }
        None => {
            return Err(
                "the first packet has no id, and CZML starts with the document packet, id document"
                    .to_string(),
            );
        }
    }
    // CzmlDataSource takes a version of two dot-separated parts whose major part is 1
    let version = first
        .version
        .ok_or("the document packet has no version, such as \"1.0\"")?;
    match version.split('.').collect::<Vec<_>>().as_slice() {
        ["1", _] => Ok(()),
        _ => Err(format!(
            "document packet version {version}: CesiumJS reads CZML 1, written as 1.<minor>"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Result<(), String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.czml");
        std::fs::write(&path, text).unwrap();
        check_document(&path)
    }

    #[test]
    fn a_document_packet_with_version_one_passes() {
        check(r#"[{"id":"document","version":"1.0"},{"id":"a","position":{"cartographicDegrees":[0,0,0]}}]"#)
            .unwrap();
    }

    #[test]
    fn anything_but_a_leading_versioned_document_packet_is_refused() {
        for (text, named) in [
            (r#"{"id":"document","version":"1.0"}"#, "invalid type: map"),
            ("[]", "no packets"),
            (r#"[{"id":"a"},{"id":"document","version":"1.0"}]"#, "id a,"),
            (r#"[{"id":"document"}]"#, "no version"),
            (r#"[{"id":"document","version":"2.0"}]"#, "2.0"),
            (
                r#"[{"id":"document","version":"1.0"}] trailing"#,
                "trailing",
            ),
        ] {
            let error = check(text).unwrap_err();
            assert!(error.contains(named), "{text}: {error}");
        }
    }
}
