use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum NormalBalance {
    Debit,
    Credit,
    Either,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Direction {
    Debit,
    Credit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AssetClass {
    Fiat,
    Crypto,
    Both,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountType {
    #[serde(rename = "type")]
    pub type_name: &'static str,
    pub normal_balance: NormalBalance,
    pub allowed_directions: &'static [&'static str],
    pub asset_class: AssetClass,
}

pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub const CHART: &[AccountType] = &[
    AccountType {
        type_name: "user_custodial",
        normal_balance: NormalBalance::Credit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
    AccountType {
        type_name: "user_payable",
        normal_balance: NormalBalance::Credit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
    AccountType {
        type_name: "operational_fiat",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Fiat,
    },
    AccountType {
        type_name: "operational_crypto",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Crypto,
    },
    AccountType {
        type_name: "treasury_fiat",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Fiat,
    },
    AccountType {
        type_name: "treasury_crypto",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Crypto,
    },
    AccountType {
        type_name: "fx_gain_loss",
        normal_balance: NormalBalance::Either,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
    AccountType {
        type_name: "fee_revenue",
        normal_balance: NormalBalance::Credit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
    AccountType {
        type_name: "rail_settlement",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Fiat,
    },
    AccountType {
        type_name: "venue_settlement",
        normal_balance: NormalBalance::Debit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
    AccountType {
        type_name: "chargeback_reserve",
        normal_balance: NormalBalance::Credit,
        allowed_directions: &["DEBIT", "CREDIT"],
        asset_class: AssetClass::Both,
    },
];

pub fn find_type(type_name: &str) -> Option<&'static AccountType> {
    CHART.iter().find(|t| t.type_name == type_name)
}

pub fn direction_allowed(account_type: &AccountType, direction: Direction) -> bool {
    let dir_str = match direction {
        Direction::Debit => "DEBIT",
        Direction::Credit => "CREDIT",
    };
    account_type.allowed_directions.contains(&dir_str)
}

pub fn asset_class_allowed(account_type: &AccountType, asset_class: AssetClass) -> bool {
    match account_type.asset_class {
        AssetClass::Both => true,
        other => other == asset_class,
    }
}
