use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chart::Direction;

#[derive(Debug, Clone, Deserialize)]
pub struct EntryInput {
    pub account_id: String,
    pub direction: String,
    pub amount: u64,
    pub asset: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostingRequest {
    pub posting_id: String,
    pub entries: Vec<EntryInput>,
    #[serde(default)]
    pub memo: Option<String>,
    #[serde(default)]
    pub ref_tx_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostingResponse {
    pub posting_id: String,
    pub status: String,
    pub entry_ids: Vec<String>,
    pub hash_head: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntryRecord {
    pub entry_id: String,
    pub posting_id: String,
    pub account_id: String,
    pub direction: String,
    pub amount: u64,
    pub asset: String,
    pub sequence_number: u64,
    pub prev_hash: String,
    pub this_hash: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostingRecord {
    pub posting_id: String,
    pub ref_tx_id: Option<String>,
    pub memo: Option<String>,
    pub status: String,
    pub hash_head: String,
    pub entries: Vec<EntryRecord>,
    pub created_at: String,
}

pub fn canonical_bytes(
    prev_hash: &str,
    entry_id: &str,
    account_id: &str,
    direction: Direction,
    amount: u64,
    asset: &str,
    created_at: &str,
) -> Vec<u8> {
    let dir_str = match direction {
        Direction::Debit => "DEBIT",
        Direction::Credit => "CREDIT",
    };
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        prev_hash, entry_id, account_id, dir_str, amount, asset, created_at
    )
    .into_bytes()
}

/// Computes the SHA-256 hash of `prev_hash || salt || canonical`.
/// The salt is mixed between the previous hash and the canonical payload so
/// that anyone with DB write access but without the salt cannot forge a
/// consistent chain.
pub fn compute_hash(prev_hash: &str, salt: &str, canonical: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash.as_bytes());
    hasher.update(salt.as_bytes());
    hasher.update(canonical);
    hex::encode(hasher.finalize())
}
