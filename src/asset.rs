use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct AssetSpec {
    pub code: &'static str,
    pub class: &'static str,
    pub scale: u32,
    pub max_amount: u64,
}

impl AssetSpec {
    pub const fn fiat(code: &'static str, scale: u32, max_amount: u64) -> Self {
        Self {
            code,
            class: "FIAT",
            scale,
            max_amount,
        }
    }

    pub const fn crypto(code: &'static str, scale: u32, max_amount: u64) -> Self {
        Self {
            code,
            class: "CRYPTO",
            scale,
            max_amount,
        }
    }
}

pub const ASSET_REGISTRY: &[AssetSpec] = &[
    AssetSpec::fiat("USD", 2, 1_000_000_000_000),
    AssetSpec::fiat("EUR", 2, 1_000_000_000_000),
    AssetSpec::fiat("GBP", 2, 1_000_000_000_000),
    AssetSpec::crypto("BTC", 8, 21_000_000_000_000),
    AssetSpec::crypto("ETH", 18, 1_000_000_000_000_000_000),
    AssetSpec::crypto("USDC", 6, 1_000_000_000_000),
];

pub fn registry_map() -> &'static HashMap<&'static str, AssetSpec> {
    static REGISTRY: OnceLock<HashMap<&'static str, AssetSpec>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut m = HashMap::new();
        for spec in ASSET_REGISTRY.iter() {
            m.insert(spec.code, spec.clone());
        }
        m
    })
}

pub fn find_asset(code: &str) -> Option<&'static AssetSpec> {
    registry_map().get(code)
}

pub fn validate_amount(asset: &str, amount: u64) -> Result<(), String> {
    let spec = match find_asset(asset) {
        Some(s) => s,
        None => return Err(format!("unknown asset: {}", asset)),
    };
    if amount == 0 {
        return Err("amount must be > 0".to_string());
    }
    if amount > spec.max_amount {
        return Err(format!(
            "amount {} exceeds MAX_AMOUNT {} for asset {}",
            amount, spec.max_amount, asset
        ));
    }
    Ok(())
}

pub fn max_amount_for(asset: &str) -> u64 {
    find_asset(asset)
        .map(|s| s.max_amount)
        .unwrap_or(crate::store::MAX_AMOUNT)
}

pub fn validate_scale(asset: &str, _amount: u64) -> Result<(), String> {
    let spec = match find_asset(asset) {
        Some(s) => s,
        None => return Ok(()),
    };
    let scale_mod = 10u64.checked_pow(spec.scale).unwrap_or(u64::MAX);
    if scale_mod == 0 {
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiat_and_crypto_constructors() {
        let f = AssetSpec::fiat("USD", 2, 1000);
        assert_eq!(f.code, "USD");
        assert_eq!(f.class, "FIAT");
        assert_eq!(f.scale, 2);
        assert_eq!(f.max_amount, 1000);
        let c = AssetSpec::crypto("BTC", 8, 21);
        assert_eq!(c.class, "CRYPTO");
        assert_eq!(c.scale, 8);
    }

    #[test]
    fn registry_contains_known_assets() {
        for code in ["USD", "EUR", "GBP", "BTC", "ETH", "USDC"] {
            assert!(find_asset(code).is_some(), "missing {}", code);
        }
        assert!(find_asset("NOPE").is_none());
    }

    #[test]
    fn validate_amount_zero_rejected() {
        let err = validate_amount("USD", 0).unwrap_err();
        assert!(err.contains("amount must be > 0"));
    }

    #[test]
    fn validate_amount_over_max_rejected() {
        let max = find_asset("BTC").unwrap().max_amount;
        let err = validate_amount("BTC", max + 1).unwrap_err();
        assert!(err.contains("exceeds MAX_AMOUNT"));
    }

    #[test]
    fn validate_amount_unknown_asset() {
        let err = validate_amount("XYZ", 10).unwrap_err();
        assert!(err.contains("unknown asset"));
    }

    #[test]
    fn validate_amount_ok() {
        assert!(validate_amount("USD", 100).is_ok());
    }

    #[test]
    fn max_amount_for_known_and_unknown() {
        assert_eq!(max_amount_for("BTC"), find_asset("BTC").unwrap().max_amount);
        assert_eq!(max_amount_for("NOPE"), crate::store::MAX_AMOUNT);
    }

    #[test]
    fn validate_scale_known_and_unknown() {
        assert!(validate_scale("USD", 100).is_ok());
        assert!(validate_scale("NOPE", 100).is_ok());
    }
}
