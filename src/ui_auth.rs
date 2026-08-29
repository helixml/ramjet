//! Cookie-backed authentication for the machine-view control surface.
//!
//! The operator enters a dedicated token once. Ramjet exchanges it for a
//! signed, `HttpOnly` browser cookie, so the secret is not retained in
//! JavaScript or reused as an engine credential.

use std::{
    env,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::session::hmac_sha256;

const TOKEN_ENV: &str = "RJ_UI_AUTH_TOKEN";
const COOKIE_NAME: &str = "ramjet_ui_session";
const SESSION_SECONDS: u64 = 30 * 24 * 60 * 60;
const MIN_TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 256;
const SESSION_DOMAIN: &[u8] = b"ramjet-ui-session-v1\0";

#[derive(Clone)]
pub struct UiAuth {
    token: Arc<[u8]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    token: String,
}

#[derive(Serialize)]
struct SessionStatus {
    authenticated: bool,
}

impl UiAuth {
    /// Loads the optional UI authority. An unset variable preserves the
    /// historical unauthenticated, observation-only machine-view deployment.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured token is not valid UTF-8, is
    /// outside the bounded length, or contains whitespace.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(token) = env::var_os(TOKEN_ENV) else {
            return Ok(None);
        };
        let token = token
            .into_string()
            .map_err(|_| anyhow::anyhow!("{TOKEN_ENV} must be valid UTF-8"))?;
        Self::new(token).map(Some)
    }

    fn new(token: String) -> anyhow::Result<Self> {
        ensure!(
            (MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&token.len()),
            "{TOKEN_ENV} must contain {MIN_TOKEN_BYTES}-{MAX_TOKEN_BYTES} bytes"
        );
        ensure!(
            !token.chars().any(char::is_whitespace),
            "{TOKEN_ENV} must not contain whitespace"
        );
        Ok(Self {
            token: Arc::from(token.into_bytes()),
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/api/ui/session", get(Self::session))
            .route("/api/ui/login", post(Self::login))
            .route("/api/ui/logout", post(Self::logout))
            .with_state(self.clone())
    }

    /// Adds the session gate to API routes. Static UI assets stay public so
    /// the browser can render the login screen.
    pub fn protect(&self, router: Router) -> Router {
        router.route_layer(middleware::from_fn_with_state(
            self.clone(),
            require_session,
        ))
    }

    fn issue_session(&self, expires: u64) -> String {
        let expiry = expires.to_string();
        let mac = hmac_sha256(&self.token, &[SESSION_DOMAIN, expiry.as_bytes()]);
        format!("v1.{expiry}.{}", hex(&mac))
    }

    fn valid_session(&self, headers: &HeaderMap, now: u64) -> bool {
        let Some(cookie) = cookie(headers, COOKIE_NAME) else {
            return false;
        };
        self.valid_session_value(cookie, now)
    }

    fn valid_session_value(&self, value: &str, now: u64) -> bool {
        let mut parts = value.split('.');
        if parts.next() != Some("v1") {
            return false;
        }
        let (Some(expiry), Some(mac), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let Ok(expiry_value) = expiry.parse::<u64>() else {
            return false;
        };
        if expiry_value < now || expiry_value > now.saturating_add(SESSION_SECONDS) {
            return false;
        }
        let expected = hex(&hmac_sha256(
            &self.token,
            &[SESSION_DOMAIN, expiry.as_bytes()],
        ));
        constant_time_equal(expected.as_bytes(), mac.as_bytes())
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn session(State(auth): State<Self>, headers: HeaderMap) -> Json<SessionStatus> {
        Json(SessionStatus {
            authenticated: auth.valid_session(&headers, unix_seconds()),
        })
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn login(State(auth): State<Self>, Json(request): Json<LoginRequest>) -> Response {
        if request.token.len() > MAX_TOKEN_BYTES
            || !constant_time_equal(auth.token.as_ref(), request.token.as_bytes())
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid login token" })),
            )
                .into_response();
        }
        let expires = unix_seconds().saturating_add(SESSION_SECONDS);
        let value = auth.issue_session(expires);
        let cookie = format!(
            "{COOKIE_NAME}={value}; Path=/; Max-Age={SESSION_SECONDS}; HttpOnly; SameSite=Strict"
        );
        response_with_cookie(StatusCode::OK, true, &cookie)
    }

    #[allow(clippy::unused_async)] // Axum handlers return futures.
    async fn logout() -> Response {
        response_with_cookie(
            StatusCode::OK,
            false,
            &format!("{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"),
        )
    }
}

async fn require_session(
    State(auth): State<UiAuth>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    if auth.valid_session(&headers, unix_seconds()) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "login required" })),
        )
            .into_response()
    }
}

fn response_with_cookie(status: StatusCode, authenticated: bool, cookie: &str) -> Response {
    let mut response = (status, Json(SessionStatus { authenticated })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(cookie).expect("session cookie contains only fixed ASCII"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .find_map(|part| part.strip_prefix(name)?.strip_prefix('='))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")
        .map_or(0, |duration| duration.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn constant_time_equal(expected: &[u8], actual: &[u8]) -> bool {
    let mut difference = expected.len() ^ actual.len();
    for index in 0..expected.len().max(actual.len()) {
        difference |= usize::from(
            expected.get(index).copied().unwrap_or(0) ^ actual.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> UiAuth {
        UiAuth::new("a-very-long-dedicated-ui-token-123456789".to_owned()).unwrap()
    }

    #[test]
    fn sessions_are_signed_expiring_and_token_bound() {
        let authority = auth();
        let session = authority.issue_session(10_000);
        assert!(authority.valid_session_value(&session, 9_999));
        assert!(!authority.valid_session_value(&session, 10_001));

        let other = UiAuth::new("another-long-dedicated-ui-token-987654321".to_owned()).unwrap();
        assert!(!other.valid_session_value(&session, 9_999));
    }

    #[test]
    fn sessions_reject_tampering_and_implausible_expiry() {
        let authority = auth();
        let session = authority.issue_session(10_000);
        let tampered = format!("{session}0");
        assert!(!authority.valid_session_value(&tampered, 9_999));
        let implausible = SESSION_SECONDS + 2;
        assert!(!authority.valid_session_value(&authority.issue_session(implausible), 1));
    }

    #[test]
    fn cookie_parser_handles_multiple_values() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; ramjet_ui_session=signed.value"),
        );
        assert_eq!(cookie(&headers, COOKIE_NAME), Some("signed.value"));
    }

    #[test]
    fn token_policy_rejects_short_or_whitespace_values() {
        assert!(UiAuth::new("short".to_owned()).is_err());
        assert!(UiAuth::new(format!("{} x", "a".repeat(32))).is_err());
    }
}
