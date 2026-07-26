//! User & organization management with authentication.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{Json, Response},
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::{Arc, LazyLock};
use uuid::Uuid;

use crate::AppState;
use crate::auth::Claims;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub role: UserRole,
    pub org_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Editor,
    Viewer,
}

impl std::str::FromStr for UserRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "admin" => Ok(UserRole::Admin),
            "editor" => Ok(UserRole::Editor),
            "viewer" => Ok(UserRole::Viewer),
            other => Err(format!(
                "unknown role '{other}' (expected admin, editor, or viewer)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub max_storage_bytes: u64,
    pub max_assets: u32,
}

#[derive(Debug, Deserialize)]
pub struct SignupRequest {
    pub email: String,
    pub password: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateOrgRequest {
    pub name: String,
    pub max_storage_bytes: Option<u64>,
    pub max_assets: Option<u32>,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hashing cannot fail with default params")
        .to_string()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// old salted-HMAC hashes look like `<hex-salt>:<hex-mac>`; argon2id hashes are
// PHC strings starting with `$argon2`. login migrates the former on success.
fn is_legacy_hash(hash: &str) -> bool {
    !hash.starts_with("$argon2")
}

fn verify_legacy_password(password: &str, hash: &str) -> bool {
    let parts: Vec<&str> = hash.split(':').collect();
    if parts.len() != 2 {
        return false;
    }
    let salt = from_hex(parts[0]);
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(password.as_bytes());
    let result = mac.finalize().into_bytes();
    to_hex(&result) == parts[1]
}

// verified against when the email is unknown so login latency does not reveal
// whether an account exists (matches collecta).
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| hash_password("no-such-user"));

fn jwt_secret() -> String {
    // the serve path refuses to start without TILETOPIA_JWT_SECRET (see
    // auth::startup_check), so a missing secret only happens in tests / embedded
    // use. fall back to a random per-process secret, never a known constant, so
    // tokens can never be forged with a published value.
    static EPHEMERAL: LazyLock<String> = LazyLock::new(|| {
        let bytes: [u8; 32] = rand::random();
        to_hex(&bytes)
    });
    std::env::var("TILETOPIA_JWT_SECRET").unwrap_or_else(|_| EPHEMERAL.clone())
}

fn create_jwt(user: &User) -> Result<String, StatusCode> {
    let role = serde_json::to_string(&user.role)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    let claims = Claims {
        sub: user.id.to_string(),
        exp: (Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
        role,
    };
    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret().as_bytes()),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub fn extract_claims(request: &Request) -> Result<Claims, StatusCode> {
    claims_from_headers(request.headers())
}

/// Same check as [`extract_claims`] against bare headers, for handlers that
/// take a body extractor and so cannot take the whole `Request`.
pub fn claims_from_headers(headers: &axum::http::HeaderMap) -> Result<Claims, StatusCode> {
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(jwt_secret().as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| StatusCode::UNAUTHORIZED)?;

    Ok(data.claims)
}

/// Middleware that requires the user to have Admin role.
pub async fn require_admin(request: Request, next: Next) -> Result<Response, StatusCode> {
    let claims = extract_claims(&request)?;
    if claims.role != "admin" {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Middleware that requires the user to have Editor or Admin role. Same JWT
/// claims check as `require_admin`, widened to the Edit permission tier.
pub async fn require_editor(request: Request, next: Next) -> Result<Response, StatusCode> {
    let claims = extract_claims(&request)?;
    if claims.role != "editor" && claims.role != "admin" {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

pub async fn signup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SignupRequest>,
) -> Result<(StatusCode, Json<AuthResponse>), StatusCode> {
    if req.email.is_empty() || req.password.is_empty() || req.name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Check if user already exists
    if let Ok(Some(_)) = state.db.get_user_by_email(&req.email).await {
        return Err(StatusCode::CONFLICT);
    }

    let password_hash = hash_password(&req.password);
    let user = User {
        id: Uuid::new_v4(),
        email: req.email,
        name: req.name,
        role: UserRole::Viewer,
        org_id: None,
        created_at: Utc::now(),
        last_login: Some(Utc::now()),
    };

    state
        .db
        .create_user(&user, &password_hash)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let token = create_jwt(&user)?;
    Ok((StatusCode::CREATED, Json(AuthResponse { token, user })))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, StatusCode> {
    let Some((mut user, password_hash)) = state
        .db
        .get_user_by_email(&req.email)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        // spend the same work on an unknown email so timing doesn't leak it
        let _ = verify_password(&req.password, &DUMMY_HASH);
        return Err(StatusCode::UNAUTHORIZED);
    };

    let ok = if is_legacy_hash(&password_hash) {
        if verify_legacy_password(&req.password, &password_hash) {
            // transparently upgrade old salted-HMAC hashes to argon2id
            let new_hash = hash_password(&req.password);
            state
                .db
                .set_password_hash(user.id, &new_hash)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            true
        } else {
            false
        }
    } else {
        verify_password(&req.password, &password_hash)
    };
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }

    user.last_login = Some(Utc::now());
    let _ = state.db.update_user(&user).await;

    let token = create_jwt(&user)?;
    Ok(Json(AuthResponse { token, user }))
}

pub async fn get_me(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<User>, StatusCode> {
    let claims = extract_claims(&request)?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let user = state
        .db
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(user))
}

pub async fn update_me(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<User>, StatusCode> {
    let claims = extract_claims(&request)?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Read body manually since we already consumed headers for claims
    let body = axum::body::to_bytes(request.into_body(), 1024 * 64)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let req: UpdateUserRequest =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut user = state
        .db
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    user.name = req.name;
    state
        .db
        .update_user(&user)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(user))
}

pub async fn list_orgs(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Organization>>, StatusCode> {
    let orgs = state
        .db
        .list_orgs()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(orgs))
}

pub async fn create_org(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateOrgRequest>,
) -> Result<(StatusCode, Json<Organization>), StatusCode> {
    let org = Organization {
        id: Uuid::new_v4(),
        name: req.name,
        created_at: Utc::now(),
        max_storage_bytes: req.max_storage_bytes.unwrap_or(10_737_418_240),
        max_assets: req.max_assets.unwrap_or(100),
    };

    state
        .db
        .create_org(&org)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(org)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_roundtrip() {
        let hash = hash_password("correct horse battery staple");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn legacy_hash_detected_and_verified() {
        // produce an old salted-HMAC hash the way the pre-argon2 code did
        let salt = [3u8; 16];
        let mut mac = Hmac::<Sha256>::new_from_slice(&salt).unwrap();
        mac.update(b"hunter2");
        let legacy = format!("{}:{}", to_hex(&salt), to_hex(&mac.finalize().into_bytes()));

        assert!(is_legacy_hash(&legacy));
        assert!(!is_legacy_hash(&hash_password("hunter2")));
        assert!(verify_legacy_password("hunter2", &legacy));
        assert!(!verify_legacy_password("nope", &legacy));
    }

    #[test]
    fn role_from_str() {
        assert_eq!("admin".parse::<UserRole>().unwrap(), UserRole::Admin);
        assert_eq!("EDITOR".parse::<UserRole>().unwrap(), UserRole::Editor);
        assert_eq!("viewer".parse::<UserRole>().unwrap(), UserRole::Viewer);
        assert!("root".parse::<UserRole>().is_err());
    }
}
