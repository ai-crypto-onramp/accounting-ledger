use accounting_ledger::audit;
use accounting_ledger::authtoken;
use accounting_ledger::config;
use accounting_ledger::grpc;
use accounting_ledger::handlers;
use accounting_ledger::store::Store;
use accounting_ledger::tls;
use opentelemetry::{global, trace::TracerProvider as _, KeyValue};
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{runtime::Tokio, Resource};
use tonic::transport::Server as TonicServer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, registry, EnvFilter};

fn init_tracing() {
    let fmt_layer = fmt::layer();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let otlp_layer = endpoint.and_then(|ep| {
        let mut exporter_builder = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&ep);
        let tls_config = load_otlp_tls().ok().flatten();
        if let Some(cfg) = tls_config {
            exporter_builder = exporter_builder.with_tls_config(cfg);
        }
        let exporter = exporter_builder.build().ok()?;
        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_batch_exporter(exporter, Tokio)
            .with_resource(Resource::new([
                KeyValue::new(
                    "service.name",
                    std::env::var("OTEL_SERVICE_NAME")
                        .unwrap_or_else(|_| "ledger-accounting".into()),
                ),
                KeyValue::new("service.namespace", "ai-crypto-onramp"),
                KeyValue::new("deployment.environment", "dev"),
            ]))
            .build();
        let tracer = provider.tracer("ledger-accounting");
        global::set_tracer_provider(provider);
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    });
    if let Some(layer) = otlp_layer {
        registry().with(filter).with(fmt_layer).with(layer).init();
    } else {
        registry().with(filter).with(fmt_layer).init();
    }
}

fn load_otlp_tls() -> anyhow::Result<Option<tonic::transport::ClientTlsConfig>> {
    let cert = std::env::var("TLS_CERT_FILE").unwrap_or_default();
    let key = std::env::var("TLS_KEY_FILE").unwrap_or_default();
    let ca = std::env::var("TLS_CA_FILE").unwrap_or_default();
    if cert.is_empty() && key.is_empty() && ca.is_empty() {
        if tls::is_dev_mode() {
            return Ok(None);
        }
        return Err(anyhow::anyhow!(
            "TLS_CERT_FILE/TLS_KEY_FILE/TLS_CA_FILE required when DEV_MODE!=1"
        ));
    }
    if cert.is_empty() || key.is_empty() || ca.is_empty() {
        return Err(anyhow::anyhow!(
            "TLS_CERT_FILE, TLS_KEY_FILE and TLS_CA_FILE must all be set together"
        ));
    }
    let cert_pem = std::fs::read_to_string(&cert)
        .map_err(|e| anyhow::anyhow!("read cert file {}: {}", cert, e))?;
    let key_pem = std::fs::read_to_string(&key)
        .map_err(|e| anyhow::anyhow!("read key file {}: {}", key, e))?;
    let ca_pem =
        std::fs::read_to_string(&ca).map_err(|e| anyhow::anyhow!("read ca file {}: {}", ca, e))?;
    let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);
    let ca_cert = tonic::transport::Certificate::from_pem(ca_pem);
    let cfg = tonic::transport::ClientTlsConfig::new()
        .identity(identity)
        .ca_certificate(ca_cert);
    Ok(Some(cfg))
}

#[allow(dead_code)]
fn app() -> axum::Router {
    if cfg!(test) && std::env::var("DEV_MODE").is_err() {
        std::env::set_var("DEV_MODE", "1");
    }
    let store = Store::new();
    let secret = authtoken::secret_from_env();
    let router = handlers::router(store).layer(axum::middleware::from_fn(authtoken::require_token));
    if let Some(s) = secret.clone() {
        router.layer(axum::Extension(authtoken::SharedSecret(s)))
    } else {
        router
    }
}

#[allow(dead_code)]
async fn serve(listener: tokio::net::TcpListener) {
    axum::serve(listener, app()).await.unwrap();
}

async fn run_grpc(store: Store, addr: std::net::SocketAddr) {
    let caller = std::env::var("ALLOWED_CALLERS")
        .unwrap_or_else(|_| "transaction-orchestrator,treasury-orchestration".to_string());
    let allowed_callers: Vec<String> = caller.split(',').map(|s| s.trim().to_string()).collect();
    let svc = grpc::server(store, allowed_callers);
    let secret = authtoken::secret_from_env();
    let mut builder = TonicServer::builder();
    let tls_result = tls::load_server_tls();
    match tls_result {
        Ok(Some(tls_material)) => {
            builder = match builder.tls_config(tls_material.server_config) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[grpc] TLS config error: {}", e);
                    return;
                }
            };
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("[grpc] TLS config error: {}", e);
            return;
        }
    }
    let router = builder
        .layer(tonic::service::interceptor(
            #[allow(clippy::result_large_err)]
            move |req: tonic::Request<()>| {
                authtoken::check_grpc(&req, secret.as_deref()).map(|()| req)
            },
        ))
        .add_service(svc);
    if let Err(e) = router.serve(addr).await {
        eprintln!("[grpc] server error: {}", e);
    }
}

async fn run_snapshot_task(store: Store) {
    let cfg = config::Config::from_env();
    let interval = cfg.snapshot_interval();
    loop {
        tokio::time::sleep(interval).await;
        let snaps = store.write_snapshots();
        eprintln!("[snapshot] wrote {} balance snapshots", snaps.len());
    }
}

fn verify_chain_at_startup(store: &Store) {
    match store.verify_chain() {
        Ok(()) => {
            eprintln!("[chain] verification passed at startup");
        }
        Err(b) => {
            eprintln!(
                "[chain] FATAL: hash chain broken at entry {}: {}",
                b.entry_id, b.reason
            );
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    init_tracing();

    let cfg = config::Config::from_env();
    if let Err(e) = cfg.assert_isolation() {
        eprintln!("[boot] {}", e);
        std::process::exit(1);
    }

    let store = if cfg.db_url.is_empty() {
        Store::new()
    } else {
        match Store::connect_with_salt(&cfg.db_url, cfg.hash_chain_salt.clone().unwrap_or_default())
            .await
        {
            Ok(s) => {
                if let Err(e) = s.run_migrations().await {
                    eprintln!("[boot] {}", e);
                    std::process::exit(1);
                }
                if let Err(e) = s.hydrate().await {
                    eprintln!("[boot] {}", e);
                    std::process::exit(1);
                }
                if cfg.hash_chain_salt.is_none() {
                    eprintln!("[boot] WARNING: HASH_CHAIN_SALT unset; hash chain is forgeable by anyone with DB write access");
                }
                eprintln!("[boot] connected to postgres at {}", cfg.db_url);
                s
            }
            Err(e) => {
                eprintln!("[boot] failed to connect to postgres: {}", e);
                std::process::exit(1);
            }
        }
    };
    let store = match audit::AuditSink::from_env() {
        Ok(sink) => store.with_audit_sink(sink),
        Err(e) => {
            eprintln!("[boot] audit sink init: {}", e);
            std::process::exit(1);
        }
    };
    verify_chain_at_startup(&store);

    let port = cfg.port;
    let rest_addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    let grpc_addr: std::net::SocketAddr = ([0, 0, 0, 0], port + 1).into();

    let grpc_store = store.clone();
    let snap_store = store.clone();
    let rest_store = store.clone();
    tokio::spawn(async move {
        run_grpc(grpc_store, grpc_addr).await;
    });
    tokio::spawn(async move {
        run_snapshot_task(snap_store).await;
    });

    let listener = tokio::net::TcpListener::bind(rest_addr).await.unwrap();
    let secret = authtoken::secret_from_env();
    let rest_router =
        handlers::router(rest_store).layer(axum::middleware::from_fn(authtoken::require_token));
    let rest_router = if let Some(s) = secret.clone() {
        rest_router.layer(axum::Extension(authtoken::SharedSecret(s)))
    } else {
        rest_router
    };
    axum::serve(listener, rest_router).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use accounting_ledger::chart;
    use accounting_ledger::posting::PostingRequest;
    use accounting_ledger::store::Store;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    fn unbalanced_posting_body(posting_id: &str) -> Value {
        json!({
            "posting_id": posting_id,
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 50, "asset": "USD" }
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
    async fn healthz_returns_ok() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val, json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn readyz_returns_ok() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let val: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(val, json!({ "status": "ready" }));
    }

    #[tokio::test]
    async fn router_returns_404_for_unknown_route() {
        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serve_handles_real_http_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"status\":\"ok\""));
    }

    #[tokio::test]
    async fn chart_of_accounts_returns_catalog() {
        let (status, val) = get_json(&app(), "/v1/chart-of-accounts").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val["version"], "1.0.0");
        let types = val["account_types"].as_array().unwrap();
        assert!(types.len() >= 11);
        let names: Vec<&str> = types.iter().map(|t| t["type"].as_str().unwrap()).collect();
        for expected in [
            "user_custodial",
            "user_payable",
            "operational_fiat",
            "operational_crypto",
            "treasury_fiat",
            "treasury_crypto",
            "fx_gain_loss",
            "fee_revenue",
            "rail_settlement",
            "venue_settlement",
            "chargeback_reserve",
        ] {
            assert!(names.contains(&expected), "missing {}", expected);
        }
    }

    #[tokio::test]
    async fn create_account_rejects_unknown_type() {
        let router = app();
        let (status, val) = post_json(
            &router,
            "/v1/accounts",
            create_account_body("a1", "bogus", "FIAT"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"]
            .as_str()
            .unwrap()
            .contains("unknown account type"));
    }

    #[tokio::test]
    async fn create_account_rejects_bad_asset_class_for_type() {
        let router = app();
        let (status, val) = post_json(
            &router,
            "/v1/accounts",
            create_account_body("a2", "operational_fiat", "CRYPTO"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"]
            .as_str()
            .unwrap()
            .contains("asset_class CRYPTO not allowed for type operational_fiat"));
    }

    #[tokio::test]
    async fn create_account_returns_201_and_id() {
        let router = app();
        let (status, val) = post_json(
            &router,
            "/v1/accounts",
            create_account_body("acct-uc", "user_custodial", "FIAT"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(val["account_id"], "acct-uc");
    }

    #[tokio::test]
    async fn balance_returns_404_for_unknown_account() {
        let router = app();
        let (status, _) = get_json(&router, "/v1/accounts/nope/balance?asset=USD").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn posting_balanced_returns_201_with_hash_head() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status, val) = post_json(&router, "/v1/postings", balanced_posting_body("p1")).await;
        assert_eq!(status, StatusCode::CREATED, "body: {:?}", val);
        assert_eq!(val["status"], "POSTED");
        let entry_ids = val["entry_ids"].as_array().unwrap();
        assert_eq!(entry_ids.len(), 2);
        let hash_head = val["hash_head"].as_str().unwrap();
        assert!(!hash_head.is_empty());
    }

    #[tokio::test]
    async fn posting_unbalanced_returns_400() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status, val) = post_json(&router, "/v1/postings", unbalanced_posting_body("p2")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("unbalanced"));
    }

    #[tokio::test]
    async fn posting_unknown_account_returns_400() {
        let router = app();
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("uc", "user_custodial", "BOTH"),
        )
        .await;
        let (status, val) = post_json(
            &router,
            "/v1/postings",
            json!({
                "posting_id": "p3",
                "entries": [
                    { "account_id": "nope", "direction": "DEBIT", "amount": 10, "asset": "USD" },
                    { "account_id": "uc", "direction": "CREDIT", "amount": 10, "asset": "USD" }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("account not found"));
    }

    #[tokio::test]
    async fn posting_max_entries_exceeded_returns_400() {
        let router = app();
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("uc", "user_custodial", "BOTH"),
        )
        .await;
        let mut entries = Vec::new();
        for i in 0..65 {
            entries.push(json!({
                "account_id": "uc",
                "direction": if i % 2 == 0 { "debit" } else { "credit" },
                "amount": 1,
                "asset": "USD"
            }));
        }
        let (status, val) = post_json(
            &router,
            "/v1/postings",
            json!({ "posting_id": "pmax", "entries": entries }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("too many entries"));
    }

    #[tokio::test]
    async fn posting_zero_amount_rejected() {
        let router = app();
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("uc", "user_custodial", "BOTH"),
        )
        .await;
        let (status, val) = post_json(
            &router,
            "/v1/postings",
            json!({
                "posting_id": "pz",
                "entries": [
                    { "account_id": "uc", "direction": "DEBIT", "amount": 0, "asset": "USD" },
                    { "account_id": "uc", "direction": "CREDIT", "amount": 0, "asset": "USD" }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"]
            .as_str()
            .unwrap()
            .contains("amount must be > 0"));
    }

    #[tokio::test]
    async fn idempotency_replay_returns_200_same_result() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status1, val1) =
            post_json(&router, "/v1/postings", balanced_posting_body("pidem")).await;
        assert_eq!(status1, StatusCode::CREATED);
        let (status2, val2) =
            post_json(&router, "/v1/postings", balanced_posting_body("pidem")).await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(val1["entry_ids"], val2["entry_ids"]);
        assert_eq!(val1["hash_head"], val2["hash_head"]);
    }

    #[tokio::test]
    async fn get_posting_returns_full_record() {
        let router = app();
        setup_two_accounts(&router).await;
        let (_, val1) = post_json(&router, "/v1/postings", balanced_posting_body("pget")).await;
        let (status, val2) = get_json(&router, "/v1/postings/pget").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(val2["posting_id"], "pget");
        assert_eq!(val2["status"], "POSTED");
        let entries = val2["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["entry_id"], val1["entry_ids"][0]);
    }

    #[tokio::test]
    async fn get_posting_unknown_returns_404() {
        let router = app();
        let (status, _) = get_json(&router, "/v1/postings/does-not-exist").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn balance_reflects_entries() {
        let router = app();
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("pb1")).await;
        let (status, val) = get_json(&router, "/v1/accounts/uc/balance?asset=USD").await;
        assert_eq!(status, StatusCode::OK);
        let bal: i128 = val["balance"].as_str().unwrap().parse().unwrap();
        assert_eq!(bal, 100);
    }

    #[tokio::test]
    async fn ledger_returns_paginated_with_running_balance() {
        let router = app();
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("l1")).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("l2")).await;
        let (status, val) = get_json(&router, "/v1/accounts/uc/ledger?limit=10").await;
        assert_eq!(status, StatusCode::OK);
        let entries = val["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["running_balance"], 100);
        assert_eq!(entries[1]["running_balance"], 200);
    }

    #[tokio::test]
    async fn ledger_404_for_unknown_account() {
        let router = app();
        let (status, _) = get_json(&router, "/v1/accounts/nope/ledger?asset=USD").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hash_chain_continuity_holds() {
        let router = app();
        setup_two_accounts(&router).await;
        let (_, val) = post_json(&router, "/v1/postings", balanced_posting_body("hc1")).await;
        let posting = get_json(&router, "/v1/postings/hc1").await.1;
        let entries = posting["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let prev0 = entries[0]["prev_hash"].as_str().unwrap();
        assert_eq!(prev0, chart::GENESIS_HASH);
        let this0 = entries[0]["this_hash"].as_str().unwrap();
        let prev1 = entries[1]["prev_hash"].as_str().unwrap();
        assert_eq!(prev1, this0);
        let this1 = entries[1]["this_hash"].as_str().unwrap();
        assert_eq!(this1, val["hash_head"].as_str().unwrap());
    }

    #[tokio::test]
    async fn audit_event_emitted_per_posting() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("ae1")).unwrap();
        let _ = store.post(req).unwrap();
        let events = store.audit_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].posting_id, "ae1");
    }

    #[tokio::test]
    async fn unit_balance_computation() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("u1")).unwrap();
        store.post(req).unwrap();
        let bal = store.balance("uc", "USD").unwrap();
        assert_eq!(bal, 100);
        let bal_op = store.balance("op", "USD").unwrap();
        assert_eq!(bal_op, -100);
    }

    #[tokio::test]
    async fn unit_reject_unbalanced_direct() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(unbalanced_posting_body("uu1")).unwrap();
        let res = store.post(req);
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn unit_idempotency_direct() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("idem1")).unwrap();
        let (r1, replay1) = store.post(req.clone()).unwrap();
        assert!(!replay1);
        let (r2, replay2) = store.post(req).unwrap();
        assert!(replay2);
        assert_eq!(r1.entry_ids, r2.entry_ids);
        assert_eq!(r1.hash_head, r2.hash_head);
        assert_eq!(store.entry_count(), 2);
    }

    #[tokio::test]
    async fn unit_hash_chain_continuity_direct() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("hcc1")).unwrap();
        let (resp, _) = store.post(req).unwrap();
        let posting = store.get_posting("hcc1").unwrap();
        let entries = &posting.entries;
        assert_eq!(entries[0].prev_hash, chart::GENESIS_HASH);
        assert_eq!(entries[1].prev_hash, entries[0].this_hash);
        assert_eq!(entries[1].this_hash, resp.hash_head);
    }

    #[tokio::test]
    async fn multi_asset_posting_per_asset_balance() {
        let router = app();
        setup_two_accounts(&router).await;
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("opc", "operational_crypto", "CRYPTO"),
        )
        .await;
        let body = json!({
            "posting_id": "multi1",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" },
                { "account_id": "uc", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
                { "account_id": "opc", "direction": "CREDIT", "amount": 50, "asset": "BTC" }
            ]
        });
        let (status, val) = post_json(&router, "/v1/postings", body).await;
        assert_eq!(status, StatusCode::CREATED, "body: {:?}", val);
        let (s1, b1) = get_json(&router, "/v1/accounts/uc/balance?asset=USD").await;
        assert_eq!(s1, StatusCode::OK);
        assert_eq!(b1["balance"], "100");
        let (s2, b2) = get_json(&router, "/v1/accounts/uc/balance?asset=BTC").await;
        assert_eq!(s2, StatusCode::OK);
        assert_eq!(b2["balance"], "50");
    }

    #[tokio::test]
    async fn unbalanced_per_asset_rejected() {
        let router = app();
        setup_two_accounts(&router).await;
        let body = json!({
            "posting_id": "ub1",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" },
                { "account_id": "uc", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
                { "account_id": "op", "direction": "CREDIT", "amount": 30, "asset": "BTC" }
            ]
        });
        let (status, val) = post_json(&router, "/v1/postings", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("BTC unbalanced"));
    }

    #[tokio::test]
    async fn disallowed_direction_for_account_type_rejected() {
        let router = app();
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("fr", "fee_revenue", "FIAT"),
        )
        .await;
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("op", "operational_fiat", "FIAT"),
        )
        .await;
        let body = json!({
            "posting_id": "dirbad",
            "entries": [
                { "account_id": "op", "direction": "CREDIT", "amount": 10, "asset": "USD" },
                { "account_id": "fr", "direction": "sideways", "amount": 10, "asset": "USD" }
            ]
        });
        let (status, val) = post_json(&router, "/v1/postings", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("invalid direction"));
    }

    #[tokio::test]
    async fn duplicate_account_id_rejected() {
        let router = app();
        let _ = post_json(
            &router,
            "/v1/accounts",
            create_account_body("dup", "user_custodial", "FIAT"),
        )
        .await;
        let (status, _) = post_json(
            &router,
            "/v1/accounts",
            create_account_body("dup", "user_custodial", "FIAT"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn empty_entries_rejected() {
        let router = app();
        let (status, _) = post_json(
            &router,
            "/v1/postings",
            json!({ "posting_id": "empty", "entries": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ledger_pagination_cursor() {
        let router = app();
        setup_two_accounts(&router).await;
        for i in 0..5 {
            let _ = post_json(
                &router,
                "/v1/postings",
                json!({
                    "posting_id": format!("page{}", i),
                    "entries": [
                        { "account_id": "uc", "direction": "DEBIT", "amount": 1, "asset": "USD" },
                        { "account_id": "op", "direction": "CREDIT", "amount": 1, "asset": "USD" }
                    ]
                }),
            )
            .await;
        }
        let (s1, v1) = get_json(&router, "/v1/accounts/uc/ledger?limit=2").await;
        assert_eq!(s1, StatusCode::OK);
        let e1 = v1["entries"].as_array().unwrap();
        assert_eq!(e1.len(), 2);
        let cursor = v1["next_cursor"].as_u64().unwrap();
        let (s2, v2) = get_json(
            &router,
            &format!("/v1/accounts/uc/ledger?limit=2&cursor={}", cursor),
        )
        .await;
        assert_eq!(s2, StatusCode::OK);
        let e2 = v2["entries"].as_array().unwrap();
        assert_eq!(e2.len(), 2);
    }

    #[tokio::test]
    async fn unknown_asset_rejected() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status, val) = post_json(
            &router,
            "/v1/postings",
            json!({
                "posting_id": "unkasset",
                "entries": [
                    { "account_id": "uc", "direction": "DEBIT", "amount": 10, "asset": "WOBBLE" },
                    { "account_id": "op", "direction": "CREDIT", "amount": 10, "asset": "WOBBLE" }
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(val["error"].as_str().unwrap().contains("unknown asset"));
    }

    #[tokio::test]
    async fn hash_chain_anchor_and_global_head() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("anchor1")).unwrap();
        let (resp, _) = store.post(req).unwrap();
        let anchor = store.hash_chain_anchor("anchor1").unwrap();
        assert_eq!(anchor.head_hash, resp.hash_head);
        assert_eq!(store.global_chain_head(), resp.hash_head);
    }

    #[tokio::test]
    async fn verify_chain_passes_clean_db() {
        let store = Store::new();
        assert!(store.verify_chain().is_ok());
    }

    #[tokio::test]
    async fn verify_chain_detects_tamper() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let req: PostingRequest = serde_json::from_value(balanced_posting_body("tamper1")).unwrap();
        store.post(req).unwrap();
        {
            let mut state = store.inner.lock();
            state.entries[0].this_hash = "deadbeef".to_string();
        }
        assert!(store.verify_chain().is_err());
    }

    #[tokio::test]
    async fn user_custodial_sum_matches_entries() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc1", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc2", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store.post(
            serde_json::from_value(json!({
                "posting_id": "ucs1",
                "entries": [
                    { "account_id": "uc1", "direction": "DEBIT", "amount": 70, "asset": "USD" },
                    { "account_id": "op", "direction": "CREDIT", "amount": 70, "asset": "USD" }
                ]
            }))
            .unwrap(),
        );
        let _ = store.post(
            serde_json::from_value(json!({
                "posting_id": "ucs2",
                "entries": [
                    { "account_id": "uc2", "direction": "DEBIT", "amount": 30, "asset": "USD" },
                    { "account_id": "op", "direction": "CREDIT", "amount": 30, "asset": "USD" }
                ]
            }))
            .unwrap(),
        );
        let sum = store.user_custodial_sum("USD");
        assert_eq!(sum, 100);
    }

    #[tokio::test]
    async fn snapshot_write_and_reconcile() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .post(serde_json::from_value(balanced_posting_body("snap1")).unwrap())
            .unwrap();
        let snaps = store.write_snapshots();
        assert!(!snaps.is_empty());
        for s in &snaps {
            assert!(store.reconcile_snapshot(s), "snapshot mismatch: {:?}", s);
        }
    }

    #[tokio::test]
    async fn balance_via_snapshot_matches_direct() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .post(serde_json::from_value(balanced_posting_body("bsnap1")).unwrap())
            .unwrap();
        store.write_snapshots();
        let _ = store
            .post(serde_json::from_value(balanced_posting_body("bsnap2")).unwrap())
            .unwrap();
        let direct = store.balance("uc", "USD").unwrap();
        let via = store.balance_via_snapshot("uc", "USD").unwrap();
        assert_eq!(direct, via);
    }

    #[tokio::test]
    async fn fx_posting_routes_gain_to_fx_gain_loss() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("opf", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("vs", "venue_settlement", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("fx", "fx_gain_loss", "BOTH")).unwrap(),
            )
            .unwrap();
        let _ = store.post(
            serde_json::from_value(json!({
                "posting_id": "fx1",
                "entries": [
                    { "account_id": "vs", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
                    { "account_id": "opf", "direction": "CREDIT", "amount": 50, "asset": "BTC" },
                    { "account_id": "opf", "direction": "DEBIT", "amount": 105, "asset": "USD" },
                    { "account_id": "vs", "direction": "CREDIT", "amount": 100, "asset": "USD" },
                    { "account_id": "fx", "direction": "CREDIT", "amount": 5, "asset": "USD" }
                ]
            }))
            .unwrap(),
        );
        let fx_bal = store.balance("fx", "USD").unwrap();
        assert_eq!(fx_bal, -5);
    }

    #[tokio::test]
    async fn list_accounts_returns_all() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status, val) = get_json(&router, "/v1/accounts").await;
        assert_eq!(status, StatusCode::OK);
        let accounts = val["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
    }

    #[tokio::test]
    async fn list_accounts_filters_by_type() {
        let router = app();
        setup_two_accounts(&router).await;
        let (status, val) = get_json(&router, "/v1/accounts?type=user_custodial").await;
        assert_eq!(status, StatusCode::OK);
        let accounts = val["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["type"], "user_custodial");
    }

    #[tokio::test]
    async fn list_postings_returns_all() {
        let router = app();
        setup_two_accounts(&router).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("lp1")).await;
        let _ = post_json(&router, "/v1/postings", balanced_posting_body("lp2")).await;
        let (status, val) = get_json(&router, "/v1/postings").await;
        assert_eq!(status, StatusCode::OK);
        let postings = val["postings"].as_array().unwrap();
        assert_eq!(postings.len(), 2);
    }

    #[tokio::test]
    async fn list_postings_respects_limit() {
        let router = app();
        setup_two_accounts(&router).await;
        for i in 0..3 {
            let _ = post_json(
                &router,
                "/v1/postings",
                balanced_posting_body(&format!("lplim{}", i)),
            )
            .await;
        }
        let (status, val) = get_json(&router, "/v1/postings?limit=2").await;
        assert_eq!(status, StatusCode::OK);
        let postings = val["postings"].as_array().unwrap();
        assert_eq!(postings.len(), 2);
    }

    #[tokio::test]
    async fn run_grpc_serves_until_cancelled() {
        let store = Store::new();
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let svc = grpc::server(store, vec![]);
        let router = TonicServer::builder().add_service(svc);
        let handle = tokio::spawn(async move {
            let _ = router.serve(addr).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn run_snapshot_task_writes_snapshots() {
        let store = Store::new();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .create_account(
                serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT"))
                    .unwrap(),
            )
            .unwrap();
        let _ = store
            .post(serde_json::from_value(balanced_posting_body("snap-task")).unwrap())
            .unwrap();
        std::env::set_var("SNAPSHOT_INTERVAL_MINUTES", "0");
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            run_snapshot_task(store_clone).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        handle.abort();
        let _ = handle.await;
        std::env::remove_var("SNAPSHOT_INTERVAL_MINUTES");
        assert!(!store.write_snapshots().is_empty());
    }

    #[test]
    fn verify_chain_at_startup_passes_on_clean_store() {
        let store = Store::new();
        verify_chain_at_startup(&store);
    }

    static OTLP_TLS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn otlp_lock() -> std::sync::MutexGuard<'static, ()> {
        OTLP_TLS_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_otlp_env() {
        std::env::remove_var("TLS_CERT_FILE");
        std::env::remove_var("TLS_KEY_FILE");
        std::env::remove_var("TLS_CA_FILE");
        std::env::set_var("DEV_MODE", "1");
    }

    #[test]
    fn load_otlp_tls_dev_mode_returns_none() {
        let _g = otlp_lock();
        clear_otlp_env();
        std::env::set_var("DEV_MODE", "1");
        let cfg = load_otlp_tls().unwrap();
        assert!(cfg.is_none());
        clear_otlp_env();
    }

    #[test]
    fn load_otlp_tls_prod_missing_env_is_error() {
        let _g = otlp_lock();
        clear_otlp_env();
        std::env::set_var("DEV_MODE", "0");
        assert!(load_otlp_tls().is_err());
        clear_otlp_env();
    }

    #[test]
    fn load_otlp_tls_partial_set_is_error() {
        let _g = otlp_lock();
        clear_otlp_env();
        std::env::set_var("TLS_CERT_FILE", "/x/cert.pem");
        std::env::set_var("DEV_MODE", "1");
        assert!(load_otlp_tls().is_err());
        clear_otlp_env();
    }

    #[test]
    fn load_otlp_tls_bad_cert_files_is_error() {
        let _g = otlp_lock();
        clear_otlp_env();
        std::env::set_var("TLS_CERT_FILE", "/no/cert.pem");
        std::env::set_var("TLS_KEY_FILE", "/no/key.pem");
        std::env::set_var("TLS_CA_FILE", "/no/ca.pem");
        std::env::set_var("DEV_MODE", "0");
        assert!(load_otlp_tls().is_err());
        clear_otlp_env();
    }

    #[test]
    fn load_otlp_tls_valid_certs_returns_config() {
        let _g = otlp_lock();
        let dir = tempfile::tempdir().unwrap();
        let certified_key =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = certified_key.cert.pem();
        let key_pem = certified_key.key_pair.serialize_pem();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();
        std::fs::write(&ca_path, &cert_pem).unwrap();
        std::env::set_var("TLS_CERT_FILE", cert_path.to_string_lossy().to_string());
        std::env::set_var("TLS_KEY_FILE", key_path.to_string_lossy().to_string());
        std::env::set_var("TLS_CA_FILE", ca_path.to_string_lossy().to_string());
        std::env::set_var("DEV_MODE", "0");
        let cfg = load_otlp_tls().unwrap().unwrap();
        assert!(format!("{:?}", cfg).contains("ClientTlsConfig"));
        clear_otlp_env();
    }
}
