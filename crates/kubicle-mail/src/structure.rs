//! A message's parts sorted as Roundcube's `rcube_message::parse_structure` sorts them: the content the reader
//! shows, in order, with each forwarded message's headers before its own, and the files it lists. mail-parser's
//! own `html_body`, `text_body` and `attachments` follow JMAP's rules, which differ, so the reader walks the part
//! tree. A `PartPath` names a forwarded message's parts and the files uuencoded in a part.

use std::{borrow::Cow, collections::HashSet, fmt, rc::Rc, str::FromStr, sync::LazyLock};

use mail_parser::{MessagePart, MimeHeaders, PartType};
use regex::Regex;

use crate::{links::Section, render, render::Mail};

/// Where a part sits: mail-parser's part id in the message, then in each forwarded message below it, and last, for
/// a file unpacked from that part, which one. Written `3`, `3.2` or `0.u1` (the second uuencoded file of part 0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PartPath {
    ids: Vec<u32>,
    unpacked: Option<u32>,
}

/// Multiparts inside multiparts: the walk shows nothing deeper, so a crafted mail cannot exhaust its stack.
const MAX_NESTING: usize = 64;
/// Files unpacked from one message's text parts, all together: each costs the list and its JSON what a MIME part
/// does (render.rs, `MAX_PARTS`), and 31 bytes of mail make one. Blocks past it stay in the text.
const MAX_UNPACKED: usize = 1000;

impl PartPath {
    #[must_use]
    fn top(id: u32) -> Self { Self { ids: vec![id], unpacked: None } }

    /// Part `id` of the message this path's part holds.
    #[must_use]
    fn inside(&self, id: u32) -> Self { Self { ids: [&self.ids[..], &[id]].concat(), unpacked: None } }

    fn unpacked(&self, file: u32) -> Self { Self { ids: self.ids.clone(), unpacked: Some(file) } }

    /// It names a part of a forwarded message, or a file unpacked from one.
    #[must_use]
    pub fn nested(&self) -> bool { self.ids.len() > 1 }

    /// It names a file unpacked from a part, not a part.
    #[must_use]
    pub(crate) fn is_unpacked(&self) -> bool { self.unpacked.is_some() }
}

impl fmt::Display for PartPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<String> = self.ids.iter().map(u32::to_string).collect();
        f.write_str(&ids.join("."))?;
        match self.unpacked {
            Some(n) => write!(f, ".u{n}"),
            None => Ok(()),
        }
    }
}

/// Reads back what `Display` wrote: only a path whose signature held (kubicle-server's sign.rs) is parsed.
impl FromStr for PartPath {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        let pieces: Vec<&str> = text.split('.').collect();
        let unpacked = match pieces.last().and_then(|p| p.split_at_checked(1)) {
            Some(("u", n)) => Some(n.parse().map_err(drop)?),
            _ => None,
        };
        let pieces = if unpacked.is_some() { &pieces[..pieces.len() - 1] } else { &pieces[..] };
        let ids = pieces.iter().map(|p| p.parse::<u32>().map_err(drop)).collect::<Result<Vec<_>, ()>>()?;
        Ok(Self { ids, unpacked })
    }
}

/// The part `path` names, the file unpacked from it aside; `None` when the message has none there.
#[must_use]
pub(crate) fn node_at<'a>(mail: &'a Mail<'a>, path: &PartPath) -> Option<Node<'a>> {
    let (&last, outer) = path.ids.split_last()?;
    let (mut msg, mut prefix) = (mail, String::new());
    for (depth, &id) in outer.iter().enumerate() {
        let path = PartPath { ids: outer[..=depth].to_vec(), unpacked: None };
        prefix = Node { msg, id, path, prefix: prefix.clone(), parents: parents(msg) }.number();
        msg = msg.parts.get(id as usize)?.message()?;
    }
    msg.parts.get(last as usize)?;
    Some(Node { msg, id: last, path: PartPath { ids: path.ids.clone(), unpacked: None }, prefix, parents: parents(msg) })
}

/// Each part's multipart parent in mail-parser's tree and its place there, 1 for the first; `None` for the
/// root. Read once per message, so no part's own work goes over every part of a mail that holds thousands.
fn parents(mail: &Mail) -> Rc<[Option<(usize, usize)>]> {
    let mut parents = vec![None; mail.parts.len()];
    for (p, part) in mail.parts.iter().enumerate() {
        if let PartType::Multipart(children) = &part.body {
            for (n, &child) in children.iter().enumerate() {
                // Children follow their parent in mail-parser's order: a later id never points back up, no loop.
                if let Some(slot) = parents.get_mut(child as usize).filter(|_| child as usize > p) {
                    *slot = Some((p, n + 1));
                }
            }
        }
    }
    parents.into()
}

/// The part or file `path` names, as the walk lists it; `None` when the message has none there. A route serves one
/// part, so it locates it without a walk.
#[must_use]
pub fn locate<'a>(mail: &'a Mail<'a>, path: &PartPath) -> Option<Listed<'a>> {
    let node = node_at(mail, path)?;
    let Some(n) = path.unpacked else { return Some(node.listed(None)) };
    let PartType::Text(text) = &node.part().body else { return None };
    let (name, data) = uudecode(text, MAX_UNPACKED).1.into_iter().nth(n as usize)?;
    Some(Listed::unpacked(path.clone(), name, data))
}

/// Roundcube's `part_image_type`, as far as kubicle serves it (`kubicle_api::image_type`): a raster image's type,
/// also for one sent as `application/octet-stream` with an image's name; `None` for anything else.
#[must_use]
pub fn image_type(content_type: &str, name: &str) -> Option<&'static str> {
    kubicle_api::image_type(if content_type == "application/octet-stream" { type_by_name(name) } else { content_type })
}

/// A file's type by its name's extension: the image types Roundcube's `part_image_type` knows, and for uuencoded
/// files the text and document types a browser shows; anything else `application/octet-stream`.
fn type_by_name(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "txt" => "text/plain",
        "htm" | "html" => "text/html",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Roundcube's `uu_decode` on a plain-text body, by its two patterns: each `begin NNN name` to `` ` `` and `end`
/// block becomes a file and leaves the text, the first `limit` of them. Answers the text left and the files, in
/// order; a file's index is the same for every `limit` that reaches it, so a signed URL's index reads back.
#[must_use]
fn uudecode(text: &str, limit: usize) -> (Cow<'_, str>, Vec<(String, Vec<u8>)>) {
    static BEGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"begin [0-7]{3,4} ([^\r\n]+)\r?\n").expect("a valid pattern"));
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`\r?\nend(\r?\n|$)").expect("a valid pattern"));
    let (mut left, mut files, mut from) = (String::new(), Vec::new(), 0);
    while files.len() < limit
        && let Some(begin) = BEGIN.captures_at(text, from)
    {
        let line = begin.get(0).expect("the match");
        let Some(end) = END.find_at(text, line.start()) else { break };
        let encoded = text.get(line.end()..end.start().saturating_sub(1)).unwrap_or_default().replace("\r\n", "\n");
        files.push((render::safe_filename(&begin[1]), uu_bytes(&encoded))); // as a MIME part's name: no `/`, no bidi
        left.push_str(&text[from..line.start()]);
        from = end.end();
    }
    if files.is_empty() {
        return (Cow::Borrowed(text), files);
    }
    left.push_str(&text[from..]);
    (Cow::Owned(left), files)
}

/// PHP's `convert_uudecode`: each line's first character says how many bytes its groups of four characters hold.
fn uu_bytes(encoded: &str) -> Vec<u8> {
    let decode = |c: u8| c.wrapping_sub(b' ') & 0o77;
    let mut out = Vec::new();
    for line in encoded.split('\n').map(str::as_bytes) {
        let Some((&count, data)) = line.split_first() else { continue };
        let count = usize::from(decode(count));
        if count == 0 {
            break;
        }
        let mut bytes = Vec::with_capacity(count + 2);
        for group in data.chunks(4) {
            let c: Vec<u8> = (0..4).map(|i| decode(group.get(i).copied().unwrap_or(b'`'))).collect();
            bytes.extend([(c[0] << 2) | (c[1] >> 4), (c[1] << 4) | (c[2] >> 2), (c[2] << 6) | c[3]]);
        }
        bytes.truncate(count);
        out.extend(bytes);
    }
    out
}

/// One part of a message, with where it sits.
#[derive(Clone)]
pub struct Node<'a> {
    /// The message, or forwarded message, that holds it.
    pub msg: &'a Mail<'a>,
    pub id: u32,
    pub path: PartPath,
    /// The IMAP number of the forwarded message part that holds `msg`, empty at the top.
    prefix: String,
    /// `parents` of `msg`, shared by all its nodes.
    parents: Rc<[Option<(usize, usize)>]>,
}

impl<'a> Node<'a> {
    // Indexes unchecked: a node is made only for ids the message has.
    pub(crate) fn part(&self) -> &'a MessagePart<'a> { &self.msg.parts[self.id as usize] }

    /// Its `type/subtype` in lower case (`render::content_type`).
    #[must_use]
    pub fn mimetype(&self) -> String { render::content_type(self.part()) }

    /// Its IMAP part number, as Roundcube names a file without a name ("Part 2.1"): its place in each multipart
    /// parent, outward, after `prefix`; 1 for a message's root.
    fn number(&self) -> String {
        let (mut places, mut at) = (Vec::new(), self.id as usize);
        while let Some(&Some((parent, n))) = self.parents.get(at) {
            places.push(n.to_string());
            at = parent;
        }
        places.reverse();
        let own = if places.is_empty() { "1".to_owned() } else { places.join(".") };
        if self.prefix.is_empty() { own } else { format!("{}.{own}", self.prefix) }
    }

    /// Roundcube's disposition: `inline` or `attachment`, any other word read as `attachment` (RFC 2183), and
    /// `inline` for a part with a Content-ID and none.
    fn disposition(&self) -> Option<&'static str> {
        match self.part().content_disposition() {
            Some(d) if d.ctype().eq_ignore_ascii_case("inline") => Some("inline"),
            Some(d) if !d.ctype().is_empty() => Some("attachment"),
            _ => self.part().content_id().map(|_| "inline"),
        }
    }

    fn filename(&self) -> Option<&'a str> { self.part().attachment_name().filter(|n| !n.trim().is_empty()) }

    /// An invite Roundcube names `calendar.ics` (`rcube_message.php`, #1490325): a `text/calendar` part with no name,
    /// disposition or Content-ID outside a multipart/alternative, where it would be the event's other form.
    fn unnamed_invite(&self) -> bool {
        let in_alternative =
            self.parents[self.id as usize].is_some_and(|(p, _)| render::content_type(&self.msg.parts[p]) == "multipart/alternative");
        self.mimetype() == "text/calendar" && self.filename().is_none() && self.disposition().is_none() && !in_alternative
    }

    /// Its name as the list shows it and a download saves it.
    pub(crate) fn name(&self) -> String {
        if self.unnamed_invite() { "calendar.ics".into() } else { render::name_of(self.part(), &self.number()) }
    }

    fn size(&self) -> usize { render::bytes(self.msg, self.part()).len() }

    pub(crate) fn section(&self) -> Section { Section { number: self.number(), size: self.size() } }

    fn children(&self) -> Vec<Self> {
        let PartType::Multipart(ids) = &self.part().body else { return Vec::new() };
        // A child follows its parent.
        ids.iter().filter(|&&id| id > self.id && (id as usize) < self.msg.parts.len()).map(|&id| self.sibling(id)).collect()
    }

    /// The path of another part of the same message.
    #[must_use]
    pub(crate) fn path_of(&self, id: u32) -> PartPath { self.sibling(id).path }

    /// Another part of the same message.
    pub(crate) fn sibling(&self, id: u32) -> Self {
        let mut ids = self.path.ids.clone();
        if let Some(last) = ids.last_mut() {
            *last = id;
        }
        Self { msg: self.msg, id, path: PartPath { ids, unpacked: None }, prefix: self.prefix.clone(), parents: Rc::clone(&self.parents) }
    }

    /// As a listed file; `parent` is the type of the multipart it sits in, where Roundcube can tell it.
    fn listed(&self, parent: Option<&str>) -> Listed<'a> {
        let part = self.part();
        let layout_image = part.content_id().is_some() && self.disposition() == Some("inline") || parent == Some("multipart/related");
        Listed {
            path: self.path.clone(),
            name: self.name(),
            content_type: self.mimetype(),
            data: Cow::Borrowed(render::bytes(self.msg, part)),
            section: Some(self.section()),
            layout_image,
            named: self.filename().is_some() || self.unnamed_invite(),
            disposition: self.disposition(),
        }
    }
}

/// Whether the mail has an HTML part outside attachments and forwarded messages, Roundcube's `has_html_part`.
#[must_use]
pub fn has_html(mail: &Mail) -> bool {
    mail.parts.iter().any(|part| {
        render::content_type(part) == "text/html"
            && part.attachment_name().is_none_or(|n| n.trim().is_empty())
            && !render::bytes(mail, part).is_empty()
    })
}

/// What the reader shows, in order.
pub enum Content<'a> {
    /// A forwarded message's headers.
    Headers(&'a Mail<'a>),
    /// A part shown as its type says: plain text, HTML, enriched text, or a delivery report as text. `text`, when
    /// set, stands for a plain part's own text: what is left once uuencoded files are cut out.
    Part { node: Node<'a>, text: Option<String> },
    /// Where an encrypted part stands.
    Encrypted,
}

/// A file the reader lists, with its bytes: the walk's own list is what a forward or a reopened draft carries.
#[derive(Clone)]
pub struct Listed<'a> {
    pub path: PartPath,
    pub name: String,
    /// Its declared type, cleaned by Roundcube's `fix_mimetype`.
    pub content_type: String,
    /// Decoded: borrowed from the parse for a part, decoded by the walk for a file unpacked from one.
    pub data: Cow<'a, [u8]>,
    /// Where an inline image's request fetches it alone; `None` for a file unpacked from a part.
    pub section: Option<Section>,
    /// An image that only the mail shows, not the list (Roundcube's `is_attachment` false): one with a Content-ID
    /// and an inline disposition, or one in a multipart/related below the top.
    pub layout_image: bool,
    /// The sender gave it a name, or Roundcube does (`calendar.ics`).
    named: bool,
    /// `inline` or `attachment`, as `Node::disposition` reads it; `None` for a file unpacked from a part.
    disposition: Option<&'static str>,
}

impl Listed<'_> {
    /// A file uuencoded in a text part (`uudecode`).
    fn unpacked(path: PartPath, name: String, data: Vec<u8>) -> Self {
        let content_type = type_by_name(&name).into();
        Self { path, name, content_type, data: Cow::Owned(data), section: None, layout_image: false, named: true, disposition: None }
    }

    /// Whether a forward or a reopened draft carries it, by Roundcube's `write_compose_attachments` for its plain text
    /// editor: a file marked `attachment`, named, or a forwarded message; and in a mail with HTML no inline part but a
    /// PDF, which Apple Mail marks inline.
    #[must_use]
    pub fn carried(&self, has_html: bool) -> bool {
        (self.disposition == Some("attachment") || self.named || self.content_type == "message/rfc822")
            && !(has_html && self.disposition == Some("inline") && self.content_type != "application/pdf")
    }
}

/// The walk's result: what shows and what is listed.
#[derive(Default)]
pub struct Structure<'a> {
    pub contents: Vec<Content<'a>>,
    pub listed: Vec<Listed<'a>>,
    /// The paths in `listed`.
    attached: HashSet<PartPath>,
    /// Parts the HTML can name (`cid:`, `Content-Location`) in related or mixed parts, Roundcube's
    /// `inline_parts`, until the end of a related or mixed part decides whether each is listed too.
    inline: Vec<Listed<'a>>,
    got_html: bool,
    /// How deep the walk is in nested multiparts and messages, which `MAX_NESTING` bounds.
    depth: usize,
    /// Files unpacked so far, which `MAX_UNPACKED` bounds.
    unpacked: usize,
    prefer_html: bool,
    parse_alternative: bool,
}

impl<'a> Structure<'a> {
    /// `mail` sorted for showing as HTML when `prefer_html`, else as text.
    #[must_use]
    pub fn of(mail: &'a Mail<'a>, prefer_html: bool) -> Self {
        let mut walk = Self { prefer_html, ..Self::default() };
        if !mail.parts.is_empty() {
            let root = Node { msg: mail, id: 0, path: PartPath::top(0), prefix: String::new(), parents: parents(mail) };
            // The top multipart's own parts have no parent Roundcube can find: its part numbers have no dot.
            walk.sort(&root, &root.mimetype(), root.children(), false, None);
        }
        walk
    }

    /// Lists a part once, in the order first listed, as Roundcube keys its list by part number.
    fn attach(&mut self, listed: Listed<'a>) {
        if self.attached.insert(listed.path.clone()) {
            self.listed.push(listed);
        }
    }

    fn content(&mut self, node: &Node<'a>, text: Option<String>) { self.contents.push(Content::Part { node: node.clone(), text }); }

    /// `parse_structure` for a part, or for a forwarded message (`recursive`), whose `mimetype` is its body's and
    /// whose `children` are its body's parts; `parent` is the type `is_attachment` reads for those parts.
    fn sort(&mut self, node: &Node<'a>, mimetype: &str, children: Vec<Node<'a>>, recursive: bool, parent: Option<&str>) {
        if self.depth == MAX_NESTING {
            return;
        }
        self.depth += 1;
        self.sort_at(node, mimetype, children, recursive, parent);
        self.depth -= 1;
    }

    fn sort_at(&mut self, node: &Node<'a>, mimetype: &str, children: Vec<Node<'a>>, recursive: bool, parent: Option<&str>) {
        let (primary, secondary) = mimetype.split_once('/').unwrap_or((mimetype, ""));
        if primary == "text" && !recursive {
            if !matches!(secondary, "plain" | "html" | "enriched") {
                return self.attach(node.listed(None));
            }
            let PartType::Text(text) = &node.part().body else { return self.content(node, None) };
            if secondary != "plain" {
                return self.content(node, None);
            }
            let (left, files) = uudecode(text, MAX_UNPACKED - self.unpacked);
            self.unpacked += files.len();
            self.content(node, matches!(left, Cow::Owned(_)).then(|| left.into_owned()));
            for (n, (name, data)) in (0u32..).zip(files) {
                self.attach(Listed::unpacked(node.path.unpacked(n), name, data));
            }
        } else if mimetype == "application/pgp" && !recursive {
            self.content(node, None);
        } else if mimetype == "multipart/alternative" && children.len() > 1 {
            self.alternative(children, parent);
        } else if mimetype == "multipart/encrypted" {
            self.contents.push(Content::Encrypted);
            for child in children.iter().filter(|c| c.mimetype() == "application/octet-stream" || c.filename().is_some()) {
                self.attach(child.listed(parent));
            }
        } else if mimetype == "application/pkcs7-mime" {
            self.contents.push(Content::Encrypted);
            if node.filename().is_some() {
                self.attach(node.listed(None));
            }
        } else if !children.is_empty() {
            self.parts(mimetype, children, parent);
        } else if node.filename().is_some() || primary == "application" {
            self.attach(node.listed(None));
        }
    }

    /// multipart/alternative: the HTML, enriched or plain part by the preference, a related part walked first.
    fn alternative(&mut self, children: Vec<Node<'a>>, parent: Option<&str>) {
        let (mut plain, mut html, mut enriched, mut related) = (None, None, None, None);
        for child in children {
            let mimetype = child.mimetype();
            let multipart =
                ["multipart/related", "multipart/relative", "multipart/mixed", "multipart/alternative"].contains(&mimetype.as_str());
            if child.size() == 0 && !multipart {
                continue;
            }
            match mimetype.as_str() {
                _ if multipart => related = Some(child),
                "text/plain" if plain.is_none() => plain = Some(child),
                "text/html" if html.is_none() => {
                    html = Some(child);
                    self.got_html = true;
                }
                "text/enriched" if enriched.is_none() => enriched = Some(child),
                _ => self.attach(child.listed(parent)),
            }
        }
        if let Some(related) = related.filter(|_| !self.parse_alternative) {
            self.parse_alternative = true;
            let (mimetype, children) = (related.mimetype(), related.children());
            self.sort(&related, &mimetype, children, true, Some(&mimetype));
            self.parse_alternative = false;
            if self.prefer_html && !self.contents.is_empty() {
                plain = None;
            }
        }
        let shown = html.clone().filter(|_| self.prefer_html).or(enriched).or(plain);
        match (shown, html) {
            (Some(part), _) => self.content(&part, None),
            // Roundcube adds a stand-in here and shows nothing for it; the HTML shows as text instead, as an HTML
            // mail does with Display HTML off (docs/design.md, Reader).
            (None, Some(html)) if self.contents.is_empty() => self.content(&html, None),
            (None, _) => {}
        }
    }

    /// The parts of a multipart of type `mimetype`, or of a forwarded message's body, each by its type and
    /// disposition.
    fn parts(&mut self, mimetype: &str, children: Vec<Node<'a>>, parent: Option<&str>) {
        let related_or_mixed = ["multipart/related", "multipart/relative", "multipart/mixed"].contains(&mimetype);
        for child in children {
            let child_type = child.mimetype();
            let (primary, secondary) = child_type.split_once('/').unwrap_or((&child_type, ""));
            let disposition = child.disposition();
            let cid = child.part().content_id().is_some();
            if let Some(nested) = child.part().message() {
                self.attach(child.listed(parent)); // a forwarded message is listed as well
                self.message(&child, nested);
            } else if primary == "multipart" {
                self.sort(&child, &child_type, child.children(), true, Some(&child_type));
            } else if child_type == "message/rfc822" {
                self.attach(child.listed(parent)); // one mail-parser could not read
            } else if (matches!(child_type.as_str(), "text/plain" | "text/html") && disposition != Some("attachment"))
                || ["message/delivery-status", "text/rfc822-headers", "message/disposition-notification"].contains(&child_type.as_str())
            {
                self.got_html |= child_type == "text/html" && child.size() > 0;
                if !self.parse_alternative || secondary == "html" && self.prefer_html || secondary == "plain" && !self.prefer_html {
                    self.content(&child, None);
                }
                if child.filename().is_some() {
                    self.attach(child.listed(parent));
                }
            } else if disposition.is_some() || cid || child.filename().is_some() {
                if mimetype == "multipart/appledouble" && secondary == "applefile" {
                    continue; // an Apple resource fork
                }
                if related_or_mixed && (cid || child.part().content_location().is_some()) {
                    self.inline.push(child.listed(parent));
                }
                if disposition != Some("inline") || !cid {
                    self.attach(child.listed(parent));
                }
            } else if child_type == "text/calendar" {
                self.attach(child.listed(parent));
            } else if mimetype == "multipart/mixed" && disposition != Some("inline") && !matches!(primary, "" | "text" | "multipart") {
                self.attach(child.listed(parent)); // Roundcube's last resort
            }
        }
        // Outlook sends files as related parts: listed when the mail has no HTML, or when they are not images.
        if related_or_mixed {
            let images = ["image/gif", "image/jpeg", "image/jpg", "image/png", "image/tiff", "image/bmp", "image/svg"];
            let image = |t: &str| images.iter().any(|i| t.starts_with(i));
            // Each is decided once: `got_html` only turns on, so a part left out now stays left out.
            let files: Vec<_> = self.inline.drain(..).filter(|i| i.named && (!self.got_html || !image(&i.content_type))).collect();
            for file in files {
                self.attach(file);
            }
        }
    }

    /// The forwarded message `path` names, sorted as a page of its own shows it: its body without its headers.
    #[must_use]
    pub(crate) fn of_message(mail: &'a Mail<'a>, path: &PartPath, prefer_html: bool) -> Option<(Self, &'a Mail<'a>)> {
        let node = node_at(mail, path).filter(|_| !path.is_unpacked())?;
        let nested = node.part().message()?;
        let mut walk = Self { prefer_html, ..Self::default() };
        walk.body_of(&node, nested);
        Some((walk, nested))
    }

    /// A forwarded message: its headers, then its body's parts as a multipart's.
    fn message(&mut self, part: &Node<'a>, nested: &'a Mail<'a>) {
        if nested.header_raw("Subject").is_some() || nested.from().is_some() || nested.to().is_some() {
            self.contents.push(Content::Headers(nested));
        }
        self.body_of(part, nested);
    }

    fn body_of(&mut self, part: &Node<'a>, nested: &'a Mail<'a>) {
        if nested.parts.is_empty() {
            return;
        }
        let root = Node { msg: nested, id: 0, path: part.path.inside(0), prefix: part.number(), parents: parents(nested) };
        let mimetype = root.mimetype();
        // Under `recursive` only `parts` shows text, so a single-part body goes as the one child, as Roundcube's
        // message/rfc822 structure holds it.
        let children = if matches!(root.part().body, PartType::Multipart(_)) { root.children() } else { vec![root] };
        self.sort(part, &mimetype, children, true, Some("message/rfc822")); // the message/rfc822 part is their parent, for `is_attachment`
    }
}
