//! A signed-in user's mailbox for kubicle's server, without HTTP: the engine that answers from IMAP and acts on it
//! (`sync.rs`, `threads.rs`), Compose's drafts and their sending over submission (`compose.rs`), and what is kept for
//! the user on disk (`uploads.rs`, `settings.rs`). kubicle-server is its HTTP side.

pub mod compose;
mod db;
pub mod settings;
pub mod sync;
mod threads;
pub mod uploads;

use std::{sync::Arc, sync::atomic::AtomicU64, sync::atomic::Ordering};

use axum::{Json, http::StatusCode, response::IntoResponse, response::Response};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use kubicle_api::ApiError;
use rustls::{ClientConfig, pki_types::ServerName};
use tracing::error;

/// Every failure in kubicle, turned into its answer once, in `into_response`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Imap(#[from] kubicle_imap::Error),
    #[error(transparent)]
    Mail(#[from] kubicle_mail::Error),
    /// What a request did wrong, or what refused it, answered as is.
    #[error("{1}")]
    Status(StatusCode, String),
}

impl Error {
    pub fn status(status: StatusCode, message: impl Into<String>) -> Self { Self::Status(status, message.into()) }

    /// Whether the IMAP connection is unusable after this error, as opposed to one command having failed.
    fn breaks_connection(&self) -> bool { matches!(self, Self::Imap(e) if e.breaks_connection()) }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Status(status, _) => *status,
            Self::Imap(kubicle_imap::Error::Expired) => StatusCode::UNAUTHORIZED, // sends the client through `sso.php`
            Self::Imap(kubicle_imap::Error::Unavailable) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Imap(kubicle_imap::Error::Rejected { .. } | kubicle_imap::Error::Invalid(_)) => StatusCode::BAD_REQUEST,
            Self::Mail(_) => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            error!(error = %self, "request failed"); // else only the browser that got the 500 hears why
        }
        let message = match self {
            Self::Status(_, message) => message,
            Self::Io(_) | Self::Db(_) => "kubicle failed; its log says why".to_owned(), // a path or SQL detail stays there
            other => other.to_string(),
        };
        (status, Json(ApiError { message })).into_response()
    }
}

/// Mailu front's internal mail ports. Both serve the same certificate, so IMAP and submission share one TLS setup.
pub struct MailServer {
    /// Front's host on the Docker network.
    pub host: String,
    pub imap_port: u16,
    pub submission_port: u16,
    /// The name front's certificate is checked against, e.g. the mail hostname.
    pub tls_name: ServerName<'static>,
    /// The public roots plus `KUBICLE_MAIL_EXTRA_CA`, the test fixture's own CA.
    pub tls: Arc<ClientConfig>,
    /// Mail domains whose addresses deliver to the signed-in user (aliases, catch-alls), so a reply can be sent
    /// from the address the mail came to. The user's own domain always counts.
    pub domains: Vec<String>,
    /// The authserv-id the mail server writes into Authentication-Results, Mailu's first `HOSTNAMES` entry. Unset,
    /// the reader shows no sender checks.
    pub authserv_id: Option<String>,
    /// The largest message submission takes, in bytes, as its EHLO last said (`compose::learn_size_limit`, and
    /// every send); 0 while it named none or was not reached yet.
    pub size_limit: AtomicU64,
}

impl MailServer {
    pub fn size_limit(&self) -> Option<u64> { Some(self.size_limit.load(Ordering::Relaxed)).filter(|&n| n > 0) }
}

/// 256 random bits, URL-safe.
#[must_use]
pub fn random_id() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    URL_SAFE_NO_PAD.encode(bytes)
}
