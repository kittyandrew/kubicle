//! The URLs a mail document links to: the routes, and `Links`, which kubicle-server's `sign.rs` implements with
//! signed URLs, so the frame loads them without the session cookie.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::structure::PartPath;

/// Routes under the base path. The CSP in `render.rs` names `INLINE` and `IMAGE` exactly, so they stay fixed.
pub const INLINE: &str = "inline";
pub const DOWNLOAD: &str = "download";
pub const VIEW: &str = "view";
pub const IMAGE: &str = "image";

/// Which route a part URL is for: the same part signed for one route does not verify on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartRoute {
    /// Shown in place: inline images in the mail frame, attachment previews.
    Inline,
    /// Saved with `Content-Disposition: attachment`.
    Download,
    /// Opened as a sandboxed page of its own: text, HTML and forwarded messages.
    View,
}

impl PartRoute {
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Inline => INLINE,
            Self::Download => DOWNLOAD,
            Self::View => VIEW,
        }
    }
}

/// Signed URLs for the parts and remote images of one message.
pub trait Links: Clone + Send + Sync + 'static {
    /// The app's public origin plus base path (`https://host/login/`).
    fn prefix(&self) -> &str;

    fn part(&self, route: PartRoute, part: &PartPath) -> String;

    /// An inline image's URL, with its IMAP section when it has one, so its request fetches that part alone.
    fn inline(&self, part: &PartPath, section: Option<&Section>) -> String;

    /// A proxy URL for one remote image.
    fn image(&self, remote: &str) -> String;
}

/// A part's IMAP section number (`2.1`, RFC 3501) and its decoded size. Served from the section only when the size
/// matches: a server can number a malformed message's parts otherwise than mail-parser does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub number: String,
    pub size: usize,
}

/// Everything but letters and digits percent-encoded: safe in a query and in an HTML attribute.
#[must_use]
pub fn enc(value: &str) -> String { utf8_percent_encode(value, NON_ALPHANUMERIC).to_string() }
