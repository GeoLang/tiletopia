//! JWT authentication middleware.

use axum::{
    extract::Request,
    http::{HeaderMap, Method, StatusCode, Uri},
    middleware::Next,
    response::Response,
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

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

/// Auth middleware — extracts and validates JWT from Authorization header.
/// If `TILETOPIA_JWT_SECRET` is not set, auth is disabled (open access);
/// [`startup_check`] is what keeps the serve path from reaching that state by
/// accident.
pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
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
