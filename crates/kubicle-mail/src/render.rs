//! Raw RFC 822 bytes to the `Message` the client shows: headers, a body and the attachment list.
//!
//! HTML mail becomes a complete document for the `srcdoc` of an iframe with
//! `sandbox="allow-popups allow-popups-to-escape-sandbox"` and nothing more: never `allow-scripts` or
//! `allow-same-origin`. That sandbox and the CSP built here stop scripts; ammonia is required too, as only it
//! stops the DNS lookups, preconnects and relative requests they let through (docs/design.md, Security).
//!
//! @NOTE: the frame also inherits the app page's CSP header, which must allow `style-src 'unsafe-inline'`
//!    and keep `img-src` to `'self' data:`. Firefox races `srcset` loads in a sandboxed srcdoc past the meta
//!    CSP below, so with the sanitizer off only that header stops them.                             - Oct 25, 2026

use std::{borrow::Cow, collections::HashMap, collections::HashSet, fmt::Write as _, sync::Arc, sync::LazyLock, sync::atomic::AtomicBool};
use std::{cell::Cell, rc::Rc, sync::atomic::AtomicU32, sync::atomic::Ordering};

use axum::{body::Body, http::HeaderValue, http::Response, http::header};
use html2text::render::RichAnnotation;
use jiff::{Timestamp, tz::TimeZone};
use kubicle_api::{Address, Attachment, Authenticity, Body as MailBody, Header, Message, Priority, Spoofed};
use mail_parser::{MessageParser, MessagePart, MimeHeaders, PartType};
use percent_encoding::percent_decode_str;
use regex::Regex;
use unicode_script::Script;
use unicode_security::mixed_script::AugmentedScriptSet;

use crate::{Error, flowed, links, links::Links, links::PartRoute, links::Section, plain, structure, structure::Content, structure::Listed};
use crate::{structure::Node, structure::PartPath, structure::Structure};

/// What a blocked remote image shows instead: a pale SVG with no size of its own, so its box takes the image's
/// `width` and `height` attributes, and under `height: auto` their ratio, as the loaded image would. Where the mail
/// declares one of the two, `PAPER` makes it square up to half the frame's width tall: icons and avatars square, a
/// wide banner near 2:1. One that declares no size, in its attributes or its style, takes no space: most are
/// tracking pixels. A dropped source would draw a broken-image icon.
macro_rules! blocked_image {
    () => {
        "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%3E%3Crect%20width='100%25'%20height='100%25'%20fill='%23e8eaed'/%3E%3C/svg%3E"
    };
}
const BLOCKED_IMAGE: &str = blocked_image!();

/// The base style under the sender's own: HTML mail on white paper in every theme, text mail on the theme's
/// colours the client passes (theme.rs, `paper`). The paper is a wrapper as tall as the mail, since a background
/// on html or body would paint the whole frame, and the iframe takes the document's `color-scheme`, else browsers
/// paint an opaque backdrop. The rest styles what kubicle draws.
const PAPER: &str = concat!(
    "html{color-scheme:light}\
body{margin:0;font:14px/1.5 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;overflow-wrap:break-word}\
#kubicle-paper{background:var(--bg,#fff);color:var(--fg,#1f2328);padding:16px}img{max-width:100%}pre{white-space:pre-wrap}\
.pre{font:13px/1.5 var(--font-mono,monospace)}.pre .line{white-space:pre-wrap}.pre .nowrap{white-space:pre}\
.pre .sig{color:var(--fg-muted,#737677)}.pre a{color:var(--link,LinkText)}\
.pre blockquote{margin:2px 0;padding:0 .4em;color:var(--quote-1,#006a9d);border:solid;border-width:0 2px;background:var(--hover,rgb(0 0 0/3%))}\
.pre blockquote blockquote{color:var(--quote-2,#1f5a24)}.pre blockquote blockquote blockquote{color:var(--quote-3,#eb0400)}\
.headers-table{font-size:90%;color:#5f6368;margin:8px 0}.headers-table th{text-align:left;vertical-align:top;padding-right:1rem;white-space:nowrap}\
.part-notice{color:#5f6368;font-style:italic}.print-head .headers-table{font-size:13px;color:inherit;margin:0 0 12px}",
    "img[src=\"",
    blocked_image!(),
    "\"]:not([width][height]){aspect-ratio:1;max-height:50vw}img[src=\"",
    blocked_image!(),
    "\"]:not([width]):not([height]):not([style*=width i]):not([style*=height i]){display:none}"
);
const PAPER_PADDING: u32 = 16;
/// The largest part shown in a document: the sanitizer's tree costs about 35 times an HTML part's size, in the one
/// process every user shares. A larger part gets a notice and its download link instead.
const MAX_SHOWN: usize = 2 << 20;
/// Image sources rewritten in one part; past it, dropped. Each is a signed URL or the blocked SVG, 200 bytes or so
/// for an 18-byte `<img src=x>`: 2 MB of those made an 18 MB document and a 109 MB peak.
const MAX_IMAGES: u32 = 1000;
/// Delimiter lines a message may have, an upper bound on its parts: generous, so a text dump of `--` lines opens.
const MAX_PARTS: usize = 50_000;
/// Messages a message may name inside it: forwards nest a few deep, and dropping nesting costs a stack frame a level.
const MAX_MESSAGES: usize = 1000;
/// Text/enriched excerpts converted per part, which quotes in real mail nest a few deep, and spans of one styling
/// parameter: each pass over the part costs its whole length.
const MAX_EXCERPTS: usize = 16;
const MAX_SPANS: usize = 256;
/// A char-op budget for the enriched span rewriting, which rebuilds the whole body each pass: the pass cap is this
/// over the body length, so cost stays near it whatever the size. 50M is about 0.1 s; real enriched is small.
const ENRICHED_WORK: usize = 50_000_000;
/// The deepest element nesting the HTML parser is given: each start tag costs it a walk up the open elements, so
/// 80,000 nested `<div>`s took 7.6 s. Real mail nests a few dozen deep.
const MAX_HTML_DEPTH: usize = 256;
/// Roundcube's words where an encrypted part stands.
const ENCRYPTED: &str = "This is an encrypted message and can not be displayed. Sorry!";

/// A message parsed once per request. Signed URLs name its parts by mail-parser's part ids (`PartPath`), which are
/// stable: the same bytes always parse to the same ids.
pub type Mail<'a> = mail_parser::Message<'a>;

/// Bounded before mail-parser reads anything, as what it builds costs most: a part costs the parse, the reader's list
/// and its JSON 1.5 KB (160,000 in 6 MB peaked at 307 MB), and a nested message 4 KB and a stack frame to drop
/// (100,000 in 3 MB peaked at 393 MB, and 90,000 aborted the process). Each part starts at a line of `--`, and each
/// unencoded nested message names its type, so counting those bounds both.
pub fn parse(raw: &[u8]) -> Result<Mail<'_>, Error> {
    if raw.split(|&b| b == b'\n').filter(|line| line.starts_with(b"--")).count() > MAX_PARTS {
        return Err(Error::TooManyParts);
    }
    let named = |w: &[u8]| w.eq_ignore_ascii_case(b"message/rfc822") || w.eq_ignore_ascii_case(b"message/global");
    if raw.windows(14).filter(|w| named(w)).count() > MAX_MESSAGES {
        return Err(Error::TooManyMessages);
    }
    MessageParser::default().parse(raw).ok_or(Error::Empty)
}

/// The header only, the body left unparsed: all Forward as attachment needs from a message it attaches whole. A
/// full parse would decode every part, another 30 MB for a 30 MB message.
pub fn parse_headers(raw: &[u8]) -> Result<Mail<'_>, Error> { MessageParser::default().parse_headers(raw).ok_or(Error::NoHeaders) }

/// A message as an import stores it: each top-level `Authentication-Results`, found as `authenticity` finds them,
/// renamed `X-Imported-Authentication-Results`, so a file cannot bring a verdict this server never gave.
#[must_use]
pub fn imported(raw: &[u8]) -> Vec<u8> {
    let named = |h: &&mail_parser::Header| h.name().eq_ignore_ascii_case("Authentication-Results");
    let at: Vec<usize> =
        parse_headers(raw).map(|m| m.headers().iter().filter(named).map(|h| h.offset_field as usize).collect()).unwrap_or_default();
    let (mut out, mut from) = (Vec::with_capacity(raw.len() + 11 * at.len()), 0);
    for start in at {
        out.extend_from_slice(&raw[from..start]);
        out.extend_from_slice(b"X-Imported-");
        from = start;
    }
    out.extend_from_slice(&raw[from..]);
    out
}

/// The decoded bytes of a part; a nested message as it appears in the mail.
pub(crate) fn bytes<'a>(mail: &'a Mail, part: &'a MessagePart) -> &'a [u8] {
    let whole = matches!(part.body, PartType::Message(_)) && part.encoding == mail_parser::Encoding::None;
    if whole { mail.raw_message.get(part.offset_body as usize..part.offset_end as usize).unwrap_or_default() } else { part.contents() }
}

/// HTML as text, the way both the reader and a reply's quote show it, in Roundcube's `rcube_html2text` form: a
/// table's cells each on a line of their own, images left out, a link's text followed by ` [n]` and its address
/// under "Links:" at the end, and at most one empty line in a row. Lines stay whole, so no address breaks: the
/// reader wraps them on screen. Laid out as columns, a table padded every row to its widest cell: 2,000 short rows
/// under one 10,000-character cell made 20 MB of text from 68 KB of mail.
#[must_use]
fn html_text(html: &str) -> String {
    if too_complex(html) {
        return TOO_DEEP.to_owned();
    }
    let config = html2text::config::rich().raw_mode(true).no_link_wrapping();
    let Ok(rendered) = config.lines_from_read(html.as_bytes(), 10_000) else { return String::new() };
    let (mut out, mut links) = (String::new(), Vec::<String>::new());
    // As Roundcube: no number for a mailto:, javascript: or # link, nor for one whose text is its address.
    let mut number = |url: &str, shown: &str| {
        if url == shown.trim() || ["mailto:", "javascript:", "#"].iter().any(|p| url.to_ascii_lowercase().starts_with(p)) {
            return String::new();
        }
        let n = links.iter().position(|l| l == url).unwrap_or_else(|| {
            links.push(url.to_owned());
            links.len() - 1
        });
        format!(" [{}]", n + 1)
    };
    for line in &rendered {
        let mut text = String::new();
        let mut open: Option<(&str, String)> = None; // the link this line is in, and its text so far
        for piece in line.tagged_strings().filter(|piece| !piece.tag.iter().any(|a| matches!(a, RichAnnotation::Image(_)))) {
            let link = piece.tag.iter().find_map(|a| if let RichAnnotation::Link(url) = a { Some(url.as_str()) } else { None });
            if open.as_ref().map(|(url, _)| *url) != link {
                text += &open.take().map(|(url, shown)| number(url, &shown)).unwrap_or_default();
                open = link.map(|url| (url, String::new()));
            }
            if let Some((_, shown)) = &mut open {
                shown.push_str(&piece.s);
            }
            text.push_str(&piece.s);
        }
        text += &open.map(|(url, shown)| number(url, &shown)).unwrap_or_default(); // a `<br>` in a link ends it here
        if !text.trim_end().is_empty() || !(out.is_empty() || out.ends_with("\n\n")) {
            out += text.trim_end();
            out.push('\n');
        }
    }
    let mut out = out.trim_end().to_owned();
    out.push_str(if links.is_empty() { "" } else { "\n\nLinks:\n------" });
    for (url, n) in links.iter().zip(1..) {
        let _ = write!(out, "\n[{n}] {url}");
    }
    out
}

/// How a message shows in this session: as text or HTML, with remote images or without, and what our server found
/// about its sender.
#[derive(Debug, Clone, Default)]
pub struct Shown {
    /// HTML shows as plain text.
    pub text: bool,
    /// Without it remote images are left out, and `remote_blocked` says so.
    pub remote_images: bool,
    pub authenticity: Option<Authenticity>,
}

/// The message as the reader shows it. `arrival` (IMAP's INTERNALDATE) stands in where mail-parser reads no valid
/// Date.
#[must_use]
pub fn message(mail: &Mail, folder: &str, uid: u32, arrival: i64, links: &impl Links, shown: Shown) -> Message {
    let structure = Structure::of(mail, !shown.text);
    let date = mail.date().filter(|d| d.is_valid()).map_or(arrival, mail_parser::DateTime::to_timestamp);
    let mut headers = header_rows(mail, Some(date));
    headers.retain(|h| !matches!(h, Header::Subject(_))); // the subject heads the reader and print, above the table
    let nested: Vec<Header> =
        structure.contents.iter().flat_map(|c| if let Content::Headers(n) = c { header_rows(n, None) } else { Vec::new() }).collect();
    let suspicious = headers.iter().chain(&nested).filter_map(Header::addresses).flatten().any(|a| look_alike(&a.email));
    Message {
        folder: folder.to_owned(),
        uid,
        date,
        from: addresses(mail.from()),
        to: addresses(mail.to()),
        cc: addresses(mail.cc()),
        subject: mail.subject().map(clean).unwrap_or_default(),
        body: body(&structure, links, shown.remote_images, shown.text),
        headers,
        has_html: structure::has_html(mail),
        suspicious,
        list_post: mail.header_raw("List-Post").and_then(list_post).is_some(),
        bcc: ["Bcc", "Resent-Bcc"].into_iter().any(|h| mail.header_raw(h).is_some()),
        attachments: listed(&structure, links),
        authenticity: shown.authenticity,
    }
}

/// The attachment list: every listed part except images only the body shows (Roundcube's `message_attachments`).
/// Images get a preview; text, HTML and forwarded messages a page of their own.
fn listed(structure: &Structure, links: &impl Links) -> Vec<Attachment> {
    let shown = structure.listed.iter().filter(|l| !(l.layout_image && l.content_type.starts_with("image/")));
    // Roundcube's display name for a signature part (`attachment_name`); its download keeps its own name.
    let signature = |t: &str| matches!(t, "application/pgp-signature" | "application/pkcs7-signature");
    let viewable = |t: &str| matches!(t, "text/plain" | "text/html" | "message/rfc822");
    let attachment = |l: &Listed| Attachment {
        filename: if signature(&l.content_type) { "Digital Signature".to_owned() } else { l.name.clone() },
        size: l.data.len() as u64,
        url: links.part(PartRoute::Download, &l.path),
        preview_url: structure::image_type(&l.content_type, &l.name).map(|_| links.inline(&l.path, l.section.as_ref())),
        view_url: viewable(&l.content_type).then(|| links.part(PartRoute::View, &l.path)),
    };
    shown.map(attachment).collect()
}

/// Roundcube's header table (`message_headers`) for a message or a forwarded one, a row only where there is a
/// value. `date` replaces the top message's Date, which falls back to its arrival.
#[must_use]
fn header_rows(mail: &Mail, date: Option<i64>) -> Vec<Header> {
    let (from, reply_to) = (addresses(mail.from()), addresses(mail.reply_to()));
    let same = |a: &[Address], b: &[Address]| a.len() == b.len() && { emails(b).is_superset(&emails(a)) };
    let date = date.or_else(|| mail.date().filter(|d| d.is_valid()).map(mail_parser::DateTime::to_timestamp));
    let subject = mail.subject().map(clean).unwrap_or_default();
    let mut rows = Vec::new();
    rows.extend((!subject.is_empty()).then_some(Header::Subject(subject)));
    rows.extend((!from.is_empty()).then(|| Header::From(from.clone())));
    rows.extend(Some(addresses(mail.sender())).filter(|s| !s.is_empty() && !same(s, &from)).map(Header::Sender));
    rows.extend(Some(addresses(mail.to())).filter(|a| !a.is_empty()).map(Header::To));
    rows.extend(Some(addresses(mail.cc())).filter(|a| !a.is_empty()).map(Header::Cc));
    rows.extend(Some(addresses(mail.bcc())).filter(|a| !a.is_empty()).map(Header::Bcc));
    rows.extend(Some(reply_to.clone()).filter(|r| !r.is_empty() && !same(r, &from)).map(Header::ReplyTo));
    let mail_reply_to = Some(raw_addresses(mail, "Mail-Reply-To")).filter(|m| !m.is_empty() && !same(m, &reply_to) && !same(m, &from));
    rows.extend(mail_reply_to.map(Header::MailReplyTo));
    rows.extend(Some(raw_addresses(mail, "Mail-Followup-To")).filter(|a| !a.is_empty()).map(Header::MailFollowupTo));
    rows.extend(date.map(Header::Date));
    rows.extend(priority(mail).map(Header::Priority));
    rows
}

/// Whether an address's domain mixes scripts as a look-alike does: Roundcube asks ICU's spoof checker at its
/// highly restrictive level (`rcube_spoofchecker`), which allows one script, or Latin with the Chinese, Japanese or
/// Korean scripts, by each character's Unicode script extensions (UTS 39, 5.2). A punycode label is read in
/// Unicode first. ICU's other checks (invisible marks, mixed digits) are not repeated.
fn look_alike(email: &str) -> bool {
    let Some((_, domain)) = email.rsplit_once('@') else { return false };
    let punycode = domain.split('.').any(|l| l.len() > 4 && l.as_bytes()[..4].eq_ignore_ascii_case(b"xn--"));
    let domain = if punycode { url::quirks::domain_to_unicode(domain) } else { domain.to_owned() };
    // The scripts every character shares, and those the characters outside Latin share: ICU's steps without its
    // identifier profile, which ICU's default leaves open to every character.
    let (mut all, mut beside_latin) = (AugmentedScriptSet::default(), AugmentedScriptSet::default());
    for set in domain.chars().map(AugmentedScriptSet::for_char) {
        all.intersect_with(set);
        if !set.base.contains_script(Script::Latin) {
            beside_latin.intersect_with(set);
        }
    }
    all.is_empty() && !(beside_latin.hanb || beside_latin.jpan || beside_latin.kore)
}

/// The verdict our mail server stamped on arrival: the topmost Authentication-Results whose authserv-id is `server`.
/// Our border MTA adds its own above every header already there, so one below it may be the sender's, our id copied
/// in (RFC 8601, sections 5 and 7.1). `owns` says whether an address is on one of the user's own domains.
pub fn authenticity(mail: &Mail, server: Option<&str>, owns: impl Fn(&str) -> bool) -> Option<Authenticity> {
    let server = server?;
    let (ours, clear) = mail.headers().iter().filter(|h| h.name().eq_ignore_ascii_case("Authentication-Results")).find_map(|h| {
        // Lossy, never skipped: a header passed over for a byte that is not UTF-8 would let the one below it count.
        let raw = mail.raw_message.get(h.offset_start as usize..h.offset_end as usize).unwrap_or_default();
        let (results, clear) = resinfos(&String::from_utf8_lossy(raw));
        matches!(results[0].tokens.first(), Some(Token::Word(id)) if id.eq_ignore_ascii_case(server)).then_some((results, clear))
    })?;
    if !clear {
        return Some(Authenticity { signed_by: Vec::new(), mailed_by: None, warning: Some(Spoofed::Unclear), from_verified: false });
    }
    let domain = |email: &str| email.rsplit_once('@').map_or(email, |(_, d)| d).to_owned();
    // `signed_in`: the envelope sender of a signed-in sender, which Postfix ties to its login.
    let (mut signed_by, mut mailed_by, mut dmarc, mut signed_in) = (Vec::new(), None, None, None);
    let mut signers = HashSet::new(); // each domain once, in order
    for result in &ours[1..] {
        // `method=result`, then `ptype.property=value`; rspamd puts the DMARC policy in a comment.
        let [Token::Word(method), Token::Equals, Token::Word(outcome), rest @ ..] = result.tokens.as_slice() else { continue };
        let prop = |name: &str| {
            rest.windows(3).find_map(|w| match w {
                [Token::Word(key), Token::Equals, Token::Word(value)] if key.eq_ignore_ascii_case(name) => Some(value.to_ascii_lowercase()),
                _ => None,
            })
        };
        match (method.to_ascii_lowercase().as_str(), outcome.to_ascii_lowercase().as_str()) {
            ("dkim", "pass") => signed_by.extend(prop("header.d").filter(|d| signers.insert(d.clone()))),
            ("spf", "pass") => mailed_by = prop("smtp.mailfrom").map(|m| domain(&m)),
            ("dmarc", outcome) => {
                let policy = result.comments.iter().find_map(|c| c.split_once("policy="));
                let policy = policy.map(|(_, p)| p.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or(""));
                // With the domain it judged: a pass counts only for the From shown, which another parser can read
                // otherwise, as of a From with two addresses.
                dmarc = Some((outcome == "pass", policy.is_some_and(|p| ["quarantine", "reject"].contains(&p)), prop("header.from")));
            }
            ("auth", "pass") => signed_in = prop("smtp.mailfrom"),
            _ => {}
        }
    }
    let from = addresses(mail.from()).into_iter().next().map(|a| a.email.to_ascii_lowercase());
    // A signature of the From domain, or of a domain above it, as DMARC's relaxed alignment takes one.
    let aligned = |from: &String| signed_by.iter().any(|d| domain(from) == *d || domain(from).ends_with(&format!(".{d}")));
    let dmarc_pass =
        dmarc.as_ref().is_some_and(|(pass, _, judged)| *pass && from.as_ref().is_some_and(|f| judged.as_deref() == Some(&*domain(f))));
    let from_verified = dmarc_pass || from.as_ref().is_some_and(aligned);
    let warning = match (dmarc, from) {
        (Some((false, true, _)), Some(from)) => Some(Spoofed::Dmarc(domain(&from))),
        // Any account may sign in and write any From: only the envelope sender is its own (Mailu's sender check).
        (_, Some(from)) if owns(&from) && !dmarc_pass => match signed_in {
            Some(sender) if sender == from => None,
            Some(sender) => Some(Spoofed::Account(sender)),
            None => Some(Spoofed::Own(domain(&from))),
        },
        _ => None,
    };
    Some(Authenticity { signed_by, mailed_by, warning, from_verified })
}

/// A word of an Authentication-Results header, or the `=` between a name and its value.
enum Token {
    Word(String),
    Equals,
}

/// One part of an Authentication-Results header between its `;`s: the authserv-id first, then each method's result.
#[derive(Default)]
struct Resinfo {
    tokens: Vec<Token>,
    /// What its comments say, apart from the words.
    comments: Vec<String>,
}

/// An Authentication-Results value split by RFC 8601's grammar, never at a `;`, `=` or space inside a quoted string
/// or a comment (RFC 5322, nested, with `\` escapes), as the server copies sender text into both: the envelope
/// sender's quoted local part. A quoted string joins the word it touches, so `"a b"@example.org` stays one address.
/// With it false when a quoted string may end elsewhere than it seems (`quoted`): then no split can be trusted.
fn resinfos(value: &str) -> (Vec<Resinfo>, bool) {
    let (mut all, mut word, mut chars, mut clear) = (vec![Resinfo::default()], String::new(), value.chars(), true);
    while let Some(c) = chars.next() {
        let at = all.last_mut().expect("starts with one, and only grows");
        if !word.is_empty() && (c.is_whitespace() || matches!(c, '(' | ';' | '=')) {
            at.tokens.push(Token::Word(std::mem::take(&mut word)));
        }
        match c {
            '"' => clear &= quoted(&mut chars, &mut word),
            '(' => {
                let (mut depth, mut text) = (1, String::new());
                while let Some(c) = chars.next() {
                    depth += match c {
                        '(' => 1,
                        ')' => -1,
                        _ => 0,
                    };
                    match c {
                        _ if depth == 0 => break,
                        // rspamd quotes the envelope sender inside its SPF comment: a `)` in it closes nothing.
                        '"' => clear &= quoted(&mut chars, &mut text),
                        '\\' => text.extend(chars.next()),
                        c => text.push(c),
                    }
                }
                at.comments.push(text);
            }
            ';' => all.push(Resinfo::default()),
            '=' => at.tokens.push(Token::Equals),
            c if c.is_whitespace() => {}
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        all.last_mut().expect("starts with one, and only grows").tokens.push(Token::Word(word));
    }
    (all, clear)
}

/// A quoted string's rest, as rspamd writes one (`maybe_smtp_quote_value`): only `"` is escaped, so `\"` is a quote and
/// any other `\` is itself. That writer also gives a value ending in `\` the same `\"`, so a string holding `\"`, or
/// one never closed, may end elsewhere: false then, and the server's later results may sit inside it.
fn quoted(chars: &mut std::str::Chars<'_>, out: &mut String) -> bool {
    while let Some(c) = chars.next() {
        match c {
            '"' => return true,
            '\\' if chars.clone().next() == Some('"') => return false,
            c => out.push(c),
        }
    }
    false
}

/// The body as Roundcube's `message_body` draws it: one frame document with every part in order, HTML sanitized
/// and text as `plain::html` writes it (HTML too with `text`), then the listed images (`inline_images`).
fn body(structure: &Structure, links: &impl Links, remote_images: bool, text: bool) -> MailBody {
    let (mut content, mut remote_seen, mut widest, mut all_text) = (String::new(), false, 0, true);
    // Every HTML part of one message names the same images: read once per message, by the path of its root part.
    let mut by_message: HashMap<PartPath, Arc<Cids>> = HashMap::new();
    let mut cids_of = |node: &Node| Arc::clone(by_message.entry(node.path_of(0)).or_insert_with(|| Arc::new(image_cids(node))));
    for c in &structure.contents {
        if let Content::Part { node, .. } = c
            && let size = bytes(node.msg, node.part()).len()
            && size > MAX_SHOWN
        {
            let (size, href) = (kubicle_api::size(size as u64), links.part(PartRoute::Download, &node.path));
            let _ = write!(
                content,
                "<p class=\"part-notice\">This part is too large to show ({size}). <a href=\"{}\">Download it</a></p>",
                Escaped(&href)
            );
            continue;
        }
        let kind = |node: &Node| content_type(node.part());
        let html_part =
            |node: &Node| matches!(kind(node).as_str(), "text/html" | "text/enriched") && !bytes(node.msg, node.part()).is_empty();
        let (html, cids) = match c {
            Content::Part { node, .. } if !text && html_part(node) => match (&node.part().body, kind(node).as_str()) {
                (PartType::Html(html) | PartType::Text(html), "text/html") => (Cow::Borrowed(html.as_ref()), cids_of(node)),
                (PartType::Text(text), _) => (Cow::Owned(enriched_html(text)), cids_of(node)),
                _ => continue,
            },
            Content::Headers(nested) => {
                headers_table(&header_rows(nested, None), &written_date(nested), &mut content);
                continue;
            }
            Content::Encrypted => {
                let _ = write!(content, "<p class=\"part-notice\">{}</p>", Escaped(ENCRYPTED));
                continue;
            }
            Content::Part { .. } => {
                if let Some(text) = content_text(c) {
                    plain::html(&text, links.prefix(), &mut content);
                }
                continue;
            }
        };
        let (safe, remote, wide) = sanitize(&html, links, remote_images, cids);
        let _ = write!(content, "<div class=\"message-htmlpart\">{safe}</div>");
        (remote_seen, widest, all_text) = (remote_seen || remote, widest.max(wide), false);
    }
    for image in structure.listed.iter().filter(|l| !l.layout_image).take(MAX_IMAGES as usize) {
        if structure::image_type(&image.content_type, &image.name).is_some() {
            let _ = write!(content, "<p><img src=\"{}\"></p>", Escaped(&links.inline(&image.path, image.section.as_ref())));
        }
    }
    MailBody { document: document(&content, links.prefix(), widest), remote_blocked: remote_seen, text: all_text }
}

/// A content part's text, for the reader and for a quote alike: a text or report part as it is (`text` once
/// uuencoded files are cut out), HTML and enriched text converted; line ends as `\n`, and format=flowed unfolded.
/// `None` for an empty part, which Roundcube leaves out, for a header table or the encrypted notice, and for HTML or
/// enriched text past `MAX_SHOWN`, whose conversion builds a tree many times its size: the reader shows only a notice
/// there, and CPUs + 1 replies to 3 MB of HTML took kubicle to 1.2 GB. Any other part reads as UTF-8, as `application/pgp` does.
#[must_use]
pub fn content_text(content: &Content) -> Option<String> {
    match content {
        Content::Part { node, text } => {
            let (part, kind) = (node.part(), content_type(node.part()));
            let size = bytes(node.msg, part).len();
            if size == 0 || size > MAX_SHOWN && matches!(kind.as_str(), "text/html" | "text/enriched") {
                return None;
            }
            let param = |name| part.content_type().and_then(|t| t.attribute(name)).map(str::to_ascii_lowercase);
            let flowed = kind == "text/plain" && param("format").as_deref() == Some("flowed");
            let delsp = flowed && param("delsp").as_deref() == Some("yes");
            let source: Cow<str> = match (&part.body, text) {
                (_, Some(text)) => Cow::Borrowed(text),
                (PartType::Html(html), _) => Cow::Owned(html_text(html)),
                (PartType::Text(t), _) if kind == "text/html" => Cow::Owned(html_text(t)),
                (PartType::Text(t), _) if kind == "text/enriched" => Cow::Owned(html_text(&enriched_html(t))),
                (PartType::Text(t), _) => Cow::Borrowed(t.as_ref()),
                _ => String::from_utf8_lossy(bytes(node.msg, part)),
            };
            let source = source.replace("\r\n", "\n");
            Some(if flowed { flowed::unfold(&source, delsp) } else { source })
        }
        Content::Headers(_) | Content::Encrypted => None,
    }
}

/// Content-ID to the part the inline route serves for it, and that part's section.
type Cids = HashMap<String, (PartPath, Section)>;

/// The Content-IDs an HTML part may show as `cid:` images.
fn image_cids(node: &Node) -> Cids {
    let mut cids = HashMap::new();
    for (id, part) in (0u32..).zip(&node.msg.parts) {
        let name = name_of(part, "");
        if let Some(cid) = part.content_id().filter(|_| structure::image_type(&content_type(part), &name).is_some()) {
            cids.entry(cid.trim_matches(['<', '>']).to_owned()).or_insert_with(|| (node.path_of(id), node.sibling(id).section()));
        }
    }
    cids
}

/// Roundcube's `rcube_enriched::to_html`: text/enriched (RFC 1896) as HTML, which then goes through the sanitizer
/// like any mail HTML.
fn enriched_html(body: &str) -> String {
    let body = body.replace("<<", "&lt;").replace("\r\n", "\n");
    // One line break joins lines with a space; n breaks in a row make n - 1.
    let (mut text, mut breaks) = (String::with_capacity(body.len()), 0);
    for c in body.chars() {
        breaks = if c == '\n' { breaks + 1 } else { 0 };
        text.push(if breaks == 1 { ' ' } else { c });
    }
    let mut html = text.replace('\n', "<br>");
    for (tag, open, close) in [
        ("bold", "<b>", "</b>"),
        ("italic", "<i>", "</i>"),
        ("fixed", "<tt>", "</tt>"),
        ("smaller", "<font size=-1>", "</font>"),
        ("bigger", "<font size=+1>", "</font>"),
        ("underline", "<span style=\"text-decoration: underline\">", "</span>"),
        ("flushleft", "<span style=\"text-align: left\">", "</span>"),
        ("flushright", "<span style=\"text-align: right\">", "</span>"),
        ("flushboth", "<span style=\"text-align: justified\">", "</span>"),
        ("indent", "<span style=\"padding-left: 20px\">", "</span>"),
        ("indentright", "<span style=\"padding-right: 20px\">", "</span>"),
    ] {
        html = replace_ignore_case(&replace_ignore_case(&html, &format!("<{tag}>"), open), &format!("</{tag}>"), close);
    }
    // `####,####,####` keeps each number's first two digits, as Roundcube does
    let hex = |color: &str| color.split(',').take(3).map(|c| c.get(..2).unwrap_or(c)).collect::<String>();
    html = enriched_param(&html, "color", |c| if c.contains(',') { format!("color: #{}", hex(c)) } else { format!("color: {c}") });
    html = enriched_param(&html, "fontfamily", |family| format!("font-family: {family}"));
    // Each pass rebuilds the body, so a stack of excerpts cost time cubic in its depth: 1,800 in 43 KB took 12 s.
    // Past `MAX_EXCERPTS` they stay as tags, which the sanitizer drops.
    for _ in 0..MAX_EXCERPTS {
        let Some((before, rest)) = split_ignore_case(&html, "<excerpt>") else { break };
        let Some((inner, after)) = split_ignore_case(rest, "</excerpt>") else { break };
        let quoted = inner.split("<br>").fold(String::new(), |quoted, line| quoted + "&gt;" + line + "<br>");
        html = format!("{before}<span class=\"quotes\">{quoted}</span>{after}");
    }
    html
}

/// `<name><param>value</param>text</name>` as a span styled by `style(value)`, innermost last, as Roundcube's
/// greedy pattern replaces them; past the pass cap they stay as tags. Each pass rebuilds the whole body, so the cap
/// falls with body size (`ENRICHED_WORK / len`, at most `MAX_SPANS`): a crafted 2 MiB part of hundreds of spans
/// cannot cost time quadratic in its size, while small real enriched mail gets the full `MAX_SPANS`.
fn enriched_param(html: &str, name: &str, style: impl Fn(&str) -> String) -> String {
    let (open, close) = (format!("<{name}><param>"), format!("</{name}>"));
    let mut html = html.to_owned();
    for _ in 0..MAX_SPANS.min(ENRICHED_WORK / html.len().max(1)) {
        let lower = html.to_ascii_lowercase();
        let (Some(start), Some(end)) = (lower.rfind(&open), lower.rfind(&close)) else { break };
        let Some(param_end) = lower[start..].find("</param>").map(|p| start + p).filter(|&p| p <= end) else { break };
        let style = style(&html[start + open.len()..param_end]);
        let (before, inner, after) = (&html[..start], &html[param_end + 8..end], &html[end + close.len()..]);
        html = format!("{before}<span style=\"{}\">{inner}</span>{after}", Escaped(&style));
    }
    html
}

/// Every `from` in `text` as `to`, matched without regard to ASCII case, in one pass over one lowered copy.
fn replace_ignore_case(text: &str, from: &str, to: &str) -> String {
    let (lower, from) = (text.to_ascii_lowercase(), from.to_ascii_lowercase()); // ASCII lowering keeps byte offsets
    let (mut out, mut at) = (String::with_capacity(text.len()), 0);
    for (start, _) in lower.match_indices(&from) {
        (out, at) = (out + &text[at..start] + to, start + from.len());
    }
    out + &text[at..]
}

/// `text` around the last `needle`, matched without regard to ASCII case, as Roundcube's greedy
/// `(.*)<excerpt>(.*)</excerpt>` takes the last of each.
fn split_ignore_case<'t>(text: &'t str, needle: &str) -> Option<(&'t str, &'t str)> {
    let at = text.to_ascii_lowercase().rfind(&needle.to_ascii_lowercase())?; // ASCII lowering keeps byte offsets
    Some((&text[..at], &text[at + needle.len()..]))
}

/// A header table, as Roundcube's `message-partheaders` and print view draw it: addresses as `Name <email>`, the
/// date as `date` writes it.
fn headers_table(rows: &[Header], date: &dyn Fn(i64) -> String, out: &mut String) {
    out.push_str("<table class=\"headers-table\">");
    for row in rows {
        let value = match row {
            Header::Subject(s) => s.clone(),
            Header::Date(d) => date(*d),
            Header::Priority(p) => p.label().to_owned(),
            _ => row.addresses().unwrap_or_default().iter().map(ToString::to_string).collect::<Vec<_>>().join(", "),
        };
        let _ = write!(out, "<tr><th>{}</th><td>{}</td></tr>", row.title(), Escaped(&value));
    }
    out.push_str("</table>");
}

/// A forwarded message's date as its sender wrote it (the reader's zone is not known here), else in UTC.
fn written_date<'m>(mail: &'m Mail) -> impl Fn(i64) -> String + 'm { |d| mail.header_raw("Date").map_or_else(|| utc(d), clean) }

fn utc(seconds: i64) -> String {
    Timestamp::from_second(seconds).map_or_else(|_| String::new(), |t| t.strftime("%a, %d %b %Y %H:%M:%S +0000").to_string())
}

/// `seconds` in `zone` as Roundcube's `date_long`, `Y-m-d H:i`.
fn zoned(seconds: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(seconds).map_or_else(|_| String::new(), |t| t.to_zoned(zone.clone()).strftime("%Y-%m-%d %H:%M").to_string())
}

/// The frame's document. DNS prefetch goes off first, as CSP does not cover it; then the CSP, so it governs
/// everything after, and a policy the mail adds on top can only narrow it. Meta CSP has no `sandbox` or
/// `frame-ancestors`: the iframe attribute and the app's own headers carry those.
fn document(content: &str, prefix: &str, widest: u32) -> String {
    let mut doc = String::with_capacity(content.len() + 700);
    doc.push_str("<!DOCTYPE html><html><head><meta http-equiv=\"x-dns-prefetch-control\" content=\"off\">");
    let _ = write!(doc, "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">", Escaped(&mail_csp(prefix)));
    doc.push_str("<meta name=\"referrer\" content=\"no-referrer\">");
    let _ = write!(doc, "<meta name=\"color-scheme\" content=\"light\"><style>{PAPER}{}</style>", fit(widest));
    let _ = write!(doc, "</head><body><div id=\"kubicle-paper\">{content}</div></body></html>");
    doc
}

/// Rules that shrink a mail laid out wider than the frame, as Gmail does. The frame cannot measure its content
/// (no scripts, an opaque origin), so the widest fixed width the mail declares stands in for it. Each 5% step
/// applies below the frame width at which the step before it fits; under 60% the frame scrolls sideways.
fn fit(widest: u32) -> String {
    let needed = u64::from(widest) + 2 * u64::from(PAPER_PADDING); // the mail declares `widest`, up to u32::MAX
    let rule = |percent: u64| format!("@media (max-width:{}px){{html{{zoom:{percent}%}}}}", needed * (percent + 5) / 100 - 1);
    // Largest zoom first: where several queries match, the last of them, the smallest zoom, wins.
    if widest == 0 { String::new() } else { (60..=95).rev().step_by(5).map(rule).collect() }
}

/// The fixed width in pixels an attribute gives an element: a `width` attribute, or `width` or `min-width` in a
/// style. Images are left out: the paper's `max-width` already shrinks them.
fn fixed_width(element: &str, attribute: &str, value: &str) -> Option<u32> {
    let pixels = |v: &str| {
        let v = v.trim().trim_end_matches("!important").trim_end();
        let digits = v.strip_suffix("px").unwrap_or(v).trim();
        digits.split('.').next()?.parse::<u32>().ok()
    };
    let declarations = value.split(';').filter_map(|declaration| declaration.split_once(':'));
    let widths = declarations.filter(|(property, _)| matches!(property.trim().to_ascii_lowercase().as_str(), "width" | "min-width"));
    match (element, attribute) {
        ("img", _) => None,
        (_, "width") => pixels(value),
        (_, "style") => widths.filter_map(|(_, v)| pixels(&v.to_ascii_lowercase())).max(),
        _ => None,
    }
}

/// The policy every mail document runs under: images only from our signed inline and proxy routes, inline styles,
/// nothing else.
fn mail_csp(prefix: &str) -> String {
    // Exact paths: CSP ignores the query.
    let mut csp = format!("default-src 'none'; img-src {prefix}{} {prefix}{} ", links::INLINE, links::IMAGE);
    csp.push_str("data:; style-src 'unsafe-inline'; form-action 'none'; base-uri 'none'");
    csp
}

/// Roundcube's print view (`messageprint.html`): the subject as a heading, the header rows with dates in `zone`,
/// the attachments' names and sizes, then the body. Every address prints, where Roundcube prints ten and a link
/// paper cannot follow. A top-level, script-free page under the mail policy and `sandbox`.
#[must_use]
pub fn print_response(message: &Message, zone: &TimeZone, prefix: &str) -> Response<Body> {
    let subject = if message.subject.is_empty() { "(no subject)" } else { &message.subject };
    let mut head = format!("<div class=\"print-head\"><h2 style=\"margin:0 0 8px;font-size:18px\">{}</h2>", Escaped(subject));
    headers_table(&message.headers, &|d| zoned(d, zone), &mut head);
    if !message.attachments.is_empty() {
        head.push_str("<ul style=\"margin:0 0 12px;padding-left:20px;font-size:13px\">");
        for a in &message.attachments {
            let _ = write!(head, "<li>{} ({})</li>", Escaped(&a.filename), kubicle_api::size(a.size));
        }
        head.push_str("</ul>");
    }
    head.push_str("</div><hr>");
    sandboxed_page(&page(&head, &message.body), &message.subject, prefix) // the subject titles the tab and the printed page, as in Roundcube
}

/// `head` above a body, inside its document.
fn page(head: &str, body: &MailBody) -> String {
    body.document.replacen("<body>", &format!("<body>{head}"), 1) // ours, first in the doc
}

/// The Date header in `zone` as Roundcube's `date_long`, `Y-m-d H:i`; `None` without a valid one.
#[must_use]
pub fn long_date(mail: &Mail, zone: &TimeZone) -> Option<String> {
    mail.date().filter(|d| d.is_valid()).map(|d| zoned(d.to_timestamp(), zone))
}

/// The `X-Priority` level, read by its leading number as Roundcube reads it; `None` without one or outside 1 to 5.
#[must_use]
pub fn priority(mail: &Mail) -> Option<Priority> {
    let value = mail.header_raw("X-Priority")?.trim_start();
    let digits = value.find(|c: char| !c.is_ascii_digit()).map_or(value, |end| &value[..end]);
    let levels = [Priority::Highest, Priority::High, Priority::Normal, Priority::Low, Priority::Lowest];
    levels.get(digits.parse::<usize>().ok()?.checked_sub(1)?).copied()
}

/// What the attribute filter needs: it runs inside ammonia, which wants it `'static`.
struct Rewrite<L> {
    links: L,
    remote_images: bool,
    /// Content-ID to the part the inline route serves for it: only images it can serve.
    cids: Arc<Cids>,
    remote_seen: AtomicBool,
    /// The widest fixed width the mail declares, for `fit`.
    widest: AtomicU32,
    /// Image sources seen so far, which `MAX_IMAGES` bounds.
    images: AtomicU32,
}

impl<L: Links> Rewrite<L> {
    /// Rewrites an image source: `cid:` to a signed inline URL; `http(s)` to a signed proxy URL, or while blocked
    /// to `BLOCKED_IMAGE` (with `placeholder`) or nothing; `data:image/` kept; anything else dropped.
    fn image(&self, value: &str, placeholder: bool) -> Option<String> {
        if self.images.fetch_add(1, Ordering::Relaxed) >= MAX_IMAGES {
            return None;
        }
        let value = trim_url(value);
        // Scheme-relative: in the frame it would load as https.
        let value = if value.starts_with("//") { Cow::Owned(format!("https:{value}")) } else { value };
        match scheme(&value)?.as_str() {
            "cid" => {
                let cid = percent_decode_str(value.get(4..)?).decode_utf8_lossy();
                let (path, section) = self.cids.get(cid.trim_matches(['<', '>']))?;
                Some(self.links.inline(path, Some(section)))
            }
            "data" if value.get(5..11).is_some_and(|t| t.eq_ignore_ascii_case("image/")) => Some(value.into_owned()),
            "http" | "https" => {
                if !self.remote_images {
                    self.remote_seen.store(true, Ordering::Relaxed);
                    return placeholder.then(|| BLOCKED_IMAGE.to_owned());
                }
                url::Url::parse(&value).ok().map(|url| self.links.image(url.as_str()))
            }
            _ => None,
        }
    }
}

const TOO_DEEP: &str = "This part is too complex to show.";

/// Formatting elements `html5ever` keeps in its active-formatting list and reconstructs into every block after an
/// unclosed one (the adoption agency). Hundreds left open across many blocks is what makes it build gigabytes.
const FORMATTING: [&str; 14] = ["a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u"];
/// Elements that do not nest, so an open one is not depth: void elements, and those whose end tag HTML lets a mail
/// leave out and `html5ever` then closes on its own (unclosed `<td>`s and `<p>`s in old mail are not deep).
const LOOSE: [&str; 33] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr", "p", "li", "dt", "dd",
    "tr", "td", "th", "tbody", "thead", "tfoot", "colgroup", "caption", "option", "optgroup", "rt", "rp", "html", "head", "body",
];
/// Reconstruction work a message may cost `html5ever` before the reader shows a notice instead: an open formatting
/// element summed over every block after it. Real mail stays in the thousands; the blow-up passes a million in its
/// first kilobytes. A heavy Word export (20-deep font nesting over 2,000 paragraphs) measured 380,000.
const MAX_HTML_WORK: usize = 2_000_000;
/// Memory `lol_html` itself may use framing one message, a backstop for anything the counters below do not model
/// (its namespace stack is not capped). Its own buffers stay in kilobytes on a 2 MiB message.
const LOL_HTML_MEMORY: usize = 16 << 20;
/// Table layout work `html2text` may cost converting a part to text: it pads every row to its column's widest cell,
/// so one 10,000-char cell over 65,000 rows (1.9 MiB) built 290 MB. The bound is cells times the longest cell's
/// text, which this over-estimates; real mail stays near a million, a crafted wide table passes a billion.
const MAX_TABLE_WORK: usize = 100_000_000;

/// The end-tag handler `too_complex` hands `lol_html`: boxed and `Send + Sync` as the crate's type demands.
type OnEnd = Box<dyn FnOnce(&mut lol_html::html_content::EndTag) -> Result<(), Box<dyn std::error::Error + Send + Sync>>>;

/// Whether a message would cost `html5ever` (ammonia's parser) or `html2text` too much to be worth showing, decided
/// by one streaming pass with `lol_html`, which has no tree builder and so cannot blow up itself (docs/design.md,
/// Security). It is refused when element nesting passes `MAX_HTML_DEPTH`, when the adoption agency's reconstruction
/// work would pass `MAX_HTML_WORK`, when a table's layout would pass `MAX_TABLE_WORK`, or when the parse is ambiguous
/// or outgrows `LOL_HTML_MEMORY`. `lol_html` tokenizes for real, so a `<` in CSS, a comment or an attribute is never
/// miscounted as a tag and a real message never refused for one.
fn too_complex(html: &str) -> bool {
    let (depth, open_fmt, work) = (Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)));
    // Table layout: a running count of cells, the text in the cell now open, and the longest a cell has reached.
    let (cells, cell_text, widest_cell) = (Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)));
    let (d, of, wk) = (Rc::clone(&depth), Rc::clone(&open_fmt), Rc::clone(&work));
    // `cell_text` is reset by the element handler and summed by the text handler, so each needs its own clone.
    let (cn, ct_reset) = (Rc::clone(&cells), Rc::clone(&cell_text));
    let (ct_sum, wc) = (Rc::clone(&cell_text), Rc::clone(&widest_cell));
    let settings = lol_html::Settings {
        memory_settings: lol_html::MemorySettings { preallocated_parsing_buffer_size: 8192, max_allowed_memory_usage: LOL_HTML_MEMORY },
        element_content_handlers: vec![
            lol_html::element!("*", move |el| {
                let name = el.tag_name().to_ascii_lowercase();
                let (fmt, loose) = (FORMATTING.contains(&name.as_str()), LOOSE.contains(&name.as_str()));
                if fmt {
                    of.set(of.get() + 1);
                } else {
                    wk.set(wk.get() + of.get()); // a block after open formatting: one reconstruction each
                }
                if !loose {
                    d.set(d.get() + 1);
                }
                if name == "td" || name == "th" {
                    cn.set(cn.get() + 1);
                    ct_reset.set(0); // the text of the cell now opening, summed by the handler below
                }
                if d.get() > MAX_HTML_DEPTH || wk.get() > MAX_HTML_WORK {
                    return Err("too complex".into());
                }
                if fmt || !loose {
                    let (d, of) = (Rc::clone(&d), Rc::clone(&of));
                    let close: OnEnd = Box::new(move |_| {
                        if fmt {
                            of.set(of.get().saturating_sub(1));
                        }
                        if !loose {
                            d.set(d.get().saturating_sub(1));
                        }
                        Ok(())
                    });
                    let _ = el.on_end_tag(close);
                }
                Ok(())
            }),
            lol_html::text!("td, th", move |t| {
                ct_sum.set(ct_sum.get() + t.as_str().len()); // `html2text` pads the column to the whole cell's width
                wc.set(wc.get().max(ct_sum.get()));
                Ok(())
            }),
        ],
        ..lol_html::Settings::new()
    };
    let mut rewriter = lol_html::HtmlRewriter::new(settings, |_: &[u8]| {});
    if rewriter.write(html.as_bytes()).and_then(|()| rewriter.end()).is_err() {
        return true;
    }
    // No counter tripped and the parse was clean: the message is refused only if a table's layout is still too wide.
    cells.get().saturating_mul(widest_cell.get()) > MAX_TABLE_WORK
}

/// Mail HTML through ammonia, `Rewrite` applied to every link and image: the clean HTML, whether remote images were
/// left out, and the widest fixed width it declares (`fit`).
fn sanitize(html: &str, links: &impl Links, remote_images: bool, cids: Arc<Cids>) -> (String, bool, u32) {
    if too_complex(html) {
        return (format!("<p class=\"part-notice\">{TOO_DEEP}</p>"), false, 0);
    }
    let rewrite = Arc::new(Rewrite {
        links: links.clone(),
        remote_images,
        cids,
        remote_seen: AtomicBool::new(false),
        widest: AtomicU32::new(0),
        images: AtomicU32::new(0),
    });
    let filter = Arc::clone(&rewrite);
    let mut builder = ammonia::Builder::default();
    // The mail's `<style>` stays as sent: ammonia filters no stylesheet, so the CSP alone stops what it loads.
    builder.add_tags(["style", "font", "big"]).rm_clean_content_tags(["style"]);
    // Dropped with everything inside: SVG and MathML are where sanitizer bypasses live, and the rest would otherwise
    // show their raw text.
    builder.add_clean_content_tags(["svg", "math", "title", "noscript", "template", "iframe", "noembed", "noframes", "xmp"]);
    builder.add_clean_content_tags(["textarea", "select"]);
    builder.add_generic_attributes(["style", "class", "id", "dir", "align", "valign", "width", "height", "bgcolor", "background", "border"]);
    builder.add_tag_attributes("a", ["name"]).add_tag_attributes("font", ["color", "face", "size"]).add_tag_attributes("hr", ["noshade"]);
    builder.add_tag_attributes("img", ["hspace", "vspace"]).add_tag_attributes("table", ["cellpadding", "cellspacing", "frame", "rules"]);
    builder.add_tag_attributes("td", ["nowrap", "abbr"]).add_tag_attributes("th", ["nowrap", "abbr"]);
    builder.add_tag_attributes("ol", ["type", "reversed"]).add_tag_attributes("ul", ["type"]).add_tag_attributes("li", ["type", "value"]);
    builder.add_tag_attributes("time", ["datetime"]).add_tag_attributes("details", ["open"]);
    builder.rm_tag_attributes("blockquote", ["cite"]).rm_tag_attributes("q", ["cite"]);
    builder.rm_tag_attributes("del", ["cite"]).rm_tag_attributes("ins", ["cite"]);
    builder.url_schemes(HashSet::from(["http", "https", "mailto", "cid", "data"]));
    builder.url_relative(ammonia::UrlRelative::Deny); // in a srcdoc they would resolve against the app itself
    builder.set_tag_attribute_value("a", "target", "_blank").attribute_filter(move |element, attribute, value| {
        let rewrite = &filter;
        if let Some(width) = fixed_width(element, attribute, value) {
            rewrite.widest.fetch_max(width, Ordering::Relaxed);
        }
        match (element, attribute) {
            (_, "href") => match scheme(&trim_url(value)).as_deref() {
                Some("http" | "https") => Some(value.into()),
                Some("mailto") => mailto(&trim_url(value), rewrite.links.prefix()).map(Cow::Owned),
                _ => None,
            },
            ("img", "src") => rewrite.image(value, true).map(Cow::Owned),
            // `height="auto"` and the like size nothing, and would hide a missing ratio from `PAPER`'s rule.
            ("img", "width" | "height") => value.trim_start().starts_with(|c: char| c.is_ascii_digit()).then(|| value.into()),
            (_, "background") => rewrite.image(value, false).map(Cow::Owned), // dropped while blocked: a tint could hide the text on it
            _ => Some(value.into()),
        }
    });
    let clean = builder.clean(html).to_string();
    (clean, rewrite.remote_seen.load(Ordering::Relaxed), rewrite.widest.load(Ordering::Relaxed))
}

/// Checks a mail's `mailto:` link as Roundcube's `washtml_link_callback` does: only addresses kubicle can send to
/// stay, and with none the link goes. It opens kubicle's own Compose in a new tab, as Roundcube's "Compose to"
/// does, since the frame's links open as popups outside its sandbox. A link without an `@` goes to the browser as
/// is (Elastic's `mailtomenu`).
fn mailto(href: &str, prefix: &str) -> Option<String> {
    let rest = href.get(7..)?; // past `mailto:`, which `scheme` found
    let (to, query) = rest.split_once('?').map_or((rest, None), |(to, query)| (to, Some(query)));
    let to = clean(&percent_decode_str(to).decode_utf8_lossy());
    if !to.contains('@') {
        return Some(href.to_owned());
    }
    let sendable = to_addresses(&to).into_iter().filter(|a| ascii_address(&a.email).is_some());
    let quoted = |a: Address| if let Some(name) = a.name { format!("\"{}\" <{}>", name.replace('"', ""), a.email) } else { a.email };
    let valid: Vec<String> = sendable.map(quoted).collect();
    if valid.is_empty() {
        return None;
    }
    let mailto = format!("mailto:{}{}", valid.join(","), query.map(|q| format!("?{q}")).unwrap_or_default());
    Some(format!("{prefix}#compose={}", links::enc(&mailto)))
}

/// `type/subtype` in lower case; `message/rfc822` for a nested message whatever it declares.
pub(crate) fn content_type(part: &MessagePart) -> String {
    match part.content_type() {
        _ if matches!(part.body, PartType::Message(_)) => "message/rfc822".into(),
        Some(ct) => mime_type(&format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or_default())),
        None if matches!(part.body, PartType::Text(_)) => "text/plain".into(),
        None => "application/octet-stream".into(),
    }
}

/// `type/subtype` from a declared Content-Type, lower case and parameters dropped, cleaned as Roundcube's
/// `fix_mimetype` cleans it (`image/jpg` and `image/pjpeg` to `image/jpeg`, Outlook's `application/pdf.<id>` to
/// `application/pdf`); anything malformed is `application/octet-stream`.
#[must_use]
pub fn mime_type(declared: &str) -> String {
    let value = declared.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b));
    match value.split_once('/') {
        _ if value == "pdf" => "application/pdf".into(),
        Some(("image", "jpg" | "jpeg" | "pjpg" | "pjpeg")) => "image/jpeg".into(),
        Some(("image", "x-ms-bmp")) => "image/bmp".into(),
        Some(("application", sub)) if sub.starts_with("pdf") && sub.len() > 3 => "application/pdf".into(),
        Some((t, sub)) if token(t) && token(sub) => value,
        _ => "application/octet-stream".into(),
    }
}

/// A part's name: its own, a forwarded message's subject, else Roundcube's `attachment_name`: "HTML Message", or
/// "Part" and its IMAP part number `number` with the type's extension ("Part 2.pdf").
pub(crate) fn name_of(part: &MessagePart, number: &str) -> String {
    let mut name = safe_filename(part.attachment_name().or_else(|| part.message().and_then(|m| m.subject())).unwrap_or_default());
    if name.is_empty() {
        let kind = content_type(part);
        let ext = match kind.split_once('/') {
            _ if kind == "text/plain" => Some("txt"),
            _ if kind == "text/calendar" => Some("ics"),
            Some((_, sub)) if sub.len() <= 4 && sub.bytes().all(|b| b.is_ascii_alphanumeric()) => Some(sub),
            _ => None,
        };
        name = match ext {
            _ if kind == "text/html" => "HTML Message".into(),
            Some(ext) => format!("Part {number}.{ext}"),
            None => format!("Part {number}"),
        };
    }
    if matches!(part.body, PartType::Message(_)) && !name.to_ascii_lowercase().ends_with(".eml") {
        name.push_str(".eml");
    }
    name
}

/// A file name fit to show and to save: no control, bidi-override or path characters, no leading dot. Past 200
/// characters the stem loses its end, but the extension a saved file is opened by stays.
#[must_use]
pub fn safe_filename(declared: &str) -> String {
    let hidden = |c: char| c.is_control() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let name: String = declared.chars().filter(|&c| !hidden(c)).map(|c| if matches!(c, '/' | '\\') { '_' } else { c }).collect();
    let name: Vec<char> = name.trim().trim_start_matches('.').chars().collect();
    let ext = name.iter().rev().position(|&c| c == '.').filter(|&n| n < 16).map_or(0, |n| n + 1); // with its dot
    name[..name.len().min(200).saturating_sub(ext)].iter().chain(&name[name.len() - ext..]).collect()
}

/// An inline image from its IMAP section alone, the part's MIME header and body; `None` unless it decodes to the
/// size the whole message gave it, so a server that numbers parts otherwise falls back to the whole message.
#[must_use]
pub fn section_response(mime: &[u8], body: &[u8], size: usize) -> Option<Response<Body>> {
    let blank: &[u8] = if mime.ends_with(b"\r\n\r\n") || mime.ends_with(b"\n\n") { b"" } else { b"\r\n" };
    let raw = [mime, blank, body].concat();
    let mail = MessageParser::default().parse(&raw)?;
    let part = mail.parts.first().filter(|p| p.contents().len() == size)?;
    Some(served(part.contents().to_vec(), structure::image_type(&content_type(part), &name_of(part, ""))?, None))
}

/// The response for a verified part URL; `None` when the part does not exist, or the route does not serve its type:
/// `inline` serves raster images, `view` text, HTML and forwarded messages as pages of their own (the HTML through
/// the sanitizer, under the mail policy and `sandbox`), `download` anything as a file. A page's images load as the
/// message's own do: `links` and `shown` are its.
#[must_use]
pub fn part_response(mail: &Mail, path: &PartPath, route: PartRoute, links: &impl Links, shown: &Shown) -> Option<Response<Body>> {
    let Shown { text, remote_images, .. } = *shown;
    let file = structure::locate(mail, path)?;
    match route {
        PartRoute::Download => Some(served(file.data.into_owned(), "application/octet-stream", Some(&file.name))),
        PartRoute::Inline => Some(served(file.data.into_owned(), structure::image_type(&file.content_type, &file.name)?, None)),
        PartRoute::View => match file.content_type.as_str() {
            "text/html" if file.data.len() <= MAX_SHOWN => {
                let cids = if path.is_unpacked() { Arc::default() } else { Arc::new(image_cids(&structure::node_at(mail, path)?)) };
                let (content, _, widest) = sanitize(&String::from_utf8_lossy(&file.data), links, remote_images, cids);
                Some(sandboxed_page(&document(&content, links.prefix(), widest), &file.name, links.prefix()))
            }
            // HTML past `MAX_SHOWN` too, as its markup: text costs only its size, where the sanitizer's tree is 35 times.
            "text/plain" | "text/html" => Some(served(String::from_utf8_lossy(&file.data).into_owned(), "text/plain; charset=utf-8", None)),
            "message/rfc822" => {
                let (structure, nested) = Structure::of_message(mail, path, !text)?;
                let mut head = String::new();
                headers_table(&header_rows(nested, None), &written_date(nested), &mut head);
                head.push_str("<hr>");
                let page = page(&head, &body(&structure, links, remote_images, text));
                Some(sandboxed_page(&page, &nested.subject().map(clean).unwrap_or_default(), links.prefix()))
            }
            _ => None,
        },
    }
}

/// A document from `document` as a top-level page titled `title`: the mail policy goes in a header too, with
/// `sandbox`, which a meta tag cannot set.
fn sandboxed_page(page: &str, title: &str, prefix: &str) -> Response<Body> {
    let page = page.replacen("<head>", &format!("<head><title>{}</title>", Escaped(title)), 1); // ours, from `document`
    let mut response = served(page, "text/html; charset=utf-8", None);
    if let Ok(csp) = HeaderValue::from_str(&format!("{}; sandbox", mail_csp(prefix))) {
        response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    response
}

/// Where a raw message's header block ends: at its first empty line, or the end of a mail without a body.
#[must_use]
pub fn header_end(raw: &[u8]) -> usize {
    let blank = |sep: &[u8]| raw.windows(sep.len()).position(|w| w == sep).map(|i| i + sep.len());
    blank(b"\r\n\r\n").into_iter().chain(blank(b"\n\n")).min().unwrap_or(raw.len())
}

/// Download (.eml): the raw message as a file named after its subject's first 128 characters, or its UID. As
/// Roundcube's `filename_from_subject`, each run of colons, slashes, tabs and line breaks, with the spaces after it,
/// becomes one space, and spaces, dots, slashes and underscores leave both ends: "Re: Invoice 4/2026" saves as
/// `Re Invoice 4 2026.eml`.
#[must_use]
pub fn eml_response(mail: &Mail, uid: u32) -> Response<Body> {
    static SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[:\t\n\r\x00\x0B/]+(?-u:\s)*").expect("a valid pattern"));
    let subject: String = mail.subject().unwrap_or_default().chars().take(128).collect();
    let cleaned = SEPARATORS.replace_all(&subject, " ");
    let cleaned = cleaned.trim_matches([' ', '.', '/', '_']);
    let name = Some(safe_filename(cleaned)).filter(|n| !n.is_empty()).unwrap_or_else(|| uid.to_string());
    served(mail.raw_message().to_vec(), "message/rfc822", Some(&format!("{name}.eml")))
}

/// Show source, the raw bytes up to `end` (the whole message, or its header block for the Headers dialog), as plain
/// text in the charset the message's own Content-Type names, as Roundcube's `viewsource.php`; UTF-8 where it names
/// none (every multipart), where Roundcube's ISO-8859-1 would garble UTF-8 text. A label that is not a plain token
/// is left out, so the mail cannot add to the header.
#[must_use]
pub fn source_response(raw: &bytes::Bytes, end: usize) -> Response<Body> {
    let declared = parse_headers(raw).ok().and_then(|m| m.content_type().and_then(|ct| ct.attribute("charset")).map(str::to_owned));
    let charset = declared.filter(|c| c.len() <= 40 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)));
    let mut response = served(raw.slice(..end), "text/plain; charset=utf-8", None);
    if let Some(value) = charset.and_then(|c| HeaderValue::from_str(&format!("text/plain; charset={c}")).ok()) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// Every byte of mail served on the app's origin, saved as `download` when given. The server's router adds the rest
/// to every answer but the app's own files (kubicle-server's `http.rs`, `router`): `nosniff`, a `sandbox` policy that
/// runs nothing if one is opened as a page, and `no-store`, as Roundcube's PHP session headers forbid storing: else a
/// tab opened from history after Sign out, or the next account asking the same URL, read it from the cache.
pub fn served(body: impl Into<Body>, content_type: &'static str, download: Option<&str>) -> Response<Body> {
    let mut response = Response::new(body.into());
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Some(name) = download {
        headers.insert(header::CONTENT_DISPOSITION, content_disposition(name));
    }
    response
}

/// `attachment` with an ASCII fallback name and the exact name in RFC 8187 form.
fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let value = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", links::enc(name));
    HeaderValue::from_str(&value).expect("visible ASCII and spaces only")
}

/// A list's addresses lowercased, to test membership in one step: a mail sets how many it holds, and comparing two
/// lists pairwise was quadratic in that.
#[must_use]
pub fn emails(list: &[Address]) -> HashSet<String> { list.iter().map(|a| a.email.to_ascii_lowercase()).collect() }

/// The addresses of a header, decoded and cleaned for display; group markers and empty addresses left out.
pub fn addresses(list: Option<&mail_parser::Address>) -> Vec<Address> {
    let all = list.into_iter().flat_map(mail_parser::Address::iter);
    let parsed = all.filter_map(|a| Some(Address { name: a.name().map(clean).filter(|n| !n.is_empty()), email: clean(a.address()?) }));
    parsed.filter(|a| !a.email.is_empty()).collect()
}

/// The addresses of a header mail-parser keeps as raw text, such as Mail-Followup-To, read as a To header's.
#[must_use]
pub fn raw_addresses(mail: &Mail, name: &'static str) -> Vec<Address> { mail.header_raw(name).map(to_addresses).unwrap_or_default() }

/// `text` read as a To header's addresses.
fn to_addresses(text: &str) -> Vec<Address> {
    let block = format!("To: {text}\r\n\r\n");
    addresses(MessageParser::default().parse_headers(block.as_bytes()).as_ref().and_then(|m| m.to()))
}

/// The posting address a List-Post value (RFC 2369) names: its first `<mailto:...>`, query left out. `None` for
/// `NO`, a web address alone, or an address kubicle would not send to.
#[must_use]
pub fn list_post(value: &str) -> Option<String> {
    let start = value.to_ascii_lowercase().find("<mailto:")? + "<mailto:".len(); // ASCII lowering keeps byte offsets
    let address = &value[start..start + value[start..].find('>')?];
    let address = address.split('?').next().unwrap_or_default().trim();
    ascii_address(&percent_decode_str(address).decode_utf8().ok()?)
}

/// `email` with its domain in ASCII, punycode for a non-ASCII one (`kontakt@müller.de` to
/// `kontakt@xn--mller-kva.de`), as Roundcube's `idn_to_ascii`; `None` unless the result passes `valid_email`.
/// SMTPUTF8, for letters beyond ASCII before the @, is not attempted.
#[must_use]
pub fn ascii_address(email: &str) -> Option<String> {
    let (local, domain) = email.rsplit_once('@')?;
    let domain = match url::Host::parse(domain).ok()? {
        url::Host::Domain(domain) => domain,
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
    };
    let email = format!("{local}@{domain}");
    kubicle_api::valid_email(&email).then_some(email)
}

/// A header value for display: control characters (CR, LF, NUL and the rest) removed.
#[must_use]
pub fn clean(value: &str) -> String { value.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_owned() }

/// Text HTML-escaped as it is written out, for `write!`: safe in element content and in quoted attributes.
pub(crate) struct Escaped<'t>(pub &'t str);

impl std::fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.chars().try_for_each(|c| match c {
            '&' => f.write_str("&amp;"),
            '<' => f.write_str("&lt;"),
            '>' => f.write_str("&gt;"),
            '"' => f.write_str("&quot;"),
            '\'' => f.write_str("&#39;"),
            _ => f.write_char(c),
        })
    }
}

/// A URL as a browser reads it: leading and trailing C0 controls and spaces cut, tabs and newlines removed.
fn trim_url(value: &str) -> Cow<'_, str> {
    let value = value.trim_matches(|c: char| c <= ' ');
    if value.contains(['\t', '\n', '\r']) { Cow::Owned(value.replace(['\t', '\n', '\r'], "")) } else { Cow::Borrowed(value) }
}

/// The lower-cased scheme, or `None` for a relative URL.
fn scheme(value: &str) -> Option<String> {
    let (scheme, _) = value.split_once(':')?;
    let mut chars = scheme.chars();
    (chars.next()?.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then(|| scheme.to_ascii_lowercase())
}
