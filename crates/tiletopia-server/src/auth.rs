//! JWT authentication middleware.

use axum::{
    extract::Request,
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

/// Shortest HS256 secret we accept, matching ptolemy and collecta.
pub const MIN_SECRET_LEN: usize = 32;

/// JWT claims.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    pub role: String,
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

/// Whether this request is an anonymous tile-DATA read. CesiumJS and deck.gl
/// fetch these with no Authorization header, so they stay open.
///
/// GET only, so the mutating `POST /v1/assets` and `POST /v1/tokens` stay
/// behind auth. Anything not listed here is protected by default.
pub fn is_public_read(method: &Method, path: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    path.contains("/tileset.json")
        || path.contains("/tiles/")
        // quantized-mesh terrain: layer.json plus the {z}/{x}/{y} tiles it
        // advertises. same data tier as tileset.json. the trailing slash keeps
        // the /api/v1/terrain-analysis/ compute routes gated.
        || path.starts_with("/api/v1/terrain/")
        // the /v1/* entries are the Ion-compat read routes
        || path == "/v1/assets"
        || path == "/v1/tokens"
        || path.starts_with("/v1/assets/")
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

    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let _claims = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(is_public_read(
            &Method::GET,
            "/api/v1/assets/abc/tiles/0/0/0.b3dm"
        ));
        assert!(is_public_read(&Method::GET, "/v1/assets"));
        assert!(is_public_read(&Method::GET, "/v1/assets/1/endpoint"));
    }

    #[test]
    fn terrain_reads_are_public() {
        assert!(is_public_read(&Method::GET, "/api/v1/terrain/layer.json"));
        assert!(is_public_read(&Method::GET, "/api/v1/terrain/12/2200/1400"));
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
        assert!(!is_public_read(&Method::GET, "/api/v1/portal/items"));
    }
}
