//! The IMAP client: one connection to Mailu front, STARTTLS with a verified certificate, the session token as
//! the password, and typed commands built from imap-types values (docs/design.md, IMAP).

pub mod client;
pub mod header;
mod stream;
mod utf7;

/// What can go wrong talking to the IMAP server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the IMAP server closed the connection")]
    Closed,
    #[error("the IMAP server sent a malformed {0}")]
    Malformed(&'static str),
    /// An untagged response imap-codec cannot parse; the connection stays usable.
    #[error("the IMAP server sent an untagged response that does not parse")]
    Unparsed,
    #[error("an IMAP response above the size limit")]
    TooLarge,
    #[error("the IMAP server answered {command} with {text:?}")]
    Rejected { command: &'static str, text: String },
    #[error("the IMAP server lacks {0}")]
    Missing(&'static str),
    /// The mail server refused the token: the Mailu session behind it ended, and the user passes `sso.php` again.
    #[error("the Mailu session ended")]
    Expired,
    /// The IMAP connection is down and being re-established, or the server said UNAVAILABLE (RFC 5530).
    #[error("the mail server is not reachable right now")]
    Unavailable,
    /// What the user asked for that IMAP cannot carry, such as a NUL byte; the server answers it 400.
    #[error("{0}")]
    Invalid(&'static str),
}

impl Error {
    /// Whether the IMAP connection is unusable after this error, as opposed to one command having failed.
    #[must_use]
    pub fn breaks_connection(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Closed | Self::Expired | Self::Malformed(_) | Self::TooLarge | Self::Unavailable)
    }
}

/// What STATUS reported about a mailbox: a change to any field is a change to the mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxStatus {
    pub messages: u32,
    pub unseen: u32,
    pub uid_next: u32,
    pub uid_validity: u32,
    /// `None` when the mailbox has no mod-sequences (RFC 7162, NOMODSEQ).
    pub highest_modseq: Option<u64>,
}

/// One message of a THREAD=REFS answer, in the server's order: its UID, `None` for one the server knows of but the
/// mailbox lacks, and the index of its parent, which comes before it. Flat, so no thread's depth costs a stack frame:
/// as a tree, a reply chain nested a level per message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thread {
    pub uid: Option<u32>,
    pub parent: Option<usize>,
}
