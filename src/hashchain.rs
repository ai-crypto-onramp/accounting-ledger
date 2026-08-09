use crate::chart::Direction;
use crate::chart::GENESIS_HASH;
use crate::posting;
use crate::store::LedgerState;

#[derive(Debug, Clone)]
pub struct ChainBreak {
    pub entry_id: String,
    pub reason: String,
}

pub fn verify_chain(state: &LedgerState, salt: &str) -> Result<(), ChainBreak> {
    let mut prev_hash = GENESIS_HASH.to_string();
    for e in &state.entries {
        let dir = match e.direction.as_str() {
            "DEBIT" => Direction::Debit,
            "CREDIT" => Direction::Credit,
            other => {
                return Err(ChainBreak {
                    entry_id: e.entry_id.clone(),
                    reason: format!("invalid direction: {}", other),
                })
            }
        };
        let canonical = posting::canonical_bytes(
            &prev_hash,
            &e.entry_id,
            &e.account_id,
            dir,
            e.amount,
            &e.asset,
            &e.created_at,
        );
        let expected = posting::compute_hash(&prev_hash, salt, &canonical);
        if e.prev_hash != prev_hash {
            return Err(ChainBreak {
                entry_id: e.entry_id.clone(),
                reason: format!(
                    "prev_hash mismatch: expected {}, got {}",
                    prev_hash, e.prev_hash
                ),
            });
        }
        if e.this_hash != expected {
            return Err(ChainBreak {
                entry_id: e.entry_id.clone(),
                reason: format!(
                    "this_hash mismatch: expected {}, got {}",
                    expected, e.this_hash
                ),
            });
        }
        prev_hash = e.this_hash.clone();
    }
    Ok(())
}

pub fn global_head(state: &LedgerState) -> String {
    state
        .entries
        .last()
        .map(|e| e.this_hash.clone())
        .unwrap_or_else(|| GENESIS_HASH.to_string())
}

pub fn posting_head(state: &LedgerState, posting_id: &str) -> Option<String> {
    state.postings.get(posting_id).map(|p| p.hash_head.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::GENESIS_HASH;
    use crate::posting::EntryRecord;
    use crate::store::LedgerState;

    fn entry(
        id: &str,
        seq: u64,
        prev_hash: &str,
        this_hash: &str,
        dir: &str,
        amount: u64,
    ) -> EntryRecord {
        EntryRecord {
            entry_id: id.to_string(),
            posting_id: "p".to_string(),
            account_id: "acct".to_string(),
            direction: dir.to_string(),
            amount,
            asset: "USD".to_string(),
            sequence_number: seq,
            prev_hash: prev_hash.to_string(),
            this_hash: this_hash.to_string(),
            created_at: "1000".to_string(),
        }
    }

    #[test]
    fn verify_chain_empty_is_ok() {
        let state = LedgerState::new();
        assert!(verify_chain(&state, "").is_ok());
    }

    #[test]
    fn verify_chain_valid_chain_passes() {
        // Build e1 with a real computed this_hash from GENESIS_HASH.
        let c1 = posting::canonical_bytes(
            GENESIS_HASH,
            "e1",
            "acct",
            Direction::Debit,
            10,
            "USD",
            "1000",
        );
        let h1 = posting::compute_hash(GENESIS_HASH, "", &c1);
        let e1 = entry("e1", 1, GENESIS_HASH, &h1, "DEBIT", 10);
        // Compute proper hash for e2 to follow e1.
        let c2 = posting::canonical_bytes(&h1, "e2", "acct", Direction::Credit, 10, "USD", "1000");
        let h2 = posting::compute_hash(&h1, "", &c2);
        let e2 = entry("e2", 2, &h1, &h2, "CREDIT", 10);
        let mut state = LedgerState::new();
        state.entries = vec![e1, e2];
        assert!(verify_chain(&state, "").is_ok());
    }

    #[test]
    fn verify_chain_invalid_direction_returns_chain_break() {
        let e1 = entry("e1", 1, GENESIS_HASH, "h1", "SIDEWAYS", 10);
        let mut state = LedgerState::new();
        state.entries = vec![e1];
        let err = verify_chain(&state, "").unwrap_err();
        assert_eq!(err.entry_id, "e1");
        assert!(err.reason.contains("invalid direction"));
    }

    #[test]
    fn verify_chain_prev_hash_mismatch_returns_chain_break() {
        let e1 = entry("e1", 1, "wrong-prev", "h1", "DEBIT", 10);
        let mut state = LedgerState::new();
        state.entries = vec![e1];
        let err = verify_chain(&state, "").unwrap_err();
        assert_eq!(err.entry_id, "e1");
        assert!(err.reason.contains("prev_hash mismatch"));
    }

    #[test]
    fn verify_chain_this_hash_mismatch_returns_chain_break() {
        let e1 = entry("e1", 1, GENESIS_HASH, "wrong-this", "DEBIT", 10);
        let mut state = LedgerState::new();
        state.entries = vec![e1];
        let err = verify_chain(&state, "").unwrap_err();
        assert_eq!(err.entry_id, "e1");
        assert!(err.reason.contains("this_hash mismatch"));
    }

    #[test]
    fn verify_chain_with_salt_detects_tamper() {
        // Entries hashed with salt "s1"; verifying with salt "s2" must break.
        let c1 = posting::canonical_bytes(
            GENESIS_HASH,
            "e1",
            "acct",
            Direction::Debit,
            10,
            "USD",
            "1000",
        );
        let h1 = posting::compute_hash(GENESIS_HASH, "s1", &c1);
        let e1 = entry("e1", 1, GENESIS_HASH, &h1, "DEBIT", 10);
        let mut state = LedgerState::new();
        state.entries = vec![e1];
        assert!(verify_chain(&state, "s1").is_ok());
        assert!(verify_chain(&state, "s2").is_err());
    }

    #[test]
    fn global_head_empty_is_genesis() {
        let state = LedgerState::new();
        assert_eq!(global_head(&state), GENESIS_HASH);
    }

    #[test]
    fn global_head_returns_last_this_hash() {
        let mut state = LedgerState::new();
        state.entries = vec![entry("e1", 1, GENESIS_HASH, "h1", "DEBIT", 1)];
        assert_eq!(global_head(&state), "h1");
    }

    #[test]
    fn posting_head_returns_some_for_known() {
        use crate::posting::PostingRecord;
        let mut state = LedgerState::new();
        state.postings.insert(
            "p1".to_string(),
            PostingRecord {
                posting_id: "p1".to_string(),
                ref_tx_id: None,
                memo: None,
                status: "POSTED".to_string(),
                hash_head: "head-p1".to_string(),
                entries: vec![],
                created_at: "1000".to_string(),
            },
        );
        assert_eq!(posting_head(&state, "p1").as_deref(), Some("head-p1"));
        assert!(posting_head(&state, "missing").is_none());
    }
}
