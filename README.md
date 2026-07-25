# Ledger / Accounting

![CI](https://github.com/ai-crypto-onramp/accounting-ledger/actions/workflows/ci.yml/badge.svg)
[![codecov](https://codecov.io/gh/ai-crypto-onramp/accounting-ledger/branch/main/graph/badge.svg)](https://codecov.io/gh/ai-crypto-onramp/accounting-ledger)

Immutable double-entry ledger — the single source of financial truth. Correctness over everything.

## Overview / Responsibilities

The Ledger / Accounting service is the immutable, append-only, double-entry ledger
of the crypto on-ramp. It is the **single source of financial truth** for every
movement of fiat and crypto: user funds, operational floats, treasury positions,
fees, and FX gains/losses. Every state change that affects money must result in a
balanced posting to this service.

Responsibilities:

- Record every financial movement as a balanced double-entry posting.
- Maintain the chart of accounts (per-user, per-asset, per-venue, treasury).
- Serve authoritative balance queries and historical ledger statements.
- Enforce strict invariants: `sum(debits) == sum(credits)`, immutability,
  serializable consistency, no lost updates.
- Provide a tamper-evident hash chain across entries for forensic integrity.
- Emit posting events consumed by Reconciliation and the Audit / Event Log.

The service is called synchronously by the **Transaction Orchestrator** on the
transaction path (`ORCH → LEDGER`) and by **Treasury Orchestration** for hedging
entries. It is read asynchronously by **Reconciliation** and consumed by the
**Audit / Event Log**.

## Language & Tech Stack

- **Language:** Rust — chosen because a bug here means lost funds. Correctness
  over everything.
- **Accounting model:** Double-entry; every posting debits and credits a
  balanced set of accounts.
- **Storage:** Append-only. Entries are never updated or deleted; corrections
  are made via reversing postings.
- **DB access:** SQLX (compile-time-checked SQL) with Diesel considered for
  schema migrations. No ORM magic on the hot path — SQL is explicit and checked.
- **Database:** PostgreSQL with strict constraints and `SERIALIZABLE`
  isolation. Balance integrity is enforced at the DB layer, not just in
  application code.
- **Balance snapshots:** Periodic materialized balance snapshots per
  account/asset to keep historical statement queries cheap and bounded.
- **Hash chain:** Each entry stores a hash of the previous entry's canonical
  representation, forming a tamper-evident chain.

## System Requirements

1. **Immutable double-entry ledger.** Entries are append-only; no updates, no
   deletes. Corrections are reversing postings referencing the original.
2. **Balanced postings.** Every posting must contain at least two entries and
   the sum of debits must equal the sum of credits, per posting and per asset.
3. **Account topology.** Maintain accounts segmented across four dimensions:
   - **Per-user** custodial and payable accounts.
   - **Per-asset** (e.g. `USD`, `EUR`, `BTC`, `ETH`, `USDC`).
   - **Per-venue** (exchange/OTC/rail settlement accounts).
   - **Treasury** operational and reserve accounts.
4. **Chart of accounts.** A versioned chart of accounts defines account types,
   normal balances, and allowable entry directions.
5. **Strict invariants on posting.**
   - `sum(debits) == sum(credits)` per posting.
   - All referenced `account_id`s exist and are active.
   - Asset and direction are consistent with the account's chart-of-accounts
     definition.
   - Posting is rejected atomically if any invariant fails.
6. **Balance queries.** Authoritative current balance per `account_id` + asset,
   computed from entries (or the latest snapshot + delta).
7. **Historical statements.** Paginated ledger entries for an account over an
   arbitrary `[from, to]` window, with running balance.
8. **Multi-currency / multi-asset.** Amounts carry an asset unit; no implicit
   conversion. FX is recorded via explicit FX postings to `fx_gain_loss`.
9. **Segregation of user funds vs operational accounts.** User custodial
   balances are held in distinct accounts from operational and treasury floats.
   The sum of user custodial accounts must equal the user-funds liability at
   all times (a reconciliation invariant, not enforced here but queryable).
10. **Idempotent posting.** Every posting carries a caller-supplied
    `posting_id` (unique). Duplicate submissions with the same `posting_id`
    return the original result without creating new entries.

## Non-Functional Requirements

| Requirement | Target |
|---|---|
| Posting latency (p99) | < 20 ms |
| Balance query latency (p99) | < 50 ms |
| Consistency | `SERIALIZABLE`; strict, no read-committed fallback |
| Lost updates | None permitted — enforced via serializable isolation + row locks |
| Mutability | Append-only; entries immutable after commit |
| Tamper-evidence | Cryptographic hash chain (`hash_chain` linking each entry to the previous entry's hash) |
| Availability | 99.99% (ledger unavailability blocks all money movement) |
| Durability | Synchronous replication to standby; no ack before WAL flush |
| Auditability | Every posting emits an event to the Audit / Event Log |

## Technical Specifications

### API Surface

- **gRPC (internal):** Posting and account management APIs used by the
  Transaction Orchestrator and Treasury Orchestration. Primary write path.
- **REST (read):** Balance and ledger-statement reads for internal services and
  ops dashboards. Read-only.

### Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| `POST` | `/v1/postings` | `{ posting_id, entries[]: { account_id, direction: debit\|credit, amount, asset }, memo, ref_tx_id }` | `entry_id[]`, posting status, hash chain head |
| `POST` | `/v1/accounts` | `{ account_id (optional), type, asset_class, label, parent_id? }` | `account_id` |
| `GET` | `/v1/accounts/:id/balance` | query: `?asset=` | `{ account_id, asset, balance, as_of_ts }` |
| `GET` | `/v1/accounts/:id/ledger` | query: `?from=&to=&limit=&cursor=` | paginated entries with running balance |
| `GET` | `/v1/chart-of-accounts` | — | versioned chart of accounts |
| `GET` | `/v1/postings/:id` | — | full posting with all entries and hashes |

`direction` is `debit` or `credit`. `amount` is an unsigned integer in the
asset's smallest unit (e.g. cents, satoshi, wei) — no floats on the wire.

### Data Model

- **`accounts`** — account definitions: `account_id`, `type`
  (`user_custodial`, `user_payable`, `operational_fiat`, `operational_crypto`,
  `treasury_fiat`, `treasury_crypto`, `fx_gain_loss`, `fee_revenue`, ...),
  `asset_class`, `label`, `parent_id`, `status`, `created_at`.
- **`chart_of_accounts`** — versioned catalog of account types, normal
  balances, and allowed directions.
- **`postings`** — a logical posting: `posting_id` (caller-supplied, unique),
  `ref_tx_id`, `memo`, `status`, `created_at`, `hash_chain_head`.
- **`entries`** — the individual debit/credit lines: `entry_id`, `posting_id`,
  `account_id`, `direction`, `amount`, `asset`, `prev_hash`, `this_hash`,
  `created_at`. Append-only; no update/delete path exists.
- **`balance_snapshots`** — materialized balances: `account_id`, `asset`,
  `balance`, `as_of_ts`, `last_entry_id`. Rebuilt on a fixed cadence.
- **`hash_chain`** — anchor/head rows for the tamper-evident chain per posting
  and per global sequence; supports verification queries.

### Invariants

1. **Balanced:** `sum(debits) == sum(credits)` per posting, per asset.
2. **Immutable:** entries have no UPDATE or DELETE path; constraints reject
   any such attempt.
3. **Hash chain:** each entry's `this_hash = H(canonical(entry) || prev_hash)`;
   a break in the chain is a fatal alert.
4. **Referential:** every `account_id` in an entry exists in `accounts` and is
   `active`; its `type` allows the given `direction` and `asset_class`.
5. **Idempotent:** `posting_id` is unique; replays return the original result.
6. **Serializable:** all read/write transactions run at `SERIALIZABLE`.

### Integrations

- **Transaction Orchestrator (caller, sync):** posts balanced entries on the
  transaction path (payment capture, crypto delivery, fee taking).
- **Treasury Orchestration (caller, sync):** posts hedging and rebalancing
  entries to treasury and FX accounts.
- **Reconciliation (reader, async):** reads entries and balances to match
  against bank, exchange, and on-chain state.
- **Audit / Event Log (consumer, async):** consumes posted-entry events for
  compliance and incident forensics.

### Posting Idempotency

`posting_id` is a caller-supplied, service-scoped unique key. The Transaction
Orchestrator derives it from its saga ID + step, so retries on transient
failure (network, timeout) are safe. The ledger:

- Inserts `postings` with `posting_id` as a unique constraint.
- On conflict, returns the existing posting's result without writing new
  entries.
- This makes the posting path safe to retry exactly-once under
  `SERIALIZABLE`.

## Dependencies

- **PostgreSQL** (>= 14) configured with `SERIALIZABLE` isolation as the
  default for ledger transactions, synchronous replication, and forced WAL
  flush (`synchronous_commit = on`). No fallback to weaker isolation.
- **audit-event-log** — receives an event per committed posting for the
  append-only audit trail.
- **Transaction Orchestrator** / **Treasury Orchestration** — upstream callers
  (not runtime dependencies, but the only authorized writers).

## Configuration

| Env var | Description | Example |
|---|---|---|
| `PORT` | gRPC + REST listen port | `8080` |
| `DB_URL` | PostgreSQL connection string | `postgres://ledger:***@db:5432/ledger` |
| `DB_ISOLATION` | Transaction isolation level (must be `serializable`) | `serializable` |
| `DB_MAX_CONNECTIONS` | SQLX pool size | `32` |
| `SNAPSHOT_INTERVAL_MINUTES` | Cadence for rebuilding balance snapshots | `15` |
| `HASH_CHAIN_ALG` | Hash algorithm for the entry chain | `sha256` |
| `HASH_CHAIN_SALT` | Optional per-deployment salt mixed into the chain | `***` |
| `AUDIT_EVENT_LOG_URL` | gRPC endpoint of the Audit / Event Log | `audit-event-log:9090` |
| `MAX_ENTRIES_PER_POSTING` | Hard cap on entries per posting | `64` |
| `MAX_AMOUNT` | Per-asset max amount sanity bound (configurable per asset) | `100000000000` |
| `LOG_LEVEL` | Structured log level | `info` |

## Local Development

```bash
# Build
cargo build --release

# Run (requires PostgreSQL with serializable isolation)
./target/release/ledger-accounting

# Tests — includes the accounting invariants test suite
# (balanced postings, idempotency, hash-chain integrity,
#  immutability, segregation of user vs operational funds)
cargo test --all
cargo test --test invariants -- --nocapture

# Lint
cargo clippy -- -D warnings
cargo fmt --check
```

## Accounting Model

Sketch of the chart of accounts. Every account is typed; the type defines the
normal balance side and allowable directions.

| Account type | Normal balance | Purpose |
|---|---|---|
| `user_custodial` | credit | Funds held on behalf of users (liability to the platform). Per-user, per-asset. |
| `user_payable` | credit | Funds owed but not yet credited to the user's custodial account. |
| `operational_fiat` | debit | Operational fiat float (settlement accounts, rail holdings). |
| `operational_crypto` | debit | Operational crypto float (hot-wallet funding buffers). |
| `treasury_fiat` | debit | Treasury fiat reserves and bank accounts. |
| `treasury_crypto` | debit | Treasury crypto reserves (cold/warm custody). |
| `fx_gain_loss` | either | Realized FX gains and losses from currency conversion. |
| `fee_revenue` | credit | Fee revenue recognized per transaction. |
| `rail_settlement` | debit | In-transit funds on a payment rail awaiting settlement. |
| `venue_settlement` | debit | In-transit funds at an exchange/OTC venue awaiting settlement. |
| `chargeback_reserve` | credit | Reserve for anticipated chargebacks and disputes. |

### Example posting — user buys BTC with USD

```
posting_id: tx_123_step_ledger
ref_tx_id:  tx_123
entries:
  - debit  user_custodial:BTC   0.001 BTC      # user receives crypto
  - credit operational_crypto:BTC 0.001 BTC    # source of crypto
  - debit  operational_fiat:USD   100.00 USD   # fiat taken in
  - credit user_custodial:USD     2.00 USD     # change / fee basis
  - debit  user_custodial:USD     2.00 USD     # fee charged
  - credit fee_revenue:USD        2.00 USD     # fee recognized
```

All six entries are committed atomically under `SERIALIZABLE`; the hash chain
is extended; an event is emitted to the Audit / Event Log; the posting is
visible to Reconciliation.
