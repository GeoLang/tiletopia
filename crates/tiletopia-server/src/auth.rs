//! JWT authentication middleware.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header::RETRY_AFTER},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;
use crate::api_keys::{ApiKey, Permission, RateLimitResult, hash_presented_key};

const TOOL_TOKEN_USE: &str = "tool";
pub const TILETOPIA_READ_SCOPE: &str = "tiletopia:read";
pub const TILETOPIA_WRITE_SCOPE: &str = "tiletopia:write";

/// Shortest HS256 secret we accept, matching ptolemy and collecta.
pub const MIN_SECRET_LEN: usize = 32;

/// JWT claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    /// Role name: `admin`, `editor` or `viewer`. Read it through
    /// [`Claims::parsed_role`], never by comparing the string, so an unknown
    /// role is refused instead of landing in a tier.
    pub role: String,
}

#[derive(Debug, Deserialize)]
struct TokenClaims {
    sub: String,
    exp: usize,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    token_use: Option<String>,
    #[serde(default)]
    scope: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedClaims {
    pub claims: Claims,
    authority: TokenAuthority,
}

#[derive(Debug, Clone)]
enum TokenAuthority {
    Platform,
    Tool(Vec<String>),
}

impl AuthenticatedClaims {
    pub fn can_write(&self) -> bool {
        match &self.authority {
            TokenAuthority::Platform => self.claims.can_write(),
            TokenAuthority::Tool(scopes) => {
                scopes.iter().any(|scope| scope == TILETOPIA_WRITE_SCOPE)
            }
        }
    }

    pub fn can_admin(&self) -> bool {
        matches!(self.authority, TokenAuthority::Platform) && self.claims.can_admin()
    }

    fn allows_scope(&self, required: &str) -> bool {
        match &self.authority {
            TokenAuthority::Platform => true,
            TokenAuthority::Tool(scopes) => scopes.iter().any(|scope| scope == required),
        }
    }
}

pub(crate) fn verify_token_with_secret(
    token: &str,
    secret: &str,
) -> Result<AuthenticatedClaims, StatusCode> {
    let claims = decode::<TokenClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| StatusCode::UNAUTHORIZED)?
    .claims;

    let (authority, role) = match claims.token_use.as_deref() {
        None => (
            TokenAuthority::Platform,
            claims.role.ok_or(StatusCode::UNAUTHORIZED)?,
        ),
        Some(TOOL_TOKEN_USE) => {
            if claims.sub.is_empty() || claims.role.is_some() {
                return Err(StatusCode::UNAUTHORIZED);
            }
            let scopes = claims
                .scope
                .and_then(|scope| scope.as_array().cloned())
                .ok_or(StatusCode::UNAUTHORIZED)?
                .into_iter()
                .map(|scope| {
                    scope
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(StatusCode::UNAUTHORIZED)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (TokenAuthority::Tool(scopes), String::new())
        }
        Some(_) => return Err(StatusCode::UNAUTHORIZED),
    };

    Ok(AuthenticatedClaims {
        claims: Claims {
            sub: claims.sub,
            exp: claims.exp,
            role,
        },
        authority,
    })
}

fn required_tool_scope(method: &Method, path: &str) -> &'static str {
    if path.starts_with(REALTIME_PREFIX)
        || !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
    {
        TILETOPIA_WRITE_SCOPE
    } else {
        TILETOPIA_READ_SCOPE
    }
}

fn authorize_request(
    token: &str,
    secret: &str,
    method: &Method,
    path: &str,
) -> Result<AuthenticatedClaims, StatusCode> {
    let authenticated = verify_token_with_secret(token, secret)?;
    authenticated
        .allows_scope(required_tool_scope(method, path))
        .then_some(authenticated)
        .ok_or(StatusCode::FORBIDDEN)
}

impl Claims {
    /// Parsed role, or `None` when the token carries a role we don't know.
    pub fn parsed_role(&self) -> Option<crate::users::UserRole> {
        crate::users::UserRole::from_claim(&self.role)
    }

    /// Edit tier: editor or admin.
    pub fn can_write(&self) -> bool {
        use crate::users::UserRole;
        matches!(
            self.parsed_role(),
            Some(UserRole::Admin) | Some(UserRole::Editor)
        )
    }

    /// Admin tier.
    pub fn can_admin(&self) -> bool {
        self.parsed_role() == Some(crate::users::UserRole::Admin)
    }
}

/// Whether auth was explicitly turned off with `TILETOPIA_AUTH_DISABLED=true`.
fn auth_disabled() -> bool {
    std::env::var("TILETOPIA_AUTH_DISABLED").as_deref() == Ok("true")
}

/// Check the auth environment before serving. Returns Err when
/// `TILETOPIA_JWT_SECRET` is missing or too short, so the server refuses to
/// start rather than coming up with every write endpoint open. Explicitly
/// setting `TILETOPIA_AUTH_DISABLED=true` allows an unauthenticated run and
/// logs a loud warning.
///
/// The error text carries the secret's length, never the secret.
pub fn startup_check() -> Result<(), String> {
    let secret = std::env::var("TILETOPIA_JWT_SECRET").ok();
    check_secret(secret.as_deref(), auth_disabled())
}

/// The [`startup_check`] rule over its two inputs, so it is testable without
/// touching process-global environment variables.
pub fn check_secret(secret: Option<&str>, auth_disabled: bool) -> Result<(), String> {
    if auth_disabled {
        // also on stderr: a warning this important must not depend on RUST_LOG
        const MSG: &str = "TILETOPIA_AUTH_DISABLED=true: authentication is OFF, every endpoint is open to anonymous callers";
        eprintln!("WARNING: {MSG}");
        tracing::warn!("{MSG}");
        return Ok(());
    }
    let secret = secret.unwrap_or_default();
    if secret.is_empty() {
        return Err(
            "TILETOPIA_JWT_SECRET is not set. Set it to 32+ random bytes shared with the other \
             platform services, or set TILETOPIA_AUTH_DISABLED=true to run without auth."
                .into(),
        );
    }
    if secret.len() < MIN_SECRET_LEN {
        return Err(format!(
            "TILETOPIA_JWT_SECRET is {} bytes, need at least {MIN_SECRET_LEN}",
            secret.len()
        ));
    }
    Ok(())
}

/// Path prefix of the realtime collaboration WebSocket.
pub const REALTIME_PREFIX: &str = "/api/v1/realtime/";

/// Subprotocol name that marks a WebSocket handshake as carrying a bearer token.
/// See [`request_token`] for the full contract.
pub const BEARER_SUBPROTOCOL: &str = "bearer";

/// Whether this request is an anonymous tile-DATA read. CesiumJS and deck.gl
/// fetch these with no Authorization header, so they stay open.
///
/// Every arm matches whole path segments anchored at the root, and each one is a
/// route that exists. This used to be substring matching, where any path holding
/// `/tiles/` or `tileset.json` anywhere went public, so `/api/v1/users/me/tiles/`
/// or the whole API mounted under a second `/tiles/v1` prefix would have skipped
/// auth for every GET. Adding a public route is now a deliberate edit here
/// rather than a side effect of what the route is called.
///
/// GET only, so the mutating `POST /v1/assets` and `POST /v1/tokens` stay
/// behind auth. Anything not listed here is protected by default.
pub fn is_public_read(method: &Method, path: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    // axum never matches a route with an empty segment, so dropping them keeps a
    // doubled slash from shifting the positions the patterns below rely on
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        // 3D Tiles: the tileset and the tile payloads it references. Exactly one
        // trailing segment, matching the route: the tilers encode octree depth
        // into the filename ("037.glb"), never into directories, so a nested tile
        // path is not something this server can serve. See the note on the route
        // in lib.rs before widening either side.
        ["api", "v1", "assets", _, "tileset.json"] => true,
        ["api", "v1", "assets", _, "tiles", _] => true,
        // the external tiler references its tile content as "data/RC0000.glb",
        // one trailing segment the same way
        ["api", "v1", "assets", _, "data", _] => true,

        // quantized-mesh terrain: layer.json, the {z}/{x}/{y} tiles it
        // advertises, the terrain-rgb variant, and the same two under
        // /bundles/{name}/ for a prebuilt bundle. A terrain provider is a tile
        // layer like any other and cannot send a header, and the bundle listing
        // names directories an operator chose to serve, so the whole subtree is
        // read-open. Matching "terrain" as a whole segment is what keeps the
        // /api/v1/terrain-analysis/ compute routes gated, rather than the
        // trailing slash this used to lean on.
        ["api", "v1", "terrain", rest @ ..] => !rest.is_empty(),

        // On-demand terrain-analysis tiles. A map library fetches these with no
        // Authorization header like any other tile layer. The op, z, x and y
        // segments are all matched, so the POST compute routes under
        // /api/v1/analysis/ stay gated.
        ["api", "v1", "analysis", "xyz", _, _, _, _] => true,

        // Vector tile source metadata. Public only because the old substring
        // reached it, so it is listed to keep this change from moving any route.
        // /api/v1/tiles/cache/stats is deliberately absent: it is operational
        // telemetry and carries an Admin gate on the route itself.
        ["api", "v1", "tiles", "sources"] => true,
        ["api", "v1", "tiles", "styles"] => true,
        ["api", "v1", "tiles", "layers"] => true,
        ["api", "v1", "tiles", _, "tilejson"] => true,

        // Ion-compat reads
        ["v1", "tokens"] => true,
        ["v1", "assets", ..] => true,

        _ => false,
    }
}

/// Header an API key is presented in.
pub const API_KEY_HEADER: &str = "X-Api-Key";

/// What a live API key needs to reach a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAccess {
    /// No key reaches this route, whatever it carries.
    None,
    /// Any live key. Reading a key's own usage needs no permission.
    AnyKey,
    /// A key carrying this permission.
    Needs(Permission),
}

/// Which routes an API key may reach, and with what permission.
///
/// Anything not listed refuses every key, so widening key access is a
/// deliberate edit here. Two kinds of route are absent on purpose:
///
/// - admin surfaces. Key management, `/api/v1/admin/`, org management and the
///   tile cache stats sit behind [`crate::users::require_admin`], which reads
///   JWTs only, and there is no Admin permission for a key to carry.
/// - routes whose handler reads the caller's JWT `sub` to scope its answer
///   (assets, exports, portal items, tilesets, `/api/v1/users/me`). A key has no
///   platform user behind it, so those would 401 inside the handler anyway.
pub fn route_access(method: &Method, path: &str) -> RouteAccess {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let read_only = matches!(*method, Method::GET | Method::HEAD);
    let computes = matches!(*method, Method::GET | Method::POST);

    match segments.as_slice() {
        // a key reading its own usage
        ["api", "v1", "api-keys", "usage"] if read_only => RouteAccess::AnyKey,

        // catalog and dataset metadata
        ["api", "v1", "catalog", ..] if read_only => RouteAccess::Needs(Permission::Read),
        ["api", "v1", "stac", ..] if read_only => RouteAccess::Needs(Permission::Read),
        ["api", "v1", "cog", ..] if read_only => RouteAccess::Needs(Permission::Read),
        ["api", "v1", "features", ..] if read_only => RouteAccess::Needs(Permission::Read),
        ["api", "v1", "geocoding", ..] if read_only => RouteAccess::Needs(Permission::Read),

        // rendered output a caller downloads. the export arm consumes the path
        // whatever the method, so the analysis compute arm below cannot claim a
        // write to it
        ["api", "v1", "analysis", "export", ..] if read_only => {
            RouteAccess::Needs(Permission::Export)
        }
        ["api", "v1", "analysis", "export", ..] => RouteAccess::None,
        ["api", "v1", "static-map", ..] if computes => RouteAccess::Needs(Permission::Export),

        // terrain and elevation compute
        ["api", "v1", "elevation", ..] if computes => RouteAccess::Needs(Permission::Terrain),
        ["api", "v1", "terrain-analysis", ..] if computes => {
            RouteAccess::Needs(Permission::Terrain)
        }

        // analysis compute
        ["api", "v1", "analysis", ..] if computes => RouteAccess::Needs(Permission::Analytics),
        ["api", "v1", "geostatistics", ..] if computes => RouteAccess::Needs(Permission::Analytics),
        ["api", "v1", "geoprocessing", ..] if computes => RouteAccess::Needs(Permission::Analytics),

        _ => RouteAccess::None,
    }
}

/// The API key a request presents, if any.
fn presented_api_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|presented| !presented.is_empty())
}

/// A refusal naming the reason class. Never the key, and never which key.
/// Boxed, so the error arm of [`authorize_api_key`] stays small.
fn key_refusal(status: StatusCode, reason: &str) -> Box<Response> {
    Box::new((status, Json(serde_json::json!({ "error": reason }))).into_response())
}

/// Authorize a request that presented an API key: resolve the digest, check
/// revocation and expiry, match the route against [`route_access`], then spend a
/// token from the rate limiter.
///
/// Every failure is a refusal, so a bad or over-budget key never falls back to
/// the JWT path.
async fn authorize_api_key(
    state: &AppState,
    presented: &str,
    method: &Method,
    path: &str,
) -> Result<ApiKey, Box<Response>> {
    let hash = hash_presented_key(presented)
        .ok_or_else(|| key_refusal(StatusCode::UNAUTHORIZED, "malformed api key"))?;

    let key = state
        .db
        .api_key_by_hash(&hash)
        .await
        .map_err(|error| {
            tracing::error!("api key lookup failed: {error}");
            key_refusal(StatusCode::INTERNAL_SERVER_ERROR, "api key lookup failed")
        })?
        .ok_or_else(|| key_refusal(StatusCode::UNAUTHORIZED, "unknown api key"))?;

    if key.revoked {
        return Err(key_refusal(StatusCode::UNAUTHORIZED, "revoked api key"));
    }
    if key.expired_at(chrono::Utc::now()) {
        return Err(key_refusal(StatusCode::UNAUTHORIZED, "expired api key"));
    }

    let allowed = match route_access(method, path) {
        RouteAccess::None => false,
        RouteAccess::AnyKey => true,
        RouteAccess::Needs(permission) => key.allows(permission),
    };
    if !allowed {
        return Err(key_refusal(
            StatusCode::FORBIDDEN,
            "api key not permitted on this route",
        ));
    }

    match state
        .api_key_rate_limiter
        .check_rate_limit(key.id, &key.rate_limit())
        .await
    {
        RateLimitResult::Allowed => {}
        RateLimitResult::Denied {
            reason,
            retry_after_ms,
        } => {
            // seconds for the standard header, milliseconds in the body for a
            // caller that wants to retry sooner than a whole second
            let retry_after_seconds = retry_after_ms.div_ceil(1000).max(1);
            return Err(Box::new(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(RETRY_AFTER, retry_after_seconds.to_string())],
                    Json(serde_json::json!({
                        "error": reason,
                        "retry_after_ms": retry_after_ms,
                        "retry_after_seconds": retry_after_seconds,
                    })),
                )
                    .into_response(),
            ));
        }
    }

    Ok(key)
}

/// Bearer token for a request: the `Authorization` header, or on the realtime
/// WebSocket path a `Sec-WebSocket-Protocol: bearer, <jwt>` offer.
///
/// The subprotocol form is there because a browser cannot set the Authorization
/// header on a WebSocket handshake. It is preferred over a query parameter
/// because proxies do not log request headers. It is scoped to the realtime
/// path, so nowhere else does a subprotocol act as a credential.
pub fn request_token<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    let header_token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match header_token {
        Some(token) => Some(token),
        None if uri.path().starts_with(REALTIME_PREFIX) => subprotocol_token(headers),
        None => None,
    };
    token.filter(|t| !t.is_empty())
}

/// Token out of a `Sec-WebSocket-Protocol: bearer, <jwt>` offer. Order is fixed:
/// the marker first, the token second, both in one header value, which is what
/// `new WebSocket(url, ["bearer", jwt])` sends.
fn subprotocol_token(headers: &HeaderMap) -> Option<&str> {
    let offered = headers.get("Sec-WebSocket-Protocol")?.to_str().ok()?;
    let mut entries = offered.split(',').map(str::trim);
    if entries.next()? != BEARER_SUBPROTOCOL {
        return None;
    }
    entries.next()
}

/// Auth middleware — validates the JWT in the Authorization header, or the API
/// key in `X-Api-Key`.
///
/// If `TILETOPIA_JWT_SECRET` is not set, auth is disabled (open access);
/// [`startup_check`] is what keeps the serve path from reaching that state by
/// accident.
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // explicit opt-out wins, so "disabled" means the same thing here as it does
    // at startup even when a secret happens to be present
    if auth_disabled() {
        return Ok(next.run(request).await);
    }

    let secret = std::env::var("TILETOPIA_JWT_SECRET").ok();

    // If no secret configured, allow all requests (development mode)
    let Some(secret) = secret else {
        return Ok(next.run(request).await);
    };

    // Health endpoint is always public
    if request.uri().path() == "/api/v1/health"
        || request.uri().path() == "/metrics"
        || request.uri().path().starts_with("/api/v1/auth/")
        || request.uri().path().starts_with("/api/v1/stories/share/")
    {
        return Ok(next.run(request).await);
    }

    if is_public_read(request.method(), request.uri().path()) {
        return Ok(next.run(request).await);
    }

    // an X-Api-Key is the credential for the request that sends it: a bad key is
    // refused rather than falling back to whatever bearer token came with it
    if let Some(presented) = presented_api_key(request.headers()) {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let key = match authorize_api_key(&state, presented, &method, &path).await {
            Ok(key) => key,
            Err(refusal) => return Ok(*refusal),
        };

        // last use is a convenience for the key listing, so it runs off the
        // request path and a failed write only logs
        let db = Arc::clone(&state.db);
        let key_id = key.id;
        tokio::spawn(async move {
            if let Err(error) = db.touch_api_key(key_id, chrono::Utc::now()).await {
                tracing::warn!("recording api key use failed: {error}");
            }
        });

        // the resolved key, so the usage handler knows who asked without
        // hashing and looking it up a second time
        request.extensions_mut().insert(key);
        return Ok(next.run(request).await);
    }

    let token = request_token(request.headers(), request.uri()).ok_or(StatusCode::UNAUTHORIZED)?;

    let _claims = authorize_request(token, &secret, request.method(), request.uri().path())?;

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn live_expiry() -> i64 {
        chrono::Utc::now().timestamp() + 300
    }

    fn signed(claims: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn tool_tokens_need_the_exact_operation_scope() {
        let read = signed(serde_json::json!({
            "sub": "user-1",
            "exp": live_expiry(),
            "token_use": "tool",
            "scope": [TILETOPIA_READ_SCOPE]
        }));
        assert!(authorize_request(&read, SECRET, &Method::GET, "/api/v1/assets").is_ok());
        assert_eq!(
            authorize_request(&read, SECRET, &Method::POST, "/api/v1/assets").unwrap_err(),
            StatusCode::FORBIDDEN
        );

        let wrong_service = signed(serde_json::json!({
            "sub": "user-1",
            "exp": live_expiry(),
            "token_use": "tool",
            "scope": ["ptolemy:read"]
        }));
        assert_eq!(
            authorize_request(&wrong_service, SECRET, &Method::GET, "/api/v1/assets").unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn a_tool_token_cannot_fall_back_to_a_role() {
        let token = signed(serde_json::json!({
            "sub": "user-1",
            "exp": live_expiry(),
            "role": "admin",
            "token_use": "tool",
            "scope": [TILETOPIA_READ_SCOPE]
        }));
        assert_eq!(
            authorize_request(&token, SECRET, &Method::GET, "/api/v1/assets").unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn resource_ownership_does_not_replace_a_missing_operation_scope() {
        let token = signed(serde_json::json!({
            "sub": "user-1",
            "exp": live_expiry(),
            "token_use": "tool",
            "scope": [TILETOPIA_READ_SCOPE]
        }));
        let authenticated = verify_token_with_secret(&token, SECRET).unwrap();
        assert!(crate::may_modify_asset(
            &authenticated.claims,
            Some("user-1")
        ));
        assert_eq!(
            authorize_request(&token, SECRET, &Method::POST, "/api/v1/assets").unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn malformed_and_unknown_tool_claims_are_unauthorized() {
        for claims in [
            serde_json::json!({
                "sub": "user-1", "exp": live_expiry(), "token_use": "tool"
            }),
            serde_json::json!({
                "sub": "user-1", "exp": live_expiry(), "token_use": "tool", "scope": "tiletopia:read"
            }),
            serde_json::json!({
                "sub": "user-1", "exp": live_expiry(), "token_use": "other", "scope": [TILETOPIA_READ_SCOPE]
            }),
        ] {
            let token = signed(claims);
            assert_eq!(
                authorize_request(&token, SECRET, &Method::GET, "/api/v1/assets").unwrap_err(),
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[test]
    fn platform_roles_keep_the_existing_authority() {
        let editor = signed(serde_json::json!({
            "sub": "user-1", "exp": live_expiry(), "role": "editor"
        }));
        let viewer = signed(serde_json::json!({
            "sub": "user-1", "exp": live_expiry(), "role": "viewer"
        }));
        assert!(
            authorize_request(&editor, SECRET, &Method::POST, "/api/v1/assets")
                .unwrap()
                .can_write()
        );
        assert!(
            !authorize_request(&viewer, SECRET, &Method::POST, "/api/v1/assets")
                .unwrap()
                .can_write()
        );
    }

    #[test]
    fn missing_secret_refuses_startup() {
        let err = check_secret(None, false).unwrap_err();
        assert!(err.contains("TILETOPIA_JWT_SECRET is not set"));
        assert!(check_secret(Some(""), false).is_err());
    }

    #[test]
    fn short_secret_refuses_startup() {
        let err = check_secret(Some("short-secret"), false).unwrap_err();
        assert!(err.contains("need at least 32"));
        // the error must not leak the secret itself
        assert!(!err.contains("short-secret"));
    }

    #[test]
    fn long_secret_starts() {
        assert!(check_secret(Some("0123456789abcdef0123456789abcdef"), false).is_ok());
    }

    #[test]
    fn explicit_opt_out_starts_without_secret() {
        assert!(check_secret(None, true).is_ok());
    }

    #[test]
    fn tile_data_reads_are_public() {
        assert!(is_public_read(
            &Method::GET,
            "/api/v1/assets/abc/tileset.json"
        ));
        // one segment, because the tilers flatten octree depth into the filename
        assert!(is_public_read(
            &Method::GET,
            "/api/v1/assets/abc/tiles/037.glb"
        ));
        assert!(is_public_read(&Method::GET, "/v1/assets"));
        assert!(is_public_read(&Method::GET, "/v1/assets/1/endpoint"));
    }

    #[test]
    fn terrain_reads_are_public() {
        assert!(is_public_read(&Method::GET, "/api/v1/terrain/layer.json"));
        assert!(is_public_read(&Method::GET, "/api/v1/terrain/12/2200/1400"));
        // a prebuilt bundle is the same tile layer from a directory, and the
        // listing is the names an operator chose to host
        assert!(is_public_read(&Method::GET, "/api/v1/terrain/bundles"));
        assert!(is_public_read(
            &Method::GET,
            "/api/v1/terrain/bundles/alps/layer.json"
        ));
        assert!(is_public_read(
            &Method::GET,
            "/api/v1/terrain/bundles/alps/9/271/183.terrain"
        ));
    }

    #[test]
    fn writes_never_ride_the_read_exemption() {
        for path in [
            "/api/v1/terrain/layer.json",
            "/api/v1/assets/abc/tileset.json",
            "/v1/assets",
            "/v1/tokens",
        ] {
            assert!(!is_public_read(&Method::POST, path), "POST {path}");
            assert!(!is_public_read(&Method::PUT, path), "PUT {path}");
            assert!(!is_public_read(&Method::DELETE, path), "DELETE {path}");
        }
    }

    #[test]
    fn non_tile_reads_stay_gated() {
        // terrain-analysis is compute, not tile data
        assert!(!is_public_read(
            &Method::GET,
            "/api/v1/terrain-analysis/operations"
        ));
        assert!(!is_public_read(&Method::GET, "/api/v1/elevation/point"));
        // the analysis compute routes, next to the public analysis tiles
        assert!(!is_public_read(&Method::GET, "/api/v1/analysis/terrain"));
        assert!(!is_public_read(&Method::GET, "/api/v1/analysis/xyz"));
        assert!(!is_public_read(
            &Method::GET,
            "/api/v1/analysis/xyz/hillshade/12/2132"
        ));
        // an export is one request costing millions of pixels, never anonymous
        assert!(!is_public_read(
            &Method::GET,
            "/api/v1/analysis/export/hillshade"
        ));
        assert!(!is_public_read(&Method::GET, "/api/v1/portal/items"));
        // an asset's jobs sit under the same prefix as its public tiles
        assert!(!is_public_read(&Method::GET, "/api/v1/assets/abc/jobs"));
        // the realtime websocket needs a token like any other non-tile route
        assert!(!is_public_read(&Method::GET, "/api/v1/realtime/room-1"));
        // a stac search reaches an upstream catalog, and a cog window streams
        // range requests at an operator's source: neither is anonymous
        assert!(!is_public_read(&Method::GET, "/api/v1/stac/search"));
        assert!(!is_public_read(&Method::GET, "/api/v1/cog/datasets"));
        assert!(!is_public_read(
            &Method::GET,
            "/api/v1/cog/datasets/ramp/window"
        ));
    }

    /// Cache hit rates and size are operational telemetry, not map data. The
    /// route carries an Admin gate, and this keeps the read exemption from
    /// letting an anonymous caller reach it first.
    #[test]
    fn tile_cache_stats_is_not_a_public_read() {
        assert!(!is_public_read(&Method::GET, "/api/v1/tiles/cache/stats"));
    }

    /// Every GET the router serves anonymously, so a tightening of the matcher
    /// that would break the viewer's golden path fails here first.
    #[test]
    fn every_public_route_still_classifies_public() {
        for path in [
            // lib.rs:234-243
            "/api/v1/assets/8d1f/tileset.json",
            "/api/v1/assets/8d1f/tiles/root.glb",
            "/api/v1/assets/8d1f/tiles/037.glb",
            "/api/v1/assets/8d1f/tiles/0.b3dm",
            // terrain_api.rs:55-56, terrain_rgb.rs:29
            "/api/v1/terrain/layer.json",
            "/api/v1/terrain/12/2200/1400",
            "/api/v1/terrain/rgb/12/2200/1400",
            // analysis_tiles::analysis_tile_routes
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png",
            "/api/v1/analysis/xyz/slope/12/2132/1493.png",
            // premium_routes.rs:487-491
            "/api/v1/tiles/sources",
            "/api/v1/tiles/styles",
            "/api/v1/tiles/layers",
            "/api/v1/tiles/basemap/tilejson",
            // ion_compat.rs:91-94
            "/v1/assets",
            "/v1/assets/42",
            "/v1/assets/42/endpoint",
            "/v1/tokens",
        ] {
            assert!(is_public_read(&Method::GET, path), "GET {path}");
        }
    }

    /// The shapes the old substring matcher let through. Each of these would
    /// have been an anonymous read of an authenticated route.
    #[test]
    fn crafted_paths_no_longer_ride_the_exemption() {
        for path in [
            // "/tiles/" under an authenticated prefix
            "/api/v1/users/me/tiles/",
            "/api/v1/users/me/tiles/1",
            "/api/v1/orgs/acme/tiles/x",
            "/api/v1/admin/stats/tiles/x",
            // the whole API mounted under a second prefix, the alias shape
            "/tiles/v1/catalog",
            "/tiles/v1/users/me",
            "/tiles/v1/assets/8d1f/tiles/0.b3dm",
            "/tiles/v1/terrain/layer.json",
            // a crafted suffix or segment named like the public ones
            "/api/v1/admin/stats/tileset.json",
            "/api/v1/portal/items/tileset.json",
            "/api/v1/catalog/tileset.json",
            // nested tile paths: the route captures one segment and the tilers
            // never emit these, so the matcher agrees with the router instead of
            // exempting a shape that can only 404
            "/api/v1/assets/8d1f/tiles/0/0/0.b3dm",
            "/api/v1/assets/8d1f/tiles/a/b",
            // near-misses on the anchored prefixes
            "/api/v1/assets",
            "/api/v1/assets/8d1f",
            "/api/v1/assets/8d1f/annotations",
            "/api/v1/terrain",
            "/api/v1/terrain-analysis/operations",
            "/v1/assetsandmore",
            "/v1/tokens/42",
            // not anchored at the root
            "/proxy/api/v1/terrain/layer.json",
            "/proxy/api/v1/assets/8d1f/tileset.json",
        ] {
            assert!(!is_public_read(&Method::GET, path), "GET {path}");
        }
    }

    /// A query string never reaches this function (axum hands it the path only),
    /// but assert it anyway so a caller that passes a full URI cannot open a hole.
    #[test]
    fn query_strings_do_not_make_a_path_public() {
        for path in [
            "/api/v1/catalog?x=/tileset.json",
            "/api/v1/catalog?x=/tiles/",
            "/api/v1/users/me?file=tileset.json",
        ] {
            assert!(!is_public_read(&Method::GET, path), "GET {path}");
        }
    }

    /// Writes never ride the exemption, including on the public-shaped paths.
    #[test]
    fn writes_on_public_shaped_paths_stay_gated() {
        for path in [
            "/api/v1/assets/8d1f/tiles/0.b3dm",
            "/api/v1/assets/8d1f/tileset.json",
            "/api/v1/terrain/12/2200/1400",
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png",
            "/api/v1/tiles/sources",
            "/v1/assets/42",
        ] {
            for method in [
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::PATCH,
                Method::HEAD,
                Method::OPTIONS,
            ] {
                assert!(!is_public_read(&method, path), "{method} {path}");
            }
        }
    }

    // ── which routes a key reaches ───────────────────────────────────────────

    #[test]
    fn each_permission_reaches_the_routes_it_names() {
        for (method, path, permission) in [
            (Method::GET, "/api/v1/catalog", Permission::Read),
            (
                Method::GET,
                "/api/v1/catalog/opentopography",
                Permission::Read,
            ),
            (Method::GET, "/api/v1/stac/collections", Permission::Read),
            (Method::GET, "/api/v1/cog/datasets", Permission::Read),
            (
                Method::GET,
                "/api/v1/cog/datasets/ramp/window",
                Permission::Read,
            ),
            (Method::GET, "/api/v1/features/query", Permission::Read),
            (Method::GET, "/api/v1/geocoding/search", Permission::Read),
            (Method::GET, "/api/v1/elevation/point", Permission::Terrain),
            (
                Method::GET,
                "/api/v1/terrain-analysis/operations",
                Permission::Terrain,
            ),
            (
                Method::POST,
                "/api/v1/analysis/viewshed",
                Permission::Analytics,
            ),
            (
                Method::POST,
                "/api/v1/geostatistics/interpolate",
                Permission::Analytics,
            ),
            (
                Method::POST,
                "/api/v1/geoprocessing/run",
                Permission::Analytics,
            ),
            (Method::GET, "/api/v1/static-map/render", Permission::Export),
            (
                Method::POST,
                "/api/v1/static-map/render",
                Permission::Export,
            ),
            (
                Method::GET,
                "/api/v1/analysis/export/hillshade",
                Permission::Export,
            ),
        ] {
            assert_eq!(
                route_access(&method, path),
                RouteAccess::Needs(permission),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn a_key_reads_its_own_usage_without_a_permission() {
        assert_eq!(
            route_access(&Method::GET, "/api/v1/api-keys/usage"),
            RouteAccess::AnyKey
        );
    }

    /// Key management, admin surfaces, and the routes whose handler scopes its
    /// answer to a platform user. A key reaching any of these would either
    /// escalate or 401 inside the handler.
    #[test]
    fn no_key_reaches_an_admin_surface_or_a_user_scoped_route() {
        for path in [
            "/api/v1/api-keys",
            "/api/v1/api-keys/8d1f",
            "/api/v1/api-keys/8d1f/revoke",
            "/api/v1/admin/stats",
            "/api/v1/admin/users",
            "/api/v1/admin/users/8d1f/role",
            "/api/v1/orgs",
            "/api/v1/tiles/cache/stats",
            "/api/v1/plugins/registry",
            "/api/v1/users/me",
            "/api/v1/assets",
            "/api/v1/assets/8d1f",
            "/api/v1/assets/8d1f/jobs",
            "/api/v1/exports",
            "/api/v1/exports/download/8d1f",
            "/api/v1/portal/items",
            "/api/v1/tilesets",
            "/v1/assets",
            "/v1/tokens",
        ] {
            for method in [Method::GET, Method::POST, Method::DELETE, Method::PUT] {
                assert_eq!(
                    route_access(&method, path),
                    RouteAccess::None,
                    "{method} {path}"
                );
            }
        }
    }

    /// The classes above are GET or POST; nothing else in them is a key route,
    /// so a write cannot ride a read class.
    #[test]
    fn a_write_never_rides_a_key_route_class() {
        for path in [
            "/api/v1/catalog",
            "/api/v1/stac/search",
            "/api/v1/cog/datasets",
            "/api/v1/features/layers",
            "/api/v1/geocoding/search",
            "/api/v1/analysis/export/slope",
            "/api/v1/api-keys/usage",
        ] {
            for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
                assert_eq!(
                    route_access(&method, path),
                    RouteAccess::None,
                    "{method} {path}"
                );
            }
        }
        for method in [Method::PUT, Method::DELETE, Method::PATCH] {
            assert_eq!(
                route_access(&method, "/api/v1/static-map/render"),
                RouteAccess::None,
                "{method}"
            );
        }
    }

    /// Matching is anchored at the root on whole segments, so a crafted path
    /// cannot borrow a class it is only named like.
    #[test]
    fn crafted_paths_reach_no_class() {
        for path in [
            "/proxy/api/v1/catalog",
            "/api/v1/admin/catalog",
            "/api/v1/users/me/catalog",
            "/api/v1/catalogue",
            "/api/v1/staccato",
            "/api/v1/cognition/datasets",
            "/api/v1/terrain-analysis-admin/x",
            "/api/v1/api-keys/usage/all",
            "/api/v1",
            "/",
        ] {
            assert_eq!(
                route_access(&Method::GET, path),
                RouteAccess::None,
                "GET {path}"
            );
        }
    }

    /// Public tile reads are decided before any key is looked at, so they need
    /// no class of their own.
    #[test]
    fn public_reads_need_no_key_class() {
        for path in [
            "/api/v1/terrain/layer.json",
            "/api/v1/assets/8d1f/tileset.json",
            "/api/v1/tiles/sources",
        ] {
            assert!(is_public_read(&Method::GET, path), "{path}");
            assert_eq!(
                route_access(&Method::GET, path),
                RouteAccess::None,
                "GET {path}"
            );
        }
    }

    #[test]
    fn a_missing_or_empty_api_key_header_is_no_credential() {
        assert_eq!(presented_api_key(&HeaderMap::new()), None);

        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, "".parse().unwrap());
        assert_eq!(presented_api_key(&headers), None);

        headers.insert(API_KEY_HEADER, "ttk_abc".parse().unwrap());
        assert_eq!(presented_api_key(&headers), Some("ttk_abc"));
    }

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn subprotocol(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Sec-WebSocket-Protocol", value.parse().unwrap());
        headers
    }

    #[test]
    fn subprotocol_token_is_only_read_on_the_realtime_path() {
        let headers = subprotocol("bearer, jwt-here");
        assert_eq!(
            request_token(&headers, &uri("/api/v1/realtime/room-1")),
            Some("jwt-here")
        );
        // anywhere else a subprotocol offer is not a credential
        assert_eq!(request_token(&headers, &uri("/api/v1/portal/items")), None);
    }

    #[test]
    fn malformed_subprotocol_offers_carry_no_token() {
        let realtime = uri("/api/v1/realtime/room-1");
        // marker without a token
        assert_eq!(request_token(&subprotocol("bearer"), &realtime), None);
        assert_eq!(request_token(&subprotocol("bearer, "), &realtime), None);
        // a bare token with no marker, and an unrelated subprotocol
        assert_eq!(request_token(&subprotocol("jwt-here"), &realtime), None);
        assert_eq!(request_token(&subprotocol("graphql-ws"), &realtime), None);
        // the marker has to come first
        assert_eq!(request_token(&subprotocol("jwt, bearer"), &realtime), None);
        assert_eq!(request_token(&HeaderMap::new(), &realtime), None);
    }

    #[test]
    fn query_string_is_never_a_credential() {
        // the realtime handshake used to accept ?token=; it must not any more
        let headers = HeaderMap::new();
        assert_eq!(
            request_token(&headers, &uri("/api/v1/realtime/room-1?token=abc")),
            None
        );
        assert_eq!(
            request_token(&headers, &uri("/api/v1/portal/items?token=abc")),
            None
        );
    }

    #[test]
    fn header_token_wins_over_the_subprotocol() {
        let mut headers = subprotocol("bearer, from-subprotocol");
        headers.insert("Authorization", "Bearer from-header".parse().unwrap());
        assert_eq!(
            request_token(&headers, &uri("/api/v1/realtime/room-1")),
            Some("from-header")
        );
    }

    #[test]
    fn non_bearer_header_is_no_token() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Basic dXNlcjpwdw==".parse().unwrap());
        assert_eq!(request_token(&headers, &uri("/api/v1/portal/items")), None);
    }
}
