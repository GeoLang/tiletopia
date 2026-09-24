//! User & organization management with authentication.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::{
    Extension,
    extract::{ConnectInfo, Request, State},
    http::{Extensions, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use jsonwebtoken::{EncodingKey, Header};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use uuid::Uuid;

use crate::AppState;
use crate::audit::AuditedResource;
use crate::auth::Claims;
use crate::db::SignupOutcome;

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

impl UserRole {
    /// Role out of a JWT `role` claim, or `None` when the token carries a role
    /// we don't know. Unknown strings are rejected rather than defaulted, so a
    /// typo or a role minted by another service can never fall through to a
    /// tier. Matches ptolemy's `Role::parse`.
    ///
    /// Exact match on purpose: [`FromStr`](std::str::FromStr) above is the
    /// lenient path for human input (the CLI's set-role), a claim is machine
    /// written and always the lowercase serde name.
    pub fn from_claim(s: &str) -> Option<UserRole> {
        match s {
            "admin" => Some(UserRole::Admin),
            "editor" => Some(UserRole::Editor),
            "viewer" => Some(UserRole::Viewer),
            _ => None,
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

pub const MAX_USERS_ENV: &str = "TILETOPIA_MAX_USERS";
pub const SIGNUPS_PER_HOUR_ENV: &str = "TILETOPIA_SIGNUPS_PER_HOUR";
pub const LOGIN_LOCKOUT_FAILURES_ENV: &str = "TILETOPIA_LOGIN_LOCKOUT_FAILURES";
pub const LOGIN_LOCKOUT_MINUTES_ENV: &str = "TILETOPIA_LOGIN_LOCKOUT_MINUTES";
pub const TRUSTED_PROXY_HOPS_ENV: &str = "TILETOPIA_TRUSTED_PROXY_HOPS";

const DEFAULT_LOGIN_LOCKOUT_FAILURES: u32 = 10;
const DEFAULT_LOGIN_LOCKOUT_MINUTES: u32 = 15;
const SIGNUP_RATE_WINDOW: chrono::Duration = chrono::Duration::hours(1);
// a stranger has to fail from this many addresses before the owner's own address is refused
const ADDRESSES_TO_LOCK_AN_ACCOUNT: u32 = 20;
pub const PASSWORD_HASHES_AT_ONCE: usize = 2;
const UNKNOWN_CLIENT_ADDRESS: &str = "unknown";
const FORWARDED_FOR: &str = "x-forwarded-for";

pub static PASSWORD_HASH_SLOTS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(PASSWORD_HASHES_AT_ONCE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountLimits {
    pub max_users: Option<u64>,
    pub signups_per_hour: Option<u64>,
    pub login_lockout_failures: Option<u32>,
    pub account_lockout_failures: Option<u32>,
    pub login_lockout: chrono::Duration,
    pub trusted_proxy_hops: usize,
}

impl Default for AccountLimits {
    fn default() -> Self {
        Self {
            max_users: None,
            signups_per_hour: None,
            login_lockout_failures: Some(DEFAULT_LOGIN_LOCKOUT_FAILURES),
            account_lockout_failures: Some(
                DEFAULT_LOGIN_LOCKOUT_FAILURES * ADDRESSES_TO_LOCK_AN_ACCOUNT,
            ),
            login_lockout: chrono::Duration::minutes(DEFAULT_LOGIN_LOCKOUT_MINUTES.into()),
            trusted_proxy_hops: 0,
        }
    }
}

impl AccountLimits {
    // a typo must not silently open signup
    pub fn from_env() -> Result<Self, String> {
        Self::resolve(|name| std::env::var(name).ok())
    }

    pub fn resolve(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let failures = count_from(
            LOGIN_LOCKOUT_FAILURES_ENV,
            lookup(LOGIN_LOCKOUT_FAILURES_ENV),
        )?
        .unwrap_or(DEFAULT_LOGIN_LOCKOUT_FAILURES.into());
        let minutes = count_from(LOGIN_LOCKOUT_MINUTES_ENV, lookup(LOGIN_LOCKOUT_MINUTES_ENV))?;
        let trusted_proxy_hops =
            count_from(TRUSTED_PROXY_HOPS_ENV, lookup(TRUSTED_PROXY_HOPS_ENV))?.unwrap_or(0);
        let login_lockout_failures = (failures > 0)
            .then(|| u32::try_from(failures))
            .transpose()
            .map_err(|_| format!("{LOGIN_LOCKOUT_FAILURES_ENV}={failures} is too large"))?;
        Ok(Self {
            max_users: count_from(MAX_USERS_ENV, lookup(MAX_USERS_ENV))?,
            signups_per_hour: count_from(SIGNUPS_PER_HOUR_ENV, lookup(SIGNUPS_PER_HOUR_ENV))?,
            // zero failures would lock before the first try
            login_lockout_failures,
            account_lockout_failures: login_lockout_failures
                .map(|failures| failures.saturating_mul(ADDRESSES_TO_LOCK_AN_ACCOUNT)),
            login_lockout: match minutes {
                None => Self::default().login_lockout,
                Some(minutes) => i64::try_from(minutes)
                    .ok()
                    .and_then(chrono::Duration::try_minutes)
                    .ok_or_else(|| format!("{LOGIN_LOCKOUT_MINUTES_ENV}={minutes} is too large"))?,
            },
            trusted_proxy_hops: usize::try_from(trusted_proxy_hops).map_err(|_| {
                format!("{TRUSTED_PROXY_HOPS_ENV}={trusted_proxy_hops} is too large")
            })?,
        })
    }
}

fn count_from(name: &str, raw: Option<String>) -> Result<Option<u64>, String> {
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("{name}={value} is not a whole number, unset it for the default")),
    }
}

#[derive(Debug)]
pub enum AccountError {
    Status(StatusCode),
    Refused(StatusCode, String),
}

impl From<StatusCode> for AccountError {
    fn from(status: StatusCode) -> Self {
        Self::Status(status)
    }
}

impl IntoResponse for AccountError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status) => status.into_response(),
            Self::Refused(status, reason) => {
                (status, Json(serde_json::json!({ "error": reason }))).into_response()
            }
        }
    }
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
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

async fn hash_password_off_runtime(password: String) -> String {
    let _slot = PASSWORD_HASH_SLOTS
        .acquire()
        .await
        .expect("the password hash semaphore is never closed");
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .expect("argon2 hashing cannot panic")
}

async fn verify_password_off_runtime(password: String, hash: Option<String>) -> bool {
    let _slot = PASSWORD_HASH_SLOTS
        .acquire()
        .await
        .expect("the password hash semaphore is never closed");
    tokio::task::spawn_blocking(move || match hash {
        Some(hash) => verify_password(&password, &hash),
        None => verify_password(&password, &DUMMY_HASH),
    })
    .await
    .expect("argon2 verification cannot panic")
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

    claims_from_token(token)
}

/// Validate a bare JWT, for callers that receive the token somewhere other than
/// the `Authorization` header. Same key and validation as
/// [`claims_from_headers`].
pub fn claims_from_token(token: &str) -> Result<Claims, StatusCode> {
    crate::auth::verify_token_with_secret(token, &jwt_secret())
        .map(|authenticated| authenticated.claims)
}

/// Middleware that requires the user to have Admin role.
pub async fn require_admin(request: Request, next: Next) -> Result<Response, StatusCode> {
    let token = crate::auth::request_token(request.headers(), request.uri())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let authenticated = crate::auth::verify_token_with_secret(token, &jwt_secret())?;
    if !authenticated.can_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Middleware that requires the user to have Editor or Admin role. Same JWT
/// claims check as `require_admin`, widened to the Edit permission tier.
pub async fn require_editor(request: Request, next: Next) -> Result<Response, StatusCode> {
    let token = crate::auth::request_token(request.headers(), request.uri())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let authenticated = crate::auth::verify_token_with_secret(token, &jwt_secret())?;
    if !authenticated.can_write() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

pub async fn signup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SignupRequest>,
) -> Result<(StatusCode, Json<AuthResponse>), AccountError> {
    if req.email.is_empty() || req.password.is_empty() || req.name.is_empty() {
        return Err(StatusCode::BAD_REQUEST.into());
    }

    // Check if user already exists
    if let Ok(Some(_)) = state.db.get_user_by_email(&req.email).await {
        return Err(StatusCode::CONFLICT.into());
    }

    let password_hash = hash_password_off_runtime(req.password.clone()).await;
    let user = User {
        id: Uuid::new_v4(),
        email: req.email,
        name: req.name,
        role: UserRole::Viewer,
        org_id: None,
        created_at: Utc::now(),
        last_login: Some(Utc::now()),
    };

    let limits = state.account_limits;
    let outcome = state
        .db
        .create_user_within_signup_limits(
            &user,
            &password_hash,
            limits.max_users,
            limits.signups_per_hour,
            Utc::now() - SIGNUP_RATE_WINDOW,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match outcome {
        SignupOutcome::Created => {}
        SignupOutcome::Full { max_users } => {
            return Err(AccountError::Refused(
                StatusCode::FORBIDDEN,
                format!("signups are closed: this server is full at {max_users} accounts"),
            ));
        }
        SignupOutcome::RateLimited { signups_per_hour } => {
            return Err(AccountError::Refused(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "too many signups: this server takes {signups_per_hour} an hour, try again later"
                ),
            ));
        }
    }

    let token = create_jwt(&user)?;
    Ok((StatusCode::CREATED, Json(AuthResponse { token, user })))
}

// entries left of the one the outermost trusted proxy appended came from the client
pub fn client_address(headers: &HeaderMap, peer: Option<IpAddr>, hops: usize) -> String {
    let forwarded: Vec<&str> = headers
        .get_all(FORWARDED_FOR)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect();
    let appended_by_trusted_proxy = hops
        .checked_sub(1)
        .and_then(|from_right| forwarded.len().checked_sub(from_right + 1))
        .and_then(|index| forwarded[index].parse::<IpAddr>().ok());
    appended_by_trusted_proxy.or(peer).map_or_else(
        || UNKNOWN_CLIENT_ADDRESS.to_string(),
        |address| address.to_string(),
    )
}

fn locked_out(minutes: i64, failures: u32, scope: &str) -> AccountError {
    AccountError::Refused(
        StatusCode::TOO_MANY_REQUESTS,
        format!(
            "this account is locked for up to {minutes} minutes after {failures} failed logins{scope}"
        ),
    )
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    extensions: Extensions,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, AccountError> {
    let Some((mut user, password_hash)) = state
        .db
        .get_user_by_email(&req.email)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        // spend the same work on an unknown email so timing doesn't leak it
        let _ = verify_password_off_runtime(req.password, None).await;
        return Err(StatusCode::UNAUTHORIZED.into());
    };

    let limits = state.account_limits;
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    let address = client_address(&headers, peer, limits.trusted_proxy_hops);
    let minutes = limits.login_lockout.num_minutes();
    // counted before the password is checked, so a burst of guesses cannot all pass
    if let Some(failures) = limits.login_lockout_failures {
        let claimed = state
            .db
            .claim_address_login_attempt(
                user.id,
                &address,
                Utc::now(),
                failures,
                limits.login_lockout,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if !claimed {
            return Err(locked_out(minutes, failures, " from this address"));
        }
    }
    if let Some(failures) = limits.account_lockout_failures {
        let claimed = state
            .db
            .claim_login_attempt(user.id, Utc::now(), failures, limits.login_lockout)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if !claimed {
            return Err(locked_out(minutes, failures, ""));
        }
    }

    let ok = if is_legacy_hash(&password_hash) {
        if verify_legacy_password(&req.password, &password_hash) {
            // transparently upgrade old salted-HMAC hashes to argon2id
            let new_hash = hash_password_off_runtime(req.password.clone()).await;
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
        verify_password_off_runtime(req.password, Some(password_hash)).await
    };
    if !ok {
        return Err(StatusCode::UNAUTHORIZED.into());
    }
    if limits.login_lockout_failures.is_some() {
        state
            .db
            .clear_address_failed_logins(user.id, &address)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    if limits.account_lockout_failures.is_some() {
        state
            .db
            .clear_failed_logins(user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
) -> Result<(StatusCode, Extension<AuditedResource>, Json<Organization>), StatusCode> {
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

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(org.id.to_string())),
        Json(org),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_address_is_the_one_the_outermost_trusted_proxy_appended() {
        let peer: Option<IpAddr> = Some("10.0.1.9".parse().unwrap());
        let mut headers = HeaderMap::new();
        // client-set, then CloudFront's viewer entry, then the edge the ALB saw, then the ALB
        headers.insert(
            FORWARDED_FOR,
            "192.0.2.66, 203.0.113.5, 130.176.1.1, 10.0.0.12"
                .parse()
                .unwrap(),
        );
        assert_eq!(client_address(&headers, peer, 3), "203.0.113.5");
        assert_eq!(client_address(&headers, peer, 0), "10.0.1.9");
        assert_eq!(client_address(&headers, peer, 5), "10.0.1.9");
        assert_eq!(
            client_address(&HeaderMap::new(), None, 3),
            UNKNOWN_CLIENT_ADDRESS
        );
    }

    #[test]
    fn the_account_ceiling_is_a_multiple_of_the_address_limit() {
        let limits = AccountLimits::resolve(|name| {
            match name {
                LOGIN_LOCKOUT_FAILURES_ENV => Some("5"),
                TRUSTED_PROXY_HOPS_ENV => Some("3"),
                _ => None,
            }
            .map(str::to_string)
        })
        .unwrap();
        assert_eq!(limits.login_lockout_failures, Some(5));
        assert_eq!(
            limits.account_lockout_failures,
            Some(5 * ADDRESSES_TO_LOCK_AN_ACCOUNT)
        );
        assert_eq!(limits.trusted_proxy_hops, 3);
    }

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
    fn unset_limits_leave_signup_open_and_keep_a_generous_lockout() {
        let limits = AccountLimits::resolve(|_| None).unwrap();
        assert_eq!(limits, AccountLimits::default());
        assert_eq!(limits.max_users, None);
        assert_eq!(limits.signups_per_hour, None);
        assert_eq!(
            limits.login_lockout_failures,
            Some(DEFAULT_LOGIN_LOCKOUT_FAILURES)
        );
    }

    #[test]
    fn demo_limits_are_read_and_zero_failures_turns_the_lockout_off() {
        let demo = AccountLimits::resolve(|name| {
            match name {
                MAX_USERS_ENV => Some("500"),
                SIGNUPS_PER_HOUR_ENV => Some(" 30 "),
                LOGIN_LOCKOUT_FAILURES_ENV => Some("5"),
                LOGIN_LOCKOUT_MINUTES_ENV => Some("20"),
                _ => None,
            }
            .map(str::to_string)
        })
        .unwrap();
        assert_eq!(demo.max_users, Some(500));
        assert_eq!(demo.signups_per_hour, Some(30));
        assert_eq!(demo.login_lockout_failures, Some(5));
        assert_eq!(demo.login_lockout, chrono::Duration::minutes(20));

        let off = AccountLimits::resolve(|name| {
            (name == LOGIN_LOCKOUT_FAILURES_ENV).then(|| "0".to_string())
        })
        .unwrap();
        assert_eq!(off.login_lockout_failures, None);
    }

    #[test]
    fn a_limit_that_is_not_a_count_refuses_startup() {
        for name in [
            MAX_USERS_ENV,
            SIGNUPS_PER_HOUR_ENV,
            LOGIN_LOCKOUT_FAILURES_ENV,
            LOGIN_LOCKOUT_MINUTES_ENV,
        ] {
            let error = AccountLimits::resolve(|asked| (asked == name).then(|| "5OO".to_string()))
                .unwrap_err();
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn role_from_str() {
        assert_eq!("admin".parse::<UserRole>().unwrap(), UserRole::Admin);
        assert_eq!("EDITOR".parse::<UserRole>().unwrap(), UserRole::Editor);
        assert_eq!("viewer".parse::<UserRole>().unwrap(), UserRole::Viewer);
        assert!("root".parse::<UserRole>().is_err());
    }

    #[test]
    fn role_from_claim_is_exact() {
        assert_eq!(UserRole::from_claim("admin"), Some(UserRole::Admin));
        assert_eq!(UserRole::from_claim("editor"), Some(UserRole::Editor));
        assert_eq!(UserRole::from_claim("viewer"), Some(UserRole::Viewer));
        // anything else is not a tier, including near misses on a real one
        for role in [
            "",
            "root",
            "superuser",
            "Admin",
            "ADMIN",
            " admin",
            "admin ",
        ] {
            assert_eq!(UserRole::from_claim(role), None, "{role}");
        }
    }
}
