use anyhow::{anyhow, Result};
use std::env;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

pub struct TlsMaterial {
    pub server_config: ServerTlsConfig,
}

pub fn is_dev_mode() -> bool {
    env::var("DEV_MODE").as_deref() == Ok("1")
}

pub fn load_server_tls() -> Result<Option<TlsMaterial>> {
    let cert = env::var("TLS_CERT_FILE").unwrap_or_default();
    let key = env::var("TLS_KEY_FILE").unwrap_or_default();
    let ca = env::var("TLS_CA_FILE").unwrap_or_default();
    if cert.is_empty() && key.is_empty() && ca.is_empty() {
        if is_dev_mode() {
            return Ok(None);
        }
        return Err(anyhow!(
            "TLS_CERT_FILE/TLS_KEY_FILE/TLS_CA_FILE required when DEV_MODE!=1"
        ));
    }
    if cert.is_empty() || key.is_empty() || ca.is_empty() {
        return Err(anyhow!(
            "TLS_CERT_FILE, TLS_KEY_FILE and TLS_CA_FILE must all be set together"
        ));
    }
    let cert_pem =
        std::fs::read_to_string(&cert).map_err(|e| anyhow!("read cert file {}: {}", cert, e))?;
    let key_pem =
        std::fs::read_to_string(&key).map_err(|e| anyhow!("read key file {}: {}", key, e))?;
    let ca_pem = std::fs::read_to_string(&ca).map_err(|e| anyhow!("read ca file {}: {}", ca, e))?;
    let identity = Identity::from_pem(cert_pem, key_pem);
    let client_ca = Certificate::from_pem(ca_pem);
    let server_config = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(client_ca);
    Ok(Some(TlsMaterial { server_config }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TLS_MUTEX: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TLS_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_tls_env() {
        env::remove_var("TLS_CERT_FILE");
        env::remove_var("TLS_KEY_FILE");
        env::remove_var("TLS_CA_FILE");
        env::set_var("DEV_MODE", "1");
    }

    fn write_self_signed_cert(dir: &std::path::Path) -> (String, String, String) {
        let certified_key =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = certified_key.cert.pem();
        let key_pem = certified_key.key_pair.serialize_pem();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let ca_path = dir.join("ca.pem");
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();
        std::fs::write(&ca_path, &cert_pem).unwrap();
        (
            cert_path.to_string_lossy().to_string(),
            key_path.to_string_lossy().to_string(),
            ca_path.to_string_lossy().to_string(),
        )
    }

    #[test]
    fn dev_mode_returns_none() {
        let _g = lock();
        clear_tls_env();
        env::set_var("DEV_MODE", "1");
        let cfg = load_server_tls().unwrap();
        assert!(cfg.is_none());
        clear_tls_env();
    }

    #[test]
    fn prod_missing_env_is_error() {
        let _g = lock();
        clear_tls_env();
        env::set_var("DEV_MODE", "0");
        assert!(load_server_tls().is_err());
        clear_tls_env();
    }

    #[test]
    fn partial_set_is_error() {
        let _g = lock();
        clear_tls_env();
        env::set_var("TLS_CERT_FILE", "/x/cert.pem");
        env::set_var("DEV_MODE", "1");
        assert!(load_server_tls().is_err());
        clear_tls_env();
    }

    #[test]
    fn bad_cert_files_is_error() {
        let _g = lock();
        clear_tls_env();
        env::set_var("TLS_CERT_FILE", "/no/cert.pem");
        env::set_var("TLS_KEY_FILE", "/no/key.pem");
        env::set_var("TLS_CA_FILE", "/no/ca.pem");
        env::set_var("DEV_MODE", "0");
        assert!(load_server_tls().is_err());
        clear_tls_env();
    }

    #[test]
    fn valid_certs_returns_config() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key, ca) = write_self_signed_cert(dir.path());
        env::set_var("TLS_CERT_FILE", &cert);
        env::set_var("TLS_KEY_FILE", &key);
        env::set_var("TLS_CA_FILE", &ca);
        env::set_var("DEV_MODE", "0");
        let cfg = load_server_tls().unwrap().unwrap();
        assert!(format!("{:?}", cfg.server_config).contains("ServerTlsConfig"));
        clear_tls_env();
    }
}
