//! HS256 service-token JWT middleware for the internal REST API.
//!
//! Validates `Authorization: Bearer <jwt>` against the shared
//! `SERVICE_TOKEN_SECRET` env var. Health/readiness/metrics bypass auth.
//! In `DEV_MODE=1` with an unset secret the middleware is a no-op.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use tonic::{Request as TonicRequest, Status};

const SKIP_PATHS: &[&str] = &["/healthz", "/readyz", "/metrics"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
}

/// Resolve the shared secret from `SERVICE_TOKEN_SECRET`. Returns `Ok(None)` in
/// DEV_MODE with no secret configured (caller bypasses auth). Fatal at boot
/// in prod.
pub fn secret_from_env() -> Option<String> {
    match std::env::var("SERVICE_TOKEN_SECRET") {
        Ok(s) if !s.is_empty() => Some(s),
        _ => {
            if std::env::var("DEV_MODE").as_deref() == Ok("1") {
                eprintln!(
                    "warn: SERVICE_TOKEN_SECRET unset and DEV_MODE=1; service-token auth disabled (NOT FOR PRODUCTION)"
                );
                None
            } else {
                eprintln!(
                    "FATAL: SERVICE_TOKEN_SECRET not set and DEV_MODE!=1; refusing to start in production mode"
                );
                std::process::exit(1);
            }
        }
    }
}

/// axum middleware function: validates the Bearer token, calling next on
/// success and returning 401 on failure. Bypassed when `secret` is `None`.
pub async fn require_token(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if SKIP_PATHS.contains(&path.as_str()) {
        return next.run(req).await;
    }
    // The secret is stashed in a request extension by `with_secret`; when
    // absent (DEV_MODE bypass) we pass through.
    let secret = req.extensions().get::<SharedSecret>().map(|s| s.0.clone());
    let Some(secret) = secret else {
        return next.run(req).await;
    };
    let auth = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    if token.is_empty() {
        return unauthorized("missing or malformed Authorization header");
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    match decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) {
        Ok(_) => next.run(req).await,
        Err(e) => unauthorized(format!("invalid token: {e}")),
    }
}

/// Extension type carrying the active shared secret through the request.
#[derive(Clone)]
pub struct SharedSecret(pub String);

/// Check the service-token JWT on a gRPC request. The interceptor is
/// responsible for bypassing health/reflection methods before calling this.
/// Returns `Ok(())` when bypassed (no secret configured) or a valid token is
/// present.
#[allow(clippy::result_large_err)]
pub fn check_grpc<T>(req: &TonicRequest<T>, secret: Option<&str>) -> Result<(), Status> {
    let Some(secret) = secret else {
        return Ok(());
    };
    let token = req
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(|t| t.to_string()))
        .or_else(|| {
            req.metadata()
                .get("x-service-token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });
    let Some(token) = token else {
        return Err(Status::unauthenticated(
            "missing or malformed Authorization metadata",
        ));
    };
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    match decode::<Claims>(
        token.as_str(),
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) {
        Ok(_) => Ok(()),
        Err(e) => Err(Status::unauthenticated(format!("invalid token: {e}"))),
    }
}

/// Issue a 24h HS256 JWT for the named service. Used by internal callers when
/// invoking other internal REST endpoints. TODO(P2.12): wire all internal REST
/// clients to attach this token.
pub fn issue(service_name: &str, secret: &str) -> anyhow::Result<String> {
    if secret.is_empty() {
        anyhow::bail!("authtoken: secret is required to issue a token");
    }
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub: service_name.to_string(),
        iat: now,
        exp: now + 24 * 60 * 60,
    };
    Ok(encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

fn unauthorized(msg: impl Into<String>) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": {"code": "unauthorized", "message": msg.into()}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Extension;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn ok_handler() -> axum::Router {
        axum::Router::new()
            .route("/v1/postings", get(|| async { "ok" }))
            .route("/healthz", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(require_token))
    }

    async fn run(req: Request<Body>, secret: Option<String>) -> (StatusCode, String) {
        let mut router = ok_handler();
        if let Some(s) = secret {
            router = router.layer(Extension(SharedSecret(s)));
        }
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn valid_token_passes() {
        let secret = "s3cret";
        let tok = issue("ledger-accounting", secret).unwrap();
        let req = Request::builder()
            .uri("/v1/postings")
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let (s, _) = run(req, Some(secret.to_string())).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_returns_401() {
        let req = Request::builder()
            .uri("/v1/postings")
            .body(Body::empty())
            .unwrap();
        let (s, body) = run(req, Some("s3cret".to_string())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(body.contains("unauthorized"));
    }

    #[tokio::test]
    async fn invalid_token_returns_401() {
        let req = Request::builder()
            .uri("/v1/postings")
            .header("authorization", "Bearer not-a-jwt")
            .body(Body::empty())
            .unwrap();
        let (s, _) = run(req, Some("s3cret".to_string())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_secret_returns_401() {
        let tok = issue("ledger-accounting", "real-secret").unwrap();
        let req = Request::builder()
            .uri("/v1/postings")
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let (s, _) = run(req, Some("different-secret".to_string())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn healthz_bypasses_auth() {
        let req = Request::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let (s, _) = run(req, Some("s3cret".to_string())).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn dev_mode_bypasses_auth() {
        let req = Request::builder()
            .uri("/v1/postings")
            .body(Body::empty())
            .unwrap();
        let (s, _) = run(req, None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[test]
    fn issue_rejects_empty_secret() {
        assert!(issue("svc", "").is_err());
    }
}
