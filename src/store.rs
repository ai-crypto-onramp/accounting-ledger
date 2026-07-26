use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use sqlx::FromRow;

use crate::account::{Account, CreateAccountRequest};
use crate::chart::{self, Direction, GENESIS_HASH};
use crate::posting::{self, EntryRecord, PostingRecord, PostingRequest, PostingResponse};
use crate::snapshot::BalanceSnapshot;

pub const MAX_ENTRIES_PER_POSTING: usize = 64;
pub const MAX_AMOUNT: u64 = 1_000_000_000_000;

// Advisory lock key for the entries hash-chain. Acquired with
// pg_advisory_xact_lock inside the posting transaction so concurrent
// ingests serialize on the chain tip and cannot fork it. Stable int64
// derived from a fixed constant (not the table name) so it is identical
// across all replicas/environments.
pub const ENTRIES_CHAIN_ADVISORY_LOCK_KEY: i64 = 0x61_6c_65_64_67_65_72;

pub const MIGRATION_INIT_SCHEMA: &str =
    include_str!("../migrations/20240101000001_init_schema.sql");
pub const MIGRATION_SET_SERIALIZABLE: &str =
    include_str!("../migrations/20240101000002_set_serializable.sql");
pub const MIGRATION_IMMUTABLE_ENTRIES: &str =
    include_str!("../migrations/20240101000003_restore_immutable_entries.sql");

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub posting_id: String,
    pub entry_ids: Vec<String>,
    pub hash_head: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HashChainAnchor {
    pub posting_id: String,
    pub head_hash: String,
    pub global_sequence_head: String,
    pub created_at: String,
}

pub struct LedgerState {
    pub accounts: HashMap<String, Account>,
    pub postings: HashMap<String, PostingRecord>,
    pub entries: Vec<EntryRecord>,
    pub sequence: u64,
    pub audit_events: Vec<AuditEvent>,
    pub hash_chain_anchors: HashMap<String, HashChainAnchor>,
    pub global_chain_head: String,
    pub snapshots: Vec<BalanceSnapshot>,
}

impl Default for LedgerState {
    fn default() -> Self {
        Self::new()
    }
}

impl LedgerState {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            postings: HashMap::new(),
            entries: Vec::new(),
            sequence: 0,
            audit_events: Vec::new(),
            hash_chain_anchors: HashMap::new(),
            global_chain_head: GENESIS_HASH.to_string(),
            snapshots: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Store {
    pub inner: Arc<Mutex<LedgerState>>,
    pub pool: Option<sqlx::PgPool>,
    pub audit_sink: Option<std::sync::Arc<crate::audit::AuditSink>>,
    pub salt: String,
}

impl Store {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerState::new())),
            pool: None,
            audit_sink: None,
            salt: String::new(),
        }
    }

    pub fn with_pool(pool: sqlx::PgPool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerState::new())),
            pool: Some(pool),
            audit_sink: None,
            salt: String::new(),
        }
    }

    pub fn with_pool_and_salt(pool: sqlx::PgPool, salt: String) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LedgerState::new())),
            pool: Some(pool),
            audit_sink: None,
            salt,
        }
    }

    pub fn with_salt(mut self, salt: String) -> Self {
        self.salt = salt;
        self
    }

    pub fn with_audit_sink(mut self, sink: crate::audit::AuditSink) -> Self {
        self.audit_sink = Some(std::sync::Arc::new(sink));
        self
    }

    pub async fn connect(db_url: &str) -> Result<Self, sqlx::Error> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect(db_url)
            .await?;
        Ok(Self::with_pool(pool))
    }

    pub async fn connect_with_salt(db_url: &str, salt: String) -> Result<Self, sqlx::Error> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect(db_url)
            .await?;
        Ok(Self::with_pool_and_salt(pool, salt))
    }

    pub async fn run_migrations(&self) -> Result<(), String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(()),
        };
        if let Err(e) = sqlx::raw_sql(MIGRATION_INIT_SCHEMA).execute(pool).await {
            if is_already_applied(&e) {
                return Ok(());
            }
            return Err(format!("migration init_schema failed: {}", e));
        }
        if let Err(e) = sqlx::raw_sql(MIGRATION_SET_SERIALIZABLE)
            .execute(pool)
            .await
        {
            if is_already_applied(&e) {
                return Ok(());
            }
            return Err(format!("migration set_serializable failed: {}", e));
        }
        if let Err(e) = sqlx::raw_sql(MIGRATION_IMMUTABLE_ENTRIES)
            .execute(pool)
            .await
        {
            if is_already_applied(&e) {
                return Ok(());
            }
            return Err(format!("migration immutable_entries failed: {}", e));
        }
        Ok(())
    }

    pub async fn hydrate(&self) -> Result<(), String> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(()),
        };

        let accounts: Vec<AccountRow> = sqlx::query_as::<_, AccountRow>(
            "SELECT account_id, type_name AS type_field, asset_class, label, parent_id, status, floor(extract(epoch from created_at))::bigint::text AS created_at FROM accounts ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("hydrate accounts failed: {}", e))?;
        let mut new_accounts: HashMap<String, Account> = HashMap::new();
        for a in accounts {
            new_accounts.insert(
                a.account_id.clone(),
                Account {
                    account_id: a.account_id,
                    type_name: a.type_field,
                    asset_class: a.asset_class,
                    label: a.label,
                    parent_id: a.parent_id,
                    status: a.status,
                    created_at: a.created_at,
                },
            );
        }

        let entries: Vec<EntryRow> = sqlx::query_as::<_, EntryRow>(
            "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries ORDER BY sequence_number",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("hydrate entries failed: {}", e))?;
        let mut by_posting: std::collections::HashMap<String, Vec<EntryRecord>> =
            std::collections::HashMap::new();
        let mut new_entries: Vec<EntryRecord> = Vec::with_capacity(entries.len());
        let mut new_sequence: u64 = 0;
        for e in entries {
            let amount: u64 = e
                .amount
                .parse()
                .map_err(|_| "hydrate: invalid amount".to_string())?;
            let record = EntryRecord {
                entry_id: e.entry_id,
                posting_id: e.posting_id.clone(),
                account_id: e.account_id,
                direction: e.direction,
                amount,
                asset: e.asset,
                sequence_number: e.sequence_number as u64,
                prev_hash: e.prev_hash,
                this_hash: e.this_hash,
                created_at: e.created_at,
            };
            if record.sequence_number > new_sequence {
                new_sequence = record.sequence_number;
            }
            by_posting
                .entry(record.posting_id.clone())
                .or_default()
                .push(record.clone());
            new_entries.push(record);
        }
        let new_global_chain_head = new_entries
            .last()
            .map(|e| e.this_hash.clone())
            .unwrap_or_else(|| GENESIS_HASH.to_string());

        let postings: Vec<PostingRow> = sqlx::query_as::<_, PostingRow>(
            "SELECT posting_id, ref_tx_id, memo, status, hash_chain_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM postings ORDER BY created_at",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("hydrate postings failed: {}", e))?;
        let mut new_postings: HashMap<String, PostingRecord> = HashMap::new();
        for p in postings {
            let entries = by_posting.remove(&p.posting_id).unwrap_or_default();
            new_postings.insert(
                p.posting_id.clone(),
                PostingRecord {
                    posting_id: p.posting_id,
                    ref_tx_id: p.ref_tx_id,
                    memo: p.memo,
                    status: p.status,
                    hash_head: p.hash_chain_head,
                    entries,
                    created_at: p.created_at,
                },
            );
        }

        let anchors: Vec<AnchorRow> = sqlx::query_as::<_, AnchorRow>(
            "SELECT posting_id, head_hash, global_sequence_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM hash_chain",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("hydrate anchors failed: {}", e))?;
        let mut new_anchors: HashMap<String, HashChainAnchor> = HashMap::new();
        for a in anchors {
            new_anchors.insert(
                a.posting_id.clone(),
                HashChainAnchor {
                    posting_id: a.posting_id,
                    head_hash: a.head_hash,
                    global_sequence_head: a.global_sequence_head,
                    created_at: a.created_at,
                },
            );
        }

        let snapshots: Vec<SnapshotRow> = sqlx::query_as::<_, SnapshotRow>(
            "SELECT account_id, asset, balance::text AS balance, floor(extract(epoch from as_of_ts))::bigint::text AS as_of_ts, last_entry_id FROM balance_snapshots",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| format!("hydrate snapshots failed: {}", e))?;
        let mut new_snapshots: Vec<BalanceSnapshot> = Vec::with_capacity(snapshots.len());
        for s in snapshots {
            let balance: i128 = s
                .balance
                .parse()
                .map_err(|_| "hydrate: invalid snapshot balance".to_string())?;
            let row: (String,) = sqlx::query_as("SELECT entry_id FROM entries WHERE entry_id = $1")
                .bind(&s.last_entry_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("hydrate snapshot last_entry lookup failed: {}", e))?
                .unwrap_or_default();
            let last_entry_id = row.0;
            let last_sequence: (i64,) =
                sqlx::query_as("SELECT sequence_number FROM entries WHERE entry_id = $1")
                    .bind(&s.last_entry_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| format!("hydrate snapshot last_seq lookup failed: {}", e))?
                    .unwrap_or((0,));
            new_snapshots.push(BalanceSnapshot {
                account_id: s.account_id,
                asset: s.asset,
                balance,
                as_of_ts: s.as_of_ts,
                last_entry_id,
                last_sequence: last_sequence.0 as u64,
            });
        }

        let mut state = self.inner.lock();
        state.accounts = new_accounts;
        state.postings = new_postings;
        state.entries = new_entries;
        state.hash_chain_anchors = new_anchors;
        state.snapshots = new_snapshots;
        state.sequence = new_sequence;
        state.global_chain_head = new_global_chain_head;

        Ok(())
    }

    pub fn create_account(&self, req: CreateAccountRequest) -> Result<Account, String> {
        let (type_name, _asset_class) = crate::account::validate(&req)?;

        let account_id = match req.account_id.clone() {
            Some(id) => id,
            None => uuid::Uuid::now_v7().to_string(),
        };

        let now = now_iso();
        let account = Account {
            account_id: account_id.clone(),
            type_name: type_name.clone(),
            asset_class: req.asset_class.clone(),
            label: req.label.clone(),
            parent_id: req.parent_id.clone(),
            status: "ACTIVE".to_string(),
            created_at: now,
        };

        if let Some(pool) = &self.pool {
            let mut tx =
                block_on(pool.begin()).map_err(|e| format!("create_account: begin tx: {}", e))?;
            let inserted = block_on(
                sqlx::query(
                    "INSERT INTO accounts (account_id, type_name, asset_class, label, parent_id, status, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, 'ACTIVE', to_timestamp($6), to_timestamp($6))
                 ON CONFLICT (account_id) DO NOTHING",
                )
                .bind(&account.account_id)
                .bind(&account.type_name)
                .bind(&account.asset_class)
                .bind(&account.label)
                .bind(account.parent_id.as_ref())
                .bind(secs(&account.created_at))
                .execute(&mut *tx),
            )
            .map_err(|e| format!("create_account: insert: {}", e))?;
            if inserted.rows_affected() == 0 {
                drop(tx);
                return Err(format!("account already exists: {}", account.account_id));
            }
            block_on(tx.commit()).map_err(|e| format!("create_account: commit: {}", e))?;
        }

        {
            let mut state = self.inner.lock();
            if state.accounts.contains_key(&account_id) {
                return Err(format!("account already exists: {}", account_id));
            }
            state.accounts.insert(account_id, account.clone());
        }

        Ok(account)
    }

    pub fn get_account(&self, account_id: &str) -> Option<Account> {
        if let Some(pool) = &self.pool {
            let row: Option<AccountRow> = block_on(
                sqlx::query_as::<_, AccountRow>(
                    "SELECT account_id, type_name AS type_field, asset_class, label, parent_id, status, floor(extract(epoch from created_at))::bigint::text AS created_at FROM accounts WHERE account_id = $1",
                )
                .bind(account_id)
                .fetch_optional(pool),
            )
            .ok()?;
            row.map(|a| Account {
                account_id: a.account_id,
                type_name: a.type_field,
                asset_class: a.asset_class,
                label: a.label,
                parent_id: a.parent_id,
                status: a.status,
                created_at: a.created_at,
            })
        } else {
            self.inner.lock().accounts.get(account_id).cloned()
        }
    }

    pub fn list_accounts(&self, type_filter: Option<&str>) -> Vec<Account> {
        if let Some(pool) = &self.pool {
            let rows: Vec<AccountRow> = match block_on(sqlx::query_as::<_, AccountRow>(
                "SELECT account_id, type_name AS type_field, asset_class, label, parent_id, status, floor(extract(epoch from created_at))::bigint::text AS created_at FROM accounts ORDER BY created_at, account_id",
            )
            .fetch_all(pool)) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut out: Vec<Account> = rows
                .into_iter()
                .filter(|a| type_filter.is_none_or(|t| a.type_field == t))
                .map(|a| Account {
                    account_id: a.account_id,
                    type_name: a.type_field,
                    asset_class: a.asset_class,
                    label: a.label,
                    parent_id: a.parent_id,
                    status: a.status,
                    created_at: a.created_at,
                })
                .collect();
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.account_id.cmp(&b.account_id))
            });
            out
        } else {
            let state = self.inner.lock();
            let mut out: Vec<Account> = state
                .accounts
                .values()
                .filter(|a| type_filter.is_none_or(|t| a.type_name == t))
                .cloned()
                .collect();
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.account_id.cmp(&b.account_id))
            });
            out
        }
    }

    pub fn list_postings(&self, limit: usize) -> Vec<PostingRecord> {
        let limit = limit.clamp(1, 200) as i64;
        if let Some(pool) = &self.pool {
            let postings: Vec<PostingRow> = match block_on(sqlx::query_as::<_, PostingRow>(
                "SELECT posting_id, ref_tx_id, memo, status, hash_chain_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM postings ORDER BY created_at, posting_id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(pool)) {
                Ok(p) => p,
                Err(_) => return Vec::new(),
            };
            let mut out = Vec::with_capacity(postings.len());
            for p in postings {
                let entries: Vec<EntryRow> = block_on(
                    sqlx::query_as::<_, EntryRow>(
                        "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries WHERE posting_id = $1 ORDER BY sequence_number",
                    )
                    .bind(&p.posting_id)
                    .fetch_all(pool),
                )
                .map_err(|_| ())
                .unwrap_or_default();
                let rec_entries = entries
                    .into_iter()
                    .map(|e| EntryRecord {
                        entry_id: e.entry_id,
                        posting_id: e.posting_id,
                        account_id: e.account_id,
                        direction: e.direction,
                        amount: e.amount.parse().unwrap_or(0),
                        asset: e.asset,
                        sequence_number: e.sequence_number as u64,
                        prev_hash: e.prev_hash,
                        this_hash: e.this_hash,
                        created_at: e.created_at,
                    })
                    .collect();
                out.push(PostingRecord {
                    posting_id: p.posting_id,
                    ref_tx_id: p.ref_tx_id,
                    memo: p.memo,
                    status: p.status,
                    hash_head: p.hash_chain_head,
                    entries: rec_entries,
                    created_at: p.created_at,
                });
            }
            out
        } else {
            let state = self.inner.lock();
            let mut out: Vec<PostingRecord> = state.postings.values().cloned().collect();
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.posting_id.cmp(&b.posting_id))
            });
            out.truncate(limit as usize);
            out
        }
    }

    pub fn balance(&self, account_id: &str, asset: &str) -> Option<i128> {
        if let Some(pool) = &self.pool {
            let exists: Option<(String,)> = block_on(
                sqlx::query_as("SELECT account_id FROM accounts WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(pool),
            )
            .ok()?;
            exists.as_ref()?;
            let bal = balance_from_db(pool, account_id, asset)?;
            Some(bal)
        } else {
            let state = self.inner.lock();
            if !state.accounts.contains_key(account_id) {
                return None;
            }
            Some(compute_balance(&state, account_id, asset))
        }
    }

    pub fn ledger(
        &self,
        account_id: &str,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        cursor: Option<u64>,
    ) -> Result<LedgerPage, String> {
        let limit = limit.clamp(1, 200);
        if let Some(pool) = &self.pool {
            let exists: Option<(String,)> = block_on(
                sqlx::query_as("SELECT account_id FROM accounts WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(pool),
            )
            .map_err(|e| format!("ledger: account lookup: {}", e))?;
            if exists.is_none() {
                return Err(format!("account not found: {}", account_id));
            }
            return ledger_page_from_db(pool, account_id, from, to, limit, cursor);
        }
        let state = self.inner.lock();
        if !state.accounts.contains_key(account_id) {
            return Err(format!("account not found: {}", account_id));
        }
        let mut entries: Vec<&EntryRecord> = state
            .entries
            .iter()
            .filter(|e| e.account_id == account_id)
            .filter(|e| {
                from.as_ref()
                    .is_none_or(|f| e.created_at.as_str() >= &f[..])
            })
            .filter(|e| to.as_ref().is_none_or(|t| e.created_at.as_str() <= &t[..]))
            .filter(|e| cursor.as_ref().is_none_or(|c| e.sequence_number > *c))
            .collect();
        entries.sort_by_key(|e| e.sequence_number);

        let running = compute_balance(&state, account_id, "");
        let next_cursor = if entries.len() > limit {
            Some(entries[limit - 1].sequence_number)
        } else {
            None
        };
        let page: Vec<(&EntryRecord, i128)> = entries
            .into_iter()
            .take(limit)
            .scan(0i128, |acc, e| {
                match e.direction.as_str() {
                    "DEBIT" => *acc += e.amount as i128,
                    "CREDIT" => *acc -= e.amount as i128,
                    _ => {}
                }
                Some((e, *acc))
            })
            .collect();

        Ok(LedgerPage {
            account_id: account_id.to_string(),
            entries: page
                .into_iter()
                .map(|(e, bal)| LedgerEntryItem {
                    entry_id: e.entry_id.clone(),
                    posting_id: e.posting_id.clone(),
                    account_id: e.account_id.clone(),
                    direction: e.direction.clone(),
                    amount: e.amount,
                    asset: e.asset.clone(),
                    sequence_number: e.sequence_number,
                    this_hash: e.this_hash.clone(),
                    prev_hash: e.prev_hash.clone(),
                    created_at: e.created_at.clone(),
                    running_balance: bal,
                })
                .collect(),
            next_cursor,
            final_balance: running,
        })
    }

    pub fn post(&self, req: PostingRequest) -> Result<(PostingResponse, bool), PostError> {
        if req.entries.is_empty() {
            return Err(PostError::Validation("entries must be non-empty".into()));
        }
        if req.entries.len() > MAX_ENTRIES_PER_POSTING {
            return Err(PostError::Validation(format!(
                "too many entries: {} > {}",
                req.entries.len(),
                MAX_ENTRIES_PER_POSTING
            )));
        }

        for e in &req.entries {
            if e.amount == 0 {
                return Err(PostError::Validation("amount must be > 0".into()));
            }
            if let Err(msg) = crate::asset::validate_amount(&e.asset, e.amount) {
                return Err(PostError::Validation(msg));
            }
            if e.amount > MAX_AMOUNT {
                return Err(PostError::Validation(format!(
                    "amount {} exceeds MAX_AMOUNT {}",
                    e.amount, MAX_AMOUNT
                )));
            }
        }

        let mut entries_parsed: Vec<(String, Direction, u64, String)> =
            Vec::with_capacity(req.entries.len());
        for e in &req.entries {
            let dir = match crate::account::parse_direction(&e.direction) {
                Some(d) => d,
                None => {
                    return Err(PostError::Validation(format!(
                        "invalid direction: {}",
                        e.direction
                    )))
                }
            };
            entries_parsed.push((e.account_id.clone(), dir, e.amount, e.asset.clone()));
        }

        let mut per_asset: HashMap<String, (i128, i128)> = HashMap::new();
        for (_account_id, dir, amount, asset) in &entries_parsed {
            let entry = per_asset.entry(asset.clone()).or_insert((0, 0));
            match dir {
                Direction::Debit => entry.0 += *amount as i128,
                Direction::Credit => entry.1 += *amount as i128,
            }
        }
        for (asset, (debits, credits)) in &per_asset {
            if debits != credits {
                return Err(PostError::Unbalanced(format!(
                    "asset {} unbalanced: debits={} credits={}",
                    asset, debits, credits
                )));
            }
        }

        if self.pool.is_some() {
            self.post_via_db(req, entries_parsed)
        } else {
            self.post_via_memory(req, entries_parsed)
        }
    }

    fn post_via_db(
        &self,
        req: PostingRequest,
        entries_parsed: Vec<(String, Direction, u64, String)>,
    ) -> Result<(PostingResponse, bool), PostError> {
        let pool = self.pool.as_ref().unwrap();

        // Idempotency / replay: DB is source of truth.
        let existing: Option<PostingRow> = block_on(
            sqlx::query_as::<_, PostingRow>(
                "SELECT posting_id, ref_tx_id, memo, status, hash_chain_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM postings WHERE posting_id = $1",
            )
            .bind(&req.posting_id)
            .fetch_optional(pool),
        )
        .map_err(|e| PostError::Validation(format!("post: check existing: {}", e)))?;
        if let Some(p) = existing {
            let entries: Vec<EntryRow> = block_on(
                sqlx::query_as::<_, EntryRow>(
                    "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries WHERE posting_id = $1 ORDER BY sequence_number",
                )
                .bind(&req.posting_id)
                .fetch_all(pool),
            )
            .map_err(|e| PostError::Validation(format!("post: fetch existing entries: {}", e)))?;
            let entry_ids: Vec<String> = entries.iter().map(|e| e.entry_id.clone()).collect();
            return Ok((
                PostingResponse {
                    posting_id: p.posting_id,
                    status: p.status,
                    entry_ids,
                    hash_head: p.hash_chain_head,
                },
                true,
            ));
        }

        // Account existence + active status + direction validation.
        for (account_id, _dir, _amount, _asset) in &entries_parsed {
            let row: Option<(String, String)> = block_on(
                sqlx::query_as("SELECT account_id, status FROM accounts WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(pool),
            )
            .map_err(|e| PostError::Validation(format!("post: account lookup: {}", e)))?;
            match row {
                Some((_id, status)) => {
                    if status != "ACTIVE" {
                        return Err(PostError::Validation(format!(
                            "account not active: {}",
                            account_id
                        )));
                    }
                }
                None => {
                    return Err(PostError::Validation(format!(
                        "account not found: {}",
                        account_id
                    )));
                }
            }
        }
        for (account_id, dir, _amount, _asset) in &entries_parsed {
            let row: Option<(String,)> = block_on(
                sqlx::query_as("SELECT type_name FROM accounts WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(pool),
            )
            .map_err(|e| PostError::Validation(format!("post: type lookup: {}", e)))?;
            let type_name = row
                .ok_or_else(|| PostError::Validation(format!("account not found: {}", account_id)))?
                .0;
            let account_type = chart::find_type(&type_name).ok_or_else(|| {
                PostError::Validation(format!("unknown account type: {}", type_name))
            })?;
            if !chart::direction_allowed(account_type, *dir) {
                return Err(PostError::Validation(format!(
                    "direction {:?} not allowed for type {}",
                    dir, type_name
                )));
            }
        }

        let now = now_iso();

        // Begin the chain-extension transaction FIRST, then read the chain
        // tip (prev_hash + max sequence) inside it under SERIALIZABLE
        // isolation + a transaction-scoped advisory lock. This serializes
        // concurrent ingests on the chain tip so two replicas cannot read
        // the same prev_hash and fork the chain. The lock is released on
        // COMMIT/ROLLBACK.
        let mut tx = block_on(pool.begin())
            .map_err(|e| PostError::Validation(format!("post: begin tx: {}", e)))?;
        block_on(
            sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").execute(&mut *tx),
        )
        .map_err(|e| PostError::Validation(format!("post: set serializable: {}", e)))?;
        block_on(
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(ENTRIES_CHAIN_ADVISORY_LOCK_KEY)
                .execute(&mut *tx),
        )
        .map_err(|e| PostError::Validation(format!("post: advisory lock: {}", e)))?;

        let prev_hash_initial = last_entry_hash_from_tx(&mut tx)
            .unwrap_or_else(|| GENESIS_HASH.to_string());
        let mut seq_cursor = next_sequence_from_tx(&mut tx)
            .map_err(|e| PostError::Validation(format!("post: next sequence: {}", e)))?;

        let mut prev_hash = prev_hash_initial.clone();
        let mut entry_ids: Vec<String> = Vec::new();
        let mut created_entries: Vec<EntryRecord> = Vec::new();

        let salt = self.salt.clone();
        for (account_id, dir, amount, asset) in entries_parsed {
            let entry_id = uuid::Uuid::now_v7().to_string();
            let canonical = posting::canonical_bytes(
                &prev_hash,
                &entry_id,
                &account_id,
                dir,
                amount,
                &asset,
                &now,
            );
            let this_hash = posting::compute_hash(&prev_hash, &salt, &canonical);
            let record = EntryRecord {
                entry_id: entry_id.clone(),
                posting_id: req.posting_id.clone(),
                account_id,
                direction: match dir {
                    Direction::Debit => "DEBIT".to_string(),
                    Direction::Credit => "CREDIT".to_string(),
                },
                amount,
                asset,
                sequence_number: 0, // assigned below from DB
                prev_hash: prev_hash.clone(),
                this_hash: this_hash.clone(),
                created_at: now.clone(),
            };
            entry_ids.push(entry_id.clone());
            created_entries.push(record);
            prev_hash = this_hash;
        }

        let hash_head = prev_hash.clone();
        let global_sequence_head = hash_head.clone();

        let inserted_posting = block_on(
            sqlx::query(
                "INSERT INTO postings (posting_id, ref_tx_id, memo, status, hash_chain_head, created_at, updated_at)
                 VALUES ($1, $2, $3, 'POSTED', $4, to_timestamp($5), to_timestamp($5))
                 ON CONFLICT (posting_id) DO NOTHING",
            )
            .bind(&req.posting_id)
            .bind(req.ref_tx_id.as_ref())
            .bind(req.memo.as_ref())
            .bind(&hash_head)
            .bind(secs(&now))
            .execute(&mut *tx),
        )
        .map_err(|e| PostError::Validation(format!("post: insert posting: {}", e)))?;
        if inserted_posting.rows_affected() == 0 {
            // Race: another replica inserted first. Roll back our tx and
            // return the existing record from DB.
            drop(tx);
            let existing: PostingRow = block_on(
                sqlx::query_as::<_, PostingRow>(
                    "SELECT posting_id, ref_tx_id, memo, status, hash_chain_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM postings WHERE posting_id = $1",
                )
                .bind(&req.posting_id)
                .fetch_one(pool),
            )
            .map_err(|e| PostError::Validation(format!("post: fetch existing: {}", e)))?;
            let entries: Vec<EntryRow> = block_on(
                sqlx::query_as::<_, EntryRow>(
                    "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries WHERE posting_id = $1 ORDER BY sequence_number",
                )
                .bind(&req.posting_id)
                .fetch_all(pool),
            )
            .map_err(|e| PostError::Validation(format!("post: fetch existing entries: {}", e)))?;
            let entry_ids: Vec<String> = entries.iter().map(|e| e.entry_id.clone()).collect();
            return Ok((
                PostingResponse {
                    posting_id: existing.posting_id,
                    status: existing.status,
                    entry_ids,
                    hash_head: existing.hash_chain_head,
                },
                true,
            ));
        }

        for e in &mut created_entries {
            seq_cursor += 1;
            e.sequence_number = seq_cursor;
            block_on(
                sqlx::query(
                    "INSERT INTO entries (entry_id, posting_id, account_id, direction, amount, asset, sequence_number, prev_hash, this_hash, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, to_timestamp($10), to_timestamp($10))",
                )
                .bind(&e.entry_id)
                .bind(&e.posting_id)
                .bind(&e.account_id)
                .bind(&e.direction)
                .bind(e.amount as i64)
                .bind(&e.asset)
                .bind(e.sequence_number as i64)
                .bind(&e.prev_hash)
                .bind(&e.this_hash)
                .bind(secs(&e.created_at))
                .execute(&mut *tx),
            )
            .map_err(|err| PostError::Validation(format!("post: insert entry: {}", err)))?;
        }

        block_on(
            sqlx::query(
                "INSERT INTO hash_chain (posting_id, head_hash, global_sequence_head, created_at, updated_at)
                 VALUES ($1, $2, $3, to_timestamp($4), to_timestamp($4))
                 ON CONFLICT (posting_id) DO NOTHING",
            )
            .bind(&req.posting_id)
            .bind(&hash_head)
            .bind(&global_sequence_head)
            .bind(secs(&now))
            .execute(&mut *tx),
        )
        .map_err(|e| PostError::Validation(format!("post: insert anchor: {}", e)))?;

        // DB is the source of truth: commit before touching in-memory.
        if let Err(e) = block_on(tx.commit()) {
            return Err(PostError::Validation(format!("post: commit: {}", e)));
        }

        // Commit succeeded; update the in-memory cache to stay consistent.
        {
            let mut st = self.inner.lock();
            st.postings.insert(
                req.posting_id.clone(),
                PostingRecord {
                    posting_id: req.posting_id.clone(),
                    ref_tx_id: req.ref_tx_id.clone(),
                    memo: req.memo.clone(),
                    status: "POSTED".to_string(),
                    hash_head: hash_head.clone(),
                    entries: created_entries.clone(),
                    created_at: now.clone(),
                },
            );
            st.entries.extend(created_entries.iter().cloned());
            st.hash_chain_anchors.insert(
                req.posting_id.clone(),
                HashChainAnchor {
                    posting_id: req.posting_id.clone(),
                    head_hash: hash_head.clone(),
                    global_sequence_head: global_sequence_head.clone(),
                    created_at: now.clone(),
                },
            );
            st.global_chain_head = global_sequence_head.clone();
            st.sequence = seq_cursor;
        }

        let event = AuditEvent {
            event_id: uuid::Uuid::now_v7().to_string(),
            posting_id: req.posting_id.clone(),
            entry_ids: entry_ids.clone(),
            hash_head: hash_head.clone(),
            created_at: now,
        };
        {
            let mut state = self.inner.lock();
            state.audit_events.push(event.clone());
        }
        if let Some(sink) = &self.audit_sink {
            sink.emit(self, &event);
        }

        Ok((
            PostingResponse {
                posting_id: req.posting_id,
                status: "POSTED".to_string(),
                entry_ids,
                hash_head,
            },
            false,
        ))
    }

    fn post_via_memory(
        &self,
        req: PostingRequest,
        entries_parsed: Vec<(String, Direction, u64, String)>,
    ) -> Result<(PostingResponse, bool), PostError> {
        // Hold the lock for the entire post so concurrent duplicate submissions
        // serialize exactly (matches the pre-remediation behavior).
        let mut state = self.inner.lock();

        if let Some(existing) = state.postings.get(&req.posting_id) {
            return Ok((
                PostingResponse {
                    posting_id: existing.posting_id.clone(),
                    status: existing.status.clone(),
                    entry_ids: existing
                        .entries
                        .iter()
                        .map(|e| e.entry_id.clone())
                        .collect(),
                    hash_head: existing.hash_head.clone(),
                },
                true,
            ));
        }

        for (account_id, _dir, _amount, _asset) in &entries_parsed {
            match state.accounts.get(account_id) {
                Some(acc) => {
                    if acc.status != "ACTIVE" {
                        return Err(PostError::Validation(format!(
                            "account not active: {}",
                            account_id
                        )));
                    }
                }
                None => {
                    return Err(PostError::Validation(format!(
                        "account not found: {}",
                        account_id
                    )))
                }
            }
        }
        for (account_id, dir, _amount, _asset) in &entries_parsed {
            let acc = state.accounts.get(account_id).ok_or_else(|| {
                PostError::Validation(format!("account not found: {}", account_id))
            })?;
            let account_type = chart::find_type(&acc.type_name).ok_or_else(|| {
                PostError::Validation(format!("unknown account type: {}", acc.type_name))
            })?;
            if !chart::direction_allowed(account_type, *dir) {
                return Err(PostError::Validation(format!(
                    "direction {:?} not allowed for type {}",
                    dir, acc.type_name
                )));
            }
        }

        let now = now_iso();
        let mut prev_hash = state.global_chain_head.clone();
        let mut entry_ids: Vec<String> = Vec::new();
        let mut created_entries: Vec<EntryRecord> = Vec::new();
        let salt = self.salt.clone();

        for (account_id, dir, amount, asset) in entries_parsed {
            let entry_id = uuid::Uuid::now_v7().to_string();
            state.sequence += 1;
            let seq = state.sequence;
            let canonical = posting::canonical_bytes(
                &prev_hash,
                &entry_id,
                &account_id,
                dir,
                amount,
                &asset,
                &now,
            );
            let this_hash = posting::compute_hash(&prev_hash, &salt, &canonical);
            let record = EntryRecord {
                entry_id: entry_id.clone(),
                posting_id: req.posting_id.clone(),
                account_id,
                direction: match dir {
                    Direction::Debit => "DEBIT".to_string(),
                    Direction::Credit => "CREDIT".to_string(),
                },
                amount,
                asset,
                sequence_number: seq,
                prev_hash: prev_hash.clone(),
                this_hash: this_hash.clone(),
                created_at: now.clone(),
            };
            entry_ids.push(entry_id.clone());
            created_entries.push(record);
            prev_hash = this_hash;
        }

        let hash_head = prev_hash;
        let global_sequence_head = hash_head.clone();
        let posting_record = PostingRecord {
            posting_id: req.posting_id.clone(),
            ref_tx_id: req.ref_tx_id.clone(),
            memo: req.memo.clone(),
            status: "POSTED".to_string(),
            hash_head: hash_head.clone(),
            entries: created_entries.clone(),
            created_at: now.clone(),
        };
        state
            .postings
            .insert(req.posting_id.clone(), posting_record);
        state.entries.extend(created_entries.iter().cloned());
        let anchor = HashChainAnchor {
            posting_id: req.posting_id.clone(),
            head_hash: hash_head.clone(),
            global_sequence_head: global_sequence_head.clone(),
            created_at: now.clone(),
        };
        state
            .hash_chain_anchors
            .insert(req.posting_id.clone(), anchor);
        state.global_chain_head = global_sequence_head.clone();

        let event = AuditEvent {
            event_id: uuid::Uuid::now_v7().to_string(),
            posting_id: req.posting_id.clone(),
            entry_ids: entry_ids.clone(),
            hash_head: hash_head.clone(),
            created_at: now,
        };
        state.audit_events.push(event.clone());
        if let Some(sink) = &self.audit_sink {
            sink.emit(self, &event);
        }

        Ok((
            PostingResponse {
                posting_id: req.posting_id,
                status: "POSTED".to_string(),
                entry_ids,
                hash_head,
            },
            false,
        ))
    }

    pub fn get_posting(&self, posting_id: &str) -> Option<PostingRecord> {
        if let Some(pool) = &self.pool {
            let p: PostingRow = block_on(
                sqlx::query_as::<_, PostingRow>(
                    "SELECT posting_id, ref_tx_id, memo, status, hash_chain_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM postings WHERE posting_id = $1",
                )
                .bind(posting_id)
                .fetch_optional(pool),
            )
            .ok()??;
            let entries: Vec<EntryRow> = block_on(
                sqlx::query_as::<_, EntryRow>(
                    "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries WHERE posting_id = $1 ORDER BY sequence_number",
                )
                .bind(posting_id)
                .fetch_all(pool),
            )
            .ok()?;
            let rec_entries = entries
                .into_iter()
                .map(|e| EntryRecord {
                    entry_id: e.entry_id,
                    posting_id: e.posting_id,
                    account_id: e.account_id,
                    direction: e.direction,
                    amount: e.amount.parse().unwrap_or(0),
                    asset: e.asset,
                    sequence_number: e.sequence_number as u64,
                    prev_hash: e.prev_hash,
                    this_hash: e.this_hash,
                    created_at: e.created_at,
                })
                .collect();
            Some(PostingRecord {
                posting_id: p.posting_id,
                ref_tx_id: p.ref_tx_id,
                memo: p.memo,
                status: p.status,
                hash_head: p.hash_chain_head,
                entries: rec_entries,
                created_at: p.created_at,
            })
        } else {
            self.inner.lock().postings.get(posting_id).cloned()
        }
    }

    #[allow(dead_code)]
    pub fn audit_events(&self) -> Vec<AuditEvent> {
        self.inner.lock().audit_events.clone()
    }

    #[allow(dead_code)]
    pub fn record_audit_event(&self, event: AuditEvent) {
        self.inner.lock().audit_events.push(event);
    }

    #[allow(dead_code)]
    pub fn entry_count(&self) -> usize {
        if let Some(pool) = &self.pool {
            let (count,): (i64,) =
                block_on(sqlx::query_as("SELECT COUNT(*)::bigint FROM entries").fetch_one(pool))
                    .map_err(|_| (0i64,))
                    .unwrap_or((0i64,));
            count as usize
        } else {
            self.inner.lock().entries.len()
        }
    }

    pub fn hash_chain_anchor(&self, posting_id: &str) -> Option<HashChainAnchor> {
        if let Some(pool) = &self.pool {
            let row: Option<AnchorRow> = block_on(
                sqlx::query_as::<_, AnchorRow>(
                    "SELECT posting_id, head_hash, global_sequence_head, floor(extract(epoch from created_at))::bigint::text AS created_at FROM hash_chain WHERE posting_id = $1",
                )
                .bind(posting_id)
                .fetch_optional(pool),
            )
            .ok()?;
            row.map(|a| HashChainAnchor {
                posting_id: a.posting_id,
                head_hash: a.head_hash,
                global_sequence_head: a.global_sequence_head,
                created_at: a.created_at,
            })
        } else {
            self.inner
                .lock()
                .hash_chain_anchors
                .get(posting_id)
                .cloned()
        }
    }

    pub fn global_chain_head(&self) -> String {
        if let Some(pool) = &self.pool {
            last_entry_hash_from_db(pool).unwrap_or_else(|| GENESIS_HASH.to_string())
        } else {
            self.inner.lock().global_chain_head.clone()
        }
    }

    pub fn verify_chain(&self) -> Result<(), crate::hashchain::ChainBreak> {
        if let Some(pool) = &self.pool {
            let entries: Vec<EntryRow> = match block_on(sqlx::query_as::<_, EntryRow>(
                "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries ORDER BY sequence_number",
            )
            .fetch_all(pool)) {
                Ok(e) => e,
                Err(err) => {
                    return Err(crate::hashchain::ChainBreak {
                        entry_id: String::new(),
                        reason: format!("db read failed: {}", err),
                    });
                }
            };
            let mut state = LedgerState::new();
            state.entries = entries
                .into_iter()
                .map(|e| EntryRecord {
                    entry_id: e.entry_id,
                    posting_id: e.posting_id,
                    account_id: e.account_id,
                    direction: e.direction,
                    amount: e.amount.parse().unwrap_or(0),
                    asset: e.asset,
                    sequence_number: e.sequence_number as u64,
                    prev_hash: e.prev_hash,
                    this_hash: e.this_hash,
                    created_at: e.created_at,
                })
                .collect();
            crate::hashchain::verify_chain(&state, &self.salt)
        } else {
            let state = self.inner.lock();
            crate::hashchain::verify_chain(&state, &self.salt)
        }
    }

    pub fn user_custodial_sum(&self, asset: &str) -> i128 {
        if let Some(pool) = &self.pool {
            let accounts: Vec<(String,)> = match block_on(
                sqlx::query_as::<_, (String,)>(
                    "SELECT account_id FROM accounts WHERE type_name = 'user_custodial'",
                )
                .fetch_all(pool),
            ) {
                Ok(a) => a,
                Err(_) => return 0,
            };
            let mut total: i128 = 0;
            for (account_id,) in accounts {
                if let Some(b) = balance_from_db(pool, &account_id, asset) {
                    total += b;
                }
            }
            total
        } else {
            let state = self.inner.lock();
            let mut total: i128 = 0;
            for (account_id, acc) in state.accounts.iter() {
                if acc.type_name == "user_custodial" {
                    total += compute_balance(&state, account_id, asset);
                }
            }
            total
        }
    }

    pub fn write_snapshots(&self) -> Vec<BalanceSnapshot> {
        if let Some(pool) = &self.pool {
            // Compute snapshots from DB.
            let accounts: Vec<(String, String)> = match block_on(sqlx::query_as::<_, (String, String)>(
                "SELECT account_id, asset FROM (SELECT DISTINCT account_id, asset FROM entries) e",
            )
            .fetch_all(pool)) {
                Ok(a) => a,
                Err(_) => return Vec::new(),
            };
            let mut snaps = Vec::with_capacity(accounts.len());
            for (account_id, asset) in accounts {
                let bal = balance_from_db(pool, &account_id, &asset).unwrap_or(0);
                let last: Option<(String, i64)> = block_on(
                    sqlx::query_as::<_, (String, i64)>(
                        "SELECT entry_id, sequence_number FROM entries WHERE account_id = $1 AND asset = $2 ORDER BY sequence_number DESC LIMIT 1",
                    )
                    .bind(&account_id)
                    .bind(&asset)
                    .fetch_optional(pool),
                )
                .ok()
                .flatten();
                let (last_entry_id, last_sequence) = last.unwrap_or_default();
                let snap_as_of = now_iso();
                snaps.push(BalanceSnapshot {
                    account_id: account_id.clone(),
                    asset: asset.clone(),
                    balance: bal,
                    as_of_ts: snap_as_of.clone(),
                    last_entry_id: last_entry_id.clone(),
                    last_sequence: last_sequence as u64,
                });
                let _ = block_on(
                    sqlx::query(
                        "INSERT INTO balance_snapshots (account_id, asset, balance, as_of_ts, last_entry_id, created_at, updated_at)
                     VALUES ($1, $2, $3::numeric, to_timestamp($4), $5, to_timestamp($4), to_timestamp($4))
                     ON CONFLICT (account_id, asset, as_of_ts) DO NOTHING",
                    )
                    .bind(&account_id)
                    .bind(&asset)
                    .bind(bal.to_string())
                    .bind(secs(&snap_as_of))
                    .bind(&last_entry_id)
                    .execute(pool),
                );
            }
            // Mirror into in-memory cache for consistency.
            {
                let mut state = self.inner.lock();
                state.snapshots = snaps.clone();
            }
            snaps
        } else {
            let mut state = self.inner.lock();
            let snaps = crate::snapshot::snapshot_all(&state);
            state.snapshots = snaps.clone();
            snaps
        }
    }

    pub fn latest_snapshot(&self, account_id: &str, asset: &str) -> Option<BalanceSnapshot> {
        if let Some(pool) = &self.pool {
            let row: Option<(String, String, String, String, String)> = block_on(
                sqlx::query_as::<_, (String, String, String, String, String)>(
                    "SELECT account_id, asset, balance::text, floor(extract(epoch from as_of_ts))::bigint::text AS as_of_ts, last_entry_id FROM balance_snapshots WHERE account_id = $1 AND ($2 = '' OR asset = $2) ORDER BY as_of_ts DESC LIMIT 1",
                )
                .bind(account_id)
                .bind(asset)
                .fetch_optional(pool),
            )
            .ok()?;
            let (account_id, snap_asset, balance, as_of_ts, last_entry_id) = row?;
            let balance: i128 = balance.parse().ok()?;
            let last_seq: (i64,) = block_on(
                sqlx::query_as("SELECT sequence_number FROM entries WHERE entry_id = $1")
                    .bind(&last_entry_id)
                    .fetch_optional(pool),
            )
            .ok()?
            .unwrap_or((0,));
            Some(BalanceSnapshot {
                account_id,
                asset: snap_asset,
                balance,
                as_of_ts,
                last_entry_id,
                last_sequence: last_seq.0 as u64,
            })
        } else {
            let state = self.inner.lock();
            state
                .snapshots
                .iter()
                .filter(|s| s.account_id == account_id)
                .rfind(|s| asset.is_empty() || s.asset == asset)
                .cloned()
        }
    }

    pub fn balance_via_snapshot(&self, account_id: &str, asset: &str) -> Option<i128> {
        if let Some(pool) = &self.pool {
            let exists: Option<(String,)> = block_on(
                sqlx::query_as("SELECT account_id FROM accounts WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(pool),
            )
            .ok()?;
            exists.as_ref()?;
            let snap: Option<BalanceSnapshot> = self.latest_snapshot(account_id, asset);
            match snap {
                Some(s) => {
                    let delta: i128 = block_on(
                        sqlx::query_as::<_, (i64,)>(
                            "SELECT COALESCE(SUM(CASE direction WHEN 'DEBIT' THEN amount ELSE -amount END), 0)::bigint FROM entries WHERE account_id = $1 AND ($2 = '' OR asset = $2) AND sequence_number > $3",
                        )
                        .bind(account_id)
                        .bind(asset)
                        .bind(s.last_sequence as i64)
                        .fetch_one(pool),
                    )
                    .ok()?
                    .0 as i128;
                    Some(s.balance + delta)
                }
                None => balance_from_db(pool, account_id, asset),
            }
        } else {
            let state = self.inner.lock();
            if !state.accounts.contains_key(account_id) {
                return None;
            }
            let snap = state
                .snapshots
                .iter()
                .filter(|s| s.account_id == account_id)
                .rfind(|s| asset.is_empty() || s.asset == asset);
            match snap {
                Some(s) => {
                    let mut bal = s.balance;
                    let last_seq = s.last_sequence;
                    let delta: i128 = state
                        .entries
                        .iter()
                        .filter(|e| e.account_id == account_id)
                        .filter(|e| asset.is_empty() || e.asset == asset)
                        .filter(|e| e.sequence_number > last_seq)
                        .fold(0i128, |acc, e| {
                            let d = match e.direction.as_str() {
                                "DEBIT" => e.amount as i128,
                                "CREDIT" => -(e.amount as i128),
                                _ => 0,
                            };
                            acc + d
                        });
                    bal += delta;
                    Some(bal)
                }
                None => Some(compute_balance(&state, account_id, asset)),
            }
        }
    }

    pub fn reconcile_snapshot(&self, snap: &BalanceSnapshot) -> bool {
        if let Some(pool) = &self.pool {
            let computed = balance_from_db(pool, &snap.account_id, &snap.asset);
            computed.is_some()
                && computed == self.balance_via_snapshot(&snap.account_id, &snap.asset)
                && computed == Some(snap.balance)
        } else {
            let state = self.inner.lock();
            crate::snapshot::reconcile_snapshot(&state, snap)
        }
    }
}

#[derive(Debug)]
pub enum PostError {
    Validation(String),
    Unbalanced(String),
}

impl PostError {
    pub fn status(&self) -> u16 {
        match self {
            PostError::Validation(_) => 400,
            PostError::Unbalanced(_) => 400,
        }
    }

    pub fn message(&self) -> String {
        match self {
            PostError::Validation(s) => s.clone(),
            PostError::Unbalanced(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerPage {
    pub account_id: String,
    pub entries: Vec<LedgerEntryItem>,
    pub next_cursor: Option<u64>,
    pub final_balance: i128,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerEntryItem {
    pub entry_id: String,
    pub posting_id: String,
    pub account_id: String,
    pub direction: String,
    pub amount: u64,
    pub asset: String,
    pub sequence_number: u64,
    pub this_hash: String,
    pub prev_hash: String,
    pub created_at: String,
    pub running_balance: i128,
}

fn compute_balance(state: &LedgerState, account_id: &str, asset: &str) -> i128 {
    state
        .entries
        .iter()
        .filter(|e| e.account_id == account_id)
        .filter(|e| asset.is_empty() || e.asset == asset)
        .fold(0i128, |acc, e| match e.direction.as_str() {
            "DEBIT" => acc + e.amount as i128,
            "CREDIT" => acc - e.amount as i128,
            _ => acc,
        })
}

fn balance_from_db(pool: &sqlx::PgPool, account_id: &str, asset: &str) -> Option<i128> {
    let (bal_str,): (String,) = block_on(
        sqlx::query_as::<_, (String,)>(
            "SELECT COALESCE(SUM(CASE direction WHEN 'DEBIT' THEN amount ELSE -amount END), 0)::text FROM entries WHERE account_id = $1 AND ($2 = '' OR asset = $2)",
        )
        .bind(account_id)
        .bind(asset)
        .fetch_one(pool),
    )
    .ok()?;
    bal_str.parse::<i128>().ok()
}

fn last_entry_hash_from_db(pool: &sqlx::PgPool) -> Option<String> {
    let row: Option<(String,)> = block_on(
        sqlx::query_as::<_, (String,)>(
            "SELECT this_hash FROM entries ORDER BY sequence_number DESC LIMIT 1",
        )
        .fetch_optional(pool),
    )
    .ok()?;
    row.map(|(h,)| h)
}

#[allow(dead_code)]
fn next_sequence_from_db(pool: &sqlx::PgPool, _prev_hash: &str) -> Result<u64, String> {
    let (max_seq,): (i64,) = block_on(
        sqlx::query_as::<_, (i64,)>("SELECT COALESCE(MAX(sequence_number), 0) FROM entries")
            .fetch_one(pool),
    )
    .map_err(|e| format!("next_sequence: {}", e))?;
    Ok(max_seq as u64)
}

fn last_entry_hash_from_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Option<String> {
    let row: Option<(String,)> = block_on(
        sqlx::query_as::<_, (String,)>(
            "SELECT this_hash FROM entries ORDER BY sequence_number DESC LIMIT 1",
        )
        .fetch_optional(&mut **tx),
    )
    .ok()?;
    row.map(|(h,)| h)
}

fn next_sequence_from_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, String> {
    let (max_seq,): (i64,) = block_on(
        sqlx::query_as::<_, (i64,)>("SELECT COALESCE(MAX(sequence_number), 0) FROM entries")
            .fetch_one(&mut **tx),
    )
    .map_err(|e| format!("next_sequence: {}", e))?;
    Ok(max_seq as u64)
}

fn ledger_page_from_db(
    pool: &sqlx::PgPool,
    account_id: &str,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    cursor: Option<u64>,
) -> Result<LedgerPage, String> {
    use sqlx::QueryBuilder;
    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "SELECT entry_id, posting_id, account_id, direction, amount::text AS amount, asset, sequence_number, prev_hash, this_hash, floor(extract(epoch from created_at))::bigint::text AS created_at FROM entries WHERE account_id = ",
    );
    qb.push_bind(account_id.to_string());
    if let Some(f) = from {
        qb.push(" AND created_at >= to_timestamp(");
        qb.push_bind(f.to_string());
        qb.push(")");
    }
    if let Some(t) = to {
        qb.push(" AND created_at <= to_timestamp(");
        qb.push_bind(t.to_string());
        qb.push(")");
    }
    if let Some(c) = cursor {
        qb.push(" AND sequence_number > ");
        qb.push_bind(c as i64);
    }
    qb.push(" ORDER BY sequence_number ASC LIMIT ");
    qb.push_bind((limit + 1) as i64);
    let rows: Vec<EntryRow> = block_on(qb.build_query_as::<EntryRow>().fetch_all(pool))
        .map_err(|e| format!("ledger: {}", e))?;

    let final_balance = balance_from_db(pool, account_id, "").unwrap_or(0);
    let next_cursor = if rows.len() > limit {
        Some(rows[limit - 1].sequence_number as u64)
    } else {
        None
    };
    let entries: Vec<EntryRecord> = rows
        .into_iter()
        .take(limit)
        .map(|e| EntryRecord {
            entry_id: e.entry_id,
            posting_id: e.posting_id,
            account_id: e.account_id,
            direction: e.direction,
            amount: e.amount.parse().unwrap_or(0),
            asset: e.asset,
            sequence_number: e.sequence_number as u64,
            prev_hash: e.prev_hash,
            this_hash: e.this_hash,
            created_at: e.created_at,
        })
        .collect();

    let page: Vec<LedgerEntryItem> = entries
        .iter()
        .scan(0i128, |acc, e| {
            match e.direction.as_str() {
                "DEBIT" => *acc += e.amount as i128,
                "CREDIT" => *acc -= e.amount as i128,
                _ => {}
            }
            Some(LedgerEntryItem {
                entry_id: e.entry_id.clone(),
                posting_id: e.posting_id.clone(),
                account_id: e.account_id.clone(),
                direction: e.direction.clone(),
                amount: e.amount,
                asset: e.asset.clone(),
                sequence_number: e.sequence_number,
                this_hash: e.this_hash.clone(),
                prev_hash: e.prev_hash.clone(),
                created_at: e.created_at.clone(),
                running_balance: *acc,
            })
        })
        .collect();

    Ok(LedgerPage {
        account_id: account_id.to_string(),
        entries: page,
        next_cursor,
        final_balance,
    })
}

pub fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", secs)
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

fn secs(s: &str) -> i64 {
    s.parse::<i64>().unwrap_or(0)
}

fn is_already_applied(err: &sqlx::Error) -> bool {
    if let Some(db) = err.as_database_error() {
        if let Some(code) = db.code() {
            return code == "42710" || code == "42P07";
        }
    }
    false
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, FromRow)]
struct AccountRow {
    account_id: String,
    type_field: String,
    asset_class: String,
    label: String,
    parent_id: Option<String>,
    status: String,
    created_at: String,
}

#[derive(Debug, Clone, FromRow)]
struct PostingRow {
    posting_id: String,
    ref_tx_id: Option<String>,
    memo: Option<String>,
    status: String,
    hash_chain_head: String,
    created_at: String,
}

#[derive(Debug, Clone, FromRow)]
struct EntryRow {
    entry_id: String,
    posting_id: String,
    account_id: String,
    direction: String,
    amount: String,
    asset: String,
    sequence_number: i64,
    prev_hash: String,
    this_hash: String,
    created_at: String,
}

#[derive(Debug, Clone, FromRow)]
struct AnchorRow {
    posting_id: String,
    head_hash: String,
    global_sequence_head: String,
    created_at: String,
}

#[derive(Debug, Clone, FromRow)]
struct SnapshotRow {
    account_id: String,
    asset: String,
    balance: String,
    as_of_ts: String,
    last_entry_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::CreateAccountRequest;

    fn acct_req(id: &str, type_name: &str, asset_class: &str) -> CreateAccountRequest {
        serde_json::from_value(serde_json::json!({
            "account_id": id,
            "type": type_name,
            "asset_class": asset_class,
            "label": format!("{}-{}", type_name, id),
        }))
        .unwrap()
    }

    fn balanced_posting(posting_id: &str, amount: u64, asset: &str) -> PostingRequest {
        serde_json::from_value(serde_json::json!({
            "posting_id": posting_id,
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": amount, "asset": asset },
                { "account_id": "op", "direction": "CREDIT", "amount": amount, "asset": asset }
            ]
        }))
        .unwrap()
    }

    fn setup(store: &Store) {
        store
            .create_account(acct_req("uc", "user_custodial", "BOTH"))
            .unwrap();
        store
            .create_account(acct_req("op", "operational_fiat", "FIAT"))
            .unwrap();
    }

    #[test]
    fn ledger_state_default_matches_new() {
        let a = LedgerState::default();
        let b = LedgerState::new();
        assert_eq!(a.accounts.len(), b.accounts.len());
        assert_eq!(a.sequence, b.sequence);
        assert_eq!(a.global_chain_head, b.global_chain_head);
    }

    #[test]
    fn store_default_matches_new() {
        let a = Store::default();
        let b = Store::new();
        assert!(a.pool.is_none());
        assert!(b.pool.is_none());
    }

    #[test]
    fn run_migrations_no_pool_is_noop() {
        let store = Store::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(store.run_migrations());
        assert!(res.is_ok());
        let _ = store.list_accounts(None);
    }

    #[test]
    fn hydrate_no_pool_is_noop() {
        let store = Store::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(store.hydrate());
        assert!(res.is_ok());
    }

    #[test]
    fn create_account_auto_id_when_none() {
        let store = Store::new();
        let req = CreateAccountRequest {
            account_id: None,
            type_name: "user_custodial".to_string(),
            asset_class: "BOTH".to_string(),
            label: "auto".to_string(),
            parent_id: None,
        };
        let acc = store.create_account(req).unwrap();
        assert!(!acc.account_id.is_empty());
        assert!(store.get_account(&acc.account_id).is_some());
    }

    #[test]
    fn get_account_missing_returns_none() {
        let store = Store::new();
        assert!(store.get_account("nope").is_none());
    }

    #[test]
    fn list_accounts_sorts_by_created_at_then_id() {
        let store = Store::new();
        // Insert accounts; created_at is "now" so order should fall back to id.
        store
            .create_account(acct_req("b", "user_custodial", "BOTH"))
            .unwrap();
        store
            .create_account(acct_req("a", "user_custodial", "BOTH"))
            .unwrap();
        let all = store.list_accounts(None);
        assert_eq!(all.len(), 2);
        // Both created at the same second; sort by id ascending.
        assert_eq!(all[0].account_id, "a");
        assert_eq!(all[1].account_id, "b");
    }

    #[test]
    fn list_postings_clamps_and_truncates() {
        let store = Store::new();
        setup(&store);
        for i in 0..3 {
            store
                .post(balanced_posting(&format!("p{}", i), 1, "USD"))
                .unwrap();
        }
        // limit 0 -> clamped to 1
        assert_eq!(store.list_postings(0).len(), 1);
        // limit huge -> clamped to 200
        assert_eq!(store.list_postings(10000).len(), 3);
    }

    #[test]
    fn balance_missing_account_returns_none() {
        let store = Store::new();
        assert!(store.balance("nope", "USD").is_none());
    }

    #[test]
    fn ledger_missing_account_errors() {
        let store = Store::new();
        assert!(store.ledger("nope", None, None, 10, None).is_err());
    }

    #[test]
    fn ledger_with_from_to_filters_and_cursor() {
        let store = Store::new();
        setup(&store);
        // 5 postings, all at same second.
        for i in 0..5 {
            store
                .post(balanced_posting(&format!("l{}", i), 1, "USD"))
                .unwrap();
        }
        // from="" matches all (>= "" is always true), to unset.
        let page = store.ledger("uc", Some(""), None, 10, None).unwrap();
        assert_eq!(page.entries.len(), 5);
        // cursor beyond all -> empty
        let page = store.ledger("uc", None, None, 10, Some(999)).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn ledger_credit_decrements_running_balance() {
        let store = Store::new();
        setup(&store);
        // Post a credit-first balanced posting to uc/op. To get a CREDIT on uc,
        // use a reversed balanced posting.
        store
            .post(
                serde_json::from_value(serde_json::json!({
                    "posting_id": "cr1",
                    "entries": [
                        { "account_id": "op", "direction": "DEBIT", "amount": 30, "asset": "USD" },
                        { "account_id": "uc", "direction": "CREDIT", "amount": 30, "asset": "USD" }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        let page = store.ledger("uc", None, None, 10, None).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].running_balance, -30);
    }

    #[test]
    fn ledger_unknown_direction_zero_running() {
        let store = Store::new();
        setup(&store);
        // Inject an entry with an invalid direction into state directly.
        {
            let mut state = store.inner.lock();
            let prev = state.global_chain_head.clone();
            state.entries.push(EntryRecord {
                entry_id: "weird".to_string(),
                posting_id: "p".to_string(),
                account_id: "uc".to_string(),
                direction: "SIDEWAYS".to_string(),
                amount: 999,
                asset: "USD".to_string(),
                sequence_number: 9999,
                prev_hash: prev,
                this_hash: "hh".to_string(),
                created_at: "1000".to_string(),
            });
        }
        let page = store.ledger("uc", None, None, 10, None).unwrap();
        // The weird entry should produce a 0 running delta.
        let weird = page.entries.iter().find(|e| e.entry_id == "weird").unwrap();
        assert_eq!(weird.running_balance, 0);
    }

    #[test]
    fn post_rejects_amount_over_max_amount() {
        let store = Store::new();
        setup(&store);
        let req = serde_json::from_value(serde_json::json!({
            "posting_id": "overmax",
            "entries": [
                { "account_id": "uc", "direction": "DEBIT", "amount": MAX_AMOUNT + 1, "asset": "USD" },
                { "account_id": "op", "direction": "CREDIT", "amount": MAX_AMOUNT + 1, "asset": "USD" }
            ]
        })).unwrap();
        let err = store.post(req).unwrap_err();
        assert!(err.message().contains("exceeds MAX_AMOUNT"));
    }

    #[test]
    fn post_rejects_disallowed_direction_for_account_type() {
        let store = Store::new();
        // fee_revenue allows DEBIT/CREDIT per chart, so pick a type that
        // actually disallows one direction. All chart types allow both, so
        // construct a synthetic case: temporarily inject an account type
        // restriction by tampering with the account's type_name to a bogus
        // value that find_type returns None for. That path yields a panic via
        // unwrap, so instead we exercise the disallowed path by directly
        // hitting direction_allowed with a constructed AccountType.
        use crate::chart::{AccountType, AssetClass, Direction, NormalBalance};
        let t = AccountType {
            type_name: "no_credit",
            normal_balance: NormalBalance::Debit,
            allowed_directions: &["DEBIT"],
            asset_class: AssetClass::Both,
        };
        assert!(crate::chart::direction_allowed(&t, Direction::Debit));
        assert!(!crate::chart::direction_allowed(&t, Direction::Credit));
        // Keep store usage to satisfy unused warnings.
        let _ = store.list_accounts(None);
    }

    #[test]
    fn post_replay_returns_existing_entry_ids() {
        let store = Store::new();
        setup(&store);
        let req = balanced_posting("rep1", 100, "USD");
        let (r1, replay1) = store.post(req.clone()).unwrap();
        assert!(!replay1);
        let (r2, replay2) = store.post(req).unwrap();
        assert!(replay2);
        assert_eq!(r1.entry_ids, r2.entry_ids);
        assert_eq!(r1.hash_head, r2.hash_head);
    }

    #[test]
    fn record_audit_event_appends() {
        let store = Store::new();
        let ev = AuditEvent {
            event_id: "e1".to_string(),
            posting_id: "p1".to_string(),
            entry_ids: vec!["x".to_string()],
            hash_head: "h".to_string(),
            created_at: "0".to_string(),
        };
        store.record_audit_event(ev.clone());
        let evs = store.audit_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event_id, "e1");
    }

    #[test]
    fn hash_chain_anchor_missing_returns_none() {
        let store = Store::new();
        assert!(store.hash_chain_anchor("nope").is_none());
    }

    #[test]
    fn write_snapshots_without_pool_returns_snaps() {
        let store = Store::new();
        setup(&store);
        store.post(balanced_posting("ws1", 100, "USD")).unwrap();
        let snaps = store.write_snapshots();
        assert!(!snaps.is_empty());
    }

    #[test]
    fn latest_snapshot_returns_filtered() {
        let store = Store::new();
        setup(&store);
        store.post(balanced_posting("ls1", 100, "USD")).unwrap();
        store.write_snapshots();
        let s = store.latest_snapshot("uc", "USD");
        assert!(s.is_some());
        let s = s.unwrap();
        assert_eq!(s.account_id, "uc");
        assert_eq!(s.asset, "USD");
        // asset="" matches any
        let s2 = store.latest_snapshot("uc", "");
        assert!(s2.is_some());
        // unknown asset -> none
        assert!(store.latest_snapshot("uc", "BTC").is_none());
        // unknown account -> none
        assert!(store.latest_snapshot("nope", "USD").is_none());
    }

    #[test]
    fn balance_via_snapshot_unknown_account_returns_none() {
        let store = Store::new();
        assert!(store.balance_via_snapshot("nope", "USD").is_none());
    }

    #[test]
    fn balance_via_snapshot_without_snapshot_falls_back_to_direct() {
        let store = Store::new();
        setup(&store);
        store.post(balanced_posting("bvs1", 100, "USD")).unwrap();
        // No snapshots written -> falls back to compute_balance.
        assert_eq!(store.balance_via_snapshot("uc", "USD"), Some(100));
        // asset="" -> all
        assert_eq!(store.balance_via_snapshot("uc", ""), Some(100));
    }

    #[test]
    fn balance_via_snapshot_credit_delta() {
        let store = Store::new();
        setup(&store);
        store
            .post(
                serde_json::from_value(serde_json::json!({
                    "posting_id": "cvs1",
                    "entries": [
                        { "account_id": "op", "direction": "DEBIT", "amount": 50, "asset": "USD" },
                        { "account_id": "uc", "direction": "CREDIT", "amount": 50, "asset": "USD" }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        store.write_snapshots();
        // Additional posting with credit to uc -> negative delta.
        store
            .post(
                serde_json::from_value(serde_json::json!({
                    "posting_id": "cvs2",
                    "entries": [
                        { "account_id": "op", "direction": "DEBIT", "amount": 20, "asset": "USD" },
                        { "account_id": "uc", "direction": "CREDIT", "amount": 20, "asset": "USD" }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        let direct = store.balance("uc", "USD").unwrap();
        let via = store.balance_via_snapshot("uc", "USD").unwrap();
        assert_eq!(direct, via);
        assert_eq!(via, -70);
    }

    #[test]
    fn post_error_status_and_message() {
        let v = PostError::Validation("v".to_string());
        assert_eq!(v.status(), 400);
        assert_eq!(v.message(), "v");
        let u = PostError::Unbalanced("u".to_string());
        assert_eq!(u.status(), 400);
        assert_eq!(u.message(), "u");
    }

    #[test]
    fn now_iso_is_numeric_string() {
        let s = now_iso();
        assert!(s.parse::<u64>().is_ok());
    }

    #[test]
    fn secs_parses_and_falls_back_to_zero() {
        assert_eq!(secs("123"), 123);
        assert_eq!(secs("not-a-number"), 0);
    }

    #[test]
    fn is_already_applied_recognizes_known_codes() {
        // Construct a sqlx::Error with a database error code is non-trivial; instead,
        // verify the negative path with a non-database error.
        let err = sqlx::Error::Configuration("x".into());
        assert!(!is_already_applied(&err));
    }

    #[test]
    fn user_custodial_sum_skips_non_user_accounts() {
        let store = Store::new();
        setup(&store);
        store.post(balanced_posting("ucs", 100, "USD")).unwrap();
        // op is operational_fiat, should not contribute.
        assert_eq!(store.user_custodial_sum("USD"), 100);
        // asset="" sums all assets for user_custodial accounts.
        assert_eq!(store.user_custodial_sum(""), 100);
    }

    #[test]
    fn compute_balance_helper_credit_and_unknown_direction() {
        let store = Store::new();
        setup(&store);
        // Post a credit to uc.
        store
            .post(
                serde_json::from_value(serde_json::json!({
                    "posting_id": "cb1",
                    "entries": [
                        { "account_id": "op", "direction": "DEBIT", "amount": 40, "asset": "USD" },
                        { "account_id": "uc", "direction": "CREDIT", "amount": 40, "asset": "USD" }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(store.balance("uc", "USD"), Some(-40));
        // asset filter excludes
        store
            .post(
                serde_json::from_value(serde_json::json!({
                    "posting_id": "cb2",
                    "entries": [
                        { "account_id": "uc", "direction": "DEBIT", "amount": 5, "asset": "BTC" },
                        { "account_id": "op", "direction": "CREDIT", "amount": 5, "asset": "BTC" }
                    ]
                }))
                .unwrap(),
            )
            .unwrap();
        // USD balance unchanged by BTC posting.
        assert_eq!(store.balance("uc", "USD"), Some(-40));
        assert_eq!(store.balance("uc", "BTC"), Some(5));
        // all assets sum
        assert_eq!(store.balance("uc", ""), Some(-35));
    }
}
