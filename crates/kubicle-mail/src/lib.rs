//! Mail parsing and display: raw RFC 822 bytes to the `Message` the reader shows, the sandboxed documents its
//! frame loads, and the axum responses those are served in (the router adds the shared security headers). No
//! network or disk I/O: the server fetches the bytes and signs the URLs (`links::Links`).

pub mod flowed;
pub mod links;
mod plain;
pub mod render;
pub mod structure;

/// Bytes mail-parser cannot read as a message; the server answers them 422.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the message has neither headers nor a body")]
    Empty,
    #[error("the message has no headers")]
    NoHeaders,
    #[error("the message holds more than 1,000 messages")]
    TooManyMessages,
    #[error("the message has more than 50,000 parts")]
    TooManyParts,
}
