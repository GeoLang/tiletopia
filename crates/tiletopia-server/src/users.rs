//! User & organization management with authentication.

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
use std::sync::Arc;
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
    let salt: [u8; 16] = rand::random();
    let mut mac = Hmac::<Sha256>::new_from_slice(&salt).unwrap();
    mac.update(password.as_bytes());
    let result = mac.finalize().into_bytes();
    format!("{}:{}", to_hex(&salt), to_hex(&result))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
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

fn jwt_secret() -> String {
    std::env::var("TILETOPIA_JWT_SECRET").unwrap_or_else(|_| "dev-secret-change-me".into())
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
    let auth_header = request
        .headers()
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
    let (mut user, password_hash) = state
        .db
        .get_user_by_email(&req.email)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if !verify_password(&req.password, &password_hash) {
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
