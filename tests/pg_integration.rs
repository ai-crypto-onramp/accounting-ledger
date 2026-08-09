use accounting_ledger::posting::PostingRequest;
use accounting_ledger::store::Store;
use serde_json::json;
use std::sync::OnceLock;
use tokio::sync::Mutex;

fn pg_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn db_url() -> Option<String> {
    match std::env::var("PG_TEST_DB_URL").ok() {
        Some(u) if !u.is_empty() => Some(u),
        _ => None,
    }
}

fn create_account_body(id: &str, type_name: &str, asset_class: &str) -> serde_json::Value {
    json!({
        "account_id": id,
        "type": type_name,
        "asset_class": asset_class,
        "label": format!("{}-{}", type_name, id),
    })
}

fn balanced_posting(posting_id: &str) -> serde_json::Value {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_roundtrip_and_persistence() {
    let url = match db_url() {
        Some(u) => u,
        None => {
            eprintln!("[pg_integration] PG_TEST_DB_URL not set; skipping");
            return;
        }
    };
    let _guard = pg_lock().lock().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    sqlx::query("DROP TABLE IF EXISTS balance_snapshots CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS hash_chain CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS entries CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS postings CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS accounts CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS chart_of_accounts CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);

    let store = Store::connect(&url).await.expect("connect store");
    store.run_migrations().await.expect("migrations");
    store.hydrate().await.expect("hydrate empty");

    store
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();

    let req: PostingRequest = serde_json::from_value(balanced_posting("pg1")).unwrap();
    let (resp, replay) = store.post(req).unwrap();
    assert!(!replay);
    assert_eq!(store.balance("uc", "USD"), Some(100));
    assert_eq!(store.balance("op", "USD"), Some(-100));
    assert!(store.verify_chain().is_ok());
    assert_eq!(store.global_chain_head(), resp.hash_head);

    let (resp2, replay2) = store
        .post(serde_json::from_value(balanced_posting("pg1")).unwrap())
        .unwrap();
    assert!(replay2);
    assert_eq!(resp.entry_ids, resp2.entry_ids);
    assert_eq!(store.entry_count(), 2);

    let store2 = Store::connect(&url).await.expect("reconnect");
    store2.run_migrations().await.expect("migrations 2");
    store2.hydrate().await.expect("hydrate 2");
    assert_eq!(store2.entry_count(), 2);
    assert_eq!(store2.balance("uc", "USD"), Some(100));
    assert_eq!(store2.balance("op", "USD"), Some(-100));
    assert!(store2.verify_chain().is_ok());
    assert_eq!(store2.global_chain_head(), resp.hash_head);
    let posting = store2.get_posting("pg1").unwrap();
    assert_eq!(posting.entries.len(), 2);
    assert_eq!(posting.hash_head, resp.hash_head);
    let anchor = store2.hash_chain_anchor("pg1").unwrap();
    assert_eq!(anchor.head_hash, resp.hash_head);

    store2
        .create_account(
            serde_json::from_value(create_account_body("opc", "operational_crypto", "CRYPTO"))
                .unwrap(),
        )
        .unwrap();
    let multi: PostingRequest = serde_json::from_value(json!({
        "posting_id": "pg2",
        "entries": [
            { "account_id": "uc", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
            { "account_id": "opc", "direction": "CREDIT", "amount": 50, "asset": "BTC" }
        ]
    }))
    .unwrap();
    let (_resp3, replay3) = store2.post(multi).unwrap();
    assert!(!replay3);
    assert_eq!(store2.balance("uc", "BTC"), Some(50));
    assert_eq!(store2.balance("opc", "BTC"), Some(-50));
    assert!(store2.verify_chain().is_ok());

    let snaps = store2.write_snapshots();
    assert!(!snaps.is_empty());
    for s in &snaps {
        assert!(store2.reconcile_snapshot(s), "snapshot mismatch: {:?}", s);
    }
    assert_eq!(
        store2.balance_via_snapshot("uc", "USD"),
        Some(100),
        "balance via snapshot should match"
    );
    assert_eq!(store2.balance_via_snapshot("uc", "BTC"), Some(50));

    assert_eq!(store2.user_custodial_sum("USD"), 100);

    let _ = std::process::Command::new("psql")
        .arg("-d")
        .arg(url.rsplit_once('/').map(|(_, db)| db).unwrap_or("ledger"))
        .arg("-c")
        .arg("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .output();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_salt_changes_hash_chain() {
    let url = match db_url() {
        Some(u) => u,
        None => return,
    };
    let _guard = pg_lock().lock().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);

    // Posting with empty salt.
    let store1 = Store::connect_with_salt(&url, String::new())
        .await
        .expect("connect store1");
    store1.run_migrations().await.expect("migrations");
    store1.hydrate().await.expect("hydrate");
    store1
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store1
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();
    let req: PostingRequest = serde_json::from_value(balanced_posting("salt1")).unwrap();
    let (resp_empty, _) = store1.post(req).unwrap();
    let head_empty = resp_empty.hash_head.clone();

    // Posting the same canonical bytes with a non-empty salt must produce a
    // different hash head.
    let store2 = Store::connect_with_salt(&url, "pepper-xyz".to_string())
        .await
        .expect("connect store2");
    store2.run_migrations().await.expect("migrations 2");
    store2.hydrate().await.expect("hydrate 2");
    // The chain head from store2's verify_chain over the salt-empty entries
    // must FAIL (different salt -> hash mismatch).
    let verify_pre = store2.verify_chain();
    assert!(
        verify_pre.is_err(),
        "verify_chain with new salt over old entries must fail"
    );
    // Wipe DB and post the same posting with the new salt.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);
    let store3 = Store::connect_with_salt(&url, "pepper-xyz".to_string())
        .await
        .expect("connect store3");
    store3.run_migrations().await.expect("migrations 3");
    store3.hydrate().await.expect("hydrate 3");
    store3
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store3
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();
    // Use a fresh posting_id so we don't get a replay.
    let req_salted: PostingRequest = serde_json::from_value(json!({
        "posting_id": "salt1",
        "memo": "test",
        "ref_tx_id": "tx1",
        "entries": [
            { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
            { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" }
        ]
    }))
    .unwrap();
    let (resp_salted, _) = store3.post(req_salted).unwrap();
    let head_salted = resp_salted.hash_head.clone();
    assert_ne!(
        head_empty, head_salted,
        "hash head must differ when salt differs"
    );
    assert!(store3.verify_chain().is_ok());

    let _ = std::process::Command::new("psql")
        .arg("-d")
        .arg(url.rsplit_once('/').map(|(_, db)| db).unwrap_or("ledger"))
        .arg("-c")
        .arg("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .output();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_entries_immutable_trigger_rejects_update_and_delete() {
    let url = match db_url() {
        Some(u) => u,
        None => return,
    };
    let _guard = pg_lock().lock().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);

    let store = Store::connect(&url).await.expect("connect");
    store.run_migrations().await.expect("migrations");
    store.hydrate().await.expect("hydrate");
    store
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();
    let req: PostingRequest = serde_json::from_value(balanced_posting("imm1")).unwrap();
    let (resp, _) = store.post(req).unwrap();
    let entry_id = resp.entry_ids[0].clone();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    // UPDATE must raise.
    let update_res = sqlx::query("UPDATE entries SET amount = 999 WHERE entry_id = $1")
        .bind(&entry_id)
        .execute(&pool)
        .await;
    assert!(
        update_res.is_err(),
        "UPDATE on entries must be rejected by trigger; got {:?}",
        update_res
    );
    // DELETE must raise.
    let delete_res = sqlx::query("DELETE FROM entries WHERE entry_id = $1")
        .bind(&entry_id)
        .execute(&pool)
        .await;
    assert!(
        delete_res.is_err(),
        "DELETE on entries must be rejected by trigger; got {:?}",
        delete_res
    );
    drop(pool);

    let _ = std::process::Command::new("psql")
        .arg("-d")
        .arg(url.rsplit_once('/').map(|(_, db)| db).unwrap_or("ledger"))
        .arg("-c")
        .arg("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .output();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_balance_survives_in_memory_restart() {
    let url = match db_url() {
        Some(u) => u,
        None => return,
    };
    let _guard = pg_lock().lock().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);

    // Write via store1.
    let store1 = Store::connect(&url).await.expect("connect store1");
    store1.run_migrations().await.expect("migrations");
    store1.hydrate().await.expect("hydrate");
    store1
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store1
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();
    let req: PostingRequest = serde_json::from_value(balanced_posting("restart1")).unwrap();
    store1.post(req).unwrap();
    let req2: PostingRequest = serde_json::from_value(balanced_posting("restart2")).unwrap();
    store1.post(req2).unwrap();
    assert_eq!(store1.balance("uc", "USD"), Some(200));
    assert_eq!(store1.balance("op", "USD"), Some(-200));

    // Drop store1 (drops its in-memory cache) and build store2 from the same DB.
    drop(store1);
    let store2 = Store::connect(&url).await.expect("connect store2");
    store2.run_migrations().await.expect("migrations 2");
    store2.hydrate().await.expect("hydrate 2");
    // In-memory cache is rebuilt; balances read from DB.
    assert_eq!(store2.balance("uc", "USD"), Some(200));
    assert_eq!(store2.balance("op", "USD"), Some(-200));
    assert!(store2.verify_chain().is_ok());
    let posting = store2.get_posting("restart1").unwrap();
    assert_eq!(posting.entries.len(), 2);

    // Posting again after restart continues the chain correctly.
    let req3: PostingRequest = serde_json::from_value(balanced_posting("restart3")).unwrap();
    let (resp3, replay3) = store2.post(req3).unwrap();
    assert!(!replay3);
    assert_eq!(store2.balance("uc", "USD"), Some(300));
    assert!(store2.verify_chain().is_ok());
    assert_eq!(store2.global_chain_head(), resp3.hash_head);

    let _ = std::process::Command::new("psql")
        .arg("-d")
        .arg(url.rsplit_once('/').map(|(_, db)| db).unwrap_or("ledger"))
        .arg("-c")
        .arg("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .output();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_unbalanced_rolls_back_and_idempotent_retry() {
    let url = match db_url() {
        Some(u) => u,
        None => return,
    };
    let _guard = pg_lock().lock().await;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP SCHEMA public CASCADE;")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE SCHEMA public;")
        .execute(&pool)
        .await
        .unwrap();
    drop(pool);

    let store = Store::connect(&url).await.unwrap();
    store.run_migrations().await.unwrap();
    store.hydrate().await.unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();

    let bad: PostingRequest = serde_json::from_value(json!({
        "posting_id": "rb1",
        "entries": [
            { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
            { "account_id": "op", "direction": "CREDIT", "amount": 50, "asset": "USD" }
        ]
    }))
    .unwrap();
    assert!(store.post(bad).is_err());
    assert_eq!(store.entry_count(), 0);

    let good: PostingRequest = serde_json::from_value(balanced_posting("rb1")).unwrap();
    let (resp, replay) = store.post(good).unwrap();
    assert!(!replay);
    assert_eq!(store.entry_count(), 2);
    assert_eq!(store.balance("uc", "USD"), Some(100));

    let store2 = Store::connect(&url).await.unwrap();
    store2.hydrate().await.unwrap();
    assert_eq!(store2.entry_count(), 2);
    assert_eq!(store2.balance("uc", "USD"), Some(100));
    assert_eq!(store2.global_chain_head(), resp.hash_head);

    let _ = std::process::Command::new("psql")
        .arg("-d")
        .arg(url.rsplit_once('/').map(|(_, db)| db).unwrap_or("ledger"))
        .arg("-c")
        .arg("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .output();
}
