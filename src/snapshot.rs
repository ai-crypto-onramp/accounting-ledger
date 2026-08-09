use serde::Serialize;

use crate::posting::EntryRecord;
use crate::store::{now_iso, LedgerState};

#[derive(Debug, Clone, Serialize)]
pub struct BalanceSnapshot {
    pub account_id: String,
    pub asset: String,
    pub balance: i128,
    pub as_of_ts: String,
    pub last_entry_id: String,
    pub last_sequence: u64,
}

pub fn build_snapshot(state: &LedgerState, account_id: &str, asset: &str) -> BalanceSnapshot {
    let (balance, last_entry_id, last_sequence) = state
        .entries
        .iter()
        .filter(|e| e.account_id == account_id)
        .filter(|e| asset.is_empty() || e.asset == asset)
        .fold((0i128, String::new(), 0u64), |(bal, _last, _seq), e| {
            let delta = match e.direction.as_str() {
                "DEBIT" => e.amount as i128,
                "CREDIT" => -(e.amount as i128),
                _ => 0,
            };
            (bal + delta, e.entry_id.clone(), e.sequence_number)
        });
    BalanceSnapshot {
        account_id: account_id.to_string(),
        asset: if asset.is_empty() {
            "all".to_string()
        } else {
            asset.to_string()
        },
        balance,
        as_of_ts: now_iso(),
        last_entry_id,
        last_sequence,
    }
}

pub fn snapshot_all(state: &LedgerState) -> Vec<BalanceSnapshot> {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut snaps: Vec<BalanceSnapshot> = Vec::new();
    for e in state.entries.iter() {
        let key = (e.account_id.clone(), e.asset.clone());
        if seen.insert(key) {
            let snap = build_snapshot(state, &e.account_id, &e.asset);
            snaps.push(snap);
        }
    }
    snaps
}

pub fn reconcile_snapshot(state: &LedgerState, snap: &BalanceSnapshot) -> bool {
    let computed = build_snapshot(state, &snap.account_id, &snap.asset);
    computed.balance == snap.balance && computed.last_entry_id == snap.last_entry_id
}

pub fn last_entry_id_before(
    state: &LedgerState,
    account_id: &str,
    asset: &str,
    as_of: &str,
) -> String {
    state
        .entries
        .iter()
        .filter(|e| e.account_id == account_id)
        .filter(|e| asset.is_empty() || e.asset == asset)
        .filter(|e| e.created_at.as_str() <= as_of)
        .map(|e| e.entry_id.clone())
        .next_back()
        .unwrap_or_default()
}

pub fn entries_since<'a>(state: &'a LedgerState, after_entry_id: &str) -> Vec<&'a EntryRecord> {
    if after_entry_id.is_empty() {
        return state.entries.iter().collect();
    }
    let mut found = false;
    state
        .entries
        .iter()
        .filter(|e| {
            if found {
                true
            } else if e.entry_id == after_entry_id {
                found = true;
                false
            } else {
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::GENESIS_HASH;
    use crate::posting::EntryRecord;

    fn entry(id: &str, seq: u64) -> EntryRecord {
        EntryRecord {
            entry_id: id.to_string(),
            posting_id: "p".to_string(),
            account_id: "acct".to_string(),
            direction: "DEBIT".to_string(),
            amount: 1,
            asset: "USD".to_string(),
            sequence_number: seq,
            prev_hash: GENESIS_HASH.to_string(),
            this_hash: format!("hash-{}", id),
            created_at: format!("100{}", seq),
        }
    }

    fn state_with(entries: Vec<EntryRecord>) -> LedgerState {
        let mut s = LedgerState::new();
        s.entries = entries;
        s
    }

    #[test]
    fn build_snapshot_sums_and_records_last() {
        let state = state_with(vec![entry("e1", 1), entry("e2", 2), entry("e3", 3)]);
        let snap = build_snapshot(&state, "acct", "USD");
        assert_eq!(snap.balance, 3);
        assert_eq!(snap.last_entry_id, "e3");
        assert_eq!(snap.last_sequence, 3);
        assert_eq!(snap.asset, "USD");
    }

    #[test]
    fn build_snapshot_filters_by_asset_and_account() {
        let state = state_with(vec![
            entry("e1", 1),
            {
                let mut e = entry("e2", 2);
                e.asset = "BTC".to_string();
                e
            },
            {
                let mut e = entry("e3", 3);
                e.account_id = "other".to_string();
                e
            },
        ]);
        let snap = build_snapshot(&state, "acct", "USD");
        assert_eq!(snap.balance, 1);
        assert_eq!(snap.last_entry_id, "e1");
    }

    #[test]
    fn build_snapshot_empty_asset_returns_all_label() {
        let state = state_with(vec![entry("e1", 1)]);
        let snap = build_snapshot(&state, "acct", "");
        assert_eq!(snap.asset, "all");
        assert_eq!(snap.balance, 1);
    }

    #[test]
    fn build_snapshot_credit_direction_subtracts() {
        let state = state_with(vec![{
            let mut e = entry("e1", 1);
            e.direction = "CREDIT".to_string();
            e.amount = 50;
            e
        }]);
        let snap = build_snapshot(&state, "acct", "USD");
        assert_eq!(snap.balance, -50);
    }

    #[test]
    fn build_snapshot_unknown_direction_is_zero_delta() {
        let state = state_with(vec![{
            let mut e = entry("e1", 1);
            e.direction = "SIDEWAYS".to_string();
            e
        }]);
        let snap = build_snapshot(&state, "acct", "USD");
        assert_eq!(snap.balance, 0);
    }

    #[test]
    fn snapshot_all_dedups_by_account_and_asset() {
        let state = state_with(vec![
            entry("e1", 1),
            {
                let mut e = entry("e2", 2);
                e.asset = "BTC".to_string();
                e
            },
            entry("e3", 3),
        ]);
        let snaps = snapshot_all(&state);
        assert_eq!(snaps.len(), 2);
        let assets: Vec<&str> = snaps.iter().map(|s| s.asset.as_str()).collect();
        assert!(assets.contains(&"USD"));
        assert!(assets.contains(&"BTC"));
    }

    #[test]
    fn reconcile_snapshot_matches_and_mismatches() {
        let state = state_with(vec![entry("e1", 1), entry("e2", 2)]);
        let good = build_snapshot(&state, "acct", "USD");
        assert!(reconcile_snapshot(&state, &good));
        let mut bad = good.clone();
        bad.balance += 1;
        assert!(!reconcile_snapshot(&state, &bad));
        let mut bad2 = good.clone();
        bad2.last_entry_id = "nope".to_string();
        assert!(!reconcile_snapshot(&state, &bad2));
    }

    #[test]
    fn last_entry_id_before_filters_by_timestamp() {
        let state = state_with(vec![
            entry("e1", 1),
            {
                let mut e = entry("e2", 2);
                e.created_at = "200".to_string();
                e
            },
            {
                let mut e = entry("e3", 3);
                e.created_at = "300".to_string();
                e
            },
        ]);
        assert_eq!(last_entry_id_before(&state, "acct", "USD", "150"), "e1");
        assert_eq!(last_entry_id_before(&state, "acct", "USD", "250"), "e2");
        assert_eq!(last_entry_id_before(&state, "acct", "USD", "0"), "");
        assert_eq!(last_entry_id_before(&state, "acct", "", "350"), "e3");
    }

    #[test]
    fn entries_since_empty_returns_all() {
        let state = state_with(vec![entry("e1", 1), entry("e2", 2)]);
        let all = entries_since(&state, "");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn entries_since_after_id_returns_tail() {
        let state = state_with(vec![entry("e1", 1), entry("e2", 2), entry("e3", 3)]);
        let tail = entries_since(&state, "e1");
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].entry_id, "e2");
        assert_eq!(tail[1].entry_id, "e3");
    }

    #[test]
    fn entries_since_unknown_id_returns_empty() {
        let state = state_with(vec![entry("e1", 1), entry("e2", 2)]);
        let none = entries_since(&state, "nope");
        assert!(none.is_empty());
    }
}
