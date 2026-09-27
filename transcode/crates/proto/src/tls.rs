//! Mutual TLS for the pool (certificates from cert-manager: pool CA -> agent/client leaves).
//!
//! Env (all three or none): `TC_TLS_CERT`, `TC_TLS_KEY` (this side's identity, PEM) and
//! `TC_TLS_CA` (the pool CA). With `TC_TLS_REQUIRED=1`, missing TLS config is a hard error, so a
//! production pod can never silently fall back to plaintext. Agents require a client
//! certificate signed by the pool CA; clients verify the agent against `TC_TLS_SERVER_NAME`
//! (default `tcpool-agent`, a SAN on every agent certificate).

use std::path::PathBuf;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
}

pub const DEFAULT_SERVER_NAME: &str = "tcpool-agent";

/// PEM bytes of (certificate, key, CA).
type Pems = (Vec<u8>, Vec<u8>, Vec<u8>);

impl TlsFiles {
    /// `Ok(None)` = plaintext (allowed only when TC_TLS_REQUIRED is not set).
    pub fn from_env() -> Result<Option<TlsFiles>, String> {
        let get = |n: &str| std::env::var(n).ok().filter(|v| !v.is_empty());
        let required = get("TC_TLS_REQUIRED").is_some_and(|v| v != "0");
        match (get("TC_TLS_CERT"), get("TC_TLS_KEY"), get("TC_TLS_CA")) {
            (Some(c), Some(k), Some(a)) => Ok(Some(TlsFiles {
                cert: c.into(),
                key: k.into(),
                ca: a.into(),
            })),
            (None, None, None) if !required => Ok(None),
            (None, None, None) => {
                Err("TC_TLS_REQUIRED is set but TC_TLS_CERT/KEY/CA are not".into())
            }
            _ => Err("set all of TC_TLS_CERT, TC_TLS_KEY and TC_TLS_CA, or none".into()),
        }
    }

    fn read(&self) -> Result<Pems, String> {
        let r = |p: &PathBuf| std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
        Ok((r(&self.cert)?, r(&self.key)?, r(&self.ca)?))
    }

    /// Server side: our identity, and require clients signed by the pool CA.
    pub fn server_config(&self) -> Result<ServerTlsConfig, String> {
        let (cert, key, ca) = self.read()?;
        Ok(ServerTlsConfig::new()
            .identity(Identity::from_pem(cert, key))
            .client_ca_root(Certificate::from_pem(ca)))
    }

    /// Client side: present our identity, verify the agent against the pool CA.
    pub fn client_config(&self) -> Result<ClientTlsConfig, String> {
        let (cert, key, ca) = self.read()?;
        let name = std::env::var("TC_TLS_SERVER_NAME")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_NAME.into());
        Ok(ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(ca))
            .identity(Identity::from_pem(cert, key))
            .domain_name(name))
    }

    /// Latest modification time of the three files (cert-manager rotates them in place).
    pub fn modified(&self) -> Option<std::time::SystemTime> {
        [&self.cert, &self.key, &self.ca]
            .iter()
            .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .max()
    }
}

/// Endpoint URL for an agent address, `https://` when TLS is configured.
pub fn endpoint_url(addr: &str, tls: bool) -> String {
    if tls {
        format!("https://{addr}")
    } else {
        format!("http://{addr}")
    }
}
