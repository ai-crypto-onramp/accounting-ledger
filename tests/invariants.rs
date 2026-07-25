use accounting_ledger::posting::PostingRequest;
use accounting_ledger::store::Store;
use serde_json::json;

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

fn setup(store: &Store) {
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
}

#[test]
fn balanced_posting_commits() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(balanced_posting("bal1")).unwrap();
    let (resp, replay) = store.post(req).unwrap();
    assert!(!replay);
    assert_eq!(resp.status, "POSTED");
    assert_eq!(store.entry_count(), 2);
    let bal = store.balance("uc", "USD").unwrap();
    assert_eq!(bal, 100);
    let bal_op = store.balance("op", "USD").unwrap();
    assert_eq!(bal_op, -100);
}

#[test]
fn unbalanced_posting_rejected_atomically() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(json!({
        "posting_id": "unbal1",
        "entries": [
            { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
            { "account_id": "op", "direction": "CREDIT", "amount": 50, "asset": "USD" }
        ]
    }))
    .unwrap();
    assert!(store.post(req).is_err());
    assert_eq!(store.entry_count(), 0);
}

#[test]
fn idempotency_replay_returns_same_result() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(balanced_posting("idem1")).unwrap();
    let (r1, replay1) = store.post(req.clone()).unwrap();
    assert!(!replay1);
    let (r2, replay2) = store.post(req).unwrap();
    assert!(replay2);
    assert_eq!(r1.entry_ids, r2.entry_ids);
    assert_eq!(r1.hash_head, r2.hash_head);
    assert_eq!(store.entry_count(), 2);
}

#[test]
fn idempotency_replay_after_non_committing_failure() {
    let store = Store::new();
    setup(&store);
    let bad: PostingRequest = serde_json::from_value(json!({
        "posting_id": "retry1",
        "entries": [
            { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
            { "account_id": "op", "direction": "CREDIT", "amount": 50, "asset": "USD" }
        ]
    }))
    .unwrap();
    assert!(store.post(bad).is_err());
    assert_eq!(store.entry_count(), 0);
    let good: PostingRequest = serde_json::from_value(balanced_posting("retry1")).unwrap();
    let (resp, replay) = store.post(good).unwrap();
    assert!(!replay);
    assert_eq!(resp.status, "POSTED");
    assert_eq!(store.entry_count(), 2);
}

#[test]
fn hash_chain_integrity_holds() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(balanced_posting("chain1")).unwrap();
    store.post(req).unwrap();
    assert!(store.verify_chain().is_ok());
}

#[test]
fn hash_chain_detects_tamper() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(balanced_posting("chain2")).unwrap();
    store.post(req).unwrap();
    {
        let mut state = store.inner.lock();
        state.entries[0].this_hash = "deadbeef".to_string();
    }
    assert!(store.verify_chain().is_err());
}

#[test]
fn immutability_no_update_path() {
    let store = Store::new();
    setup(&store);
    let req: PostingRequest = serde_json::from_value(balanced_posting("imm1")).unwrap();
    store.post(req).unwrap();
    let before = store.get_posting("imm1").unwrap();
    let _ = store.get_posting("imm1");
    let after = store.get_posting("imm1").unwrap();
    assert_eq!(before.entries.len(), after.entries.len());
    assert_eq!(before.hash_head, after.hash_head);
    assert_eq!(before.entries[0].entry_id, after.entries[0].entry_id);
    assert_eq!(before.entries[0].this_hash, after.entries[0].this_hash);
}

#[test]
fn segregation_user_vs_operational_funds() {
    let store = Store::new();
    store
        .create_account(
            serde_json::from_value(create_account_body("uc1", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("uc2", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    store
        .create_account(
            serde_json::from_value(create_account_body("op", "operational_fiat", "FIAT")).unwrap(),
        )
        .unwrap();
    let _ = store.post(
        serde_json::from_value(json!({
            "posting_id": "seg1",
            "entries": [
                { "account_id": "uc1", "direction": "DEBIT", "amount": 70, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 70, "asset": "USD" }
            ]
        }))
        .unwrap(),
    );
    let _ = store.post(
        serde_json::from_value(json!({
            "posting_id": "seg2",
            "entries": [
                { "account_id": "uc2", "direction": "DEBIT", "amount": 30, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 30, "asset": "USD" }
            ]
        }))
        .unwrap(),
    );
    let uc_sum = store.user_custodial_sum("USD");
    assert_eq!(uc_sum, 100);
    let op_bal = store.balance("op", "USD").unwrap();
    assert_eq!(op_bal, -100);
    assert_eq!(uc_sum, -op_bal);
}

#[test]
fn serializable_concurrency_serializes_postings() {
    let store = Store::new();
    setup(&store);
    let store1 = store.clone();
    let store2 = store.clone();
    let h1 = std::thread::spawn(move || {
        let req: PostingRequest = serde_json::from_value(balanced_posting("conc1")).unwrap();
        store1.post(req)
    });
    let h2 = std::thread::spawn(move || {
        let req: PostingRequest = serde_json::from_value(balanced_posting("conc2")).unwrap();
        store2.post(req)
    });
    let r1 = h1.join().unwrap().unwrap();
    let r2 = h2.join().unwrap().unwrap();
    assert_ne!(r1.0.entry_ids, r2.0.entry_ids);
    assert_eq!(store.entry_count(), 4);
}

#[test]
fn concurrent_duplicate_submissions_single_winner() {
    let store = Store::new();
    setup(&store);
    let store1 = store.clone();
    let store2 = store.clone();
    let body = balanced_posting("dupconcurrent");
    let body1 = body.clone();
    let h1 = std::thread::spawn(move || {
        let req: PostingRequest = serde_json::from_value(body1).unwrap();
        store1.post(req)
    });
    let h2 = std::thread::spawn(move || {
        let req: PostingRequest = serde_json::from_value(body).unwrap();
        store2.post(req)
    });
    let r1 = h1.join().unwrap().unwrap();
    let r2 = h2.join().unwrap().unwrap();
    assert_eq!(r1.0.entry_ids, r2.0.entry_ids);
    assert_eq!(store.entry_count(), 2);
}

#[test]
fn multi_asset_posting_balances_per_asset() {
    let store = Store::new();
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
    store
        .create_account(
            serde_json::from_value(create_account_body("opc", "operational_crypto", "CRYPTO"))
                .unwrap(),
        )
        .unwrap();
    let _ = store.post(
        serde_json::from_value(json!({
            "posting_id": "multi1",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" },
                { "account_id": "uc", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
                { "account_id": "opc", "direction": "CREDIT", "amount": 50, "asset": "BTC" }
            ]
        }))
        .unwrap(),
    );
    assert_eq!(store.balance("uc", "USD").unwrap(), 100);
    assert_eq!(store.balance("uc", "BTC").unwrap(), 50);
}

#[test]
fn unbalanced_per_asset_rejected() {
    let store = Store::new();
    setup(&store);
    let res = store.post(
        serde_json::from_value(json!({
            "posting_id": "ubasset",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 100, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": 100, "asset": "USD" },
                { "account_id": "uc", "direction": "DEBIT", "amount": 50, "asset": "BTC" },
                { "account_id": "op", "direction": "CREDIT", "amount": 30, "asset": "BTC" }
            ]
        }))
        .unwrap(),
    );
    assert!(res.is_err());
}

#[test]
fn inactive_account_rejected() {
    let store = Store::new();
    store
        .create_account(
            serde_json::from_value(create_account_body("uc", "user_custodial", "BOTH")).unwrap(),
        )
        .unwrap();
    {
        let mut state = store.inner.lock();
        state.accounts.get_mut("uc").unwrap().status = "INACTIVE".to_string();
    }
    let res = store.post(
        serde_json::from_value(json!({
            "posting_id": "inactive1",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": 10, "asset": "USD" },
                { "account_id": "uc", "direction": "CREDIT", "amount": 10, "asset": "USD" }
            ]
        }))
        .unwrap(),
    );
    assert!(res.is_err());
}

#[test]
fn snapshot_matches_entry_sum() {
    let store = Store::new();
    setup(&store);
    let _ = store.post(serde_json::from_value(balanced_posting("snap1")).unwrap());
    let snaps = store.write_snapshots();
    for s in &snaps {
        assert!(store.reconcile_snapshot(s), "snapshot mismatch: {:?}", s);
    }
}

#[test]
fn balance_via_snapshot_matches_direct() {
    let store = Store::new();
    setup(&store);
    let _ = store.post(serde_json::from_value(balanced_posting("bsnap1")).unwrap());
    store.write_snapshots();
    let _ = store.post(serde_json::from_value(balanced_posting("bsnap2")).unwrap());
    let direct = store.balance("uc", "USD").unwrap();
    let via = store.balance_via_snapshot("uc", "USD").unwrap();
    assert_eq!(direct, via);
}
