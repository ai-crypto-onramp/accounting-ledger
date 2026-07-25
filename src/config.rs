use std::env;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub db_url: String,
    pub db_isolation: String,
    pub db_max_connections: u32,
    pub snapshot_interval_minutes: u64,
    pub hash_chain_alg: String,
    pub hash_chain_salt: Option<String>,
    pub audit_event_log_url: Option<String>,
    pub kafka_brokers: Option<String>,
    pub dev_mode: bool,
    pub max_entries_per_posting: usize,
    pub max_amount: u64,
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8080,
            db_url: "postgres://ledger:ledger@localhost:5432/ledger".to_string(),
            db_isolation: "serializable".to_string(),
            db_max_connections: 32,
            snapshot_interval_minutes: 15,
            hash_chain_alg: "sha256".to_string(),
            hash_chain_salt: None,
            audit_event_log_url: None,
            kafka_brokers: None,
            dev_mode: false,
            max_entries_per_posting: 64,
            max_amount: 1_000_000_000_000,
            log_level: "info".to_string(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = env::var("PORT") {
            if let Ok(p) = v.parse() {
                c.port = p;
            }
        }
        if let Ok(v) = env::var("DB_URL") {
            c.db_url = v;
        }
        if let Ok(v) = env::var("DB_ISOLATION") {
            c.db_isolation = v;
        }
        if let Ok(v) = env::var("DB_MAX_CONNECTIONS") {
            if let Ok(p) = v.parse() {
                c.db_max_connections = p;
            }
        }
        if let Ok(v) = env::var("SNAPSHOT_INTERVAL_MINUTES") {
            if let Ok(p) = v.parse() {
                c.snapshot_interval_minutes = p;
            }
        }
        if let Ok(v) = env::var("HASH_CHAIN_ALG") {
            c.hash_chain_alg = v;
        }
        if let Ok(v) = env::var("HASH_CHAIN_SALT") {
            c.hash_chain_salt = Some(v);
        }
        if let Ok(v) = env::var("AUDIT_EVENT_LOG_URL") {
            c.audit_event_log_url = Some(v);
        }
        if let Ok(v) = env::var("KAFKA_BROKERS") {
            c.kafka_brokers = Some(v);
        }
        if let Ok(v) = env::var("DEV_MODE") {
            c.dev_mode = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Ok(v) = env::var("MAX_ENTRIES_PER_POSTING") {
            if let Ok(p) = v.parse() {
                c.max_entries_per_posting = p;
            }
        }
        if let Ok(v) = env::var("MAX_AMOUNT") {
            if let Ok(p) = v.parse() {
                c.max_amount = p;
            }
        }
        if let Ok(v) = env::var("LOG_LEVEL") {
            c.log_level = v;
        }
        c
    }

    pub fn snapshot_interval(&self) -> Duration {
        Duration::from_secs(self.snapshot_interval_minutes * 60)
    }

    pub fn assert_isolation(&self) -> Result<(), String> {
        if self.db_isolation.eq_ignore_ascii_case("serializable") {
            Ok(())
        } else {
            Err(format!(
                "refusing to boot: DB_ISOLATION must be 'serializable', got '{}'",
                self.db_isolation
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn default_has_expected_values() {
        let c = Config::default();
        assert_eq!(c.port, 8080);
        assert_eq!(c.db_isolation, "serializable");
        assert_eq!(c.db_max_connections, 32);
        assert_eq!(c.snapshot_interval_minutes, 15);
        assert_eq!(c.hash_chain_alg, "sha256");
        assert!(c.hash_chain_salt.is_none());
        assert!(c.audit_event_log_url.is_none());
        assert_eq!(c.max_entries_per_posting, 64);
        assert_eq!(c.max_amount, 1_000_000_000_000);
        assert_eq!(c.log_level, "info");
        assert!(!c.db_url.is_empty());
    }

    #[test]
    fn snapshot_interval_is_minutes_to_seconds() {
        let c = Config {
            snapshot_interval_minutes: 2,
            ..Default::default()
        };
        assert_eq!(c.snapshot_interval(), Duration::from_secs(120));
    }

    #[test]
    fn assert_isolation_accepts_serializable_variants() {
        let mut c = Config::default();
        assert!(c.assert_isolation().is_ok());
        c.db_isolation = "SERIALIZABLE".to_string();
        assert!(c.assert_isolation().is_ok());
        c.db_isolation = "Serializable".to_string();
        assert!(c.assert_isolation().is_ok());
    }

    #[test]
    fn assert_isolation_rejects_other() {
        let c = Config {
            db_isolation: "read_committed".to_string(),
            ..Default::default()
        };
        let err = c.assert_isolation().unwrap_err();
        assert!(err.contains("read_committed"));
        assert!(err.contains("serializable"));
    }

    #[test]
    fn from_env_reads_overrides() {
        let _g = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        // Save existing values to restore afterwards.
        let keys = [
            "PORT",
            "DB_URL",
            "DB_ISOLATION",
            "DB_MAX_CONNECTIONS",
            "SNAPSHOT_INTERVAL_MINUTES",
            "HASH_CHAIN_ALG",
            "HASH_CHAIN_SALT",
            "AUDIT_EVENT_LOG_URL",
            "MAX_ENTRIES_PER_POSTING",
            "MAX_AMOUNT",
            "LOG_LEVEL",
        ];
        let saved: Vec<(String, Option<String>)> = keys
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();

        std::env::set_var("PORT", "9999");
        std::env::set_var("DB_URL", "postgres://x/x");
        std::env::set_var("DB_ISOLATION", "serializable");
        std::env::set_var("DB_MAX_CONNECTIONS", "7");
        std::env::set_var("SNAPSHOT_INTERVAL_MINUTES", "3");
        std::env::set_var("HASH_CHAIN_ALG", "sha512");
        std::env::set_var("HASH_CHAIN_SALT", "salty");
        std::env::set_var("AUDIT_EVENT_LOG_URL", "http://log");
        std::env::set_var("MAX_ENTRIES_PER_POSTING", "9");
        std::env::set_var("MAX_AMOUNT", "12345");
        std::env::set_var("LOG_LEVEL", "debug");
        let c = Config::from_env();
        assert_eq!(c.port, 9999);
        assert_eq!(c.db_url, "postgres://x/x");
        assert_eq!(c.db_isolation, "serializable");
        assert_eq!(c.db_max_connections, 7);
        assert_eq!(c.snapshot_interval_minutes, 3);
        assert_eq!(c.hash_chain_alg, "sha512");
        assert_eq!(c.hash_chain_salt.as_deref(), Some("salty"));
        assert_eq!(c.audit_event_log_url.as_deref(), Some("http://log"));
        assert_eq!(c.max_entries_per_posting, 9);
        assert_eq!(c.max_amount, 12345);
        assert_eq!(c.log_level, "debug");

        // Restore so other tests are not affected.
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(&k, &val),
                None => std::env::remove_var(&k),
            }
        }
    }

    #[test]
    fn from_env_ignores_invalid_numeric_overrides() {
        let _g = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let saved = std::env::var("PORT").ok();
        std::env::set_var("PORT", "not-a-port");
        let c = Config::from_env();
        // Falls back to default port (8080).
        assert_eq!(c.port, 8080);
        match saved {
            Some(v) => std::env::set_var("PORT", &v),
            None => std::env::remove_var("PORT"),
        }
    }
}
