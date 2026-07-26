use tonic::{Request, Response, Status};

use crate::account::CreateAccountRequest as AcctReq;
use crate::posting::PostingRequest as PostReq;
use crate::store::Store;

pub mod ledger {
    tonic::include_proto!("ledger.v1");
}

use ledger::ledger_service_server::{LedgerService, LedgerServiceServer};
use ledger::{
    BalanceResponse, CreateAccountRequest as PbCreateAccountRequest, CreateAccountResponse,
    GetBalanceRequest, GetPostingRequest, PostDoubleEntryRequest, PostDoubleEntryResponse,
    PostingRecord as PbPostingRecord, PostingRequest as PbPostingRequest,
    PostingResponse as PbPostingResponse, VerifyChainRequest, VerifyChainResponse,
};

pub struct LedgerGrpc {
    pub store: Store,
    pub allowed_callers: Vec<String>,
}

impl LedgerGrpc {
    pub fn new(store: Store, allowed_callers: Vec<String>) -> Self {
        Self {
            store,
            allowed_callers,
        }
    }

    #[allow(clippy::result_large_err)]
    fn authorize(&self, caller: Option<&str>) -> Result<(), Status> {
        match caller {
            Some(c) if self.allowed_callers.iter().any(|a| a == c) => Ok(()),
            _ => Err(Status::unauthenticated(
                "caller is not an authorized orchestrator",
            )),
        }
    }
}

#[tonic::async_trait]
impl LedgerService for LedgerGrpc {
    async fn create_account(
        &self,
        req: Request<PbCreateAccountRequest>,
    ) -> Result<Response<CreateAccountResponse>, Status> {
        self.authorize(req.metadata().get("x-caller").and_then(|v| v.to_str().ok()))?;
        let req = req.into_inner();
        let account_req = AcctReq {
            account_id: opt_string(req.account_id),
            type_name: req.r#type,
            asset_class: req.asset_class,
            label: req.label,
            parent_id: opt_string(req.parent_id),
        };
        match self.store.create_account(account_req) {
            Ok(acc) => Ok(Response::new(CreateAccountResponse {
                account_id: acc.account_id,
            })),
            Err(e) => Err(Status::invalid_argument(e)),
        }
    }

    async fn post_posting(
        &self,
        req: Request<PbPostingRequest>,
    ) -> Result<Response<PbPostingResponse>, Status> {
        self.authorize(req.metadata().get("x-caller").and_then(|v| v.to_str().ok()))?;
        let req = req.into_inner();
        let entries: Vec<crate::posting::EntryInput> = req
            .entries
            .into_iter()
            .map(|e| crate::posting::EntryInput {
                account_id: e.account_id,
                direction: e.direction,
                amount: e.amount,
                asset: e.asset,
            })
            .collect();
        let post_req = PostReq {
            posting_id: req.posting_id,
            entries,
            memo: opt_string(req.memo),
            ref_tx_id: opt_string(req.ref_tx_id),
        };
        match self.store.post(post_req) {
            Ok((resp, _replay)) => Ok(Response::new(PbPostingResponse {
                posting_id: resp.posting_id,
                status: resp.status,
                entry_ids: resp.entry_ids,
                hash_head: resp.hash_head,
            })),
            Err(e) => Err(Status::invalid_argument(e.message())),
        }
    }

    async fn post_double_entry(
        &self,
        req: Request<PostDoubleEntryRequest>,
    ) -> Result<Response<PostDoubleEntryResponse>, Status> {
        self.authorize(req.metadata().get("x-caller").and_then(|v| v.to_str().ok()))?;
        let req = req.into_inner();
        let debit_account = opt_string(req.debit_account_id);
        let credit_account = opt_string(req.credit_account_id);
        let (debit, credit) = match (debit_account, credit_account) {
            (Some(d), Some(c)) => (d, c),
            _ => resolve_double_entry_accounts(&self.store, &req.asset, &req.rail)?,
        };
        let entries = vec![
            crate::posting::EntryInput {
                account_id: debit.clone(),
                direction: "DEBIT".to_string(),
                amount: req.amount,
                asset: req.asset.clone(),
            },
            crate::posting::EntryInput {
                account_id: credit.clone(),
                direction: "CREDIT".to_string(),
                amount: req.amount,
                asset: req.asset.clone(),
            },
        ];
        let post_req = PostReq {
            posting_id: req.tx_id.clone(),
            entries,
            memo: opt_string(req.memo),
            ref_tx_id: Some(req.tx_id),
        };
        match self.store.post(post_req) {
            Ok((resp, _replay)) => Ok(Response::new(PostDoubleEntryResponse {
                journal_id: resp.posting_id,
                status: resp.status,
                hash_head: resp.hash_head,
            })),
            Err(e) => Err(Status::invalid_argument(e.message())),
        }
    }

    async fn get_posting(
        &self,
        req: Request<GetPostingRequest>,
    ) -> Result<Response<PbPostingRecord>, Status> {
        let req = req.into_inner();
        match self.store.get_posting(&req.posting_id) {
            Some(p) => Ok(Response::new(posting_to_pb(&p))),
            None => Err(Status::not_found(format!(
                "posting not found: {}",
                req.posting_id
            ))),
        }
    }

    async fn get_balance(
        &self,
        req: Request<GetBalanceRequest>,
    ) -> Result<Response<BalanceResponse>, Status> {
        let req = req.into_inner();
        let asset = req.asset;
        match self.store.balance(&req.account_id, &asset) {
            Some(bal) => Ok(Response::new(BalanceResponse {
                account_id: req.account_id,
                asset: if asset.is_empty() {
                    "all".to_string()
                } else {
                    asset
                },
                balance: bal.to_string(),
                as_of_ts: crate::store::now_iso(),
            })),
            None => Err(Status::not_found(format!(
                "account not found: {}",
                req.account_id
            ))),
        }
    }

    async fn verify_chain(
        &self,
        _req: Request<VerifyChainRequest>,
    ) -> Result<Response<VerifyChainResponse>, Status> {
        let result = self.store.verify_chain();
        match result {
            Ok(()) => Ok(Response::new(VerifyChainResponse {
                ok: true,
                entry_id: String::new(),
                reason: String::new(),
            })),
            Err(b) => Ok(Response::new(VerifyChainResponse {
                ok: false,
                entry_id: b.entry_id,
                reason: b.reason,
            })),
        }
    }
}

// opt_string converts a canonical proto String field (which is "" when unset
// under proto3) to an Option<String>, treating empty as absent. This preserves
// the existing internal-store semantics where empty fields are None.
fn opt_string(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn posting_to_pb(p: &crate::posting::PostingRecord) -> PbPostingRecord {
    PbPostingRecord {
        posting_id: p.posting_id.clone(),
        ref_tx_id: p.ref_tx_id.clone().unwrap_or_default(),
        memo: p.memo.clone().unwrap_or_default(),
        status: p.status.clone(),
        hash_head: p.hash_head.clone(),
        entries: p
            .entries
            .iter()
            .map(|e| ledger::EntryRecord {
                entry_id: e.entry_id.clone(),
                posting_id: e.posting_id.clone(),
                account_id: e.account_id.clone(),
                direction: e.direction.clone(),
                amount: e.amount,
                asset: e.asset.clone(),
                sequence_number: e.sequence_number,
                prev_hash: e.prev_hash.clone(),
                this_hash: e.this_hash.clone(),
                created_at: e.created_at.clone(),
            })
            .collect(),
        created_at: p.created_at.clone(),
    }
}

// resolve_double_entry_accounts resolves the canonical debit/credit account
// ids for a (asset, rail) pair by looking up accounts of the appropriate type
// in the store. For the fiat leg of a purchase saga: debit user_custodial and
// credit operational_fiat. Returns InvalidArgument if no matching accounts are
// found.
fn resolve_double_entry_accounts(
    store: &Store,
    asset: &str,
    _rail: &str,
) -> Result<(String, String), Status> {
    let debit = store
        .list_accounts(Some("user_custodial"))
        .into_iter()
        .next()
        .ok_or_else(|| {
            Status::invalid_argument("no user_custodial account found for double-entry")
        })?;
    let credit = store
        .list_accounts(Some("operational_fiat"))
        .into_iter()
        .next()
        .ok_or_else(|| {
            Status::invalid_argument("no operational_fiat account found for double-entry")
        })?;
    let _ = asset;
    Ok((debit.account_id, credit.account_id))
}

pub fn server(store: Store, allowed_callers: Vec<String>) -> LedgerServiceServer<LedgerGrpc> {
    LedgerServiceServer::new(LedgerGrpc::new(store, allowed_callers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::metadata::MetadataMap;

    fn store_with_account(id: &str, type_name: &str, asset_class: &str) -> Store {
        let store = Store::new();
        store
            .create_account(
                serde_json::from_value(serde_json::json!({
                    "account_id": id,
                    "type": type_name,
                    "asset_class": asset_class,
                    "label": format!("{}-{}", type_name, id),
                }))
                .unwrap(),
            )
            .unwrap();
        store
    }

    fn balanced_posting(posting_id: &str) -> PostReq {
        PostReq {
            posting_id: posting_id.to_string(),
            entries: vec![
                crate::posting::EntryInput {
                    account_id: "uc".to_string(),
                    direction: "DEBIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
                crate::posting::EntryInput {
                    account_id: "op".to_string(),
                    direction: "CREDIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
            ],
            memo: None,
            ref_tx_id: None,
        }
    }

    #[tokio::test]
    async fn authorize_rejects_missing_caller() {
        let svc = LedgerGrpc::new(Store::new(), vec!["transaction-orchestrator".to_string()]);
        assert!(svc.authorize(None).is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_unknown_caller() {
        let svc = LedgerGrpc::new(Store::new(), vec!["transaction-orchestrator".to_string()]);
        assert!(svc.authorize(Some("intruder")).is_err());
    }

    #[tokio::test]
    async fn authorize_accepts_known_caller() {
        let svc = LedgerGrpc::new(Store::new(), vec!["transaction-orchestrator".to_string()]);
        assert!(svc.authorize(Some("transaction-orchestrator")).is_ok());
    }

    #[tokio::test]
    async fn create_account_grpc_success_and_failure() {
        let store = store_with_account("uc", "user_custodial", "BOTH");
        let svc = LedgerGrpc::new(store, vec!["caller".to_string()]);

        // Success path with authorized caller.
        let mut req = Request::new(PbCreateAccountRequest {
            account_id: "op".to_string(),
            r#type: "operational_fiat".to_string(),
            asset_class: "FIAT".to_string(),
            label: "op".to_string(),
            parent_id: String::new(),
        });
        req.metadata_mut()
            .insert("x-caller", "caller".parse().unwrap());
        let resp = svc.create_account(req).await.unwrap();
        assert_eq!(resp.into_inner().account_id, "op");

        // Failure path: unknown account type with authorized caller.
        let mut req2 = Request::new(PbCreateAccountRequest {
            account_id: "bad".to_string(),
            r#type: "bogus".to_string(),
            asset_class: "FIAT".to_string(),
            label: "bad".to_string(),
            parent_id: String::new(),
        });
        req2.metadata_mut()
            .insert("x-caller", "caller".parse().unwrap());
        assert!(svc.create_account(req2).await.is_err());

        // Unauthorized: no caller header.
        let req3 = Request::new(PbCreateAccountRequest {
            account_id: "x".to_string(),
            r#type: "user_custodial".to_string(),
            asset_class: "FIAT".to_string(),
            label: "x".to_string(),
            parent_id: String::new(),
        });
        assert!(svc.create_account(req3).await.is_err());
    }

    #[tokio::test]
    async fn post_posting_grpc_success_and_failure() {
        let store = store_with_account("uc", "user_custodial", "BOTH");
        store
            .create_account(
                serde_json::from_value(serde_json::json!({
                    "account_id": "op",
                    "type": "operational_fiat",
                    "asset_class": "FIAT",
                    "label": "op",
                }))
                .unwrap(),
            )
            .unwrap();
        let svc = LedgerGrpc::new(store, vec!["caller".to_string()]);

        let mut req = Request::new(PbPostingRequest {
            posting_id: "p1".to_string(),
            entries: vec![],
            memo: String::new(),
            ref_tx_id: String::new(),
        });
        req.metadata_mut()
            .insert("x-caller", "caller".parse().unwrap());
        // Empty entries -> error.
        assert!(svc.post_posting(req).await.is_err());

        // Authorized balanced post -> ok.
        let mut req = Request::new(PbPostingRequest {
            posting_id: "p1".to_string(),
            entries: vec![
                ledger::EntryInput {
                    account_id: "uc".to_string(),
                    direction: "DEBIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
                ledger::EntryInput {
                    account_id: "op".to_string(),
                    direction: "CREDIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
            ],
            memo: String::new(),
            ref_tx_id: String::new(),
        });
        req.metadata_mut()
            .insert("x-caller", "caller".parse().unwrap());
        let resp = svc.post_posting(req).await.unwrap().into_inner();
        assert_eq!(resp.posting_id, "p1");
        assert_eq!(resp.status, "POSTED");
        assert_eq!(resp.entry_ids.len(), 2);

        // Unauthorized caller.
        let req = Request::new(PbPostingRequest {
            posting_id: "p2".to_string(),
            entries: vec![
                ledger::EntryInput {
                    account_id: "uc".to_string(),
                    direction: "DEBIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
                ledger::EntryInput {
                    account_id: "op".to_string(),
                    direction: "CREDIT".to_string(),
                    amount: 100,
                    asset: "USD".to_string(),
                },
            ],
            memo: String::new(),
            ref_tx_id: String::new(),
        });
        assert!(svc.post_posting(req).await.is_err());
    }

    #[tokio::test]
    async fn get_posting_grpc_found_and_missing() {
        let store = store_with_account("uc", "user_custodial", "BOTH");
        store
            .create_account(
                serde_json::from_value(serde_json::json!({
                    "account_id": "op",
                    "type": "operational_fiat",
                    "asset_class": "FIAT",
                    "label": "op",
                }))
                .unwrap(),
            )
            .unwrap();
        store.post(balanced_posting("g1")).unwrap();
        let svc = LedgerGrpc::new(store, vec!["caller".to_string()]);

        let req = Request::new(GetPostingRequest {
            posting_id: "g1".to_string(),
        });
        let resp = svc.get_posting(req).await.unwrap().into_inner();
        assert_eq!(resp.posting_id, "g1");
        assert_eq!(resp.entries.len(), 2);

        let req = Request::new(GetPostingRequest {
            posting_id: "missing".to_string(),
        });
        assert!(svc.get_posting(req).await.is_err());
    }

    #[tokio::test]
    async fn get_balance_grpc_found_and_missing() {
        let store = store_with_account("uc", "user_custodial", "BOTH");
        store
            .create_account(
                serde_json::from_value(serde_json::json!({
                    "account_id": "op",
                    "type": "operational_fiat",
                    "asset_class": "FIAT",
                    "label": "op",
                }))
                .unwrap(),
            )
            .unwrap();
        store.post(balanced_posting("b1")).unwrap();
        let svc = LedgerGrpc::new(store, vec!["caller".to_string()]);

        let req = Request::new(GetBalanceRequest {
            account_id: "uc".to_string(),
            asset: "USD".to_string(),
        });
        let resp = svc.get_balance(req).await.unwrap().into_inner();
        assert_eq!(resp.account_id, "uc");
        assert_eq!(resp.asset, "USD");
        assert_eq!(resp.balance, "100");

        // No asset -> "all".
        let req = Request::new(GetBalanceRequest {
            account_id: "uc".to_string(),
            asset: String::new(),
        });
        let resp = svc.get_balance(req).await.unwrap().into_inner();
        assert_eq!(resp.asset, "all");

        // Unknown account -> not found.
        let req = Request::new(GetBalanceRequest {
            account_id: "nope".to_string(),
            asset: String::new(),
        });
        assert!(svc.get_balance(req).await.is_err());
    }

    #[tokio::test]
    async fn verify_chain_grpc_ok_and_broken() {
        let store = store_with_account("uc", "user_custodial", "BOTH");
        store
            .create_account(
                serde_json::from_value(serde_json::json!({
                    "account_id": "op",
                    "type": "operational_fiat",
                    "asset_class": "FIAT",
                    "label": "op",
                }))
                .unwrap(),
            )
            .unwrap();
        store.post(balanced_posting("v1")).unwrap();
        let svc = LedgerGrpc::new(store.clone(), vec!["caller".to_string()]);

        let req = Request::new(VerifyChainRequest {});
        let resp = svc.verify_chain(req).await.unwrap().into_inner();
        assert!(resp.ok);

        // Tamper with a hash to break the chain.
        {
            let mut state = store.inner.lock();
            if let Some(e) = state.entries.first_mut() {
                e.this_hash = "deadbeef".to_string();
            }
        }
        let req = Request::new(VerifyChainRequest {});
        let resp = svc.verify_chain(req).await.unwrap().into_inner();
        assert!(!resp.ok);
        assert!(!resp.entry_id.is_empty());
        assert!(!resp.reason.is_empty());
    }

    #[test]
    fn server_constructs() {
        let _ = server(Store::new(), vec!["caller".to_string()]);
    }

    #[test]
    fn posting_to_pb_maps_fields() {
        let rec = crate::posting::PostingRecord {
            posting_id: "p1".to_string(),
            ref_tx_id: Some("tx".to_string()),
            memo: Some("m".to_string()),
            status: "POSTED".to_string(),
            hash_head: "hh".to_string(),
            entries: vec![crate::posting::EntryRecord {
                entry_id: "e1".to_string(),
                posting_id: "p1".to_string(),
                account_id: "uc".to_string(),
                direction: "DEBIT".to_string(),
                amount: 100,
                asset: "USD".to_string(),
                sequence_number: 1,
                prev_hash: "prev".to_string(),
                this_hash: "this".to_string(),
                created_at: "1000".to_string(),
            }],
            created_at: "1000".to_string(),
        };
        let pb = posting_to_pb(&rec);
        assert_eq!(pb.posting_id, "p1");
        assert_eq!(pb.ref_tx_id, "tx");
        assert_eq!(pb.memo, "m");
        assert_eq!(pb.status, "POSTED");
        assert_eq!(pb.hash_head, "hh");
        assert_eq!(pb.entries.len(), 1);
        assert_eq!(pb.entries[0].entry_id, "e1");
        assert_eq!(pb.entries[0].amount, 100);
        assert_eq!(pb.created_at, "1000");
    }

    // Silence unused import warning when MetadataMap isn't otherwise used.
    #[test]
    fn _metadata_map_drops() {
        let _ = MetadataMap::new();
    }
}
