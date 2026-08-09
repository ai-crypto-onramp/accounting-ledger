use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::account::{Account, AccountResponse, CreateAccountRequest};
use crate::chart::{CHART, GENESIS_HASH};
use crate::posting::{PostingRecord, PostingRequest};
use crate::store::{LedgerPage, Store};

pub fn router(store: Store) -> axum::Router {
    axum::Router::new()
        .merge(read_router(store.clone()))
        .merge(write_router(store))
}

pub fn read_router(store: Store) -> axum::Router {
    axum::Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/readyz", axum::routing::get(readyz))
        .route(
            "/v1/chart-of-accounts",
            axum::routing::get(chart_of_accounts),
        )
        .route("/v1/accounts", axum::routing::get(list_accounts))
        .route(
            "/v1/accounts/:id/balance",
            axum::routing::get(account_balance),
        )
        .route(
            "/v1/accounts/:id/ledger",
            axum::routing::get(account_ledger),
        )
        .route("/v1/postings", axum::routing::get(list_postings))
        .route("/v1/postings/:id", axum::routing::get(get_posting))
        .route(
            "/v1/reconciliation/user-custodial-sum",
            axum::routing::get(user_custodial_sum),
        )
        .route("/v1/chain/verify", axum::routing::get(verify_chain))
        .with_state(store)
}

pub fn write_router(store: Store) -> axum::Router {
    axum::Router::new()
        .route("/v1/accounts", axum::routing::post(create_account))
        .route("/v1/postings", axum::routing::post(create_posting))
        .with_state(store)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readyz() -> Json<Value> {
    Json(json!({ "status": "ready" }))
}

async fn chart_of_accounts(State(_store): State<Store>) -> Json<Value> {
    Json(json!({
        "version": "1.0.0",
        "genesis_hash": GENESIS_HASH,
        "account_types": CHART,
    }))
}

#[derive(Debug, Deserialize)]
struct ListAccountsQuery {
    #[serde(rename = "type")]
    type_filter: Option<String>,
}

async fn list_accounts(
    State(store): State<Store>,
    Query(q): Query<ListAccountsQuery>,
) -> Json<Value> {
    let accounts: Vec<Account> = store.list_accounts(q.type_filter.as_deref());
    Json(json!({ "accounts": accounts }))
}

#[derive(Debug, Deserialize)]
struct ListPostingsQuery {
    limit: Option<usize>,
}

async fn list_postings(
    State(store): State<Store>,
    Query(q): Query<ListPostingsQuery>,
) -> Json<Value> {
    let postings: Vec<PostingRecord> = store.list_postings(q.limit.unwrap_or(50));
    Json(json!({ "postings": postings }))
}

async fn create_account(
    State(store): State<Store>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<AccountResponse>), (StatusCode, Json<Value>)> {
    match store.create_account(req) {
        Ok(account) => Ok((
            StatusCode::CREATED,
            Json(AccountResponse {
                account_id: account.account_id,
            }),
        )),
        Err(e) => Err((StatusCode::BAD_REQUEST, Json(json!({ "error": e })))),
    }
}

#[derive(Debug, Deserialize)]
struct BalanceQuery {
    asset: Option<String>,
}

async fn account_balance(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(q): Query<BalanceQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let asset = q.asset.unwrap_or_default();
    match store.balance(&id, &asset) {
        Some(bal) => {
            let account = store.get_account(&id);
            let as_of_ts = account
                .map(|a| a.created_at)
                .unwrap_or_else(crate::store::now_iso);
            Ok(Json(json!({
                "account_id": id,
                "asset": if asset.is_empty() { "all" } else { &asset },
                "balance": bal.to_string(),
                "as_of_ts": as_of_ts,
            })))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("account not found: {}", id) })),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct LedgerQuery {
    from: Option<String>,
    to: Option<String>,
    limit: Option<usize>,
    cursor: Option<u64>,
}

async fn account_ledger(
    State(store): State<Store>,
    Path(id): Path<String>,
    Query(q): Query<LedgerQuery>,
) -> Result<Json<LedgerPage>, (StatusCode, Json<Value>)> {
    let limit = q.limit.unwrap_or(50);
    match store.ledger(&id, q.from.as_deref(), q.to.as_deref(), limit, q.cursor) {
        Ok(page) => Ok(Json(page)),
        Err(e) => Err((StatusCode::NOT_FOUND, Json(json!({ "error": e })))),
    }
}

async fn create_posting(
    State(store): State<Store>,
    Json(req): Json<PostingRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    match store.post(req) {
        Ok((resp, replay)) => {
            let status = if replay {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok((
                status,
                Json(json!({
                    "posting_id": resp.posting_id,
                    "status": resp.status,
                    "entry_ids": resp.entry_ids,
                    "hash_head": resp.hash_head,
                })),
            ))
        }
        Err(e) => {
            let code = match e.status() {
                400 => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            Err((code, Json(json!({ "error": e.message() }))))
        }
    }
}

async fn get_posting(
    State(store): State<Store>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match store.get_posting(&id) {
        Some(p) => Ok(Json(serde_json::to_value(&p).unwrap())),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("posting not found: {}", id) })),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct UserCustodialQuery {
    asset: Option<String>,
}

async fn user_custodial_sum(
    State(store): State<Store>,
    Query(q): Query<UserCustodialQuery>,
) -> Json<Value> {
    let asset = q.asset.unwrap_or_default();
    let sum = store.user_custodial_sum(&asset);
    Json(json!({
        "asset": if asset.is_empty() { "all" } else { &asset },
        "user_custodial_sum": sum.to_string(),
    }))
}

async fn verify_chain(State(store): State<Store>) -> Json<Value> {
    match store.verify_chain() {
        Ok(()) => Json(json!({ "ok": true })),
        Err(b) => Json(json!({
            "ok": false,
            "entry_id": b.entry_id,
            "reason": b.reason,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    fn create_account_body(id: &str, type_name: &str, asset_class: &str) -> Value {
        json!({
            "account_id": id,
            "type": type_name,
            "asset_class": asset_class,
            "label": format!("{}-{}", type_name, id),
        })
    }

    async fn post_json(router: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, val)
    }

    async fn get_json(router: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let res = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, val)
    }

    fn balanced_posting_body(posting_id: &str) -> Value {
        json!({
            "posting_id": posting_id,
            "memo": "test",
            "ref_tx_id": "tx1",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" }
            ]
        })
    }

    async fn setup_two_accounts(router: &axum::Router) {
        let _ = post_json(
            router,
            "/v1/accounts",
            create_account_body("uc", "user_custodial", "BOTH"),
        )
        .await;
        let _ = post_json(
            router,
            "/v1/accounts",
            create_account_body("op", "operational_fiat", "FIAT"),
        )
        .await;
    }

    #[tokio::test]
    async fn user_custodial_sum_route_returns_sum() {
        let router = router(Store::new());
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("ucs1")).await;
        let (status, val) =
            get_json(&router, "/v1/reconciliation/user-custodial-sum?asset=USD").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["asset"], "USD");
        assert_eq!(val["user_custodial_sum"], "100");
    }

    #[tokio::test]
    async fn user_custodial_sum_route_default_asset_is_all() {
        let router = router(Store::new());
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("ucs2")).await;
        let (status, val) = get_json(&router, "/v1/reconciliation/user-custodial-sum").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["asset"], "all");
        assert_eq!(val["user_custodial_sum"], "100");
    }

    #[tokio::test]
    async fn verify_chain_route_ok() {
        let router = router(Store::new());
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("vc1")).await;
        let (status, val) = get_json(&router, "/v1/chain/verify").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["ok"], true);
    }

    #[tokio::test]
    async fn verify_chain_route_reports_break() {
        let store = Store::new();
        let _ = store.create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        );
        let _ = store.create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        );
        let _ = store.post(serde_json::from_value(balanced_posting_body("vc2")).unwrap());
        {
            let mut state = store.inner.lock();
            if let Some(e) = state.entries.first_mut() {
                e.this_hash = "deadbeef".to_string();
            }
        }
        let router = router(store);
        let (status, val) = get_json(&router, "/v1/chain/verify").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["ok"], false);
        assert!(val["entry_id"].is_string());
        assert!(val["reason"].as_str().unwrap().contains("mismatch"));
    }

    #[tokio::test]
    async fn account_balance_default_asset_is_all() {
        let router = router(Store::new());
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("ab1")).await;
        let (status, val) = get_json(&router, "/v1/accounts/uc/balance").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["asset"], "all");
        assert_eq!(val["balance"], "100");
    }

    #[tokio::test]
    async fn read_and_write_routers_build_independently() {
        let store = Store::new();
        let r = read_router(store.clone());
        let w = write_router(store);
        // Smoke test: hit healthz on read_router.
        let res = r
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // And create an account via write_router.
        let res = w
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/accounts")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&create_account_body("x", "user_custodial", "FIAT"))
                            .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }
}
