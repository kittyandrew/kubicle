//! Everything kubicle reads from its environment, read once at start and parsed into the values the rest uses. The
//! NixOS module sets these; the names and defaults are the deployment contract with it.

use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, sync::atomic::AtomicU64};

use jiff::tz::TimeZone;
use kubicle_mailbox::MailServer;
use rustls::{ClientConfig, RootCertStore, pki_types::CertificateDer, pki_types::ServerName, pki_types::pem::PemObject};

pub struct Config {
    pub listen: SocketAddr,
    /// The built client: index.html, the wasm, styles and built-in themes.
    pub web_dir: PathBuf,
    /// The settings database and the users' unsent uploads.
    pub data_dir: PathBuf,
    /// Extra themes the deployment provides, listed as System themes.
    pub themes_dir: Option<PathBuf>,
    pub mail: Arc<MailServer>,
    /// The zone that dates sent mail when Settings names none, and a quote when the browser names none.
    pub time_zone: TimeZone,
    /// The browser-facing origin, e.g. `https://mail.example.com`: CSP sources and the Origin check need it absolute.
    pub public_origin: String,
    /// Where front serves kubicle (Mailu's `WEB_WEBMAIL` plus a slash), e.g. `/login/`.
    pub base_path: String,
    /// Mailu's sign-out page, where kubicle's logout sends the browser.
    pub logout_url: String,
    /// Mailu's own pages for the user, Mailu's `WEB_ADMIN`, which About links to.
    pub admin_url: String,
    /// Where kubicle's source is published, which About links to; unset, About names none.
    pub source_url: Option<String>,
    /// Peers allowed to talk to kubicle: the network kubicle shares only with front, `127.0.0.1/32` for a local run.
    /// Required: `sso.php` proves an identity by an IMAP login on a port Mailu does not rate-limit, so any other peer
    /// could try passwords there (docs/design.md, Rate limits). Against a forged identity it is the second guard.
    pub trusted_proxy: ipnet::IpNet,
}

/// A TLS client trusting `roots`, on ring.
pub fn client_tls(roots: RootCertStore) -> Arc<ClientConfig> {
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

fn var(name: &str) -> Option<String> { env::var(name).ok().filter(|v| !v.is_empty()) }

fn required(name: &str) -> Result<String, String> { var(name).ok_or(format!("{name} is not set")) }

fn parsed<T: std::str::FromStr>(name: &str) -> Result<Option<T>, String> {
    var(name).map(|value| value.parse().map_err(|_| format!("{name} is invalid: {value:?}"))).transpose()
}

/// The address the server listens on, which `--health` asks too.
pub fn listen() -> Result<SocketAddr, String> { Ok(parsed("KUBICLE_LISTEN")?.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 80)))) }

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let mut base_path = var("KUBICLE_BASE_PATH").unwrap_or_else(|| "/login/".into());
        if !base_path.starts_with('/') {
            return Err(format!("KUBICLE_BASE_PATH is invalid: {base_path:?}"));
        }
        if !base_path.ends_with('/') {
            base_path.push('/');
        }
        let public_origin = required("KUBICLE_PUBLIC_ORIGIN")?;
        if !public_origin.starts_with("https://") && !public_origin.starts_with("http://") || public_origin.ends_with('/') {
            return Err(format!("KUBICLE_PUBLIC_ORIGIN is invalid: {public_origin:?}"));
        }
        let host = var("KUBICLE_MAIL_HOST").unwrap_or_else(|| "front".into()); // front's alias on Mailu's network
        let tls_name = var("KUBICLE_MAIL_TLS_NAME").unwrap_or_else(|| host.clone());
        let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = var("KUBICLE_MAIL_EXTRA_CA") {
            let certs = CertificateDer::pem_file_iter(&path).and_then(Iterator::collect::<Result<Vec<_>, _>>);
            let certs = certs.map_err(|e| format!("KUBICLE_MAIL_EXTRA_CA {path}: {e}"))?;
            if certs.is_empty() {
                return Err(format!("KUBICLE_MAIL_EXTRA_CA {path}: no certificate"));
            }
            for cert in certs {
                roots.add(cert).map_err(|e| format!("KUBICLE_MAIL_EXTRA_CA {path}: {e}"))?;
            }
        }
        let time_zone = var("KUBICLE_TIME_ZONE").unwrap_or_else(|| "UTC".into());
        Ok(Self {
            listen: listen()?,
            web_dir: required("KUBICLE_WEB_DIR")?.into(),
            data_dir: var("KUBICLE_DATA_DIR").unwrap_or_else(|| "/var/lib/kubicle".into()).into(),
            themes_dir: var("KUBICLE_THEMES_DIR").map(Into::into),
            mail: Arc::new(MailServer {
                tls_name: ServerName::try_from(tls_name).map_err(|e| format!("KUBICLE_MAIL_TLS_NAME is invalid: {e}"))?,
                host,
                imap_port: parsed("KUBICLE_IMAP_PORT")?.unwrap_or(10143), // front's internal IMAP for webmail tokens
                submission_port: parsed("KUBICLE_SUBMISSION_PORT")?.unwrap_or(10025), // and its internal submission
                tls: client_tls(roots),
                domains: var("KUBICLE_MAIL_DOMAINS")
                    .map(|v| v.split(',').map(str::trim).filter(|d| !d.is_empty()).map(str::to_ascii_lowercase).collect())
                    .unwrap_or_default(),
                authserv_id: var("KUBICLE_AUTHSERV_ID"),
                size_limit: AtomicU64::new(0),
            }),
            time_zone: TimeZone::get(&time_zone).map_err(|e| format!("KUBICLE_TIME_ZONE {time_zone}: {e}"))?,
            public_origin,
            base_path,
            logout_url: var("KUBICLE_LOGOUT_URL").unwrap_or_else(|| "/sso/logout".into()),
            admin_url: var("KUBICLE_ADMIN_URL").unwrap_or_else(|| "/admin".into()), // Mailu's default WEB_ADMIN
            source_url: var("KUBICLE_SOURCE_URL"),
            trusted_proxy: parsed("KUBICLE_TRUSTED_PROXY")?.ok_or("KUBICLE_TRUSTED_PROXY is not set")?,
        })
    }
}
