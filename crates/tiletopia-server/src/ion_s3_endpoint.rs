use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as RoutePath, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::any,
};
use chrono::{NaiveDateTime, Utc};
use futures::StreamExt;
use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, percent_encode};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::AppState;
use crate::ion_upload::{IonUpload, UPLOAD_BUCKET, parts_dir, staging_dir, upload_prefix};
use crate::upload::{MAX_UPLOAD_BYTES, checked_file_name};
use crate::users::to_hex;
use crate::zip_unpack::MOST_ZIP_ENTRIES;

type HmacSha256 = Hmac<Sha256>;

// s3 refuses a request signed more than 15 minutes away from its own clock
const MAX_CLOCK_SKEW_SECONDS: i64 = 15 * 60;

// s3 numbers the parts of a multipart upload from 1 to 10,000
const MAX_PART_NUMBER: u32 = 10_000;

// 10,000 listed parts with a checksum each come to about 1.6 MB of XML
const MAX_REQUEST_XML_BYTES: usize = 4 * 1024 * 1024;

const MAX_CHUNK_LINE_BYTES: usize = 4096;

const ETAG_HEX_DIGITS: usize = 32;

const S3_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

const MULTIPART_KEY_FILE: &str = "key";

const INCOMING_EXTENSION: &str = "incoming";

const JOINING_EXTENSION: &str = "joining";

// s3 refuses any part but the last under 5 MiB, so smaller files go in one PutObject
const S3_SMALLEST_PART_BYTES: u64 = 5 * 1024 * 1024;

const STAGED_CAPS: StagedCaps = StagedCaps {
    bytes: MAX_UPLOAD_BYTES as u64,
    // zip_unpack takes 10,000 entries as one source, and s3 adds up to 10,000 parts for one big file
    files: MOST_ZIP_ENTRIES as u64 + MAX_PART_NUMBER as u64,
    sessions: MAX_UPLOAD_BYTES as u64 / S3_SMALLEST_PART_BYTES,
};

// sigv4 leaves A-Z, a-z, 0-9 and - _ . ~ as they are and writes every other byte as %XY
const SIGV4_ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn ion_s3_routes() -> Router<Arc<AppState>> {
    Router::new().route(&format!("/{UPLOAD_BUCKET}/{{*key}}"), any(serve))
}

#[derive(Debug)]
struct S3Error {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn s3_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> S3Error {
    S3Error {
        status,
        code,
        message: message.into(),
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{}</Code><Message>{}</Message></Error>",
            self.code,
            xml_escape(&self.message)
        );
        (
            self.status,
            [(header::CONTENT_TYPE, "application/xml")],
            body,
        )
            .into_response()
    }
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> S3Error {
    tracing::error!("{context}: {error}");
    s3_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", context)
}

fn not_implemented(request: &str) -> S3Error {
    s3_error(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        format!(
            "{request}: this endpoint takes PutObject, CreateMultipartUpload, UploadPart, CompleteMultipartUpload and AbortMultipartUpload"
        ),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_response(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, "application/xml")],
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}"),
    )
        .into_response()
}

fn etag_response(digest: &str) -> Response {
    (
        StatusCode::OK,
        [(header::ETAG, format!("\"{}\"", &digest[..ETAG_HEX_DIGITS]))],
    )
        .into_response()
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, S3Error> {
    headers
        .get(name)
        .ok_or_else(|| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                format!("the {name} header is missing"),
            )
        })?
        .to_str()
        .map_err(|_| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                format!("the {name} header is not ASCII"),
            )
        })
}

#[derive(Debug)]
struct Authorization {
    access_key: String,
    date: String,
    region: String,
    service: String,
    signed_headers: Vec<String>,
    signature: String,
}

fn parse_authorization(headers: &HeaderMap) -> Result<Authorization, S3Error> {
    let malformed = |reason: &str| {
        s3_error(
            StatusCode::BAD_REQUEST,
            "AuthorizationHeaderMalformed",
            format!("the Authorization header {reason}"),
        )
    };
    let text = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| {
            s3_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "requests to this endpoint are signed with AWS Signature Version 4 in the Authorization header",
            )
        })?
        .to_str()
        .map_err(|_| malformed("is not ASCII"))?;
    let fields = text
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or_else(|| malformed("does not start with AWS4-HMAC-SHA256"))?;

    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for field in fields.split(',') {
        let (name, value) = field
            .trim()
            .split_once('=')
            .ok_or_else(|| malformed("has a field with no value"))?;
        match name {
            "Credential" => credential = Some(value),
            "SignedHeaders" => signed_headers = Some(value),
            "Signature" => signature = Some(value),
            _ => return Err(malformed(&format!("has an unknown field {name}"))),
        }
    }

    let credential = credential.ok_or_else(|| malformed("has no Credential"))?;
    let credential_parts: Vec<&str> = credential.split('/').collect();
    let [access_key, date, region, service, "aws4_request"] = credential_parts[..] else {
        return Err(malformed(
            "Credential is not access-key/date/region/service/aws4_request",
        ));
    };
    if service != "s3" {
        return Err(malformed(&format!(
            "Credential names service {service}, not s3"
        )));
    }
    let signed_headers: Vec<String> = signed_headers
        .ok_or_else(|| malformed("has no SignedHeaders"))?
        .split(';')
        .map(str::to_string)
        .collect();
    if !signed_headers.iter().any(|name| name == "host") {
        return Err(malformed("does not sign the host header"));
    }

    Ok(Authorization {
        access_key: access_key.to_string(),
        date: date.to_string(),
        region: region.to_string(),
        service: service.to_string(),
        signed_headers,
        signature: signature
            .ok_or_else(|| malformed("has no Signature"))?
            .to_string(),
    })
}

fn sigv4_escaped(text: &str) -> String {
    let bytes: Vec<u8> = percent_decode_str(text).collect();
    percent_encode(&bytes, SIGV4_ESCAPED).to_string()
}

fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (sigv4_escaped(name), sigv4_escaped(value))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_request(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    signed_headers: &[String],
    payload_hash: &str,
) -> Result<String, S3Error> {
    let mut canonical_headers = String::new();
    for name in signed_headers {
        let values = headers
            .get_all(name.as_str())
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                s3_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    format!("the {name} header is not ASCII"),
                )
            })?;
        // http/2 carries the host in the :authority pseudo header, which hyper puts in the uri
        let value = match (values.is_empty(), name.as_str(), uri.authority()) {
            (false, _, _) => values.join(","),
            (true, "host", Some(authority)) => authority.to_string(),
            _ => {
                return Err(s3_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidRequest",
                    format!("the signed header {name} is not in the request"),
                ));
            }
        };
        canonical_headers.push_str(&format!("{name}:{value}\n"));
    }
    let path = match uri.path() {
        "" => "/",
        path => path,
    };
    Ok(format!(
        "{method}\n{path}\n{}\n{canonical_headers}\n{}\n{payload_hash}",
        canonical_query(uri.query().unwrap_or("")),
        signed_headers.join(";")
    ))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac takes keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|start| u8::from_str_radix(&text[start..start + 2], 16).ok())
        .collect()
}

fn check_signature(
    secret_access_key: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    authorization: &Authorization,
) -> Result<(), S3Error> {
    let amz_date = header_text(headers, "x-amz-date")?;
    if !amz_date.starts_with(&authorization.date) {
        return Err(s3_error(
            StatusCode::BAD_REQUEST,
            "AuthorizationHeaderMalformed",
            format!(
                "the Credential date {} is not the day of x-amz-date {amz_date}",
                authorization.date
            ),
        ));
    }
    let payload_hash = header_text(headers, "x-amz-content-sha256")?;
    let canonical_request = canonical_request(
        method,
        uri,
        headers,
        &authorization.signed_headers,
        payload_hash,
    )?;
    let scope = format!(
        "{}/{}/{}/aws4_request",
        authorization.date, authorization.region, authorization.service
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        to_hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let signing_key = [
        authorization.date.as_str(),
        authorization.region.as_str(),
        authorization.service.as_str(),
        "aws4_request",
    ]
    .iter()
    .fold(
        format!("AWS4{secret_access_key}").into_bytes(),
        |key, part| hmac_sha256(&key, part.as_bytes()),
    );

    let mismatch = || {
        s3_error(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "the request signature does not match the one computed from the secret access key",
        )
    };
    let presented = hex_bytes(&authorization.signature).ok_or_else(mismatch)?;
    let mut mac = HmacSha256::new_from_slice(&signing_key).expect("hmac takes keys of any length");
    mac.update(string_to_sign.as_bytes());
    mac.verify_slice(&presented).map_err(|_| mismatch())
}

async fn authenticated_upload(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<IonUpload, S3Error> {
    let authorization = parse_authorization(headers)?;
    let upload = state
        .db
        .ion_upload_by_access_key(&authorization.access_key)
        .await
        .map_err(|error| internal_error("reading the upload credentials failed", error))?
        .ok_or_else(|| {
            s3_error(
                StatusCode::FORBIDDEN,
                "InvalidAccessKeyId",
                format!(
                    "access key {} belongs to no asset awaiting files: its upload completed, it expired, or it never existed",
                    authorization.access_key
                ),
            )
        })?;
    check_signature(
        &upload.secret_access_key,
        method,
        uri,
        headers,
        &authorization,
    )?;

    if header_text(headers, "x-amz-security-token")? != upload.session_token {
        return Err(s3_error(
            StatusCode::FORBIDDEN,
            "InvalidToken",
            "the session token is not the one issued with this access key",
        ));
    }

    let now = Utc::now();
    if now >= upload.expires_at {
        return Err(s3_error(
            StatusCode::BAD_REQUEST,
            "ExpiredToken",
            format!(
                "these credentials expired at {}",
                upload.expires_at.to_rfc3339()
            ),
        ));
    }
    let amz_date = header_text(headers, "x-amz-date")?;
    let signed_at = NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
        .map_err(|_| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                format!("x-amz-date {amz_date} is not a YYYYMMDDTHHMMSSZ time"),
            )
        })?
        .and_utc();
    if (now - signed_at).num_seconds().abs() > MAX_CLOCK_SKEW_SECONDS {
        return Err(s3_error(
            StatusCode::FORBIDDEN,
            "RequestTimeTooSkewed",
            format!(
                "the request was signed at {}, more than 15 minutes from this server's clock at {}",
                signed_at.to_rfc3339(),
                now.to_rfc3339()
            ),
        ));
    }

    Ok(upload)
}

#[derive(Debug, PartialEq)]
enum PayloadMode {
    Hashed(String),
    Unsigned,
    AwsChunked { decoded_length: u64 },
}

fn payload_mode(headers: &HeaderMap) -> Result<PayloadMode, S3Error> {
    match header_text(headers, "x-amz-content-sha256")? {
        "UNSIGNED-PAYLOAD" => Ok(PayloadMode::Unsigned),
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER" => {
            let decoded_length = header_text(headers, "x-amz-decoded-content-length")?
                .parse()
                .map_err(|_| {
                    s3_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidArgument",
                        "x-amz-decoded-content-length is not a byte count",
                    )
                })?;
            Ok(PayloadMode::AwsChunked { decoded_length })
        }
        hash if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(PayloadMode::Hashed(hash.to_ascii_lowercase()))
        }
        other if other.starts_with("STREAMING-") => Err(s3_error(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            format!(
                "x-amz-content-sha256 {other}: this endpoint takes a SHA-256 hex digest, UNSIGNED-PAYLOAD or STREAMING-UNSIGNED-PAYLOAD-TRAILER, so turn chunk signing off in the client"
            ),
        )),
        other => Err(s3_error(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            format!("x-amz-content-sha256 {other} is not a SHA-256 hex digest or a payload mode"),
        )),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum ChunkState {
    #[default]
    SizeLine,
    Data(u64),
    DataEnd,
    Trailer,
    Done,
}

#[derive(Debug, Default)]
struct AwsChunkedDecoder {
    state: ChunkState,
    pending: Vec<u8>,
}

fn chunk_line_end(rest: &[u8]) -> Result<Option<usize>, String> {
    match rest.windows(2).position(|pair| pair == b"\r\n") {
        Some(end) => Ok(Some(end)),
        None if rest.len() > MAX_CHUNK_LINE_BYTES => Err(format!(
            "a chunk header or trailer line runs past {MAX_CHUNK_LINE_BYTES} bytes"
        )),
        None => Ok(None),
    }
}

impl AwsChunkedDecoder {
    fn feed(&mut self, input: &[u8], output: &mut Vec<u8>) -> Result<(), String> {
        self.pending.extend_from_slice(input);
        let mut start = 0;
        loop {
            let rest = &self.pending[start..];
            match self.state {
                ChunkState::SizeLine => {
                    let Some(end) = chunk_line_end(rest)? else {
                        break;
                    };
                    let line = String::from_utf8_lossy(&rest[..end]);
                    let size_text = line.split(';').next().unwrap_or_default();
                    let size = u64::from_str_radix(size_text, 16)
                        .map_err(|_| format!("chunk size {line} is not hexadecimal"))?;
                    start += end + 2;
                    self.state = match size {
                        0 => ChunkState::Trailer,
                        size => ChunkState::Data(size),
                    };
                }
                ChunkState::Data(remaining) => {
                    if rest.is_empty() {
                        break;
                    }
                    let taken = remaining.min(rest.len() as u64);
                    output.extend_from_slice(&rest[..taken as usize]);
                    start += taken as usize;
                    self.state = match remaining - taken {
                        0 => ChunkState::DataEnd,
                        left => ChunkState::Data(left),
                    };
                }
                ChunkState::DataEnd => {
                    if rest.len() < 2 {
                        break;
                    }
                    if !rest.starts_with(b"\r\n") {
                        return Err("a chunk's data does not end with CRLF".to_string());
                    }
                    start += 2;
                    self.state = ChunkState::SizeLine;
                }
                ChunkState::Trailer => {
                    let Some(end) = chunk_line_end(rest)? else {
                        break;
                    };
                    start += end + 2;
                    if end == 0 {
                        self.state = ChunkState::Done;
                    }
                }
                ChunkState::Done => {
                    if !rest.is_empty() {
                        return Err("bytes follow the final chunk".to_string());
                    }
                    break;
                }
            }
        }
        self.pending.drain(..start);
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        if self.state == ChunkState::Done {
            return Ok(());
        }
        Err("the body ended before the final chunk and its trailer".to_string())
    }
}

fn upload_cap_allows(
    staged_bytes: u64,
    replaced_bytes: u64,
    incoming_bytes: u64,
    cap_bytes: u64,
) -> bool {
    staged_bytes + incoming_bytes <= cap_bytes + replaced_bytes
}

fn try_count_up(counter: &AtomicU64, adding: u64, replacing: u64, cap: u64) -> bool {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            upload_cap_allows(count, replacing, adding, cap).then_some(count + adding)
        })
        .is_ok()
}

#[derive(Clone, Copy)]
struct StagedCaps {
    bytes: u64,
    files: u64,
    sessions: u64,
}

#[derive(Debug, Default, PartialEq)]
struct StagedUsage {
    bytes: u64,
    files: u64,
    sessions: u64,
}

struct StagedLedger {
    staged_bytes: AtomicU64,
    staged_files: AtomicU64,
    open_sessions: AtomicU64,
    caps: StagedCaps,
    replacing_stored_files: tokio::sync::Mutex<()>,
}

impl StagedLedger {
    fn new(usage: StagedUsage, caps: StagedCaps) -> Self {
        Self {
            staged_bytes: AtomicU64::new(usage.bytes),
            staged_files: AtomicU64::new(usage.files),
            open_sessions: AtomicU64::new(usage.sessions),
            caps,
            replacing_stored_files: tokio::sync::Mutex::new(()),
        }
    }

    fn try_reserve(&self, incoming_bytes: u64, replaced_bytes: u64) -> bool {
        try_count_up(
            &self.staged_bytes,
            incoming_bytes,
            replaced_bytes,
            self.caps.bytes,
        )
    }

    fn try_open_session(&self) -> bool {
        try_count_up(&self.open_sessions, 1, 0, self.caps.sessions)
    }

    fn release(&self, bytes: u64) {
        self.staged_bytes.fetch_sub(bytes, Ordering::SeqCst);
    }

    fn release_file(&self, bytes: u64) {
        self.release(bytes);
        self.staged_files.fetch_sub(1, Ordering::SeqCst);
    }

    fn release_session(&self, session: &StagedUsage) {
        self.release(session.bytes);
        self.staged_files.fetch_sub(session.files, Ordering::SeqCst);
        self.open_sessions
            .fetch_sub(session.sessions, Ordering::SeqCst);
    }
}

struct Reservation {
    ledger: Arc<StagedLedger>,
    replaced_bytes: u64,
    held_bytes: u64,
    held_files: u64,
}

impl Reservation {
    fn new(ledger: Arc<StagedLedger>, replaced_bytes: u64) -> Self {
        Self {
            ledger,
            replaced_bytes,
            held_bytes: 0,
            held_files: 0,
        }
    }

    fn try_add(&mut self, incoming_bytes: u64) -> bool {
        let reserved = self.ledger.try_reserve(incoming_bytes, self.replaced_bytes);
        if reserved {
            self.held_bytes += incoming_bytes;
        }
        reserved
    }

    fn try_add_file(&mut self, replaced_files: u64) -> bool {
        let reserved = try_count_up(
            &self.ledger.staged_files,
            1,
            replaced_files,
            self.ledger.caps.files,
        );
        if reserved {
            self.held_files += 1;
        }
        reserved
    }

    fn keep(mut self) {
        self.held_bytes = 0;
        self.held_files = 0;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.ledger.release(self.held_bytes);
        self.ledger
            .staged_files
            .fetch_sub(self.held_files, Ordering::SeqCst);
    }
}

fn too_large() -> S3Error {
    s3_error(
        StatusCode::BAD_REQUEST,
        "EntityTooLarge",
        format!("the files of one upload hold {MAX_UPLOAD_BYTES} bytes at most"),
    )
}

fn too_many_files(ledger: &StagedLedger) -> S3Error {
    s3_error(
        StatusCode::BAD_REQUEST,
        "TooManyFiles",
        format!(
            "one upload holds {} files and multipart parts at most",
            ledger.caps.files
        ),
    )
}

struct IncomingFile {
    path: PathBuf,
    renamed: bool,
}

impl IncomingFile {
    async fn create(parts: &Path) -> Result<(Self, tokio::fs::File), S3Error> {
        tokio::fs::create_dir_all(parts)
            .await
            .map_err(|error| internal_error("creating the upload directory failed", error))?;
        let path = parts.join(format!("{}.{INCOMING_EXTENSION}", Uuid::new_v4()));
        // tokio::fs::File::create runs on after its future is dropped and recreates a file Drop removed
        let file = std::fs::File::create(&path)
            .map_err(|error| internal_error("writing the upload failed", error))?;
        Ok((
            Self {
                path,
                renamed: false,
            },
            tokio::fs::File::from_std(file),
        ))
    }

    async fn store_as(mut self, destination: &Path) -> std::io::Result<()> {
        tokio::fs::rename(&self.path, destination).await?;
        self.renamed = true;
        Ok(())
    }
}

impl Drop for IncomingFile {
    fn drop(&mut self) {
        if self.renamed {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("removing {} failed: {error}", self.path.display());
        }
    }
}

async fn run_past_disconnect<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> T {
    // hyper drops the handler future when the client disconnects, and spawned tasks run on
    tokio::spawn(work)
        .await
        .expect("the upload's storing task panicked")
}

struct StoredFile {
    path: PathBuf,
    bytes: u64,
}

async fn stored_files(directory: &Path) -> std::io::Result<Vec<StoredFile>> {
    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        if metadata.is_file() {
            files.push(StoredFile {
                path: entry.path(),
                bytes: metadata.len(),
            });
        }
    }
    Ok(files)
}

async fn session_usage(session: &Path) -> std::io::Result<StagedUsage> {
    let mut parts = stored_files(session).await?;
    parts.retain(|file| !file.path.ends_with(MULTIPART_KEY_FILE));
    Ok(StagedUsage {
        bytes: parts.iter().map(|file| file.bytes).sum(),
        files: parts.len() as u64,
        sessions: 1,
    })
}

async fn staged_usage_on_disk(staging: &Path, parts: &Path) -> std::io::Result<StagedUsage> {
    let staged = stored_files(staging).await?;
    let mut usage = StagedUsage {
        bytes: staged.iter().map(|file| file.bytes).sum(),
        files: staged.len() as u64,
        sessions: 0,
    };
    let mut entries = match tokio::fs::read_dir(parts).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(usage),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let session = session_usage(&entry.path()).await?;
            usage.bytes += session.bytes;
            usage.files += session.files;
            usage.sessions += session.sessions;
        }
    }
    Ok(usage)
}

async fn clear_interrupted_writes(parts: &Path) -> std::io::Result<()> {
    let mut entries = match tokio::fs::read_dir(parts).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some(INCOMING_EXTENSION) => tokio::fs::remove_file(&path).await?,
            Some(JOINING_EXTENSION) => tokio::fs::rename(&path, path.with_extension("")).await?,
            _ => {}
        }
    }
    Ok(())
}

type LedgerCell = tokio::sync::OnceCell<Arc<StagedLedger>>;

#[derive(Default)]
pub struct StagedLedgers {
    by_asset: tokio::sync::Mutex<HashMap<Uuid, Arc<LedgerCell>>>,
}

async fn asset_ledger_cell(state: &AppState, asset_id: Uuid) -> Result<Arc<LedgerCell>, S3Error> {
    let mut ledgers = state.staged_upload_ledgers.by_asset.lock().await;
    if let Some(cell) = ledgers.get(&asset_id) {
        return Ok(Arc::clone(cell));
    }

    let awaiting_files = state
        .db
        .asset_ids_awaiting_files()
        .await
        .map_err(|error| internal_error("reading the upload credentials failed", error))?;
    ledgers.retain(|asset_id, _| awaiting_files.contains(asset_id));
    Ok(Arc::clone(ledgers.entry(asset_id).or_default()))
}

async fn staged_ledger(
    state: &AppState,
    asset_id: Uuid,
    target: &ObjectTarget,
) -> Result<Arc<StagedLedger>, S3Error> {
    let cell = asset_ledger_cell(state, asset_id).await?;
    let ledger = cell
        .get_or_try_init(|| async {
            let usage = async {
                clear_interrupted_writes(&target.parts).await?;
                staged_usage_on_disk(&target.staging, &target.parts).await
            }
            .await
            .map_err(|error| internal_error("reading the uploaded files failed", error))?;
            Ok::<_, S3Error>(Arc::new(StagedLedger::new(usage, STAGED_CAPS)))
        })
        .await?;
    Ok(Arc::clone(ledger))
}

async fn write_payload(
    body: Body,
    payload: &PayloadMode,
    mut file: tokio::fs::File,
    reservation: &mut Reservation,
) -> Result<String, S3Error> {
    let write_failed = |error: std::io::Error| internal_error("writing the upload failed", error);
    let mut decoder =
        matches!(payload, PayloadMode::AwsChunked { .. }).then(AwsChunkedDecoder::default);
    let not_aws_chunked = |reason: String| {
        s3_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            format!("the aws-chunked body is malformed: {reason}"),
        )
    };
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut decoded = Vec::new();
    let mut frames = body.into_data_stream();
    while let Some(frame) = frames.next().await {
        let frame = frame.map_err(|error| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "IncompleteBody",
                format!("reading the body failed: {error}"),
            )
        })?;
        let data: &[u8] = match decoder.as_mut() {
            Some(decoder) => {
                decoded.clear();
                decoder
                    .feed(&frame, &mut decoded)
                    .map_err(not_aws_chunked)?;
                &decoded
            }
            None => &frame,
        };
        written += data.len() as u64;
        if !reservation.try_add(data.len() as u64) {
            return Err(too_large());
        }
        hasher.update(data);
        file.write_all(data).await.map_err(write_failed)?;
    }
    file.flush().await.map_err(write_failed)?;
    if let Some(decoder) = &decoder {
        decoder.finish().map_err(not_aws_chunked)?;
    }

    let digest = to_hex(&hasher.finalize());
    match payload {
        PayloadMode::Hashed(expected) if *expected != digest => Err(s3_error(
            StatusCode::BAD_REQUEST,
            "XAmzContentSHA256Mismatch",
            "the body's SHA-256 is not the one x-amz-content-sha256 names",
        )),
        PayloadMode::AwsChunked { decoded_length } if *decoded_length != written => Err(s3_error(
            StatusCode::BAD_REQUEST,
            "IncompleteBody",
            format!(
                "the body decoded to {written} bytes where x-amz-decoded-content-length says {decoded_length}"
            ),
        )),
        _ => Ok(digest),
    }
}

async fn read_request_body(body: Body, payload: &PayloadMode) -> Result<Bytes, S3Error> {
    let bytes = axum::body::to_bytes(body, MAX_REQUEST_XML_BYTES)
        .await
        .map_err(|error| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "MaxMessageLengthExceeded",
                format!(
                    "reading the request body failed, it holds {MAX_REQUEST_XML_BYTES} bytes at most: {error}"
                ),
            )
        })?;
    match payload {
        PayloadMode::Hashed(expected) if *expected != to_hex(&Sha256::digest(&bytes)) => {
            Err(s3_error(
                StatusCode::BAD_REQUEST,
                "XAmzContentSHA256Mismatch",
                "the body's SHA-256 is not the one x-amz-content-sha256 names",
            ))
        }
        PayloadMode::AwsChunked { .. } => Err(s3_error(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "aws-chunked bodies are taken by PutObject and UploadPart only",
        )),
        _ => Ok(bytes),
    }
}

#[derive(Clone)]
struct ObjectTarget {
    key: String,
    file_name: String,
    staging: PathBuf,
    parts: PathBuf,
}

async fn existing_file_size(path: &Path) -> std::io::Result<Option<u64>> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

async fn move_into_staging(
    incoming: IncomingFile,
    target: &ObjectTarget,
    ledger: &StagedLedger,
) -> Result<(), S3Error> {
    let destination = target.staging.join(&target.file_name);
    let replaced_file_size = async {
        tokio::fs::create_dir_all(&target.staging).await?;
        let replaced_file_size = existing_file_size(&destination).await?;
        incoming.store_as(&destination).await?;
        Ok::<Option<u64>, std::io::Error>(replaced_file_size)
    }
    .await
    .map_err(|error| internal_error("storing the uploaded file failed", error))?;
    if let Some(bytes) = replaced_file_size {
        ledger.release_file(bytes);
    }
    Ok(())
}

async fn put_object(
    target: &ObjectTarget,
    headers: &HeaderMap,
    payload: &PayloadMode,
    body: Body,
    ledger: &Arc<StagedLedger>,
) -> Result<Response, S3Error> {
    if headers.contains_key("x-amz-copy-source") {
        return Err(not_implemented("CopyObject"));
    }
    let earlier_copy = existing_file_size(&target.staging.join(&target.file_name))
        .await
        .map_err(|error| internal_error("reading the uploaded files failed", error))?;
    let mut reservation = Reservation::new(Arc::clone(ledger), earlier_copy.unwrap_or(0));
    if !reservation.try_add_file(u64::from(earlier_copy.is_some())) {
        return Err(too_many_files(ledger));
    }
    let (incoming, file) = IncomingFile::create(&target.parts).await?;
    let digest = write_payload(body, payload, file, &mut reservation).await?;

    let (target, ledger) = (target.clone(), Arc::clone(ledger));
    run_past_disconnect(async move {
        let _replacing = ledger.replacing_stored_files.lock().await;
        move_into_staging(incoming, &target, &ledger).await?;
        reservation.keep();
        Ok::<(), S3Error>(())
    })
    .await?;
    Ok(etag_response(&digest))
}

async fn multipart_session(target: &ObjectTarget, upload_id: &str) -> Result<PathBuf, S3Error> {
    let no_such_upload = || {
        s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            format!("there is no multipart upload {upload_id} of {}", target.key),
        )
    };
    let upload_id = Uuid::try_parse(upload_id).map_err(|_| no_such_upload())?;
    let session = target.parts.join(upload_id.simple().to_string());
    match tokio::fs::read_to_string(session.join(MULTIPART_KEY_FILE)).await {
        Ok(file_name) if file_name == target.file_name => Ok(session),
        Ok(_) => Err(no_such_upload()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(no_such_upload()),
        Err(error) => Err(internal_error("reading the multipart upload failed", error)),
    }
}

async fn create_multipart_upload(
    target: &ObjectTarget,
    payload: &PayloadMode,
    body: Body,
    ledger: &Arc<StagedLedger>,
) -> Result<Response, S3Error> {
    read_request_body(body, payload).await?;
    if !ledger.try_open_session() {
        return Err(s3_error(
            StatusCode::BAD_REQUEST,
            "TooManyMultipartUploads",
            format!(
                "one upload holds {} open multipart uploads at most, so complete or abort one first",
                ledger.caps.sessions
            ),
        ));
    }
    let upload_id = Uuid::new_v4().simple().to_string();
    let session = target.parts.join(&upload_id);
    let file_name = target.file_name.clone();
    let opened_ledger = Arc::clone(ledger);
    run_past_disconnect(async move {
        let created = async {
            tokio::fs::create_dir_all(&session).await?;
            tokio::fs::write(session.join(MULTIPART_KEY_FILE), &file_name).await
        }
        .await;
        let Err(error) = created else {
            return Ok(());
        };
        opened_ledger.release_session(&StagedUsage {
            sessions: 1,
            ..StagedUsage::default()
        });
        if let Err(error) = tokio::fs::remove_dir_all(&session).await {
            tracing::warn!("removing {} failed: {error}", session.display());
        }
        Err(internal_error(
            "creating the multipart upload failed",
            error,
        ))
    })
    .await?;
    Ok(xml_response(format!(
        "<InitiateMultipartUploadResult xmlns=\"{S3_NAMESPACE}\"><Bucket>{UPLOAD_BUCKET}</Bucket><Key>{}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>",
        xml_escape(&target.key)
    )))
}

async fn same_numbered_parts(
    session: &Path,
    part_prefix: &str,
) -> std::io::Result<Vec<StoredFile>> {
    let mut files = stored_files(session).await?;
    files.retain(|file| {
        file.path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(part_prefix))
    });
    Ok(files)
}

async fn upload_part(
    target: &ObjectTarget,
    upload_id: &str,
    part_number: Option<&str>,
    payload: &PayloadMode,
    body: Body,
    ledger: &Arc<StagedLedger>,
) -> Result<Response, S3Error> {
    let session = multipart_session(target, upload_id).await?;
    let part_number = part_number
        .and_then(|text| text.parse::<u32>().ok())
        .filter(|number| (1..=MAX_PART_NUMBER).contains(number))
        .ok_or_else(|| {
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                format!("partNumber is a whole number from 1 to {MAX_PART_NUMBER}"),
            )
        })?;
    let part_prefix = format!("{part_number:05}-");
    let earlier_copies = same_numbered_parts(&session, &part_prefix)
        .await
        .map_err(|error| internal_error("reading the multipart upload failed", error))?;
    let mut reservation = Reservation::new(
        Arc::clone(ledger),
        earlier_copies.iter().map(|file| file.bytes).sum(),
    );
    if !reservation.try_add_file(earlier_copies.len() as u64) {
        return Err(too_many_files(ledger));
    }
    let (incoming, file) = IncomingFile::create(&target.parts).await?;
    let digest = write_payload(body, payload, file, &mut reservation).await?;

    let (target, upload_id, ledger) = (target.clone(), upload_id.to_string(), Arc::clone(ledger));
    let part_name = format!("{part_prefix}{}", &digest[..ETAG_HEX_DIGITS]);
    run_past_disconnect(async move {
        let _replacing = ledger.replacing_stored_files.lock().await;
        let session = multipart_session(&target, &upload_id).await?;
        let stored = async {
            for earlier_copy in same_numbered_parts(&session, &part_prefix).await? {
                tokio::fs::remove_file(&earlier_copy.path).await?;
                ledger.release_file(earlier_copy.bytes);
            }
            incoming.store_as(&session.join(part_name)).await
        }
        .await;
        stored.map_err(|error| internal_error("storing the part failed", error))?;
        reservation.keep();
        Ok::<(), S3Error>(())
    })
    .await?;
    Ok(etag_response(&digest))
}

fn element_text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

fn listed_parts(request: &[u8]) -> Result<Vec<(u32, String)>, S3Error> {
    let malformed = || {
        s3_error(
            StatusCode::BAD_REQUEST,
            "MalformedXML",
            "the CompleteMultipartUpload body is not a list of parts with a PartNumber and an ETag each",
        )
    };
    let text = std::str::from_utf8(request).map_err(|_| malformed())?;
    let mut parts = Vec::new();
    for block in text.split("<Part>").skip(1) {
        let (block, _) = block.split_once("</Part>").ok_or_else(malformed)?;
        let part_number = element_text(block, "PartNumber")
            .and_then(|number| number.trim().parse::<u32>().ok())
            .ok_or_else(malformed)?;
        let etag = element_text(block, "ETag")
            .ok_or_else(malformed)?
            .replace("&quot;", "")
            .replace('"', "");
        let etag = etag.trim();
        if etag.len() != ETAG_HEX_DIGITS || !etag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidPart",
                format!("ETag {etag} of part {part_number} is not one this endpoint returned"),
            ));
        }
        parts.push((part_number, etag.to_ascii_lowercase()));
    }
    if parts.is_empty() {
        return Err(malformed());
    }
    Ok(parts)
}

async fn join_parts(parts: &[PathBuf], mut output: tokio::fs::File) -> std::io::Result<()> {
    for part in parts {
        let mut input = tokio::fs::File::open(part).await?;
        tokio::io::copy(&mut input, &mut output).await?;
    }
    output.flush().await
}

struct ClaimedSession {
    session: PathBuf,
    joining: PathBuf,
    usage: StagedUsage,
    part_paths: Vec<PathBuf>,
    reservation: Reservation,
}

async fn claim_session(
    target: &ObjectTarget,
    upload_id: &str,
    listed: &[(u32, String)],
    ledger: &Arc<StagedLedger>,
) -> Result<ClaimedSession, S3Error> {
    let _replacing = ledger.replacing_stored_files.lock().await;
    let session = multipart_session(target, upload_id).await?;
    let joining = session.with_extension(JOINING_EXTENSION);

    let mut part_paths = Vec::with_capacity(listed.len());
    let mut total_bytes: u64 = 0;
    let mut previous_part_number = 0;
    for (part_number, etag) in listed {
        if *part_number <= previous_part_number {
            return Err(s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidPartOrder",
                "the parts are listed in ascending part number order",
            ));
        }
        previous_part_number = *part_number;
        let part_name = format!("{part_number:05}-{etag}");
        let metadata = tokio::fs::metadata(session.join(&part_name))
            .await
            .map_err(|_| {
                s3_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    format!("part {part_number} with ETag {etag} was not uploaded"),
                )
            })?;
        total_bytes += metadata.len();
        part_paths.push(joining.join(part_name));
    }
    if total_bytes > MAX_UPLOAD_BYTES as u64 {
        return Err(s3_error(
            StatusCode::BAD_REQUEST,
            "EntityTooLarge",
            format!("an uploaded file holds {MAX_UPLOAD_BYTES} bytes at most"),
        ));
    }

    let (usage, earlier_copy) = async {
        let usage = session_usage(&session).await?;
        let earlier_copy = existing_file_size(&target.staging.join(&target.file_name)).await?;
        Ok::<_, std::io::Error>((usage, earlier_copy))
    }
    .await
    .map_err(|error| internal_error("reading the multipart upload failed", error))?;
    let mut reservation =
        Reservation::new(Arc::clone(ledger), usage.bytes + earlier_copy.unwrap_or(0));
    if !reservation.try_add(total_bytes) {
        return Err(too_large());
    }
    if !reservation.try_add_file(usage.files + u64::from(earlier_copy.is_some())) {
        return Err(too_many_files(ledger));
    }

    tokio::fs::rename(&session, &joining)
        .await
        .map_err(|error| internal_error("claiming the multipart upload failed", error))?;
    Ok(ClaimedSession {
        session,
        joining,
        usage,
        part_paths,
        reservation,
    })
}

async fn store_claimed_session(
    target: ObjectTarget,
    upload_id: String,
    listed: Vec<(u32, String)>,
    ledger: Arc<StagedLedger>,
) -> Result<(), S3Error> {
    let ClaimedSession {
        session,
        joining,
        usage,
        part_paths,
        reservation,
    } = claim_session(&target, &upload_id, &listed, &ledger).await?;

    let joined = async {
        let (incoming, file) = IncomingFile::create(&target.parts).await?;
        join_parts(&part_paths, file)
            .await
            .map_err(|error| internal_error("joining the parts failed", error))?;
        Ok::<IncomingFile, S3Error>(incoming)
    }
    .await;

    let _replacing = ledger.replacing_stored_files.lock().await;
    let stored = match joined {
        Ok(incoming) => move_into_staging(incoming, &target, &ledger).await,
        Err(error) => Err(error),
    };
    if let Err(error) = stored {
        // s3 clients retry a CompleteMultipartUpload that answered 500
        if let Err(error) = tokio::fs::rename(&joining, &session).await {
            tracing::warn!("reopening {} failed: {error}", session.display());
        }
        return Err(error);
    }
    reservation.keep();
    if let Err(error) = tokio::fs::remove_dir_all(&joining).await {
        tracing::warn!("removing {} failed: {error}", joining.display());
    }
    ledger.release_session(&usage);
    Ok(())
}

async fn complete_multipart_upload(
    target: &ObjectTarget,
    upload_id: &str,
    payload: &PayloadMode,
    body: Body,
    ion_base_url: &str,
    ledger: &Arc<StagedLedger>,
) -> Result<Response, S3Error> {
    multipart_session(target, upload_id).await?;
    let listed = listed_parts(&read_request_body(body, payload).await?)?;
    let listed_etags: String = listed.iter().map(|(_, etag)| etag.as_str()).collect();
    let listed_count = listed.len();

    run_past_disconnect(store_claimed_session(
        target.clone(),
        upload_id.to_string(),
        listed,
        Arc::clone(ledger),
    ))
    .await?;

    let etag = format!(
        "{}-{listed_count}",
        &to_hex(&Sha256::digest(listed_etags.as_bytes()))[..ETAG_HEX_DIGITS],
    );
    let location = format!("{ion_base_url}/{UPLOAD_BUCKET}/{}", target.key);
    Ok(xml_response(format!(
        "<CompleteMultipartUploadResult xmlns=\"{S3_NAMESPACE}\"><Location>{}</Location><Bucket>{UPLOAD_BUCKET}</Bucket><Key>{}</Key><ETag>&quot;{etag}&quot;</ETag></CompleteMultipartUploadResult>",
        xml_escape(&location),
        xml_escape(&target.key)
    )))
}

async fn abort_multipart_upload(
    target: &ObjectTarget,
    upload_id: &str,
    ledger: &Arc<StagedLedger>,
) -> Result<Response, S3Error> {
    let (target, upload_id, ledger) = (target.clone(), upload_id.to_string(), Arc::clone(ledger));
    run_past_disconnect(async move {
        let _replacing = ledger.replacing_stored_files.lock().await;
        let session = multipart_session(&target, &upload_id).await?;
        let usage = async {
            let usage = session_usage(&session).await?;
            tokio::fs::remove_dir_all(&session).await?;
            Ok::<StagedUsage, std::io::Error>(usage)
        }
        .await
        .map_err(|error| internal_error("removing the multipart upload failed", error))?;
        ledger.release_session(&usage);
        Ok::<Response, S3Error>(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

fn query_parameters(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                percent_decode_str(name).decode_utf8_lossy().into_owned(),
                percent_decode_str(value).decode_utf8_lossy().into_owned(),
            )
        })
        .collect()
}

async fn serve(
    State(state): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    RoutePath(key): RoutePath<String>,
    body: Body,
) -> Result<Response, S3Error> {
    let upload = authenticated_upload(&state, &method, &uri, &headers).await?;
    let payload = payload_mode(&headers)?;

    let prefix = upload_prefix(upload.asset_id);
    let file_name = key
        .strip_prefix(&prefix)
        .filter(|name| checked_file_name(name).is_ok())
        .ok_or_else(|| {
            s3_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                format!("{key}: these credentials write a file name directly under {prefix}"),
            )
        })?
        .to_string();
    let target = ObjectTarget {
        key: key.clone(),
        file_name,
        staging: staging_dir(&state.data_dir, upload.asset_id),
        parts: parts_dir(&state.data_dir, upload.asset_id),
    };

    let ledger = staged_ledger(&state, upload.asset_id, &target).await?;

    let parameters = query_parameters(uri.query().unwrap_or(""));
    let parameter = |name: &str| {
        parameters
            .iter()
            .find(|(parameter, _)| parameter == name)
            .map(|(_, value)| value.as_str())
    };
    match (&method, parameter("uploadId")) {
        (&Method::PUT, None) => put_object(&target, &headers, &payload, body, &ledger).await,
        (&Method::PUT, Some(upload_id)) => {
            upload_part(
                &target,
                upload_id,
                parameter("partNumber"),
                &payload,
                body,
                &ledger,
            )
            .await
        }
        (&Method::POST, None) if parameter("uploads").is_some() => {
            create_multipart_upload(&target, &payload, body, &ledger).await
        }
        (&Method::POST, Some(upload_id)) => {
            complete_multipart_upload(
                &target,
                upload_id,
                &payload,
                body,
                &state.ion_base_url,
                &ledger,
            )
            .await
        }
        (&Method::DELETE, Some(upload_id)) => {
            abort_multipart_upload(&target, upload_id, &ledger).await
        }
        _ => Err(not_implemented(&format!(
            "{method} with query {}",
            uri.query().unwrap_or("none")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // aws cli 2.38, aws-sdk 2.1693 and @aws-sdk/client-s3 3.1160 signed the requests below with this
    const CAPTURED_SECRET: &str = "secret";

    const CLI_AWS_CHUNKED_PUT: &[(&str, &str)] = &[
        ("content-type", "text/plain"),
        (
            "amz-sdk-invocation-id",
            "fc4554a3-dc45-4561-a149-a1e1a02e0e07",
        ),
        ("amz-sdk-request", "attempt=1"),
        ("host", "localhost:18555"),
        ("content-encoding", "aws-chunked"),
        ("x-amz-trailer", "x-amz-checksum-crc64nvme"),
        ("x-amz-decoded-content-length", "16"),
        ("content-length", "66"),
        ("x-amz-security-token", "sessiontoken"),
        ("x-amz-date", "20261105T061235Z"),
        (
            "authorization",
            "AWS4-HMAC-SHA256 Credential=AKIDTEST/20261105/us-east-1/s3/aws4_request, SignedHeaders=amz-sdk-invocation-id;amz-sdk-request;content-encoding;content-length;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length;x-amz-security-token;x-amz-trailer, Signature=5120fb71b782624149659c616ae4fb5f931441493b6954eea69e605dc1cf20fd",
        ),
        ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
    ];

    const CLI_AWS_CHUNKED_BODY: &[u8] =
        b"10\r\nion upload body\n\r\n0\r\nx-amz-checksum-crc64nvme:Xz+OYXeVs8A=\r\n\r\n";

    const SDK_V2_CREATE_MULTIPART_UPLOAD: &[(&str, &str)] = &[
        ("content-type", "application/octet-stream"),
        (
            "x-amz-content-sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        ("content-length", "0"),
        ("host", "localhost:18555"),
        ("x-amz-date", "20261105T061259Z"),
        ("x-amz-security-token", "sessiontoken"),
        (
            "authorization",
            "AWS4-HMAC-SHA256 Credential=AKIDTEST/20261105/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=50a6f58904f2c7ff16feaf6b2f6c68af521a2ab8e1975af60b79024615cf37e2",
        ),
    ];

    const SDK_V2_UPLOAD_PART: &[(&str, &str)] = &[
        ("content-type", "application/octet-stream"),
        (
            "x-amz-content-sha256",
            "e8a348c9fd680c4bcb68e3ad0c8ceffa72d7164276fb80a049b8603aade9ce3e",
        ),
        ("content-length", "16"),
        ("host", "localhost:18555"),
        ("x-amz-date", "20261105T061259Z"),
        ("x-amz-security-token", "sessiontoken"),
        (
            "authorization",
            "AWS4-HMAC-SHA256 Credential=AKIDTEST/20261105/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature=2e62cc78dc560607a6dda2897e381705f07363509bae64ed71afb059a14eb2df",
        ),
    ];

    const SDK_V3_PUT_OBJECT: &[(&str, &str)] = &[
        ("content-type", "application/octet-stream"),
        ("content-length", "16"),
        ("x-amz-sdk-checksum-algorithm", "CRC32"),
        ("x-amz-checksum-crc32", "VZCXcg=="),
        ("x-amz-user-agent", "aws-sdk-js/3.1160.0"),
        ("host", "localhost:18555"),
        (
            "amz-sdk-invocation-id",
            "99ae8c9b-888a-4675-b2cd-4a79d3860879",
        ),
        ("amz-sdk-request", "attempt=1; max=3"),
        ("x-amz-date", "20261105T061300Z"),
        ("x-amz-security-token", "sessiontoken"),
        (
            "x-amz-content-sha256",
            "e8a348c9fd680c4bcb68e3ad0c8ceffa72d7164276fb80a049b8603aade9ce3e",
        ),
        (
            "authorization",
            "AWS4-HMAC-SHA256 Credential=AKIDTEST/20261105/us-east-1/s3/aws4_request, SignedHeaders=amz-sdk-invocation-id;amz-sdk-request;content-length;content-type;host;x-amz-checksum-crc32;x-amz-content-sha256;x-amz-date;x-amz-sdk-checksum-algorithm;x-amz-security-token;x-amz-user-agent, Signature=f4cb59bed45f03f5908ad910d49e5cded261a0b12edc3cc39efa3caed0993e8f",
        ),
    ];

    fn header_map(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    fn check_captured(
        method: Method,
        path_and_query: &str,
        headers: &HeaderMap,
    ) -> Result<(), S3Error> {
        let uri: Uri = path_and_query.parse().unwrap();
        let authorization = parse_authorization(headers).unwrap();
        check_signature(CAPTURED_SECRET, &method, &uri, headers, &authorization)
    }

    #[test]
    fn the_aws_cli_aws_chunked_put_verifies() {
        let headers = header_map(CLI_AWS_CHUNKED_PUT);
        check_captured(
            Method::PUT,
            "/tiletopia_ion_uploads/sources/7/hello.txt",
            &headers,
        )
        .unwrap();
        assert_eq!(
            payload_mode(&headers).unwrap(),
            PayloadMode::AwsChunked { decoded_length: 16 }
        );
    }

    #[test]
    fn the_v2_sdk_multipart_calls_verify_with_their_query_strings() {
        check_captured(
            Method::POST,
            "/tiletopia_ion_uploads/sources/7/hello.txt?uploads",
            &header_map(SDK_V2_CREATE_MULTIPART_UPLOAD),
        )
        .unwrap();
        check_captured(
            Method::PUT,
            "/tiletopia_ion_uploads/sources/7/hello.txt?partNumber=1&uploadId=71c18c6cf76144b69a5eb22a61946be1",
            &header_map(SDK_V2_UPLOAD_PART),
        )
        .unwrap();
    }

    #[test]
    fn the_v3_sdk_put_verifies_with_a_spaced_header_value() {
        check_captured(
            Method::PUT,
            "/tiletopia_ion_uploads/sources/7/hello.txt?x-id=PutObject",
            &header_map(SDK_V3_PUT_OBJECT),
        )
        .unwrap();
    }

    #[test]
    fn a_changed_signed_header_or_secret_does_not_verify() {
        let mut headers = header_map(CLI_AWS_CHUNKED_PUT);
        headers.insert("x-amz-decoded-content-length", "17".parse().unwrap());
        let error = check_captured(
            Method::PUT,
            "/tiletopia_ion_uploads/sources/7/hello.txt",
            &headers,
        )
        .unwrap_err();
        assert_eq!(error.code, "SignatureDoesNotMatch");

        let headers = header_map(SDK_V3_PUT_OBJECT);
        let uri: Uri = "/tiletopia_ion_uploads/sources/7/hello.txt?x-id=PutObject"
            .parse()
            .unwrap();
        let authorization = parse_authorization(&headers).unwrap();
        let error =
            check_signature("secrets", &Method::PUT, &uri, &headers, &authorization).unwrap_err();
        assert_eq!(error.code, "SignatureDoesNotMatch");
    }

    #[test]
    fn the_cli_aws_chunked_body_decodes_whole_and_byte_by_byte() {
        let mut decoder = AwsChunkedDecoder::default();
        let mut whole = Vec::new();
        decoder.feed(CLI_AWS_CHUNKED_BODY, &mut whole).unwrap();
        decoder.finish().unwrap();
        assert_eq!(whole, b"ion upload body\n");

        let mut decoder = AwsChunkedDecoder::default();
        let mut bytewise = Vec::new();
        for byte in CLI_AWS_CHUNKED_BODY {
            decoder.feed(&[*byte], &mut bytewise).unwrap();
        }
        decoder.finish().unwrap();
        assert_eq!(bytewise, b"ion upload body\n");
    }

    #[test]
    fn a_truncated_aws_chunked_body_is_refused() {
        let mut decoder = AwsChunkedDecoder::default();
        let mut output = Vec::new();
        decoder
            .feed(&CLI_AWS_CHUNKED_BODY[..20], &mut output)
            .unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn the_cap_check_counts_staged_and_incoming_bytes_less_what_they_replace() {
        assert!(upload_cap_allows(90, 0, 10, 100));
        assert!(!upload_cap_allows(90, 0, 11, 100));
        assert!(upload_cap_allows(90, 40, 50, 100));
        assert!(!upload_cap_allows(90, 40, 51, 100));
    }

    fn ledger_of(staged_bytes: u64, cap_bytes: u64) -> Arc<StagedLedger> {
        Arc::new(StagedLedger::new(
            StagedUsage {
                bytes: staged_bytes,
                ..StagedUsage::default()
            },
            StagedCaps {
                bytes: cap_bytes,
                ..STAGED_CAPS
            },
        ))
    }

    #[test]
    fn concurrent_reservations_share_one_cap_and_a_dropped_one_gives_its_bytes_back() {
        let ledger = ledger_of(0, 100);
        let mut first = Reservation::new(Arc::clone(&ledger), 0);
        let mut second = Reservation::new(Arc::clone(&ledger), 0);
        assert!(first.try_add(60));
        assert!(second.try_add(40));
        assert!(!second.try_add(1));

        drop(first);
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 40);
        second.keep();
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 40);
    }

    #[test]
    fn a_retried_copy_counts_once_after_it_replaces_the_earlier_one() {
        let earlier_copy_bytes = 80;
        let ledger = ledger_of(earlier_copy_bytes, 100);

        let mut retry = Reservation::new(Arc::clone(&ledger), earlier_copy_bytes);
        assert!(retry.try_add(80));
        ledger.release(earlier_copy_bytes);
        retry.keep();
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 80);

        assert!(!Reservation::new(Arc::clone(&ledger), 0).try_add(21));
    }

    #[tokio::test]
    async fn the_count_after_a_restart_is_the_stored_files_and_parts() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let parts = dir.path().join("parts");
        assert_eq!(
            staged_usage_on_disk(&staging, &parts).await.unwrap(),
            StagedUsage::default()
        );

        let session = parts.join("session");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(staging.join("cloud.ply"), [0u8; 10]).unwrap();
        std::fs::write(session.join("00001-etag"), [0u8; 20]).unwrap();
        std::fs::write(session.join(MULTIPART_KEY_FILE), [0u8; 5]).unwrap();
        std::fs::write(parts.join("cut-off.incoming"), [0u8; 40]).unwrap();

        assert_eq!(
            staged_usage_on_disk(&staging, &parts).await.unwrap(),
            StagedUsage {
                bytes: 30,
                files: 2,
                sessions: 1,
            }
        );
    }

    #[tokio::test]
    async fn the_first_scan_removes_cut_off_files_and_reopens_interrupted_joins() {
        let dir = tempfile::tempdir().unwrap();
        let parts = dir.path().join("parts");
        let session = parts.join("session");
        let joining = session.with_extension(JOINING_EXTENSION);
        std::fs::create_dir_all(&joining).unwrap();
        std::fs::write(joining.join(MULTIPART_KEY_FILE), "cloud.ply").unwrap();
        let cut_off = parts.join(format!("cut-off.{INCOMING_EXTENSION}"));
        std::fs::write(&cut_off, [0u8; 40]).unwrap();

        clear_interrupted_writes(&parts).await.unwrap();

        assert!(!cut_off.exists());
        assert!(!joining.exists());
        assert!(session.join(MULTIPART_KEY_FILE).exists());
    }

    fn target_in(dir: &Path) -> ObjectTarget {
        ObjectTarget {
            key: "sources/7/cloud.ply".to_string(),
            file_name: "cloud.ply".to_string(),
            staging: dir.join("staging"),
            parts: dir.join("parts"),
        }
    }

    fn incoming_files(parts: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(parts)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == INCOMING_EXTENSION))
            .collect()
    }

    async fn opened_session(target: &ObjectTarget, ledger: &Arc<StagedLedger>) -> String {
        let response =
            create_multipart_upload(target, &PayloadMode::Unsigned, Body::empty(), ledger)
                .await
                .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        element_text(std::str::from_utf8(&body).unwrap(), "UploadId")
            .unwrap()
            .to_string()
    }

    async fn stored_part(
        target: &ObjectTarget,
        upload_id: &str,
        part_number: &str,
        contents: &'static [u8],
        ledger: &Arc<StagedLedger>,
    ) -> String {
        let response = upload_part(
            target,
            upload_id,
            Some(part_number),
            &PayloadMode::Unsigned,
            Body::from(contents),
            ledger,
        )
        .await
        .unwrap();
        response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_string()
    }

    fn complete_request(parts: &[(u32, &str)]) -> Body {
        let parts: String = parts
            .iter()
            .map(|(part_number, etag)| {
                format!("<Part><ETag>{etag}</ETag><PartNumber>{part_number}</PartNumber></Part>")
            })
            .collect();
        Body::from(format!(
            "<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>"
        ))
    }

    async fn complete(
        target: &ObjectTarget,
        upload_id: &str,
        parts: &[(u32, &str)],
        ledger: &Arc<StagedLedger>,
    ) -> Result<Response, S3Error> {
        complete_multipart_upload(
            target,
            upload_id,
            &PayloadMode::Unsigned,
            complete_request(parts),
            "http://tiletopia.test",
            ledger,
        )
        .await
    }

    struct PausedBody {
        body: Body,
        paused: tokio::sync::oneshot::Receiver<()>,
        resume: tokio::sync::oneshot::Sender<()>,
    }

    fn body_pausing_after(first_frame: &'static [u8]) -> PausedBody {
        let (paused_sender, paused) = tokio::sync::oneshot::channel();
        let (resume, resume_receiver) = tokio::sync::oneshot::channel();
        let frames = futures::stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(first_frame))
        })
        .chain(futures::stream::once(async move {
            paused_sender.send(()).unwrap();
            let _: Result<(), _> = resume_receiver.await;
            Ok(Bytes::new())
        }));
        PausedBody {
            body: Body::from_stream(frames),
            paused,
            resume,
        }
    }

    #[tokio::test]
    async fn a_put_dropped_mid_body_leaves_no_incoming_file_and_gives_its_reservation_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = ledger_of(0, 100);
        let PausedBody {
            body,
            paused,
            resume: _resume,
        } = body_pausing_after(b"body");
        let headers = HeaderMap::new();

        let mut put = Box::pin(put_object(
            &target,
            &headers,
            &PayloadMode::Unsigned,
            body,
            &ledger,
        ));
        tokio::select! {
            _ = &mut put => panic!("the put finished with its body still open"),
            _ = paused => {}
        }
        assert_eq!(incoming_files(&target.parts).len(), 1);
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 4);
        assert_eq!(ledger.staged_files.load(Ordering::SeqCst), 1);

        drop(put);

        assert!(incoming_files(&target.parts).is_empty());
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(ledger.staged_files.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_abort_during_a_part_leaves_no_incoming_file_and_answers_no_such_upload() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = ledger_of(0, 100);
        let upload_id = opened_session(&target, &ledger).await;
        let PausedBody {
            body,
            paused,
            resume,
        } = body_pausing_after(b"part");

        let mut part = Box::pin(upload_part(
            &target,
            &upload_id,
            Some("1"),
            &PayloadMode::Unsigned,
            body,
            &ledger,
        ));
        tokio::select! {
            _ = &mut part => panic!("the part finished with its body still open"),
            _ = paused => {}
        }
        assert_eq!(incoming_files(&target.parts).len(), 1);

        abort_multipart_upload(&target, &upload_id, &ledger)
            .await
            .unwrap();
        resume.send(()).unwrap();

        assert_eq!(part.await.unwrap_err().code, "NoSuchUpload");
        assert!(incoming_files(&target.parts).is_empty());
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(ledger.staged_files.load(Ordering::SeqCst), 0);
        assert_eq!(ledger.open_sessions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_second_complete_and_a_late_part_answer_no_such_upload() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = ledger_of(0, 100);
        let upload_id = opened_session(&target, &ledger).await;
        let etag = stored_part(&target, &upload_id, "1", b"body", &ledger).await;

        complete(&target, &upload_id, &[(1, etag.as_str())], &ledger)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(target.staging.join(&target.file_name)).unwrap(),
            b"body"
        );

        let second = complete(&target, &upload_id, &[(1, etag.as_str())], &ledger).await;
        assert_eq!(second.unwrap_err().code, "NoSuchUpload");
        let late_part = upload_part(
            &target,
            &upload_id,
            Some("2"),
            &PayloadMode::Unsigned,
            Body::from("late"),
            &ledger,
        )
        .await;
        assert_eq!(late_part.unwrap_err().code, "NoSuchUpload");
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 4);
        assert_eq!(ledger.staged_files.load(Ordering::SeqCst), 1);
        assert_eq!(ledger.open_sessions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_join_past_the_cap_is_refused_before_it_writes_and_the_session_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = ledger_of(0, 100);
        let joining_upload = opened_session(&target, &ledger).await;
        stored_part(&target, &joining_upload, "1", &[1u8; 60], &ledger).await;
        let refused_upload = opened_session(&target, &ledger).await;
        let etag = stored_part(&target, &refused_upload, "1", &[2u8; 40], &ledger).await;

        let mut other_join = Reservation::new(Arc::clone(&ledger), 60);
        assert!(other_join.try_add(60));

        let refused = complete(&target, &refused_upload, &[(1, etag.as_str())], &ledger).await;
        assert_eq!(refused.unwrap_err().code, "EntityTooLarge");
        assert!(incoming_files(&target.parts).is_empty());
        assert!(!target.staging.join(&target.file_name).exists());
        multipart_session(&target, &refused_upload).await.unwrap();

        drop(other_join);
        complete(&target, &refused_upload, &[(1, etag.as_str())], &ledger)
            .await
            .unwrap();
        assert_eq!(ledger.staged_bytes.load(Ordering::SeqCst), 100);
    }

    #[tokio::test]
    async fn file_and_session_caps_refuse_what_is_past_them_and_a_repeat_counts_once() {
        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = Arc::new(StagedLedger::new(
            StagedUsage::default(),
            StagedCaps {
                bytes: 100,
                files: 2,
                sessions: 1,
            },
        ));
        let put_empty = |file_name: &str| {
            let named = ObjectTarget {
                file_name: file_name.to_string(),
                ..target.clone()
            };
            let ledger = Arc::clone(&ledger);
            async move {
                put_object(
                    &named,
                    &HeaderMap::new(),
                    &PayloadMode::Unsigned,
                    Body::empty(),
                    &ledger,
                )
                .await
            }
        };

        put_empty("a.ply").await.unwrap();
        put_empty("b.ply").await.unwrap();
        assert_eq!(put_empty("c.ply").await.unwrap_err().code, "TooManyFiles");
        assert!(!target.staging.join("c.ply").exists());
        assert!(incoming_files(&target.parts).is_empty());
        put_empty("a.ply").await.unwrap();
        assert_eq!(ledger.staged_files.load(Ordering::SeqCst), 2);

        let upload_id = opened_session(&target, &ledger).await;
        let second_session =
            create_multipart_upload(&target, &PayloadMode::Unsigned, Body::empty(), &ledger).await;
        assert_eq!(second_session.unwrap_err().code, "TooManyMultipartUploads");

        let part_past_the_cap = upload_part(
            &target,
            &upload_id,
            Some("1"),
            &PayloadMode::Unsigned,
            Body::empty(),
            &ledger,
        )
        .await;
        assert_eq!(part_past_the_cap.unwrap_err().code, "TooManyFiles");

        abort_multipart_upload(&target, &upload_id, &ledger)
            .await
            .unwrap();
        opened_session(&target, &ledger).await;
    }

    #[cfg(unix)]
    struct FifoHoldingTheJoin {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl Drop for FifoHoldingTheJoin {
        fn drop(&mut self) {
            // tokio's runtime shutdown waits for a blocking thread stuck opening a fifo with no writer
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path);
        }
    }

    #[cfg(unix)]
    async fn wait_until(condition: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        while !condition() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_disconnect_during_the_join_leaves_it_running_with_its_session_closed() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let target = target_in(dir.path());
        let ledger = ledger_of(0, 100);
        let upload_id = opened_session(&target, &ledger).await;
        let first_etag = stored_part(&target, &upload_id, "1", b"first", &ledger).await;

        let fifo = FifoHoldingTheJoin {
            path: dir.path().join("second-part"),
        };
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo.path)
            .status()
            .unwrap();
        assert!(made.success());
        let session = target.parts.join(&upload_id);
        let second_etag = "0".repeat(ETAG_HEX_DIGITS);
        std::os::unix::fs::symlink(&fifo.path, session.join(format!("00002-{second_etag}")))
            .unwrap();

        let request = tokio::spawn({
            let (target, upload_id, ledger) =
                (target.clone(), upload_id.clone(), Arc::clone(&ledger));
            let (first_etag, second_etag) = (first_etag.clone(), second_etag.clone());
            async move {
                complete(
                    &target,
                    &upload_id,
                    &[(1, first_etag.as_str()), (2, second_etag.as_str())],
                    &ledger,
                )
                .await
            }
        });
        let joining = session.with_extension(JOINING_EXTENSION);
        wait_until(|| joining.exists()).await;

        let reserved_during_the_join = ledger.staged_bytes.load(Ordering::SeqCst);
        let second_complete =
            complete(&target, &upload_id, &[(1, first_etag.as_str())], &ledger).await;
        let late_part = upload_part(
            &target,
            &upload_id,
            Some("3"),
            &PayloadMode::Unsigned,
            Body::from("late"),
            &ledger,
        )
        .await;
        request.abort();
        let disconnected = request.await.unwrap_err().is_cancelled();

        let fifo_path = fifo.path.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(fifo_path)
                .unwrap()
                .write_all(b"second")
                .unwrap();
        })
        .await
        .unwrap();
        let stored = target.staging.join(&target.file_name);
        wait_until(|| stored.exists() && !joining.exists()).await;

        assert_eq!(reserved_during_the_join, 10);
        assert_eq!(second_complete.unwrap_err().code, "NoSuchUpload");
        assert_eq!(late_part.unwrap_err().code, "NoSuchUpload");
        assert!(disconnected);
        assert_eq!(std::fs::read(&stored).unwrap(), b"firstsecond");
        assert!(incoming_files(&target.parts).is_empty());
    }

    #[test]
    fn signed_chunk_uploads_are_refused_by_name() {
        let headers = header_map(&[("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")]);
        let error = payload_mode(&headers).unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_IMPLEMENTED);
        assert!(
            error.message.contains("UNSIGNED-PAYLOAD"),
            "{}",
            error.message
        );
    }

    #[test]
    fn listed_part_etags_outside_the_hex_this_endpoint_returns_are_refused() {
        let traversal = b"<CompleteMultipartUpload><Part><ETag>../../../etc/passwd</ETag><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>";
        assert_eq!(listed_parts(traversal).unwrap_err().code, "InvalidPart");

        let quoted = b"<CompleteMultipartUpload><Part><ETag>&quot;0C63CC0086D9C2EA7ED41D2228AB2799&quot;</ETag><PartNumber>1</PartNumber></Part></CompleteMultipartUpload>";
        assert_eq!(
            listed_parts(quoted).unwrap(),
            vec![(1, "0c63cc0086d9c2ea7ed41d2228ab2799".to_string())]
        );
    }
}
