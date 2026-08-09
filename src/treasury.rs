use serde::Serialize;

use crate::posting::PostingRequest;
use crate::store::Store;

#[derive(Debug, Clone, Serialize)]
pub struct TreasuryPattern {
    pub name: &'static str,
    pub posting_id: String,
}

pub fn hedge_entry(
    store: &Store,
    posting_id: &str,
    treasury_account: &str,
    venue_account: &str,
    asset: &str,
    amount: u64,
) -> Result<(), String> {
    let req = PostingRequest {
        posting_id: posting_id.to_string(),
        entries: vec![
            crate::posting::EntryInput {
                account_id: treasury_account.to_string(),
                direction: "DEBIT".to_string(),
                amount,
                asset: asset.to_string(),
            },
            crate::posting::EntryInput {
                account_id: venue_account.to_string(),
                direction: "CREDIT".to_string(),
                amount,
                asset: asset.to_string(),
            },
        ],
        memo: Some(format!("hedge:{}", posting_id)),
        ref_tx_id: Some(posting_id.to_string()),
    };
    store.post(req).map(|_| ()).map_err(|e| e.message())
}

pub fn rebalance(
    store: &Store,
    posting_id: &str,
    from_account: &str,
    to_account: &str,
    asset: &str,
    amount: u64,
) -> Result<(), String> {
    let req = PostingRequest {
        posting_id: posting_id.to_string(),
        entries: vec![
            crate::posting::EntryInput {
                account_id: to_account.to_string(),
                direction: "DEBIT".to_string(),
                amount,
                asset: asset.to_string(),
            },
            crate::posting::EntryInput {
                account_id: from_account.to_string(),
                direction: "CREDIT".to_string(),
                amount,
                asset: asset.to_string(),
            },
        ],
        memo: Some(format!("rebalance:{}", posting_id)),
        ref_tx_id: Some(posting_id.to_string()),
    };
    store.post(req).map(|_| ()).map_err(|e| e.message())
}

pub fn derive_posting_id(saga_id: &str, step: &str) -> String {
    format!("{}_{}", saga_id, step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::CreateAccountRequest;
    use crate::store::Store;

    fn acct_req(id: &str, type_name: &str, asset_class: &str) -> CreateAccountRequest {
        serde_json::from_value(serde_json::json!({
            "account_id": id,
            "type": type_name,
            "asset_class": asset_class,
            "label": format!("{}-{}", type_name, id),
        }))
        .unwrap()
    }

    fn setup(store: &Store) {
        store
            .create_account(acct_req("treasury", "treasury_crypto", "CRYPTO"))
            .unwrap();
        store
            .create_account(acct_req("venue", "venue_settlement", "BOTH"))
            .unwrap();
        store
            .create_account(acct_req("op", "operational_crypto", "CRYPTO"))
            .unwrap();
    }

    fn balanced_two(
        account_id: &str,
        posting_id: &str,
        asset: &str,
        amount: u64,
    ) -> crate::posting::PostingRequest {
        serde_json::from_value(serde_json::json!({
            "posting_id": posting_id,
            "entries": [
                { "account_id": account_id, "direction": "DEBIT", "amount": amount, "asset": asset },
                { "account_id": "op", "direction": "CREDIT", "amount": amount, "asset": asset }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn derive_posting_id_concatenates() {
        assert_eq!(derive_posting_id("saga-1", "step3"), "saga-1_step3");
    }

    #[test]
    fn hedge_entry_posts_balanced() {
        let store = Store::new();
        setup(&store);
        hedge_entry(&store, "hedge-1", "treasury", "venue", "BTC", 1000).unwrap();
        let bal_t = store.balance("treasury", "BTC").unwrap();
        let bal_v = store.balance("venue", "BTC").unwrap();
        assert_eq!(bal_t, 1000);
        assert_eq!(bal_v, -1000);
        let posting = store.get_posting("hedge-1").unwrap();
        assert_eq!(posting.memo.as_deref(), Some("hedge:hedge-1"));
        assert_eq!(posting.ref_tx_id.as_deref(), Some("hedge-1"));
    }

    #[test]
    fn hedge_entry_propagates_post_error() {
        let store = Store::new();
        setup(&store);
        // Unknown asset -> error from store.post
        let err = hedge_entry(&store, "hedge-bad", "treasury", "venue", "NOPE", 10).unwrap_err();
        assert!(err.contains("unknown asset"));
        assert!(store.get_posting("hedge-bad").is_none());
    }

    #[test]
    fn rebalance_moves_funds() {
        let store = Store::new();
        setup(&store);
        // Seed venue with BTC via a balanced posting against op.
        store
            .post(balanced_two("venue", "seed1", "BTC", 500))
            .unwrap();
        // Now rebalance 200 from venue -> treasury.
        rebalance(&store, "reb1", "venue", "treasury", "BTC", 200).unwrap();
        assert_eq!(store.balance("treasury", "BTC").unwrap(), 200);
        assert_eq!(store.balance("venue", "BTC").unwrap(), 300);
        let posting = store.get_posting("reb1").unwrap();
        assert_eq!(posting.memo.as_deref(), Some("rebalance:reb1"));
        assert_eq!(posting.ref_tx_id.as_deref(), Some("reb1"));
    }

    #[test]
    fn rebalance_propagates_error() {
        let store = Store::new();
        setup(&store);
        let err = rebalance(&store, "reb-bad", "venue", "treasury", "NOPE", 10).unwrap_err();
        assert!(err.contains("unknown asset"));
    }
}
