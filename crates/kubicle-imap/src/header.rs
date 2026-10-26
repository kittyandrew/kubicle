//! FETCH data to owned values: ENVELOPE fields decoded with mail-parser's header parsers (encoded words in any
//! charset, raw 8-bit text, dates), and the attachment mark from the top Content-Type.

use imap_codec::{imap_types::core::NString, imap_types::envelope::Address as ImapAddress, imap_types::envelope::Envelope};
use imap_codec::{imap_types::fetch::MessageDataItem, imap_types::flag::Flag, imap_types::flag::FlagFetch};
use kubicle_api::{Address, Attached, Flag as Ours, Flags};
use mail_parser::{ContentType, DateTime, HeaderValue, MessageParser, MimeHeaders, parsers::MessageStream};

#[derive(Debug, Clone)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Flags,
    /// Seconds since the epoch.
    pub arrival: i64,
    /// The Date header in seconds since the epoch, or `arrival` when it is missing or unparseable.
    pub date: i64,
    pub size: u32,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// What the envelope has: the Bcc of the user's own mail in Sent, which autocomplete offers, else nothing.
    pub bcc: Vec<Address>,
    pub subject: String,
    /// The List-Post value as sent, unread: kubicle-mail's `render::list_post` reads its address.
    pub list_post: Option<String>,
    pub attached: Attached,
}

/// The items of one FETCH response that kubicle reads, each `None` when absent.
#[derive(Default)]
pub(crate) struct Items<'a> {
    pub uid: Option<u32>,
    pub flags: Option<Flags>,
    arrival: Option<i64>,
    size: Option<u32>,
    envelope: Option<&'a Envelope<'a>>,
    list_post: Option<String>,
    attached: Attached,
}

impl<'a> Items<'a> {
    pub fn of(items: &'a [MessageDataItem<'a>]) -> Self {
        let mut found = Self::default();
        for item in items {
            match item {
                MessageDataItem::Uid(value) => found.uid = Some(value.get()),
                MessageDataItem::Flags(value) => found.flags = Some(flags_of(value)),
                MessageDataItem::InternalDate(value) => found.arrival = Some(value.as_ref().timestamp()),
                MessageDataItem::Rfc822Size(value) => found.size = Some(*value),
                MessageDataItem::Envelope(value) => found.envelope = Some(value),
                MessageDataItem::BodyExt { data, .. } => (found.list_post, found.attached) = fields_of(data),
                _ => {}
            }
        }
        found
    }

    /// `None` when the FETCH lacks an item `fetch_rows` asked for, as an unsolicited FETCH of another client's flag
    /// change does: `fetch_rows` then counts it as a change.
    pub fn fetched(self) -> Option<Fetched> {
        let (envelope, arrival) = (self.envelope?, self.arrival?);
        let date = nstring(&envelope.date).and_then(|raw| DateTime::parse_rfc822(&String::from_utf8_lossy(raw)));
        Some(Fetched {
            uid: self.uid?,
            flags: self.flags?,
            arrival,
            date: date.filter(DateTime::is_valid).map_or(arrival, |d| d.to_timestamp()), // a parsed date can hold day 0 or second 60
            size: self.size?,
            from: envelope.from.iter().find_map(address),
            to: envelope.to.iter().filter_map(address).collect(),
            cc: envelope.cc.iter().filter_map(address).collect(),
            bcc: envelope.bcc.iter().filter_map(address).collect(),
            subject: nstring(&envelope.subject).map(unstructured).unwrap_or_default(),
            list_post: self.list_post,
            attached: self.attached,
        })
    }
}

/// imap-types' wire flags as ours; other keywords are left out.
fn flags_of(flags: &[FlagFetch<'_>]) -> Flags {
    let mut out = Flags::default();
    for flag in flags {
        let ours = match flag {
            FlagFetch::Flag(Flag::Seen) => Ours::Seen,
            FlagFetch::Flag(Flag::Flagged) => Ours::Flagged,
            FlagFetch::Flag(Flag::Answered) => Ours::Answered,
            FlagFetch::Flag(Flag::Draft) => Ours::Draft,
            FlagFetch::Flag(Flag::Keyword(keyword)) if keyword.inner().eq_ignore_ascii_case("$Forwarded") => Ours::Forwarded,
            _ => continue,
        };
        out.set(ours, true);
    }
    out
}

fn nstring<'a>(value: &'a NString<'_>) -> Option<&'a [u8]> { value.0.as_ref().map(AsRef::as_ref) }

/// A header value as mail-parser's field parsers take it: they end a value at a newline.
fn line(raw: &[u8]) -> Vec<u8> { [raw, b"\n"].concat() }

/// An unstructured header value: encoded words decoded, raw 8-bit read as UTF-8 where it is.
fn unstructured(raw: &[u8]) -> String {
    if let HeaderValue::Text(text) = MessageStream::new(&line(raw)).parse_unstructured() { text.into_owned() } else { String::new() }
}

/// List-Post's value as sent, and the attachment mark, from the fetched `List-Post` and `Content-Type` lines.
fn fields_of(block: &NString<'_>) -> (Option<String>, Attached) {
    let Some(fields) = nstring(block).and_then(|block| MessageParser::new().parse_headers(block)) else { return (None, Attached::None) };
    (fields.header_raw("List-Post").map(str::to_owned), fields.content_type().map_or(Attached::None, attached))
}

/// An envelope address, or `None` for the markers around a group (RFC 3501, 7.4.2: NIL host).
fn address(address: &ImapAddress<'_>) -> Option<Address> {
    let mailbox = String::from_utf8_lossy(nstring(&address.mailbox)?);
    let host = String::from_utf8_lossy(nstring(&address.host)?);
    let name = nstring(&address.name).map(unstructured).filter(|name| !name.is_empty());
    Some(Address { name, email: format!("{mailbox}@{host}") })
}

/// The attachment cell's mark, from the top Content-Type alone, as Roundcube's list sets it (`app.js`,
/// `add_message_row`): the structure below is not read, so a multipart/mixed of text alone shows one too.
fn attached(content_type: &ContentType<'_>) -> Attached {
    let kind = format!("{}/{}", content_type.ctype(), content_type.subtype().unwrap_or_default()).to_ascii_lowercase();
    match kind.as_str() {
        "multipart/report" => Attached::Report,
        "multipart/encrypted" | "application/pkcs7-mime" => Attached::Encrypted,
        _ if kind.starts_with("application/") || kind.starts_with("multipart/m") || kind.starts_with("multipart/signed") => Attached::Files,
        _ => Attached::None,
    }
}
