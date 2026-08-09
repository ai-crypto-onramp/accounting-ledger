# Project Plan — Ledger / Accounting

Immutable double-entry ledger — the single source of financial truth for the
crypto on-ramp. Built in Rust on PostgreSQL with `SERIALIZABLE` isolation.
Correctness over everything: balanced postings, append-only entries,
tamper-evident hash chain, exact-once idempotency, and audit emission.

Implementation is broken into the stages below. Each stage is independently
mergeable and has explicit acceptance criteria wired to the invariants in the
README. Stages are ordered so that each builds on a previously verified
foundation: schema and the chart of accounts first, then the write path, then
idempotency, then the cryptographic chain, then reads, then multi-currency and
snapshots, then upstream integrations, and finally a hardened invariants test
suite.

## Stage 1 — Chart of Accounts & DB Schema (Serializable Isolation)

### Goal

Land the append-only PostgreSQL schema and the versioned chart of accounts as
the foundation for every later stage. All ledger transactions will run at
`SERIALIZABLE`; balance integrity is enforced at the DB layer, not just in
application code.

### Tasks

- [x] Add Diesel migrations for `accounts`, `chart_of_accounts`, `postings`,
      `entries`, `balance_snapshots`, and `hash_chain` tables.
- [x] Define `entries` as append-only: no UPDATE/DELETE paths; triggers or
      grants reject any such attempt.
- [x] Add constraints: `entries.amount > 0`, `direction` CHECK in
      (`debit`, `credit`), `posting_id` UNIQUE on `postings`.
- [x] Add FK from `entries.account_id` → `accounts.account_id` with status
      check (`active` only).
- [x] Seed the versioned chart of accounts (`chart_of_accounts`) with the
      account types from the README (`user_custodial`, `user_payable`,
      `operational_fiat`, `operational_crypto`, `treasury_fiat`,
      `treasury_crypto`, `fx_gain_loss`, `fee_revenue`, `rail_settlement`,
      `venue_settlement`, `chargeback_reserve`) including normal balance and
      allowed directions.
- [x] Configure SQLX pool with `DB_ISOLATION=serializable`; assert the
      isolation level at startup and refuse to boot if weaker.
- [x] Add `GET /v1/chart-of-accounts` returning the versioned catalog.
- [x] Add `POST /v1/accounts` and a minimal `accounts` service module.

### Acceptance criteria

- Migrations apply cleanly to a fresh PostgreSQL >= 14 instance and set the
  session default isolation to `SERIALIZABLE`.
- An attempt to `UPDATE` or `DELETE` an `entries` row is rejected by the DB.
- `GET /v1/chart-of-accounts` returns all seeded account types with correct
  normal balances and allowed directions.
- `POST /v1/accounts` creates an account and rejects an unknown `type` or
  `asset_class` per the chart of accounts.
- The service refuses to start if the DB isolation is not `SERIALIZABLE`.

## Stage 2 — Posting Endpoint with Strict Invariants

### Goal

Implement `POST /v1/postings` as the single write path, enforcing all posting
invariants atomically under `SERIALIZABLE`. A posting either commits all
entries or none.

### Tasks

- [x] Define request/response DTOs for `POST /v1/postings` matching the README
      contract (`posting_id`, `entries[]`, `memo`, `ref_tx_id`).
- [x] Validate `MAX_ENTRIES_PER_POSTING` and `MAX_AMOUNT` bounds.
- [x] Enforce invariant: every posting has at least two entries.
- [x] Enforce invariant: `sum(debits) == sum(credits)` per posting, per asset.
- [x] Enforce invariant: every `account_id` exists and is `active`.
- [x] Enforce invariant: `direction` and `asset_class` are consistent with the
      account's chart-of-accounts definition.
- [x] Insert `postings` + `entries` in a single `SERIALIZABLE` transaction;
      reject atomically if any invariant fails.
- [x] Return `entry_id[]`, posting status, and (for now) a placeholder hash
      chain head.

### Acceptance criteria

- A balanced posting commits all entries; an unbalanced posting is rejected
  with no partial writes.
- A posting referencing an inactive or nonexistent account is rejected.
- A posting with a direction disallowed by the account type is rejected.
- All validation runs in a `SERIALIZABLE` transaction; no read-committed
  fallback path exists.

## Stage 3 — Posting Idempotency (`posting_id`)

### Goal

Make the posting path safe to retry exactly-once. Duplicate submissions with
the same caller-supplied `posting_id` return the original result without
creating new entries.

### Tasks

- [x] Enforce `posting_id` UNIQUE on `postings`.
- [x] On insert conflict for `posting_id`, load and return the original
      posting's result (entry ids, status, hash chain head) without writing.
- [x] Add tests for: replay after success, replay after transient failure
  that did not commit, concurrent duplicate submissions.
- [x] Document that the Transaction Orchestrator derives `posting_id` from
      saga ID + step, making retries safe.

### Acceptance criteria

- Submitting the same `posting_id` twice returns identical results and
  produces exactly one set of entries.
- Concurrent duplicate submissions under `SERIALIZABLE` never create duplicate
  entries; exactly one wins.
- Replays after a non-committing failure create the posting fresh on retry.

## Stage 4 — Hash Chain (Cryptographic Chaining of Entries)

### Goal

Establish the tamper-evident hash chain across entries. Each entry stores
`prev_hash` and `this_hash = H(canonical(entry) || prev_hash)`, forming a
chain whose break is a fatal alert.

### Tasks

- [x] Define the canonical byte representation of an entry for hashing
      (deterministic ordering, no floats, fixed-width where possible).
- [x] Implement `this_hash = H(canonical(entry) || prev_hash)` using
      `HASH_CHAIN_ALG` (default `sha256`) and mix in `HASH_CHAIN_SALT` if set.
- [x] Compute hashes inside the posting transaction so the chain is
      consistent at commit.
- [x] Maintain `hash_chain` anchor/head rows: per-posting head and a global
      sequence head.
- [x] Return `hash_chain_head` from `POST /v1/postings`.
- [x] Implement a verification query that walks the chain and detects any
      break.
- [x] Add a startup/background check that verifies the chain and raises a
      fatal alert on mismatch.

### Acceptance criteria

- Every committed entry has a valid `this_hash` linking to the previous
  entry's hash.
- Mutating an entry (which is already blocked by append-only constraints)
  would be detected by the verification query.
- `POST /v1/postings` returns the correct `hash_chain_head`.
- Chain verification passes on a clean DB and fails on any tampered row.

## Stage 5 — Balance Queries & Historical Statements

### Goal

Serve authoritative current balances and paginated historical ledger
statements with running balance. Reads are read-only at `SERIALIZABLE`.

### Tasks

- [x] Implement `GET /v1/accounts/:id/balance?asset=` computing the current
      balance from entries (debit/credit per the account's normal balance).
- [x] Implement `GET /v1/accounts/:id/ledger?from=&to=&limit=&cursor=`
      returning paginated entries with a running balance column.
- [x] Implement `GET /v1/postings/:id` returning the full posting with all
      entries and hashes.
- [x] Add REST (read) router distinct from the internal gRPC write path.
- [x] Ensure balance queries target p99 < 50 ms (indexed by `account_id`,
      `asset`, `created_at`).

### Acceptance criteria

- `GET /v1/accounts/:id/balance` returns the correct authoritative balance
  matching the sum of entries.
- `GET /v1/accounts/:id/ledger` returns entries in order with a correct
  running balance and paginates via cursor.
- `GET /v1/postings/:id` returns all entries with their hash chain fields.

## Stage 6 — Multi-Currency & Asset Accounts

### Goal

Support multiple assets (`USD`, `EUR`, `BTC`, `ETH`, `USDC`, ...) with no
implicit conversion. FX is recorded via explicit postings to `fx_gain_loss`.

### Tasks

- [x] Carry `asset` on every entry; enforce that all entries in a posting
      balance per asset (sum debits == sum credits per asset).
- [x] Validate `asset` against a configured asset registry (smallest-unit
      scale, `MAX_AMOUNT` per asset).
- [x] Document and test the FX posting pattern: convert via
      `operational_fiat` / `operational_crypto` and book the difference to
      `fx_gain_loss`.
- [x] Ensure user custodial accounts are per-user, per-asset and segregated
      from operational/treasury floats.
- [x] Add a query that returns the sum of `user_custodial` balances per asset
      (input to the reconciliation invariant: sum of user custodial ==
      user-funds liability).

### Acceptance criteria

- A posting with unbalanced amounts in any single asset is rejected.
- A multi-asset posting that balances per asset is accepted.
- FX postings correctly route gains/losses to `fx_gain_loss`.
- User custodial balances are queryable separately from operational balances.

## Stage 7 — Balance Snapshots

### Goal

Materialize periodic balance snapshots per account/asset so historical
statement queries stay cheap and bounded as the entries table grows.

### Tasks

- [x] Implement `balance_snapshots` writer: `(account_id, asset, balance,
      as_of_ts, last_entry_id)`.
- [x] Add a background task on `SNAPSHOT_INTERVAL_MINUTES` cadence that
      rebuilds snapshots under `SERIALIZABLE`.
- [x] Rewrite balance queries to use the latest snapshot + delta entries
      since `last_entry_id`.
- [x] Rewrite historical statements to anchor running balance at the latest
      snapshot at or before `from`.
- [x] Add a reconciliation check that snapshot balance equals the sum of
      entries up to `last_entry_id`.

### Acceptance criteria

- Snapshots are written on the configured cadence and are consistent with the
  entry sum at `as_of_ts`.
- Balance queries return identical results whether computed from entries
  alone or from snapshot + delta.
- Historical statement performance stays bounded as entries grow.

## Stage 8 — Tx-Orchestrator & Treasury Integration

### Goal

Wire the ledger into the Transaction Orchestrator (sync, transaction path)
and Treasury Orchestration (sync, hedging/rebalancing). The ledger is the only
authorized writer of financial state.

### Tasks

- [x] Add the gRPC (internal) API surface for posting and account management
      used by the orchestrators.
- [x] Confirm the only authorized callers are Transaction Orchestrator and
      Treasury Orchestration (auth/mTLS or equivalent internal gating).
- [ ] Implement the example posting from the README (user buys BTC with USD)
      as an integration test against the orchestrator call path.
- [x] Add Treasury Orchestration posting patterns: hedging entries to
      `treasury_fiat` / `treasury_crypto` and rebalancing across venues.
- [x] Ensure `posting_id` derivation from saga ID + step makes orchestrator
      retries exactly-once.

### Acceptance criteria

- Transaction Orchestrator can post the full user-buys-BTC flow atomically.
- Treasury Orchestration can post hedging and rebalancing entries.
- Retries from either orchestrator with the same `posting_id` do not create
  duplicate entries.

## Stage 9 — Reconciliation Reads, Audit Emission & Invariants Test Suite

### Goal

Expose reconciliation reads, emit per-posting audit events, and lock down
correctness with a dedicated invariants test suite plus coverage and Docker
hardening.

### Tasks

- [x] Add read-only endpoints/queries consumed by Reconciliation: balances,
      entry streams, and the user-funds-liability vs user-custodial sum check.
- [x] Emit one event per committed posting to the Audit / Event Log
      (`AUDIT_EVENT_LOG_URL`) with posting id, entry ids, and hash chain head.
- [x] Add `tests/invariants.rs` covering: balanced postings, idempotency,
      hash-chain integrity, immutability, segregation of user vs operational
      funds, and serializable concurrency.
- [x] Wire `cargo test --test invariants -- --nocapture` in CI.
- [x] Enforce `cargo clippy -- -D warnings` and `cargo fmt --check` in CI.
- [x] Finalize the Dockerfile for release builds and verify the
      `cargo build --release` image boots and passes healthcheck.
- [x] Confirm codecov reporting is wired for the invariants suite.

### Acceptance criteria

- Reconciliation can pull all entries and balances needed to match bank,
  exchange, and on-chain state.
- Every committed posting produces exactly one audit event.
- `cargo test --all` passes, including the invariants suite.
- `cargo clippy -- -D warnings` and `cargo fmt --check` are clean.
- The release Docker image boots and `/healthz` returns ok.