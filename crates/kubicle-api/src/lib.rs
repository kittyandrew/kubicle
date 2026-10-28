//! The JSON the server sends and the client reads: one definition, so the two cannot drift. Every path below
//! is relative to the base path the app was loaded under (Mailu's `WEB_WEBMAIL`, e.g. `/login/`).

use std::{collections::BTreeMap, collections::BTreeSet, fmt, ops::RangeInclusive};

use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;

/// `GET api/session?folder=&page=`: everything the client needs before drawing anything, the page the address names
/// among it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub user: String,
    /// Sent back as the `X-Kubicle-Csrf` header on every POST.
    pub csrf: String,
    pub settings: Settings,
    pub folders: Vec<Folder>,
    pub themes: Vec<ThemeInfo>,
    /// Mailu's sign-out page: after `POST api/logout` the client navigates here.
    pub logout_url: String,
    /// Mailu's own pages for the user (password, auto-reply, forwarding), which About links to as Roundcube's
    /// "Get support" does under Mailu.
    pub admin_url: String,
    /// Where kubicle's source is published, which About links to (AGPL-3.0 section 13); `None` when not configured.
    pub source_url: Option<String>,
    /// The client build this server serves, its wasm's content-named file: a page that started on another, from
    /// the service worker's stored copy after a deploy, reloads once onto this one.
    pub build: String,
    /// The largest message the mail server takes, in bytes, as its submission port says (`SIZE`, RFC 1870; Mailu's
    /// `MESSAGE_SIZE_LIMIT`); `None` when it names none or was not reached yet. Compose checks attachments
    /// against it with `encoded_size`; the server refuses a message past it before sending.
    pub max_message_size: Option<u64>,
    /// The account's storage quota as last read; `Event::Quota` brings changes.
    pub quota: Option<Quota>,
    /// INBOX's UIDVALIDITY at the account's first sign-in. The browser keeps draft copies under it, so an account
    /// made anew under the address never finds the last one's.
    pub account: u32,
    /// The page of `folder` the query named, in the view the user's settings store for it, read after the folder
    /// list: the start waits for one answer, and nothing acts before its first page is here. `None` without a
    /// folder in the query, or when the page could not be read, which the client then asks `api/list` for.
    pub page: Option<ListPage>,
}

/// The account's storage quota (RFC 9208 `STORAGE`), in bytes. `None` where the mail server has no `QUOTA` or sets
/// no limit for the account: Mailu's Dovecot leaves an unlimited resource out (`quota_bytes: 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub used: u64,
    pub limit: u64,
}

/// `content_type` when kubicle serves it as an image (inline parts, previews, proxied images and Compose's Open): a
/// raster type; `None` for any other. SVG is left out: opened on its own it is a document that can run script.
#[must_use]
pub fn image_type(content_type: &str) -> Option<&'static str> {
    let types =
        ["image/png", "image/jpeg", "image/gif", "image/webp", "image/avif", "image/bmp", "image/x-icon", "image/vnd.microsoft.icon"];
    types.into_iter().find(|&t| t == content_type)
}

/// The bytes a file of `size` bytes takes in a sent message: base64 in lines of 76 characters and CRLF, 78 bytes
/// per 57, as mail-builder writes it; a nested message (`message/rfc822`) goes as it is (RFC 2046).
#[must_use]
pub fn encoded_size(size: u64, content_type: &str) -> u64 {
    if content_type.eq_ignore_ascii_case("message/rfc822") { size } else { size.div_ceil(57) * 78 }
}

/// The largest file whose `encoded_size` fits `limit`, the mail server's message size limit.
#[must_use]
pub fn largest_file(limit: u64) -> u64 { limit / 78 * 57 }

/// What a message counts beside its files against that limit, as Roundcube's `check_message_size` and
/// `write_forward_attachments` count its body: 10 KB.
pub const TEXT_ALLOWANCE: u64 = 10 * 1024;

/// Roundcube's `show_bytes`: whole KB rounded; MB and GB with one decimal below 10 and whole ones from 10, cut
/// rather than rounded (its `%d` and `%.1f`).
#[must_use]
pub fn size(bytes: u64) -> String {
    let unit = |size: u64, name: &str| {
        let tenths = (bytes * 10 + size / 2) / size; // `%.1f` rounds the tenth
        if bytes >= 10 * size { format!("{} {name}", bytes / size) } else { format!("{}.{} {name}", tenths / 10, tenths % 10) }
    };
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KB", (bytes + 512) / 1024),
        1_048_576..1_073_741_824 => unit(1_048_576, "MB"),
        _ => unit(1_073_741_824, "GB"),
    }
}

/// An address kubicle shows, links and sends as one: Roundcube's `rcube_utils::check_email` without its DNS lookup,
/// narrowed to what is safe in a header or an SMTP command. ASCII, a dot-atom local part (no quoted one, no address
/// literal), and a domain of two labels or more whose last is not all digits.
#[must_use]
pub fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else { return false };
    let atom = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~".contains(&b));
    let labels: Vec<&str> = domain.split('.').collect();
    let label_ok = |l: &str| {
        let l = l.strip_prefix("xn--").unwrap_or(l);
        (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-')
    };
    let last = labels.last().copied().unwrap_or_default();
    email.len() <= 254
        && local.split('.').all(atom)
        && labels.len() >= 2
        && labels.iter().all(|l| label_ok(l))
        && (last.starts_with("xn--") || last.bytes().all(|b| b.is_ascii_alphanumeric()) && !last.bytes().all(|b| b.is_ascii_digit()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// The full IMAP name in UTF-8 (decoded from modified UTF-7).
    pub name: String,
    /// The hierarchy separator, `.` on Mailu's Dovecot; `None` for a flat namespace.
    pub delimiter: Option<char>,
    /// The special roles it holds, as the user's choices over the server's SPECIAL-USE flags give them: none, one or
    /// several, as Roundcube keeps a `<role>_mbox` per role and two may name one folder. Never `Other`.
    pub roles: BTreeSet<FolderRole>,
    /// Always true for INBOX.
    pub subscribed: bool,
    /// False for `\Noselect` folders that only hold children.
    pub selectable: bool,
    pub total: u32,
    pub unseen: u32,
    /// The UID the next message will get, as the last STATUS read it: it grows with new mail, which Roundcube's
    /// "recent" mark needs; 1 for a folder that cannot be opened, which STATUS skips.
    pub uid_next: u32,
}

impl Folder {
    /// The role it is named by, as Roundcube's `folder_classname` picks one: Inbox, Sent, Drafts, Trash, then Junk;
    /// else Archive, which the archive plugin names; else `Other`.
    #[must_use]
    pub fn role(&self) -> FolderRole {
        use FolderRole::{Archive, Drafts, Inbox, Junk, Other, Sent, Trash};
        [Inbox, Sent, Drafts, Trash, Junk, Archive].into_iter().find(|r| self.roles.contains(r)).unwrap_or(Other)
    }
}

/// Whether the folder `name` is `root` or below it, by the hierarchy `delimiter`: `Archive-old` is not below
/// `Archive`.
#[must_use]
pub fn within(name: &str, root: &str, delimiter: Option<char>) -> bool {
    name == root || delimiter.is_some_and(|d| name.strip_prefix(root).is_some_and(|rest| rest.starts_with(d)))
}

/// Whether the folder `name` is the Sent or Drafts folder or below one: the list's From/To column names the recipients
/// there and the From/To sort sorts by To, as Roundcube's `message_list_smart_column_name` decides.
#[must_use]
pub fn lists_recipients(folders: &[Folder], name: &str) -> bool {
    folders.iter().any(|f| f.roles.iter().any(|r| matches!(r, FolderRole::Sent | FolderRole::Drafts)) && within(name, &f.name, f.delimiter))
}

/// A folder's full name from its parent's and its own, as the Parent folder choice joins them.
#[must_use]
pub fn folder_path(parent: Option<&str>, name: &str, delimiter: Option<char>) -> String {
    if let (Some(parent), Some(d)) = (parent, delimiter) { format!("{parent}{d}{name}") } else { name.to_owned() }
}

/// Roundcube's folder name checks (`folder_save.php`, `rcube_imap::folder_validate`), with its messages: not
/// empty, at most 128 characters, and none of the hierarchy delimiter, control characters, `%` or `*`, which LIST
/// reads as wildcards.
pub fn check_folder_name(name: &str, delimiter: Option<char>) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty.".into());
    }
    if name.chars().count() > 128 {
        return Err("Name is too long.".into());
    }
    let forbidden = name.chars().find(|&c| Some(c) == delimiter || c.is_ascii_control() || matches!(c, '%' | '*'));
    forbidden.map_or(Ok(()), |c| Err(format!("Folder name contains a forbidden character. ({c})")))
}

/// `GET api/sizes`: each folder's size in bytes, the sum of its messages' sizes, for the folder manager.
pub type FolderSizes = BTreeMap<String, u64>;

/// Declared in the order the folder pane lists sibling folders, which sorts a folder by the first role it holds, as
/// Roundcube's `sort_folder_list`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FolderRole {
    Inbox,
    Drafts,
    Sent,
    Junk,
    Trash,
    Archive,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortColumn {
    /// Roundcube's "None": the folder's own order, by UID.
    #[default]
    Unsorted,
    /// The IMAP internal date: Roundcube's "arrival" order.
    Arrival,
    /// The Date header: Roundcube's "sent date".
    Date,
    Subject,
    /// From, or To in Sent and Drafts, as the list's sender column shows.
    FromTo,
    From,
    To,
    Cc,
    Size,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    Ascending,
    /// Roundcube's default: newest first.
    #[default]
    Descending,
}

/// Roundcube's rows per page: at least 2 (`prefs_save.php`) and at most its `max_pagesize`, 200. `api/list` and
/// `api/settings` clamp any other size to the nearer end, as Roundcube's save does, rather than refuse it.
pub const PAGE_SIZES: RangeInclusive<u32> = 2..=200;

#[must_use]
pub fn clamp_page_size(size: u32) -> u32 { size.clamp(*PAGE_SIZES.start(), *PAGE_SIZES.end()) }

/// IANA's current name for a zone whose place was only renamed (`Europe/Kiev` is `Europe/Kyiv`), which Chromium's
/// `Intl`, on CLDR's names, still gives. Of tzdata's `backward` links only these: the rest merge one place into
/// another's zone (`Europe/Amsterdam` into `Europe/Brussels`), and a user knows the zone by its own city.
#[must_use]
pub fn current_zone(zone: &str) -> &str {
    match zone {
        "Europe/Kiev" => "Europe/Kyiv",
        "Asia/Calcutta" => "Asia/Kolkata",
        "Asia/Katmandu" => "Asia/Kathmandu",
        "Asia/Rangoon" => "Asia/Yangon",
        "Asia/Saigon" => "Asia/Ho_Chi_Minh",
        "Atlantic/Faeroe" => "Atlantic/Faroe",
        "America/Godthab" => "America/Nuuk",
        _ => zone,
    }
}

/// `GET api/list?folder=&page=&size=&sort=&order=&threads=&search=`: the query names the whole view, so the server
/// reads no saved settings for it and a sort change needs no save first (only `Session::page` takes the stored one).
/// Enums are spelled as their variant names, e.g. `sort=Subject`. With `around=UID` in place of `page` the answer is
/// the page that holds that message, as a message window needs for Previous, Next and the message after a delete;
/// its `query.page` says which. `search`, as JSON, makes it a page of that search's hits in the folders its scope
/// covers, threaded only within one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListQuery {
    /// The folder on screen, which a search's scope widens.
    pub folder: String,
    /// Zero-based.
    pub page: u32,
    pub size: u32,
    pub sort: SortColumn,
    pub order: SortOrder,
    pub threads: bool,
    pub search: Option<Search>,
}

/// `POST api/view`: the folder on screen, the one the server watches for new mail. Only this call sets it, so
/// loading other folders' pages ahead of time leaves the watch where the user is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewing {
    pub folder: String,
}

/// One page of a folder or of a search's hits, answering `query`; `total` counts threads where it is threaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPage {
    pub query: ListQuery,
    pub total: u32,
    pub rows: Vec<Hit>,
}

/// A row and the folder it is in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub folder: String,
    pub row: Row,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub uid: u32,
    /// Seconds since the Unix epoch: the Date header, or the arrival time when the header is missing.
    pub date: i64,
    /// Seconds since the Unix epoch: the arrival time (IMAP's INTERNALDATE), which the list shows instead of `date`
    /// under Arrival sort, as Roundcube's.
    pub arrival: i64,
    pub from: Address,
    pub to: Vec<Address>,
    pub subject: String,
    pub size: u32,
    pub flags: Flags,
    pub attached: Attached,
    /// Thread tree depth, 0 for a root or when threading is off.
    pub depth: u16,
    /// Messages below this one in its thread.
    pub replies: u32,
    /// The mail names a list address in List-Post, as `Message::list_post`, so Reply list knows it in the list, as
    /// Roundcube's does.
    pub list_post: bool,
}

/// What the list's attachment cell shows, from the message's top Content-Type, as Roundcube's list chooses it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attached {
    #[default]
    None,
    /// `application/*`, `multipart/mixed` or `multipart/signed`: Roundcube's guess that files are attached.
    Files,
    /// `multipart/report`: a bounce or delivery report.
    Report,
    /// `multipart/encrypted` or `application/pkcs7-mime`.
    Encrypted,
}

/// A set of `Flag`s, each the bit its discriminant names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Flags(pub u8);

impl Flags {
    #[must_use]
    pub fn has(self, flag: Flag) -> bool { self.0 & flag as u8 != 0 }

    pub fn set(&mut self, flag: Flag, on: bool) { if on { self.0 |= flag as u8 } else { self.0 &= !(flag as u8) } }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address {
    /// Never empty: a missing name is `None`.
    pub name: Option<String>,
    pub email: String,
}

/// `Name <email>`, or the address alone. A name holding a list separator, `<` or `"` is quoted, so the client's
/// address fields split the text back into the same addresses.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) if name.contains([',', ';', '<', '"']) => write!(f, "\"{}\" <{}>", name.replace('"', ""), self.email),
            Some(name) => write!(f, "{name} <{}>", self.email),
            None => f.write_str(&self.email),
        }
    }
}

/// `GET api/message?folder=&uid=&images=&text=`: `images=true` loads remote images through the proxy, `text=true`
/// shows HTML as text, `text=false` the HTML. The session remembers both (Roundcube's `safe_messages`,
/// `msg_formats`), else `prefer_html` decides; print and replies follow. `api/source`, `api/print` and
/// `api/download` take the same query for the raw message, a print page and a `.eml` (render.rs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // independent facts about one mail, each read on its own
pub struct Message {
    pub folder: String,
    pub uid: u32,
    pub date: i64,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    pub body: Body,
    /// Roundcube's header table (`message_headers`), the rows in its order, without the subject, which heads the
    /// reader and print instead.
    pub headers: Vec<Header>,
    /// The mail has an HTML part outside its attachments and forwarded messages, Roundcube's `has_html_part`:
    /// the reader offers "Plain text" or "HTML", and `text=true` shows the HTML as text.
    pub has_html: bool,
    /// A shown address has a domain that mixes scripts, as a look-alike of another domain does (Roundcube's
    /// `rcube_spoofchecker`): the reader says so above the mail.
    pub suspicious: bool,
    /// The mail names a list address in List-Post (RFC 2369), which Reply list answers.
    pub list_post: bool,
    /// The mail carries Bcc or Resent-Bcc lines, which Resend and Forward as attachment send on: both warn, as
    /// Roundcube does.
    pub bcc: bool,
    pub attachments: Vec<Attachment>,
    /// `None` for mail without an Authentication-Results of our server, as kubicle's own copies in Sent and Drafts,
    /// and without `KUBICLE_AUTHSERV_ID`.
    pub authenticity: Option<Authenticity>,
}

/// How this session shows a message, as the reader chose it: remote images loaded, and the plain-text (`Some(true)`)
/// or HTML body, `None` for the one `Settings::prefer_html` picks. `api/message`'s `images` and `text` ask for one; the
/// server keeps it for the session, so print, a new window and a reply show the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Chosen {
    pub images: bool,
    pub text: Option<bool>,
}

/// What the mail server found about the sender when the message arrived, from the Authentication-Results it
/// stamped (docs/design.md, Sender authentication).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authenticity {
    /// The domains whose DKIM signatures verified.
    pub signed_by: Vec<String>,
    /// The envelope sender's domain, when SPF verified it.
    pub mailed_by: Option<String>,
    pub warning: Option<Spoofed>,
    /// The From domain passed DMARC, or signed the mail itself (DKIM): its address is who sent it.
    pub from_verified: bool,
}

/// Why the From domain may not be who sent the mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Spoofed {
    /// The domain failed DMARC and asks receivers to quarantine or reject such mail.
    Dmarc(String),
    /// One of the user's own domains, with neither a signed-in sender nor DMARC behind it.
    Own(String),
    /// One of the user's own domains, sent by a signed-in account whose envelope sender, this address, is another.
    Account(String),
    /// The server's Authentication-Results cannot be split for sure, so nothing it says is shown.
    Unclear,
}

/// One row of Roundcube's header table, each only where the mail has a value for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Header {
    Subject(String),
    From(Vec<Address>),
    /// Only where it names someone From does not.
    Sender(Vec<Address>),
    To(Vec<Address>),
    Cc(Vec<Address>),
    Bcc(Vec<Address>),
    /// Only where it names someone From does not.
    ReplyTo(Vec<Address>),
    /// Mail-Reply-To, only where it names someone neither Reply-To nor From does.
    MailReplyTo(Vec<Address>),
    MailFollowupTo(Vec<Address>),
    Date(i64),
    /// From `X-Priority` 1 to 5; 3 is Normal, and a mail without one has no row.
    Priority(Priority),
}

impl Header {
    /// Roundcube's label for the row.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Subject(_) => "Subject",
            Self::From(_) => "From",
            Self::Sender(_) => "Sender",
            Self::To(_) => "To",
            Self::Cc(_) => "Cc",
            Self::Bcc(_) => "Bcc",
            Self::ReplyTo(_) | Self::MailReplyTo(_) => "Reply-To",
            Self::MailFollowupTo(_) => "Followup-To",
            Self::Date(_) => "Date",
            Self::Priority(_) => "Priority",
        }
    }

    /// The row's addresses, for the rows that hold some.
    #[must_use]
    pub fn addresses(&self) -> Option<&[Address]> {
        match self {
            Self::From(a)
            | Self::Sender(a)
            | Self::To(a)
            | Self::Cc(a)
            | Self::Bcc(a)
            | Self::ReplyTo(a)
            | Self::MailReplyTo(a)
            | Self::MailFollowupTo(a) => Some(a),
            Self::Subject(_) | Self::Date(_) | Self::Priority(_) => None,
        }
    }
}

impl Priority {
    /// Roundcube's name for the level.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Highest => "Highest",
            Self::High => "High",
            Self::Normal => "Normal",
            Self::Low => "Low",
            Self::Lowest => "Lowest",
        }
    }
}

/// A complete sanitized HTML document for the sandboxed frame's `srcdoc`, its CSP included: every part of the mail
/// in order, text parts and forwarded messages' headers drawn in it, as Roundcube shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    pub document: String,
    /// The mail asks for remote images that were not loaded.
    pub remote_blocked: bool,
    /// Every part shows as text, which the reader draws in the app's theme.
    pub text: bool,
    /// A part was too large to show: it is left out of `document` and listed with the attachments.
    pub too_large: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub size: u64,
    /// Signed URL that downloads it.
    pub url: String,
    /// Image types only: a signed URL serving the same part inline, for the preview.
    pub preview_url: Option<String>,
    /// Text, HTML and forwarded messages: a signed URL showing it as a sandboxed page of its own, as print opens.
    pub view_url: Option<String>,
}

/// `POST api/change`: everything the user changes in the mailbox, answered with no body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    /// Mark all as read is `Flag` with `Messages::All`.
    Flag { folder: String, messages: Messages, flag: Flag, set: bool },
    /// Also Delete to Trash and Empty outside Trash. Into Trash the server marks the messages read first, inside the
    /// same job, when the setting `read_when_deleted` says so, as Roundcube's `rcube_imap::move_message`.
    Move { folder: String, messages: Messages, to: String },
    /// Roundcube's archive plugin: to the Archive folder, or below it as `Settings::archive_type` divides it, marked
    /// read first under `read_on_archive`. Messages already in Archive or below it stay where they are. `zone` is the
    /// IANA zone a division by date reads the Date in, the user's; the server's own when it names none that exists.
    Archive { folder: String, messages: Messages, zone: String },
    /// Roundcube's markasjunk: Junk (`spam`) stores the keyword `Junk` and drops `NonJunk`, then moves to the Junk
    /// folder; Not junk the reverse, then to INBOX. The keywords go first, in one job, so they travel with the message.
    Report { folder: String, messages: Messages, spam: bool },
    /// Copy to...: the messages stay and a copy of each lands in `to`.
    Copy { folder: String, messages: Messages, to: String },
    /// Deletes for good: in Trash, with Shift+Delete, a draft's copy in Drafts, or a folder emptied.
    Delete { folder: String, messages: Messages },
    /// `name` inside `parent`, or at the top; the server checks `name` with `check_folder_name`.
    CreateFolder { name: String, parent: Option<String> },
    /// Gives `from` the name `name` inside `parent`, which moves it when the parent changes.
    RenameFolder { from: String, name: String, parent: Option<String> },
    /// Deletes the folder `name` and every folder below it, deepest first, each unsubscribed once deleted.
    DeleteFolder { name: String },
    /// Settings' folder list ticks it: the folder pane lists the subscribed folders.
    Subscribe { name: String, subscribed: bool },
}

/// The messages of one folder a change takes: some by UID, or every one, on every page, as after Roundcube's
/// Select all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Messages {
    Uids(Vec<u32>),
    All,
}

/// Every IMAP flag a row carries and a change may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Flag {
    Seen = 1,
    Flagged = 2,
    Answered = 4,
    /// The `$Forwarded` keyword.
    Forwarded = 8,
    /// On the copy in Drafts, so other mail apps list it as a draft.
    Draft = 16,
}

/// Roundcube's search, run as the user types (`ListQuery::search`). The header phase (`full` false) searches
/// Subject, From, To and Cc in milliseconds; the full phase adds the fields that read the message text (the body,
/// the entire message, Reply-To, Followup-To, Bcc) and the priority, and its answer replaces the header phase's.
/// Over several folders without a sort column the hits come grouped by folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Search {
    pub text: String,
    pub fields: SearchFields,
    pub filter: SearchFilter,
    pub interval: SearchInterval,
    pub scope: SearchScope,
    pub full: bool,
}

/// What a search looks for and where: the text and the fields, after a prefix chose them (`Search::terms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terms<'a> {
    pub fields: SearchFields,
    pub text: &'a str,
    /// A prefix chose the fields: `reply-to:` then searches Reply-To alone, not Mail-Reply-To too.
    pub prefixed: bool,
}

const NO_FIELDS: SearchFields = SearchFields {
    subject: false,
    from: false,
    reply_to: false,
    followup_to: false,
    to: false,
    cc: false,
    bcc: false,
    body: false,
    text: false,
};

/// Roundcube's search prefixes (`search.php`, `search_input`), each searching one field in place of those ticked.
const PREFIXES: [(&str, SearchFields); 7] = [
    ("from:", SearchFields { from: true, ..NO_FIELDS }),
    ("to:", SearchFields { to: true, ..NO_FIELDS }),
    ("reply-to:", SearchFields { reply_to: true, ..NO_FIELDS }),
    ("cc:", SearchFields { cc: true, ..NO_FIELDS }),
    ("bcc:", SearchFields { bcc: true, ..NO_FIELDS }),
    ("subject:", SearchFields { subject: true, ..NO_FIELDS }),
    ("body:", SearchFields { body: true, ..NO_FIELDS }),
];

impl Search {
    /// The text and the fields, as Roundcube reads a prefix at the start of the box, in any case: `from:alice`
    /// searches From alone for "alice". Unlike Roundcube's, the text after the prefix keeps any further colon
    /// ("subject:Re: plan" looks for "Re: plan"). One parser for the server's search and the client's phases.
    #[must_use]
    pub fn terms(&self) -> Terms<'_> {
        let text = self.text.trim();
        let prefix = PREFIXES.iter().find(|(p, _)| text.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)));
        match prefix {
            Some((p, fields)) => Terms { fields: *fields, text: text[p.len()..].trim(), prefixed: true },
            None => Terms { fields: self.fields, text, prefixed: false },
        }
    }

    /// Nothing narrows it: no text, type or date. The folder's own list answers that, so it is never run.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.terms().text.is_empty() && self.filter == SearchFilter::All && self.interval == SearchInterval::Any
    }

    /// Whether the full phase has anything to add: text in a field that reads the message text, or a priority.
    #[must_use]
    pub fn needs_full_phase(&self) -> bool {
        let Terms { fields: f, text, .. } = self.terms();
        let text = !text.is_empty() && (f.reply_to || f.followup_to || f.bcc || f.body || f.text);
        text || matches!(self.filter, SearchFilter::Priority(_))
    }
}

/// Roundcube's search fields; Sender is From, Reply-To and Followup-To, Recipient is To, Cc and Bcc. Remembered
/// per folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // a field added later reads as its default from settings stored before it
#[allow(clippy::struct_excessive_bools)] // independent toggles, as in Roundcube's search menu
pub struct SearchFields {
    pub subject: bool,
    pub from: bool,
    pub reply_to: bool,
    pub followup_to: bool,
    pub to: bool,
    pub cc: bool,
    pub bcc: bool,
    pub body: bool,
    /// IMAP `TEXT`: the headers and the body together. Roundcube's "Entire message".
    pub text: bool,
}

impl Default for SearchFields {
    // Mailu's `search_mods`: every field but Entire message, which the others already cover.
    fn default() -> Self {
        Self { subject: true, from: true, reply_to: true, followup_to: true, to: true, cc: true, bcc: true, body: true, text: false }
    }
}

/// Roundcube's search type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchFilter {
    #[default]
    All,
    Unread,
    Flagged,
    Unanswered,
    WithAttachment,
    /// The `X-Priority` header; Normal also takes a message without one.
    Priority(Priority),
}

/// Roundcube's date intervals: younger or older than a week, a month or a year.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchInterval {
    #[default]
    Any,
    YoungerThan(Period),
    OlderThan(Period),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Period {
    Week,
    Month,
    Year,
}

impl Period {
    #[must_use]
    pub fn days(self) -> i64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
            Self::Year => 365,
        }
    }
}

/// The default is Mailu's `search_scope`, which Roundcube 1.6 never reads (docs/design.md, Roundcube parity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchScope {
    Folder,
    /// The folder and every subscribed folder below it.
    #[default]
    Subfolders,
    /// Every subscribed folder, as Roundcube's `list_folders_subscribed`.
    All,
}

/// `POST api/compose`: a draft prefilled for a reply or forward, or empty for new mail, answered as a `Prefill`. It
/// is a POST because a forward stores the files it carries as uploads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeStart {
    pub mode: ComposeMode,
    /// The message answered, forwarded or reopened; only `ForwardAttachment` takes several, a folder's `All` in its
    /// saved sort, as Roundcube's Select all.
    pub messages: Vec<(String, Messages)>,
    /// The zone the user sees dates in, for the quote's "On <date>" line; the server's own when it names none that
    /// exists.
    pub zone: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComposeMode {
    New,
    Reply,
    ReplyAll,
    /// To the List-Post address, or Mail-Followup-To when the mail has one; offered only where
    /// `Message::list_post` is set.
    ReplyList,
    /// Forward inline: the text quoted in the body, the attachments carried over.
    Forward,
    /// Each original whole as a `message/rfc822` attachment, stored as an upload.
    ForwardAttachment,
    /// Reopen a message from Drafts.
    EditDraft,
    /// A new draft from any message's recipients, subject, body and attachments, which replaces nothing.
    EditAsNew,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // a field added later reads as its default from a copy kubicle-web stored before it
#[allow(clippy::struct_excessive_bools)] // independent options, as Roundcube's compose options column
pub struct Draft {
    /// The draft's current copy, which the next save replaces in its folder and Send and Discard remove.
    pub copy: Option<DraftCopy>,
    /// The Message-ID every save and the sent mail carry, without angle brackets: one per draft, as Roundcube
    /// keeps one per compose. `api/compose` picks it; a reopened draft keeps its own.
    pub message_id: Option<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    /// Roundcube's Reply-To and Followup-To fields: Reply-To goes out as Reply-To and Mail-Reply-To,
    /// Followup-To as Mail-Followup-To.
    pub reply_to: Vec<Address>,
    pub followup_to: Vec<Address>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Uploaded files (`POST api/upload`) and parts carried over from a forwarded message.
    pub attachments: Vec<DraftAttachment>,
    /// The message this draft answers or forwards, flagged `\Answered` or `$Forwarded` once sent; several when it
    /// forwards several as attachments.
    pub sources: Vec<SourceRef>,
    /// Roundcube's compose options: `X-Priority`, a read receipt request (`Disposition-Notification-To`), a
    /// delivery status notification (SMTP DSN) and where the sent copy goes.
    pub priority: Priority,
    pub return_receipt: bool,
    pub delivery_status: bool,
    pub sent_copy: SentCopy,
    /// Roundcube's "Keep formatting": the sent text goes out as typed, not wrapped at 72 as format=flowed.
    pub keep_formatting: bool,
    /// Opened from Drafts with Edit, not started in this Compose: Discard moves its copy to Trash, where a copy only
    /// this Compose made is deleted for good.
    pub opened: bool,
}

/// `POST api/compose`'s answer: the draft, and what Compose says as it opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefill {
    pub draft: Draft,
    /// Messages forwarded as attachments that the size limit left out, in Roundcube's words.
    pub notice: Option<String>,
    /// A message forwarded as an attachment carries Bcc or Resent-Bcc lines, which go on with it: Compose warns, as
    /// Roundcube does.
    pub forwards_bcc: bool,
}

/// Roundcube's five priorities, `X-Priority` 1 to 5 in this order; `Normal` sends no header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Highest,
    High,
    #[default]
    Normal,
    Low,
    Lowest,
}

/// Where a sent message's copy is filed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SentCopy {
    /// The folder with the Sent role.
    #[default]
    Sent,
    Folder(String),
    NotSaved,
}

/// The largest file `POST api/upload` takes, in MiB, and the most `POST api/import?folder=` takes in all: a ceiling
/// on the request, under which the mail server's own limit (`Session::max_message_size`) decides an upload. Import
/// takes one or more `.eml` or mbox files as multipart fields `file`, appends their messages to the folder as they
/// are and answers with how many it stored.
pub const MAX_UPLOAD_MIB: usize = 36;

/// A draft's copy on the server: in Drafts, or in the folder below it that the draft was opened from, which its
/// saves keep it in. The folder and the UID travel together, so no save can name a UID in another folder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DraftCopy {
    pub folder: String,
    pub uid: u32,
}

/// `POST api/upload` (multipart, the file in the field `file`) answers with one. `POST api/draft` takes a `Draft`
/// and answers with its new `DraftCopy`; `POST api/send` takes one and answers with no body.
/// `GET api/upload?id=&name=` serves the stored file back, as a download named `name`, or with `open=true` an image
/// inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachment {
    pub id: String,
    /// The name the file goes out with: Rename changes it here, and the server sends it as the draft names it.
    pub filename: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub folder: String,
    pub uid: u32,
    pub mode: ComposeMode,
}

/// `POST api/redirect`: Roundcube's bounce. The message goes out unchanged to new recipients, with Resent-From,
/// Resent-To, Resent-Cc, Resent-Date and Resent-Message-ID on top; its copy, with Resent-Bcc too, is filed as
/// `sent_copy` says. Answered with no body once submission took it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    pub folder: String,
    pub uid: u32,
    /// Resent-From, which must be one of the user's addresses, as a draft's From.
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub sent_copy: SentCopy,
}

/// Per-user preferences, stored on the server. `GET api/settings` reads them; `POST api/settings` replaces them whole
/// and answers with the new `revision`, or 409 when they were changed since this copy was read.
#[derive(Debug, Clone, SmartDefault, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // a setting added later reads as its default from settings stored before it
#[allow(clippy::struct_excessive_bools)] // each is one of Roundcube's independent on/off preferences
pub struct Settings {
    /// Counts the saves: a save from an older copy is refused, so a tab left open cannot undo what another tab or
    /// device saved meanwhile. The server sets it.
    pub revision: u64,
    /// The name on sent mail; empty means the address alone.
    pub display_name: String,
    #[default = 50]
    pub page_size: u32,
    #[default = "latte"]
    pub light_theme: String,
    #[default = "frappe"]
    pub dark_theme: String,
    pub theme_mode: ThemeMode,
    /// Sort, threading, search fields and collapsed subfolders per folder, keyed by folder name.
    pub folders: BTreeMap<String, FolderPrefs>,
    pub remote_images: RemoteImages,
    /// Addresses whose mail loads remote images through the proxy without asking, under `RemoteImages::Trusted`.
    pub trusted_senders: Vec<String>,
    /// Pasted or uploaded themes, CSS by id; `GET api/theme/<id>.css` serves one.
    pub themes: BTreeMap<String, String>,
    /// The reader shows every address and the date in a table, not only the summary line.
    pub message_details: bool,
    /// An IANA zone for every date shown, by its `current_zone` name; `None` follows the browser's own, as
    /// Roundcube's "Auto".
    pub time_zone: Option<String>,
    // Roundcube's preferences for what kubicle does, with its defaults (program/actions/settings/index.php).
    /// Seconds a previewed message stays open before it counts as read: 0 at once, `None` never.
    #[default(Some(0))]
    pub mark_read_after: Option<u32>,
    pub expand_threads: ExpandThreads,
    /// Roundcube's "Display HTML": off shows every mail as plain text, HTML converted.
    #[default = true]
    pub prefer_html: bool,
    /// Roundcube's "Show email address with display name": the reader shows `Name <email>` where it would show the
    /// name alone, with the address as its tooltip.
    pub show_email: bool,
    /// After a delete or move, open the next message.
    #[default = true]
    pub display_next: bool,
    pub reply_mode: ReplyMode,
    /// Forward attaches the message instead of quoting it.
    pub forward_attachment: bool,
    /// Every Compose opens in a tab of its own, as Roundcube's `compose_extwin`; never on a phone.
    pub compose_extwin: bool,
    /// Compose starts with Return receipt ticked, as Roundcube's `mdn_default`; a reopened draft keeps its own.
    pub mdn_default: bool,
    /// Compose starts with Delivery status notification ticked (`dsn_default`).
    pub dsn_default: bool,
    /// A reply or inline forward files its sent copy in the original's folder when that folder is subscribed, as
    /// Roundcube's `reply_same_folder`.
    pub reply_same_folder: bool,
    /// The Reply all button and `R` answer the list on list mail, as Roundcube's `reply_all_mode = 1`, which Mailu
    /// sets; off, they answer everyone.
    #[default = true]
    pub reply_all_list: bool,
    /// Roundcube's identity fields for each of the user's addresses, by the address in lower case.
    pub identities: BTreeMap<String, IdentityPrefs>,
    /// Mark messages read when they are deleted or moved to Trash.
    #[default = true]
    pub read_when_deleted: bool,
    /// Delete in Junk deletes for good, as in Trash.
    pub delete_junk: bool,
    /// Roundcube's "Clear Trash on logout".
    pub logout_purge: LogoutPurge,
    /// The archive plugin's "Mark the message as read on archive".
    pub read_on_archive: bool,
    /// Folders chosen for the special roles, over the server's SPECIAL-USE flags: a role left out is as the server
    /// marks it, `None` is Roundcube's "---", no folder for that role. Two roles may name one folder; INBOX is refused
    /// on save.
    pub special_folders: BTreeMap<FolderRole, Option<String>>,
    /// The archive plugin's "Divide archive by".
    pub archive_type: ArchiveType,
}

/// The archive plugin's "Divide archive by" (`archive.php`, `move_messages`): the folder below Archive a message
/// goes to, created on first use. Dates are the message's Date in the user's zone, as Roundcube's `format_date`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveType {
    /// Archive itself.
    #[default]
    None,
    /// `Archive/2012`.
    Year,
    /// `Archive/2012/06`.
    Month,
    /// `Archive/2012/2012-06`, as Thunderbird divides it.
    ThunderbirdMonth,
    /// `Archive/<the sender's address>`, `unknown` without one.
    Sender,
    /// `Archive/<the folder it came from>`.
    Folder,
    /// `Archive/2012/<the folder it came from>`.
    FolderYear,
    /// `Archive/2012/06/<the folder it came from>`.
    FolderMonth,
}

/// Roundcube's "Clear Trash on logout" (`rcmail::logout_actions`): Trash and every folder below it lose all their
/// messages, or those that arrived more than the days ago (IMAP's internal date); a subfolder goes too once nothing
/// is left in it or below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogoutPurge {
    #[default]
    Never,
    All,
    OlderThan(u16),
}

/// Roundcube's "Allow remote resources": when remote images in HTML mail load without asking.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteImages {
    Never,
    /// From a trusted sender, when the From domain passed DMARC or signed the mail (`Authenticity::from_verified`).
    #[default]
    Trusted,
    Always,
}

/// Roundcube's "Expand message threads".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpandThreads {
    #[default]
    Never,
    All,
    Unread,
}

/// Roundcube's "When replying".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyMode {
    /// No quote.
    Empty,
    /// Below the quote.
    #[default]
    Below,
    /// Above the quote.
    Above,
    /// Above the original, which follows unquoted under a forward's header block.
    AboveNoIndent,
}

/// What Compose adds when this identity is the From, as Roundcube's identities: Reply-To and Bcc go into their
/// fields when it is picked and out again when another is; Organization goes out as a header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityPrefs {
    pub organization: String,
    pub reply_to: Vec<Address>,
    pub bcc: Vec<Address>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    /// Follow the system's light or dark preference.
    #[default]
    Auto,
    Light,
    Dark,
}

/// The default is Roundcube's: no sort column, so the folder's own order, newest first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)] // prefs stored before a field existed read it as its default
pub struct FolderPrefs {
    pub sort: SortColumn,
    pub order: SortOrder,
    pub threads: bool,
    pub search: SearchFields,
    /// The folder pane hides this folder's subfolders.
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeInfo {
    pub id: String,
    pub name: String,
    pub dark: bool,
    pub source: ThemeSource,
    /// The file's content hash, which its address carries (`?v=`): a changed theme is a new address, so the
    /// browser keeps each one for good instead of asking again at every start.
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeSource {
    BuiltIn,
    /// From the themes directory the NixOS module provides.
    System,
    /// Pasted or uploaded by this user.
    User,
}

/// One message on `GET api/events`: server-sent events, one JSON object each. Every stream starts with `Resync`,
/// so a client that opens it again catches up on whatever it missed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Every folder and the quota as they stand, at the start of a stream and when this listener fell behind: reload
    /// what is shown, since changes may have gone unheard.
    Resync(Vec<Folder>, Option<Quota>),
    /// Totals or unread counts changed.
    Folders(Vec<Folder>),
    /// The storage quota changed, as read after a change and in each check of the folders.
    Quota(Option<Quota>),
    /// The settings as saved, by any tab or device, or after a folder rename moved its preferences. Boxed: they
    /// are many times the size of every other event.
    Settings(Box<Settings>),
    /// Messages arrived, left or changed flags in this folder: refetch the visible page.
    FolderChanged { folder: String },
    /// The Mailu session behind this one ended: go through `sso.php` again.
    SessionExpired,
    /// Something done in the background failed, such as filing a sent message's copy: tell the user.
    Notice(String),
    /// Sent after `KEEPALIVE_SECONDS` without another event. A browser's `EventSource` hides SSE comments, so
    /// only a real event lets the client tell a quiet stream from a dead one.
    Keepalive,
}

/// `GET api/zip?mode=&sets=`: Roundcube's zipdownload of a selection, streamed. `sets` is JSON, a list of
/// `[folder, set]` with each set in `uid_set`'s form; Maildir gives one `.eml` per message, Mbox one mailbox file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZipMode {
    Maildir,
    Mbox,
}

/// `messages` as text a URL or header carries: `*` for every message, else an IMAP sequence set of UIDs, sorted
/// with runs joined (`1:5,9`), as Roundcube's `compressMessageSet`; UID 0, which names no message, left out. A
/// page of 500 selected messages stays well inside a request line, and a command for thousands under Dovecot's
/// line limit.
#[must_use]
pub fn uid_set(messages: &Messages) -> String { uid_runs(messages).map_or_else(|| "*".into(), |runs| runs.join(",")) }

/// `uid_set` split so each piece's text is at most `max_len` bytes, for an IMAP command: a sparse selection of
/// thousands of UIDs serialises past the server's command-line limit as one set, and the whole command is refused,
/// so the caller runs it once per piece. `All` is one `*`; a single run longer than `max_len` is not split (a UID
/// cannot be).
#[must_use]
pub fn uid_set_chunks(messages: &Messages, max_len: usize) -> Vec<String> {
    let Some(runs) = uid_runs(messages) else { return vec!["*".into()] };
    let (mut chunks, mut current) = (Vec::new(), String::new());
    for run in runs {
        if !current.is_empty() && current.len() + 1 + run.len() > max_len {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(&run);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// The sorted UID runs of `messages` as `1:5` or `9`, as Roundcube's `compressMessageSet`; UID 0 (no message) left
/// out. `None` for `All`, which is `*`.
fn uid_runs(messages: &Messages) -> Option<Vec<String>> {
    let Messages::Uids(uids) = messages else { return None };
    let mut sorted: Vec<u32> = uids.iter().copied().filter(|&uid| uid > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let run = |run: &[u32]| if let [first, .., last] = run { format!("{first}:{last}") } else { run[0].to_string() };
    Some(sorted.chunk_by(|a, b| a + 1 == *b).map(run).collect())
}

/// The most UIDs a set from a URL or a draft's `X-Draft-Info` may name in all, so a crafted one cannot fill memory.
pub const MAX_UIDS: usize = 100_000;

/// `uid_set`'s text back as `Messages`; `None` when it is not one, or names more than `MAX_UIDS`.
#[must_use]
pub fn parse_uid_set(text: &str) -> Option<Messages> {
    if text == "*" {
        return Some(Messages::All);
    }
    let mut uids = Vec::new();
    for part in text.split(',') {
        let (first, last) = part.split_once(':').unwrap_or((part, part));
        let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
        if first == 0 || last < first || uids.len() + (last - first) as usize >= MAX_UIDS {
            return None;
        }
        uids.extend(first..=last);
    }
    Some(Messages::Uids(uids))
}

/// The longest gap between two events on a stream.
pub const KEEPALIVE_SECONDS: u64 = 15;

/// The body of every non-2xx JSON answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub message: String,
}
