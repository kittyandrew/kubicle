//! Compose: drafts prefilled for each `ComposeMode`; uploads; building the MIME, or a redirect's Resent-* block,
//! and sending it through front's submission port.
//!
//! The browser keeps every keystroke itself and saves the draft about ten seconds after typing pauses, on leaving
//! it and on closing it: `build` makes the copy, which the engine stores in IMAP Drafts in place of the previous
//! one. After `send` the caller files the copy in Sent and flags the source.

use std::{borrow::Cow, collections::BTreeMap, collections::HashMap, collections::HashSet, fmt::Write as _, sync::Arc, sync::LazyLock};
use std::{sync::atomic::Ordering, time::Duration};

use axum::http::StatusCode;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use jiff::{Timestamp, tz::TimeZone};
use kubicle_api::{Address, ComposeMode, Draft, DraftAttachment, DraftCopy, Folder, FolderRole, IdentityPrefs, Messages, Priority};
use kubicle_api::{Redirect, ReplyMode, SentCopy, Settings, SourceRef};
use kubicle_imap::header::Fetched;
use kubicle_mail::{flowed, render, render::Mail, render::ascii_address, render::list_post, render::raw_addresses, structure};
use kubicle_mail::{structure::Content, structure::Structure};
use mail_builder::{MessageBuilder, headers::Header, headers::address::Address as MimeAddress, headers::content_type::ContentType};
use mail_builder::{headers::message_id::MessageId, headers::raw::Raw, headers::text::Text, mime::MimePart};
use mail_parser::HeaderValue;
use regex::Regex;
use tokio::{io::AsyncBufRead, io::AsyncBufReadExt, io::AsyncRead, io::AsyncReadExt, io::AsyncWrite, io::AsyncWriteExt};
use tokio::{io::BufReader, net::TcpStream};
use tracing::warn;
use zeroize::Zeroizing;

use crate::{Error, MailServer, random_id, uploads::Uploads};

const SMTP_TIMEOUT: Duration = Duration::from_secs(60);

fn refused(message: impl Into<String>) -> Error { Error::status(StatusCode::BAD_REQUEST, message) }

fn smtp_error(message: impl Into<String>) -> Error {
    Error::status(StatusCode::BAD_GATEWAY, format!("the mail server refused: {}", message.into()))
}

/// The message is larger than the mail server takes: refused before any byte goes out, where the server would
/// refuse it only after DATA.
fn too_large(size: u64, limit: u64) -> Error {
    let (size, limit) = (kubicle_api::size(size), kubicle_api::size(limit));
    Error::status(StatusCode::PAYLOAD_TOO_LARGE, format!("the message is {size}, more than the {limit} the mail server takes"))
}

/// The largest message an EHLO answer names (`SIZE n`, RFC 1870); `SIZE 0` and no SIZE mean none.
fn size_limit_of(capabilities: &[String]) -> u64 {
    let size = capabilities.iter().find_map(|line| line.strip_prefix("SIZE ").or_else(|| line.strip_prefix("size ")));
    size.and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// Asks the submission port its size limit, without signing in: Mailu's front answers EHLO with Postfix's
/// `MESSAGE_SIZE_LIMIT`. Run from start until front answers; every send reads it again.
pub async fn learn_size_limit(server: &MailServer) -> Result<(), Error> {
    let session = async {
        let (mut plain, _) = greet(server).await?;
        let _ = command(&mut plain, "QUIT", &[221]).await;
        Ok(())
    };
    smtp_session(session).await
}

/// Runs one session with the submission port within `SMTP_TIMEOUT`. A failed read, write or handshake there is the
/// mail server's failure (502), not kubicle's: the user sees why the send failed, as Roundcube's SMTP Error does.
async fn smtp_session<T>(session: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
    let timeout = || Error::status(StatusCode::GATEWAY_TIMEOUT, "the mail server did not answer in time");
    let ended = tokio::time::timeout(SMTP_TIMEOUT, session).await.map_err(|_| timeout())?;
    ended.map_err(|e| match e {
        Error::Io(e) => Error::status(StatusCode::BAD_GATEWAY, format!("the connection to the mail server failed: {e}")),
        other => other,
    })
}

/// A plain connection to the submission port, past its greeting and an EHLO, and what the EHLO offers; its size
/// limit is stored.
async fn greet(server: &MailServer) -> Result<(BufReader<TcpStream>, Vec<String>), Error> {
    let tcp = TcpStream::connect((server.host.as_str(), server.submission_port)).await?;
    tcp.set_nodelay(true)?; // else a command's last bytes wait out the server's delayed ACK (Nagle): 40 ms a command
    let mut plain = BufReader::new(tcp);
    reply(&mut plain, &[220]).await?;
    let capabilities = command(&mut plain, "EHLO kubicle", &[250]).await?;
    server.size_limit.store(size_limit_of(&capabilities), Ordering::Relaxed);
    Ok((plain, capabilities))
}

/// An upload the mail server could never send, on its own: the error says the largest file that fits.
pub fn check_upload(server: &MailServer, size: u64, content_type: &str) -> Result<(), Error> {
    let exceeded = server.size_limit().filter(|&limit| kubicle_api::encoded_size(size, content_type) > limit);
    let Some(limit) = exceeded else { return Ok(()) };
    let largest = kubicle_api::size(kubicle_api::largest_file(limit));
    Err(Error::status(StatusCode::PAYLOAD_TOO_LARGE, format!("the uploaded file exceeds the maximum size of {largest}")))
}

/// How many identities compose offers besides the account's own.
const FROM_CHOICES: usize = 20;

/// Who is writing: the signed-in Mailu user and the addresses that count as theirs.
pub struct Identity<'a> {
    /// The Mailu account address.
    pub address: &'a str,
    /// `Settings.display_name`; empty sends the address alone.
    pub name: &'a str,
    /// Domains whose addresses deliver to this user (aliases, catch-alls); the account's own domain counts too.
    pub domains: &'a [String],
    /// The mail in Sent, which `identities` reads; empty where only `owns` is asked.
    pub sent: &'a [Fetched],
}

impl Identity<'_> {
    /// The user's identities, as Roundcube keeps a list of them: the account first, then the addresses on the
    /// user's domains it has sent from, most used first. Others on those domains (family, colleagues) are not
    /// among them, though any address there may be typed as From: the send path checks `owns`.
    fn identities(&self) -> Vec<String> {
        let mut counts: HashMap<String, u32> = HashMap::new();
        for email in self.sent.iter().filter_map(|h| h.from.as_ref()).map(|a| a.email.to_lowercase()) {
            if self.owns(&email) && !email.eq_ignore_ascii_case(self.address) {
                *counts.entry(email).or_default() += 1;
            }
        }
        let mut found: Vec<_> = counts.into_iter().collect();
        found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        std::iter::once(self.address.to_owned()).chain(found.into_iter().take(FROM_CHOICES).map(|(email, _)| email)).collect()
    }

    /// The From addresses compose offers: the identities, with the display name.
    #[must_use]
    pub fn addresses(&self) -> Vec<Address> { self.identities().iter().map(|email| self.named(email)).collect() }

    #[must_use]
    pub fn owns(&self, email: &str) -> bool {
        let Some((_, domain)) = email.rsplit_once('@') else { return false };
        let own = self.address.rsplit_once('@').map(|(_, d)| d);
        own.is_some_and(|d| d.eq_ignore_ascii_case(domain)) || self.domains.iter().any(|d| d.eq_ignore_ascii_case(domain))
    }

    fn named(&self, email: &str) -> Address {
        Address { name: (!self.name.is_empty()).then(|| self.name.to_owned()), email: email.to_owned() }
    }
}

/// A draft to start from: empty for new mail, else from `source`, the message it answers, forwards inline or
/// reopens (`ForwardAttachments` makes the rest). Forwarded and reopened attachments are copied into the uploads;
/// a reopened draft keeps its Message-ID and the copy (`DraftCopy`) its first save replaces. With `html` a reply or
/// forward quotes the HTML, as the reader showed it.
pub fn prefill(
    me: &Identity, source: Option<(SourceRef, &Mail)>, uploads: &Uploads, tz: &TimeZone, settings: &Settings, folders: &[Folder], html: bool,
) -> Result<Draft, Error> {
    let Some((source, mail)) = source.filter(|(source, _)| source.mode != ComposeMode::New) else {
        return Ok(finish(Draft { from: me.named(me.address), ..Draft::default() }, ComposeMode::New, settings));
    };
    let (from, to, cc) = (render::addresses(mail.from()), render::addresses(mail.to()), render::addresses(mail.cc()));
    let date = render::long_date(mail, tz);
    let (mode, uid, subject) = (source.mode, source.uid, mail.subject().map(render::clean).unwrap_or_default());
    // Only a draft is edited in place, as its save replaces it: anywhere but Drafts and below (a `#compose-start`
    // link can name any message) the edit starts anew, so a received message is never replaced.
    let in_drafts =
        folders.iter().any(|f| f.roles.contains(&FolderRole::Drafts) && kubicle_api::within(&source.folder, &f.name, f.delimiter));
    let mode = if mode == ComposeMode::EditDraft && !in_drafts { ComposeMode::EditAsNew } else { mode };
    let from_identity = reply_from(&me.identities(), mail, mode);
    let mut draft = Draft { from: me.named(&from_identity), sources: vec![source.clone()], ..Draft::default() };
    draft.priority = render::priority(mail).unwrap_or_default();
    // A reply or forward refers to the original; a reopened draft takes its own below.
    draft.in_reply_to = mail.message_id().map(str::to_owned);
    draft.references = references(mail);
    match mode {
        ComposeMode::New => unreachable!("filtered above"),
        ComposeMode::ForwardAttachment => unreachable!("ForwardAttachments makes these"),
        ComposeMode::Reply | ComposeMode::ReplyAll | ComposeMode::ReplyList => {
            (draft.to, draft.cc) = reply_recipients(mail, mode, &draft.from.email);
            draft.subject = format!("Re: {}", base_subject(&strip_was(&subject), &["re"], false));
            let text = source_text(&Structure::of(mail, html), true);
            let sender = from.first().map(|a| a.name.clone().unwrap_or_else(|| a.email.clone())).unwrap_or_default();
            let intro = date.as_ref().map_or_else(|| format!("{sender} wrote:"), |date| format!("On {date}, {sender} wrote:"));
            // Roundcube's shapes: three empty lines to write in above the quote, or the line right below it; with no
            // indent, a forward's short header block over the original as it is.
            draft.body = match settings.reply_mode {
                ReplyMode::Empty => String::new(),
                ReplyMode::Below => format!("{intro}\n{}\n", quote(&text)),
                ReplyMode::Above => format!("\n\n\n{intro}\n{}", quote(&text)),
                ReplyMode::AboveNoIndent =>
                    format!("\n\n\n{}{}", forward_header(mail, &subject, date.as_deref(), false), text.trim_matches('\n')),
            };
        }
        ComposeMode::Forward => {
            draft.subject = format!("Fwd: {}", base_subject(&subject, &["fwd", "fw"], false));
            let structure = Structure::of(mail, html);
            draft.body = forward_header(mail, &subject, date.as_deref(), true) + source_text(&structure, true).trim_matches('\n');
            draft.attachments = carry_attachments(&structure, mail, uploads)?;
        }
        ComposeMode::EditDraft | ComposeMode::EditAsNew => {
            draft.from = from.into_iter().next().filter(|a| me.owns(&a.email)).unwrap_or_else(|| me.named(me.address));
            draft.subject = subject;
            (draft.to, draft.cc, draft.bcc) = (to, cc, render::addresses(mail.bcc()));
            let mail_reply_to = Some(raw_addresses(mail, "Mail-Reply-To")).filter(|r| !r.is_empty());
            draft.reply_to = mail_reply_to.unwrap_or_else(|| render::addresses(mail.reply_to()));
            draft.followup_to = raw_addresses(mail, "Mail-Followup-To");
            let structure = Structure::of(mail, false);
            draft.body = source_text(&structure, false); // a Roundcube draft is flowed: unfolded here
            draft.in_reply_to = ids(mail.in_reply_to()).next().map(str::to_owned);
            draft.references = ids(mail.references()).map(str::to_owned).collect();
            draft.attachments = carry_attachments(&structure, mail, uploads)?;
            draft.return_receipt = mail.header_raw("Disposition-Notification-To").is_some();
            let edit = mode == ComposeMode::EditDraft; // EditAsNew replaces nothing and answers nothing
            let (sources, dsn) = draft_info(mail);
            draft.delivery_status = dsn;
            draft.sources = if edit { sources } else { Vec::new() };
            draft.copy = edit.then(|| DraftCopy { folder: source.folder.clone(), uid });
            draft.opened = edit;
            draft.message_id = mail.message_id().map(str::to_owned).filter(|id| edit && valid_id(id));
        }
    }
    // Roundcube's `reply_same_folder`: replies and inline forwards, and a reopened draft of one, from its draft info.
    let original = match mode {
        _ if !settings.reply_same_folder => None,
        ComposeMode::EditDraft => draft.sources.first().map(|s| s.folder.clone()),
        _ => (mode != ComposeMode::EditAsNew).then_some(source.folder),
    };
    if let Some(copy) = original.and_then(|folder| same_folder(folders, &folder)) {
        draft.sent_copy = copy;
    }
    Ok(finish(draft, mode, settings))
}

/// Forward as attachment (`write_forward_attachments`), built one message at a time so only one is held at once.
/// Each is named as Roundcube names it: the subject's first 64 characters, or `message_rfc822`, with `_2`, `_3`
/// and on for a taken name, plus `.eml`. References lists every Message-ID; In-Reply-To only a single message's.
/// Dropped before `finish`, it removes the uploads it stored: else an error or a closed request part way left them
/// to fill the user's uploads until they expire in a month.
pub struct ForwardAttachments<'u> {
    draft: Draft,
    names: Vec<(String, u32)>,
    uploads: &'u Uploads,
}

impl Drop for ForwardAttachments<'_> {
    fn drop(&mut self) {
        let ids: Vec<&str> = self.draft.attachments.iter().map(|a| a.id.as_str()).collect();
        if !ids.is_empty()
            && let Err(e) = self.uploads.remove(&ids)
        {
            warn!(error = %e, "a forward ended early, and its uploads stay until pruned");
        }
    }
}

impl<'u> ForwardAttachments<'u> {
    /// `subject` is the first non-empty subject among all asked for, those left out too.
    #[must_use]
    pub fn new(subject: &str, uploads: &'u Uploads) -> Self {
        let subject = format!("Fwd: {}", base_subject(subject, &["fwd", "fw"], false));
        Self { draft: Draft { subject, ..Draft::default() }, names: Vec::new(), uploads }
    }

    /// Attaches `raw` whole, as an upload; `mail` is its header. The first message attached picks the From identity.
    pub fn add(&mut self, me: &Identity, source: SourceRef, mail: &Mail, raw: &[u8]) -> Result<(), Error> {
        if self.draft.sources.is_empty() {
            self.draft.from = me.named(&reply_from(&me.identities(), mail, ComposeMode::ForwardAttachment));
        }
        let name: String = mail.subject().map(render::clean).unwrap_or_default().chars().take(64).collect();
        let mut name = if name.is_empty() { "message_rfc822".to_owned() } else { name };
        match self.names.iter_mut().find(|(taken, _)| *taken == name) {
            Some((_, count)) => {
                *count += 1;
                name = format!("{name}_{count}");
            }
            None => self.names.push((name.clone(), 1)),
        }
        self.draft.attachments.push(upload(self.uploads, &format!("{name}.eml"), "message/rfc822", raw)?);
        self.draft.references.extend(mail.message_id().map(str::to_owned));
        self.draft.sources.push(source);
        Ok(())
    }

    #[must_use]
    pub fn finish(mut self, me: &Identity, settings: &Settings) -> Draft {
        let mut draft = std::mem::take(&mut self.draft); // the draft holds its uploads from here on
        if draft.sources.is_empty() {
            draft.from = me.named(me.address);
        }
        if let [id] = draft.references.as_slice() {
            draft.in_reply_to = Some(id.clone());
        }
        finish(draft, ComposeMode::ForwardAttachment, settings)
    }
}

/// Roundcube's notice when forwarding left messages out: `limit` is the message size they would have passed.
#[must_use]
pub fn left_out_notice(count: u32, limit: u64) -> String {
    format!("Maximum size of a message ({}) exceeded. {count} message(s) have not been attached.", kubicle_api::size(limit))
}

/// What every new Compose takes from the settings, as Roundcube's compose does: Return receipt and Delivery status
/// notification ticked by `mdn_default` and `dsn_default` (a reopened draft keeps its receipt request), Don't save
/// when Sent is set to no folder, the From identity's Reply-To and Bcc added where they are not yet, and a Message-ID.
fn finish(mut draft: Draft, mode: ComposeMode, settings: &Settings) -> Draft {
    if draft.sent_copy == SentCopy::Sent && settings.special_folders.get(&FolderRole::Sent) == Some(&None) {
        draft.sent_copy = SentCopy::NotSaved;
    }
    if !matches!(mode, ComposeMode::EditDraft | ComposeMode::EditAsNew) {
        draft.return_receipt = settings.mdn_default;
    }
    draft.delivery_status |= settings.dsn_default;
    if let Some(identity) = settings.identities.get(&draft.from.email.to_lowercase()) {
        for (field, add) in [(&mut draft.reply_to, &identity.reply_to), (&mut draft.bcc, &identity.bcc)] {
            for address in add {
                if !field.iter().any(|a| a.email.eq_ignore_ascii_case(&address.email)) {
                    field.push(address.clone());
                }
            }
        }
    }
    let domain = draft.from.email.rsplit_once('@').map_or("localhost", |(_, d)| d).to_owned();
    draft.message_id = draft.message_id.or_else(|| Some(format!("{}@{domain}", random_id())));
    draft
}

/// Where `reply_same_folder` files a reply's copy: the original's folder when it is there and subscribed, as
/// Roundcube's `check_sent_folder`; the Sent role as itself, so Compose's choice shows it.
fn same_folder(folders: &[Folder], name: &str) -> Option<SentCopy> {
    let folder = folders.iter().find(|f| f.name == name && f.selectable && f.subscribed)?;
    Some(if folder.roles.contains(&FolderRole::Sent) { SentCopy::Sent } else { SentCopy::Folder(folder.name.clone()) })
}

/// Roundcube's forward header block: three empty lines, then Subject, Date, From and To, each even when empty;
/// `extended` adds Cc when there is one and Reply-To when it is not From.
fn forward_header(mail: &Mail, subject: &str, date: Option<&str>, extended: bool) -> String {
    let (from, to, cc) = (render::addresses(mail.from()), render::addresses(mail.to()), render::addresses(mail.cc()));
    let reply_to = render::addresses(mail.reply_to());
    let senders = render::emails(&from);
    let reply_to = reply_to.iter().any(|r| !senders.contains(&r.email.to_ascii_lowercase())).then_some(reply_to);
    let line = |list: &[Address]| list.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    let (date, from, to) = (date.unwrap_or_default(), line(&from), line(&to));
    let mut block = format!("\n\n\n-------- Original Message --------\nSubject: {subject}\nDate: {date}\nFrom: {from}\nTo: {to}\n");
    if extended && !cc.is_empty() {
        let _ = writeln!(block, "Cc: {}", line(&cc));
    }
    if let Some(reply_to) = reply_to.filter(|_| extended) {
        let _ = writeln!(block, "Reply-To: {}", line(&reply_to));
    }
    block.push('\n');
    block
}

/// A reply's To and Cc, as Roundcube's `compose_header_value` picks them. To: the first of Mail-Followup-To (Reply
/// list, Reply all), List-Post (Reply list), Mail-Reply-To, Reply-To, From; the original To when `identity` sent
/// the original or is that choice, unless Reply-To gave it. Reply all adds To, Cc, and From when Reply-To lists
/// every To and From is not among the To (a list replying to itself).
fn reply_recipients(mail: &Mail, mode: ComposeMode, identity: &str) -> (Vec<Address>, Vec<Address>) {
    let (from, to, reply_to) = (render::addresses(mail.from()), render::addresses(mail.to()), render::addresses(mail.reply_to()));
    let followup = raw_addresses(mail, "Mail-Followup-To");
    let list = mode == ComposeMode::ReplyList;
    let post = || mail.header_raw("List-Post").and_then(list_post).filter(|_| list).map(|email| vec![Address { name: None, email }]);
    let mail_reply_to = Some(raw_addresses(mail, "Mail-Reply-To")).filter(|r| !r.is_empty());
    let has = |list: &[Address], email: &str| list.iter().any(|a| a.email.eq_ignore_ascii_case(email));
    let (mut chosen, by_reply_to) = if mode != ComposeMode::Reply && !followup.is_empty() {
        (followup, false)
    } else if let Some(post) = post() {
        (post, false)
    } else if let Some(mail_reply_to) = mail_reply_to {
        (mail_reply_to, false)
    } else if !reply_to.is_empty() {
        (reply_to.clone(), true)
    } else {
        (from.clone(), false)
    };
    if !by_reply_to && (has(&chosen, identity) || has(&from, identity)) {
        chosen.clone_from(&to); // your own mail goes to its recipients again
    }
    let mut cc = Vec::new();
    if mode == ComposeMode::ReplyAll {
        cc.extend(to.iter().cloned().chain(render::addresses(mail.cc())));
        let (replying, sent_to) = (render::emails(&reply_to), render::emails(&to));
        let lower = |a: &Address| a.email.to_ascii_lowercase();
        if !reply_to.is_empty() && to.iter().all(|a| replying.contains(&lower(a))) && from.iter().any(|a| !sent_to.contains(&lower(a))) {
            cc.extend(from);
        }
    }
    let mut seen = HashSet::new(); // each address once across To and Cc, case ignored, unlike Roundcube
    let mut once = |list: Vec<Address>, skip: &str| -> Vec<Address> {
        list.into_iter().filter(|a| !a.email.eq_ignore_ascii_case(skip) && seen.insert(a.email.to_ascii_lowercase())).collect()
    };
    (once(chosen, ""), once(cc, identity))
}

/// The files the reader lists that a forward or a reopened draft carries, as Roundcube's `write_compose_attachments`
/// (`Listed::carried`): a forwarded message's own files travel inside it.
fn carry_attachments(structure: &Structure, mail: &Mail, uploads: &Uploads) -> Result<Vec<DraftAttachment>, Error> {
    let has_html = structure::has_html(mail);
    let mut stored = Vec::new();
    for file in structure.listed.iter().filter(|l| !l.path.nested() && l.carried(has_html)) {
        match upload(uploads, &file.name, &file.content_type, &file.data) {
            Ok(attachment) => stored.push(attachment),
            // Left behind, a half-carried forward would fill the user's uploads until they expire in a month.
            Err(e) => return uploads.remove(&stored.iter().map(|a| a.id.as_str()).collect::<Vec<_>>()).and(Err(e)),
        }
    }
    Ok(stored)
}

/// Stores a file for a draft until it is sent; the name and type it gets here are the ones it is sent with.
pub fn upload(uploads: &Uploads, filename: &str, content_type: &str, data: &[u8]) -> Result<DraftAttachment, Error> {
    let filename = Some(render::safe_filename(filename)).filter(|n| !n.is_empty()).unwrap_or_else(|| "attachment".into());
    let attachment = DraftAttachment { id: random_id(), filename, content_type: render::mime_type(content_type), size: data.len() as u64 };
    if !uploads.add(&attachment, data)? {
        return Err(Error::status(StatusCode::PAYLOAD_TOO_LARGE, "your uploads are full: send or discard a draft"));
    }
    Ok(attachment)
}

pub struct Built {
    /// For SMTP: no Bcc header.
    wire: Vec<u8>,
    /// For Sent or Drafts: Bcc kept, so the user still sees it.
    pub copy: Vec<u8>,
    from: String,
    recipients: Vec<String>,
    /// Ask the submission server for delivery status notifications (RFC 3461), where it offers DSN.
    delivery_status: bool,
}

/// The draft as a message. For a Drafts copy (`send` false) an invalid address is left out, since the user may
/// still be typing it; for sending it is refused. This is where text from the client becomes header text, checked
/// once: addresses by `render::ascii_address`, ids by `valid_id`, names, file names and the subject cleaned of line
/// breaks. The From identity's Organization, from `identities`, goes out as Roundcube sends it. The files must fit
/// `limit`, the mail server's size limit, before any is read: a draft listing one upload 22,000 times asked for
/// 100 GiB of copies.
pub fn build(
    draft: &Draft, uploads: &Uploads, tz: &TimeZone, identities: &BTreeMap<String, IdentityPrefs>, send: bool, limit: Option<u64>,
) -> Result<Built, Error> {
    let ids: Vec<&str> = draft.attachments.iter().map(|a| a.id.as_str()).collect();
    if ids.iter().collect::<HashSet<_>>().len() < ids.len() {
        return Err(refused("an attachment is listed twice"));
    }
    let sizes = uploads.sizes(&ids)?;
    if sizes.len() < ids.len() {
        return Err(Error::status(StatusCode::NOT_FOUND, "no such attachment"));
    }
    let total = sizes.iter().map(|(_, size)| size).sum::<u64>();
    if let Some(limit) = limit.filter(|&limit| total > limit) {
        return Err(too_large(total, limit));
    }
    let checked = |list: &[Address]| checked_addresses(list, send);
    let from = checked(std::slice::from_ref(&draft.from))?.pop().ok_or_else(|| refused("invalid From address"))?;
    let (to, cc, bcc) = (checked(&draft.to)?, checked(&draft.cc)?, checked(&draft.bcc)?);
    let (reply_to, followup_to) = (checked(&draft.reply_to)?, checked(&draft.followup_to)?);
    let recipients = envelope(&to, &cc, &bcc);
    let domain = from.email.rsplit_once('@').expect("a checked address holds an @").1;
    // Lines end in CRLF, text/plain's canonical form, also inside base64. Sent mail is wrapped as Roundcube sends
    // it, unless Keep formatting; a Drafts copy stays as typed.
    let flowed = send && !draft.keep_formatting;
    let text = if flowed { flowed::wrap(&draft.body) } else { draft.body.lines().collect::<Vec<_>>().join("\r\n") };
    let text_type = ContentType::new("text/plain").attribute("charset", "utf-8");
    let mut message = MessageBuilder::new()
        .from(mime(std::slice::from_ref(&from)))
        .subject(render::clean(&draft.subject))
        .header("Date", Raw::new(date(tz))) // mail-builder's own is UTC
        .header("X-Sender", Raw::new(from.email.clone())) // as Roundcube's `headers_input`, also in Drafts
        .message_id(draft.message_id.clone().filter(|id| valid_id(id)).unwrap_or_else(|| format!("{}@{domain}", random_id())));
    message.text_body = Some(MimePart::new(if flowed { text_type.attribute("format", "flowed") } else { text_type }, text.as_str()));
    // As Roundcube sends it: Cc alone becomes To, and Bcc alone gets an empty group as To. Drafts keep the fields.
    let (to, cc) = if send && to.is_empty() { (cc, Vec::new()) } else { (to, cc) };
    if !to.is_empty() {
        message = message.to(mime(&to));
    } else if send && !bcc.is_empty() {
        message = message.header("To", Raw::new("undisclosed-recipients:;")); // mail-builder would quote the name
    }
    // Reply-To and Mail-Reply-To both, as Roundcube writes.
    for (name, list) in [("Cc", &cc), ("Reply-To", &reply_to), ("Mail-Reply-To", &reply_to), ("Mail-Followup-To", &followup_to)] {
        if !list.is_empty() {
            message = message.header(name, mime(list));
        }
    }
    if let Some(id) = draft.in_reply_to.as_deref().filter(|id| valid_id(id)) {
        message = message.in_reply_to(id.to_owned());
    }
    let references: Vec<String> = draft.references.iter().filter(|id| valid_id(id)).cloned().collect();
    if !references.is_empty() {
        message = message.references(MessageId::from(references));
    }
    if let Some(value) = priority_header(draft.priority) {
        message = message.header("X-Priority", Raw::new(value));
    }
    if draft.return_receipt {
        message = message.header("Disposition-Notification-To", mime(std::slice::from_ref(&from))); // with the name, as From
    }
    let organization = identities.get(&from.email.to_lowercase()).map(|i| render::clean(&i.organization));
    if let Some(organization) = organization.filter(|o| !o.is_empty()) {
        message = message.header("Organization", Text::new(organization));
    }
    if !send {
        for info in draft_info_headers(&draft.sources, draft.delivery_status) {
            message = message.header("X-Draft-Info", Raw::new(info));
        }
    }
    for attachment in &draft.attachments {
        let (stored, data) = uploads.get(&attachment.id)?.ok_or_else(|| Error::status(StatusCode::NOT_FOUND, "no such attachment"))?;
        // The draft's name, which Rename may have changed; the stored one where it cleans to nothing.
        let filename = Some(render::safe_filename(&attachment.filename)).filter(|n| !n.is_empty()).unwrap_or(stored.filename);
        // RFC 2046 allows no base64 for a nested message, which mail-builder would pick for a non-text part.
        let nested = (stored.content_type == "message/rfc822").then(|| if data.is_ascii() { "7bit" } else { "8bit" });
        let content_type = ContentType::new(stored.content_type).attribute("name", name_parameter(&filename));
        let part = MimePart::new(content_type, data).attachment(filename);
        let part = if let Some(encoding) = nested { part.transfer_encoding(encoding) } else { part };
        message.attachments.get_or_insert_with(Vec::new).push(part);
    }
    let wire = message.clone().write_to_vec()?;
    let copy = if bcc.is_empty() { wire.clone() } else { message.bcc(mime(&bcc)).write_to_vec()? };
    Ok(Built { wire, copy, from: from.email, recipients, delivery_status: draft.delivery_status })
}

/// Roundcube's bounce: the raw message unchanged, its own Bcc lines included (the dialog warns of them, as
/// Roundcube's does), with Resent-* headers on top. The wire copy leaves the new Resent-Bcc out; the filed copy
/// keeps it, so the user still sees it. `send` checks that Resent-From is the user's.
pub fn redirect(raw: &[u8], redirect: &Redirect, tz: &TimeZone) -> Result<Built, Error> {
    let checked = |list: &[Address]| checked_addresses(list, true);
    let from = checked(std::slice::from_ref(&redirect.from))?.pop().ok_or_else(|| refused("invalid From address"))?;
    let (to, cc, bcc) = (checked(&redirect.to)?, checked(&redirect.cc)?, checked(&redirect.bcc)?);
    let domain = from.email.rsplit_once('@').expect("a checked address holds an @").1;
    let mut wire = Vec::new();
    for (name, list) in [("Resent-From", std::slice::from_ref(&from)), ("Resent-To", &to), ("Resent-Cc", &cc)] {
        if !list.is_empty() {
            write_header(&mut wire, name, &mime(list));
        }
    }
    write_header(&mut wire, "Resent-Date", &Raw::new(date(tz)));
    write_header(&mut wire, "Resent-Message-ID", &MessageId::new(format!("{}@{domain}", random_id())));
    let mut copy = wire.clone();
    if !bcc.is_empty() {
        write_header(&mut copy, "Resent-Bcc", &mime(&bcc));
    }
    wire.extend_from_slice(raw);
    copy.extend_from_slice(raw);
    Ok(Built { wire, copy, from: from.email, recipients: envelope(&to, &cc, &bcc), delivery_status: false })
}

/// A file's name for Content-Type's `name`, beside the disposition's `filename` (RFC 2231), as Roundcube's
/// `Mail_mime` writes both: itself in printable ASCII, else RFC 2047 words of at most 75 characters, which readers
/// that know only `name`, such as older Outlook, decode. RFC 2047 allows no words in a quoted string, but those
/// readers expect them there.
fn name_parameter(name: &str) -> String {
    if name.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return name.to_owned();
    }
    let (mut words, mut chunk) = (Vec::new(), String::new());
    for c in name.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", BASE64.encode(&chunk))); // 45 bytes: 60 characters of base64
            chunk.clear();
        }
        chunk.push(c);
    }
    words.push(format!("=?UTF-8?B?{}?=", BASE64.encode(&chunk)));
    words.join(" ")
}

/// One header line, its value written by mail-builder's encoder for the value's type.
fn write_header(out: &mut Vec<u8>, name: &str, value: &impl Header) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    value.write_header(out, name.len() + 2);
}

/// Addresses from the client, checked once: each must pass `render::ascii_address`, names lose their line breaks.
/// An invalid one is refused when sending and left out of a Drafts copy, where the user may still be typing it.
fn checked_addresses(list: &[Address], send: bool) -> Result<Vec<Address>, Error> {
    let mut out = Vec::new();
    for address in list {
        let email = address.email.trim();
        if let Some(email) = ascii_address(email) {
            out.push(Address { name: address.name.as_deref().map(render::clean).filter(|n| !n.is_empty()), email });
        } else if send && email.split('@').next().is_some_and(|local| !local.is_ascii()) {
            return Err(refused(format!("letters beyond ASCII before the @ are not supported in the address: {}", render::clean(email))));
        } else if send {
            return Err(refused(format!("invalid address: {}", render::clean(email))));
        }
    }
    Ok(out)
}

/// The SMTP recipients: every address once, whatever its case.
fn envelope(to: &[Address], cc: &[Address], bcc: &[Address]) -> Vec<String> {
    let mut seen = HashSet::new();
    to.iter().chain(cc).chain(bcc).filter(|a| seen.insert(a.email.to_ascii_lowercase())).map(|a| a.email.clone()).collect()
}

/// mail-builder's own address type, for the header encoding it does.
fn mime(list: &[Address]) -> MimeAddress<'_> {
    MimeAddress::new_list(list.iter().map(|a| MimeAddress::new_address(a.name.as_deref(), a.email.as_str())).collect())
}

/// Now in `tz`, as a Date header writes it.
fn date(tz: &TimeZone) -> String { Timestamp::now().to_zoned(tz.clone()).strftime("%a, %d %b %Y %H:%M:%S %z").to_string() }

/// Sends a built draft or redirect over submission as the user, with the Mailu token as the password.
pub async fn send(server: &MailServer, me: &Identity<'_>, token: &str, built: &Built) -> Result<(), Error> {
    if !me.owns(&built.from) {
        return Err(refused("the From address is not one of yours"));
    }
    if built.recipients.is_empty() {
        return Err(refused("no recipients"));
    }
    let session = async {
        let (mut plain, capabilities) = greet(server).await?;
        if !capabilities.iter().any(|line| line.eq_ignore_ascii_case("STARTTLS")) {
            return Err(smtp_error("no STARTTLS offered"));
        }
        command(&mut plain, "STARTTLS", &[220]).await?;
        if !plain.buffer().is_empty() {
            return Err(smtp_error("data before the TLS handshake")); // STARTTLS command injection
        }
        let tls = tokio_rustls::TlsConnector::from(Arc::clone(&server.tls)).connect(server.tls_name.clone(), plain.into_inner()).await?;
        let stream = &mut BufReader::new(tls);
        let capabilities = command(stream, "EHLO kubicle", &[250]).await?;
        server.size_limit.store(size_limit_of(&capabilities), Ordering::Relaxed);
        if let Some(limit) = server.size_limit()
            && built.wire.len() as u64 > limit
        {
            return Err(too_large(built.wire.len() as u64, limit));
        }
        let dsn = built.delivery_status && capabilities.iter().any(|line| line.eq_ignore_ascii_case("DSN"));
        let credentials = Zeroizing::new(format!("\0{}\0{token}", me.address));
        let auth = Zeroizing::new(format!("AUTH PLAIN {}", BASE64.encode(credentials.as_bytes())));
        command(stream, &auth, &[235]).await.map_err(|_| smtp_error("authentication failed"))?;
        let (ret, notify) = if dsn { (" RET=HDRS", " NOTIFY=SUCCESS,FAILURE") } else { ("", "") }; // as Roundcube's rcube_smtp
        command(stream, &format!("MAIL FROM:<{}>{ret}", built.from), &[250]).await?;
        for recipient in &built.recipients {
            command(stream, &format!("RCPT TO:<{recipient}>{notify}"), &[250, 251]).await?;
        }
        command(stream, "DATA", &[354]).await?;
        stream.write_all(&dot_stuff(&built.wire)).await?;
        command(stream, ".", &[250]).await?;
        let _ = command(stream, "QUIT", &[221]).await;
        Ok(())
    };
    smtp_session(session).await
}

/// The From for a reply or forward, as Roundcube's `identity_select` picks it from `identities` (the account
/// first): the first the original was sent to (To, Cc); for a reply, the one it came from (the user's own mail);
/// the one in its Return-Path, also in a list's VERP form (`-local=domain@`); else the account itself.
fn reply_from(identities: &[String], mail: &Mail, mode: ComposeMode) -> String {
    let recipients: Vec<String> = render::addresses(mail.to()).into_iter().chain(render::addresses(mail.cc())).map(|a| a.email).collect();
    let from = render::addresses(mail.from()).into_iter().next().map(|a| a.email);
    let reply = matches!(mode, ComposeMode::Reply | ComposeMode::ReplyAll | ComposeMode::ReplyList);
    let return_path = mail.header_raw("Return-Path").unwrap_or_default().trim().to_ascii_lowercase();
    let in_return_path = |identity: &String| {
        let identity = identity.to_ascii_lowercase();
        return_path == format!("<{identity}>") || return_path.contains(&format!("-{}@", identity.replace('@', "=")))
    };
    let is = |identity: &String, email: &String| identity.eq_ignore_ascii_case(email);
    let found = identities.iter().find(|i| recipients.iter().any(|r| is(i, r)));
    let found = found.or_else(|| identities.iter().find(|i| reply && from.as_ref().is_some_and(|f| is(i, f))));
    found.or_else(|| identities.iter().find(|i| in_return_path(i))).or(identities.first()).cloned().unwrap_or_default()
}

/// The original's References and Message-ID, trimmed to 20 IDs keeping the thread's root.
fn references(mail: &Mail) -> Vec<String> {
    let mut refs: Vec<String> = ids(mail.references()).map(str::to_owned).collect();
    if refs.is_empty() {
        refs.extend(ids(mail.in_reply_to()).next().map(str::to_owned));
    }
    refs.extend(mail.message_id().map(str::to_owned));
    if refs.len() > 20 {
        refs.drain(1..refs.len() - 19);
    }
    refs
}

fn ids<'a>(value: &'a HeaderValue) -> impl Iterator<Item = &'a str> {
    value.as_text_list().unwrap_or_default().iter().map(AsRef::as_ref) // one ID or several
}

/// The original's text as Roundcube's plain-text Compose takes it (`compose_part_body`): the parts the reader
/// shows, but not a forwarded message's, each as `render::content_text` reads it, a plain one's signature cut when
/// `strip_signature`. An HTML or enriched part keeps its signature: Roundcube converts it
/// with `html2text` alone. The parts join with a line break.
fn source_text(structure: &Structure, strip_signature: bool) -> String {
    let own = structure.contents.iter().filter_map(|c| match c {
        Content::Part { node, .. } if !node.path.nested() => Some((node.mimetype(), c)),
        Content::Part { .. } | Content::Headers(_) | Content::Encrypted => None,
    });
    let texts = own.filter_map(|(kind, c)| Some((kind, render::content_text(c)?))).map(|(kind, text)| {
        let plain = !matches!(kind.as_str(), "text/html" | "text/enriched");
        if strip_signature && plain { remove_signature(&text) } else { text }
    });
    texts.filter(|text| !text.trim().is_empty()).collect::<Vec<_>>().join("\n")
}

/// Roundcube's `remove_signature`: the last `-- ` line that starts a line goes, with everything after it, when
/// fewer than 15 line breaks follow it. A longer tail is not a signature, as a stray `-- ` in a body shows.
fn remove_signature(text: &str) -> String {
    let mut end = text.len();
    while let Some(at) = text[..end].rfind("-- \n") {
        if at == 0 || text.as_bytes()[at - 1] == b'\n' {
            let short = text[at..].matches('\n').count() < 15;
            return if short { text[..at.saturating_sub(1)].to_owned() } else { text.to_owned() };
        }
        end = at + 3; // the next match must start before this one
    }
    text.to_owned()
}

/// Quote marks before each line of `text`, its leading and trailing empty lines left out, as Roundcube's
/// `quote_text`; an empty line gets `>` alone, which `flowed::wrap` keeps from joining the paragraphs.
fn quote(text: &str) -> String {
    let lines = text.trim_start_matches('\n').trim_end().split('\n');
    let quoted = lines.map(|line| if line.is_empty() || line.starts_with('>') { format!(">{line}") } else { format!("> {line}") });
    quoted.collect::<Vec<_>>().join("\n")
}

/// Roundcube's reply subject: a trailing "(was: ...)", which a renamed thread carries, goes.
fn strip_was(subject: &str) -> Cow<'_, str> {
    static WAS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\s)*\([wW]as:[^)]+\)(?-u:\s)*$").expect("a valid pattern"));
    WAS.replace(subject, "")
}

/// Roundcube's `X-Priority` values; Normal sends none.
fn priority_header(priority: Priority) -> Option<&'static str> {
    match priority {
        Priority::Highest => Some("1 (Highest)"),
        Priority::High => Some("2 (High)"),
        Priority::Normal => None,
        Priority::Low => Some("4 (Low)"),
        Priority::Lowest => Some("5 (Lowest)"),
    }
}

/// Roundcube's `X-Draft-Info`, so drafts move between the two with their reply or forward and the delivery status
/// tick intact. Several forwarded messages go as Roundcube writes them, a UID set (`uid=1:3,7`); one header per
/// folder, as Roundcube's names one folder, the delivery status in the first.
fn draft_info_headers(sources: &[SourceRef], dsn: bool) -> Vec<String> {
    let mut folders: Vec<(&str, &str, Vec<u32>)> = Vec::new();
    for source in sources {
        let kind = match source.mode {
            ComposeMode::Reply | ComposeMode::ReplyAll | ComposeMode::ReplyList => "reply",
            ComposeMode::Forward | ComposeMode::ForwardAttachment => "forward",
            ComposeMode::New | ComposeMode::EditDraft | ComposeMode::EditAsNew => continue,
        };
        match folders.iter_mut().find(|(f, _, _)| *f == source.folder) {
            Some((_, _, uids)) => uids.push(source.uid),
            None => folders.push((&source.folder, kind, vec![source.uid])),
        }
    }
    let mut headers = Vec::new();
    for (folder, kind, uids) in folders {
        headers.push(format!("type={kind}; uid={}; folder=B::{}", kubicle_api::uid_set(&Messages::Uids(uids)), BASE64.encode(folder)));
    }
    match headers.first_mut() {
        Some(first) if dsn => first.push_str("; dsn=on"),
        None if dsn => headers.push("dsn=on".into()),
        _ => {}
    }
    headers
}

/// Every `X-Draft-Info` of a draft, kubicle's or Roundcube's: the messages it answers or forwards, and whether it
/// asks for delivery status notifications.
fn draft_info(mail: &Mail) -> (Vec<SourceRef>, bool) {
    let (mut sources, mut dsn) = (Vec::new(), false);
    for (_, value) in mail.headers_raw().filter(|(name, _)| name.eq_ignore_ascii_case("X-Draft-Info")) {
        let (mut mode, mut uids, mut folder) = (None, Vec::new(), None);
        for (key, value) in value.split(';').filter_map(|field| field.trim().split_once('=')) {
            match (key, value.trim()) {
                ("type", "reply") => mode = Some(ComposeMode::Reply),
                ("type", "forward") => mode = Some(ComposeMode::Forward),
                ("uid", value) => {
                    // `*`, or a set past `parse_uid_set`'s limit, names no UID here.
                    uids = if let Some(Messages::Uids(found)) = kubicle_api::parse_uid_set(value) { found } else { Vec::new() };
                }
                ("folder", value) =>
                    folder = BASE64.decode(value.strip_prefix("B::").unwrap_or(value)).ok().and_then(|v| String::from_utf8(v).ok()),
                ("dsn", "on") => dsn = true,
                _ => {}
            }
        }
        if let (Some(mode), Some(folder)) = (mode, folder) {
            sources.extend(uids.into_iter().map(|uid| SourceRef { folder: folder.clone(), uid, mode }));
        }
    }
    (sources, dsn)
}

/// A Message-ID without its angle brackets: printable ASCII, no space or bracket.
fn valid_id(id: &str) -> bool { !id.is_empty() && id.len() <= 250 && id.bytes().all(|b| b.is_ascii_graphic() && b != b'<' && b != b'>') }

/// Sends one command line and reads the reply. Only kubicle builds `line`, from validated parts.
async fn command<S>(stream: &mut BufReader<S>, line: &str, expect: &[u16]) -> Result<Vec<String>, Error>
where S: AsyncRead + AsyncWrite + Unpin {
    let wire = Zeroizing::new([line.as_bytes(), b"\r\n"].concat()); // one TLS record and segment; AUTH's line holds the token
    stream.write_all(&wire).await?;
    stream.flush().await?;
    reply(stream, expect).await
}

/// Reads one reply, multi-line or not, and returns its texts when the code is expected.
async fn reply<S: AsyncBufRead + Unpin>(stream: &mut S, expect: &[u16]) -> Result<Vec<String>, Error> {
    let mut lines = Vec::new();
    for _ in 0..100 {
        let mut line = String::new();
        if (&mut *stream).take(4096).read_line(&mut line).await? == 0 {
            return Err(smtp_error("connection closed"));
        }
        let line = line.trim_end();
        let code = line.get(..3).and_then(|c| c.parse::<u16>().ok()).ok_or_else(|| smtp_error("malformed reply"))?;
        lines.push(line.get(4..).unwrap_or_default().to_owned());
        if line.as_bytes().get(3) != Some(&b'-') {
            return if expect.contains(&code) { Ok(lines) } else { Err(smtp_error(render::clean(line))) };
        }
    }
    Err(smtp_error("reply too long"))
}

/// RFC 5321 framing: every line ends in CRLF, a bare CR or LF too, which 2.3.8 forbids a client to send and a
/// server may read as a line end (SMTP smuggling: a redirected mail's bytes are the sender's); a line starting with
/// `.` gets a second one. The last line ends in CRLF as well, so the `.` that ends DATA stands on a line of its own.
fn dot_stuff(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 64);
    let mut rest = message;
    while let Some(&first) = rest.first() {
        let end = rest.iter().position(|&b| b == b'\r' || b == b'\n').unwrap_or(rest.len());
        if first == b'.' {
            out.push(b'.');
        }
        out.extend_from_slice(&rest[..end]);
        out.extend_from_slice(b"\r\n");
        let after = &rest[end..];
        let crs = after.iter().take_while(|&&b| b == b'\r').count(); // CR CR LF, a doubled conversion, is one line end
        rest = if after.get(crs) == Some(&b'\n') { &after[crs + 1..] } else { after.get(1..).unwrap_or_default() };
    }
    out
}

/// Roundcube's autocomplete sources that contain `text`: the addresses the user wrote to (To, Cc and Bcc of `sent`,
/// the mail in Sent: Roundcube's collected recipients) and `trusted` (its trusted senders). Most used first, then
/// the latest, each with the display name of the latest mail that gave one; a trusted sender never written to last.
#[must_use]
pub fn suggestions(sent: &[Fetched], text: &str, trusted: &[String], limit: usize) -> Vec<Address> {
    let needle = text.to_lowercase();
    let mut seen: HashMap<String, (Address, u32, i64)> = HashMap::new();
    for h in sent {
        for address in h.to.iter().chain(&h.cc).chain(&h.bcc) {
            let entry = seen.entry(address.email.to_lowercase()).or_insert_with(|| (address.clone(), 0, h.date));
            entry.1 += 1;
            if h.date >= entry.2 {
                entry.2 = h.date;
                if address.name.is_some() {
                    entry.0.name.clone_from(&address.name);
                }
            }
        }
    }
    for email in trusted {
        seen.entry(email.to_lowercase()).or_insert_with(|| (Address { name: None, email: email.clone() }, 0, i64::MIN));
    }
    let contains = |text: &str| text.to_lowercase().contains(&needle);
    let mut found: Vec<_> = seen.into_values().filter(|(a, _, _)| contains(&a.email) || a.name.as_deref().is_some_and(contains)).collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
    found.into_iter().take(limit).map(|(address, _, _)| address).collect()
}

/// `subject` without leading reply or forward prefixes, each of `prefixes` with an optional counter (`Re:`,
/// `Re[2]:`, `Re-2:`), and with `tags` also without leading `[list]` tags: RFC 5256's base subject for sorting.
pub(crate) fn base_subject<'a>(subject: &'a str, prefixes: &[&str], tags: bool) -> &'a str {
    let mut rest = subject.trim();
    loop {
        let refwd = prefixes.iter().find_map(|prefix| {
            let tail = rest.get(..prefix.len()).filter(|head| head.eq_ignore_ascii_case(prefix)).map(|_| &rest[prefix.len()..])?;
            let tail = match tail.strip_prefix('[') {
                Some(blob) => &blob[blob.find(']')? + 1..],
                None => tail.strip_prefix('-').map_or(tail, |n| n.trim_start_matches(|c: char| c.is_ascii_digit())),
            };
            tail.strip_prefix(':')
        });
        let tag = rest.strip_prefix('[').filter(|_| tags).and_then(|inner| Some(&inner[inner.find(']')? + 1..]));
        match refwd.or(tag.filter(|after| !after.trim().is_empty())) {
            Some(after) => rest = after.trim_start(),
            None => return rest,
        }
    }
}
