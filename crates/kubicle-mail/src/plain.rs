//! Plain text as Roundcube shows it (`rcube_text2html` with `rcmail_string_replacer`) in a mail document:
//! format=flowed unfolded, quote marks read into depths, the signature found, links and addresses picked out.

use std::{borrow::Cow, fmt::Write as _, sync::LazyLock};

use regex::bytes::Regex;

use crate::{links::enc, render::Escaped};

/// Roundcube's `sig_max_lines`: a `-- ` line with this many lines or more after it is not a signature.
const SIGNATURE_MAX_LINES: usize = 15;

/// One line: its quote depth, whether it belongs to a unified diff (which never wraps), and its text.
type Line = (u8, bool, String);

/// `text`, as `render::content_text` reads it, drawn into a mail document as Roundcube's `text2html` writes it: a
/// `div.pre` of lines, nested `blockquote`s for quote depths, the signature's lines muted (`.sig`). Links open in a
/// new tab; an address opens kubicle's own Compose there, as a `mailto:` link in HTML mail does (render.rs).
/// `prefix` is the app's address.
pub fn html(text: &str, prefix: &str, out: &mut String) {
    let mut diff = false; // inside a unified diff
    let mut lines: Vec<Line> = Vec::new();
    for raw in text.split('\n') {
        let (depth, rest) = quote_marks(raw);
        let rest = rest.replace('\r', "").replace('\t', "    ");
        // A unified diff keeps its lines whole until a line that cannot belong to it (`convert_line`).
        let belongs = rest.is_empty() || rest == "-- " || rest.starts_with([' ', '-', '+', '@']);
        diff = if diff { belongs } else { diff_start(&rest) };
        lines.push((depth, diff && !rest.is_empty() && rest != "-- ", rest));
    }
    let signature = signature(&lines).unwrap_or(lines.len());
    out.push_str("<div class=\"pre\">");
    let mut depth = 0;
    for (n, (level, nowrap, rest)) in lines.iter().enumerate() {
        out.push_str(&"<blockquote>".repeat(level.saturating_sub(depth).into()));
        out.push_str(&"</blockquote>".repeat(depth.saturating_sub(*level).into()));
        depth = *level;
        let (nowrap, sig) = (if *nowrap { " nowrap" } else { "" }, if n >= signature { " sig" } else { "" });
        let _ = write!(out, "<div class=\"line{nowrap}{sig}\">");
        if rest.is_empty() {
            out.push_str("<br>");
        }
        links(rest, prefix, out);
        out.push_str("</div>");
    }
    let _ = write!(out, "{}</div>", "</blockquote>".repeat(depth.into()));
}

/// Roundcube's `/^(>+ {0,1})+/`: the quote marks, each run of them followed by at most one space, and the rest.
fn quote_marks(line: &str) -> (u8, &str) {
    static MARKS: LazyLock<Regex> = LazyLock::new(|| Regex::new("^(?:>+ ?)*").expect("a valid pattern"));
    let end = MARKS.find(line.as_bytes()).map_or(0, |m| m.end());
    (u8::try_from(line[..end].matches('>').count()).unwrap_or(u8::MAX), &line[end..])
}

/// Roundcube's start of a unified diff (`convert_line`).
fn diff_start(line: &str) -> bool {
    static DIFF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u)^(?:--- \S|\+\+\+ \S|@@ [0-9 ,+-]+ @@)").expect("a valid pattern"));
    DIFF.is_match(line.as_bytes())
}

/// The last `-- ` line that starts a line of Roundcube's joined text, with fewer than `SIGNATURE_MAX_LINES` line
/// breaks after it. Roundcube joins a line whose quote depth differs from the one before onto that line, so such a
/// line never starts one, and only the breaks left between lines count.
fn signature(lines: &[Line]) -> Option<usize> {
    let starts = |n: usize| if n == 0 { lines[0].0 == 0 } else { lines[n].0 == lines[n - 1].0 };
    // A joined line still gets a break of its own after a line of nothing but spaces (`$length`).
    let breaks_before = |n: usize| usize::from(starts(n) || lines[n - 1].2.trim_matches(' ').is_empty());
    let found = (0..lines.len().saturating_sub(1)).rev().find(|&n| starts(n) && lines[n].2 == "-- ")?;
    let breaks: usize = (found + 1..lines.len()).map(breaks_before).sum();
    (breaks < SIGNATURE_MAX_LINES).then_some(found)
}

/// Roundcube's `$utf_domain`. Its patterns run without `/u`, so they match bytes: a byte beyond ASCII counts as a
/// letter, and a top-level domain of one Cyrillic letter has the two it needs.
const DOMAIN: &str = r#"[^?&@"'/()<>\s\r\t\n]+\.?([^\x00-\x2f\x3b-\x40\x5b-\x60\x7b-\x7f]{2,}|xn--[a-zA-Z0-9]{2,})"#;

/// Roundcube's `link_pattern`: `scheme://`, or `www.` at the start or after a `$noword` character, then a domain
/// and a path.
static LINK: LazyLock<Regex> = LazyLock::new(|| {
    let url2 = r"a-zA-Z0-9%=#$@+?|!&/_~\[\](){}*\x80-\xFE-";
    let pattern = format!(r"(?-u)(\w+://|[^\w@.#-][Ww][Ww][Ww]\.|^[Ww][Ww][Ww]\.){DOMAIN}([.:;,]*[{url2}]+)*");
    Regex::new(&pattern).expect("a valid pattern")
});

/// Roundcube's `mailto_pattern`: a local part, `@`, a domain and an optional `?query`.
static MAILTO: LazyLock<Regex> = LazyLock::new(|| {
    let atom = r"[-\w!#$%&*+~/^`|{}=]+";
    Regex::new(&format!(r"(?-u){atom}(\.{atom})*@{DOMAIN}(\?[^<>\s]+)?")).expect("a valid pattern")
});

/// A line's text with its links and addresses as Roundcube's replacer finds them: links first, then addresses in
/// the text between them. A scheme other than http, ftp or file (with an `s`) is matched but not linked. What
/// `brackets` cuts off a match stays text that neither pattern sees again.
fn links(line: &str, prefix: &str, out: &mut String) {
    let mut from = 0;
    // `find_iter` and the prefix read back: `captures_iter` ran 25 times slower on a hostile 2 MB line.
    for all in LINK.find_iter(line.as_bytes()) {
        let head = &line.as_bytes()[all.start()..];
        let word = head.iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
        let scheme = head[word..].starts_with(b"://").then(|| head[..word].to_ascii_lowercase());
        if scheme.as_ref().is_some_and(|s| !matches!(s.as_slice(), b"http" | b"https" | b"ftp" | b"ftps" | b"file" | b"files")) {
            continue; // stays text, which the address pattern still sees
        }
        // A `www.` after a `$noword` character leaves that character as text.
        let start = all.start() + usize::from(scheme.is_none() && !head[0].eq_ignore_ascii_case(&b'w'));
        let (url, end) = brackets(&line[start..all.end()], start);
        emails(&line[from..start], prefix, out);
        let href = if scheme.is_some() { Cow::Borrowed(url) } else { Cow::Owned(format!("http://{url}")) };
        let _ = write!(out, "<a target=\"_blank\" rel=\"noreferrer\" href=\"{}\">{}</a>", Escaped(&href), Escaped(url));
        let _ = write!(out, "{}", Escaped(&line[end..all.end()]));
        from = all.end();
    }
    emails(&line[from..], prefix, out);
}

/// Roundcube's `parse_url_brackets`: a URL ends at a bracket, then a parenthesis, that would close what it never
/// opened or open a second; `offset` is the URL's place in its line, to answer where it now ends.
fn brackets(url: &str, offset: usize) -> (&str, usize) {
    let mut url = url;
    for (open, close) in [('[', ']'), ('(', ')')] {
        let mut inside = false;
        let cut = url.char_indices().find_map(|(i, c)| {
            let stray = (c == open && inside) || (c == close && !inside); // a second opener, or a closer of nothing
            if c == open || c == close {
                inside = c == open;
            }
            stray.then_some(i)
        });
        url = cut.map_or(url, |cut| &url[..cut]);
    }
    (url, offset + url.len())
}

/// Roundcube's `mailto_pattern` over text without links, an address linked only when the part before its query
/// passes `valid_email`.
fn emails(text: &str, prefix: &str, out: &mut String) {
    let mut from = 0;
    for found in MAILTO.find_iter(text.as_bytes()) {
        let (address, end) = brackets(&text[found.start()..found.end()], found.start());
        if !kubicle_api::valid_email(address.split('?').next().unwrap_or_default()) {
            continue;
        }
        let href = enc(&format!("mailto:{address}"));
        let (before, link, after) = (Escaped(&text[from..found.start()]), Escaped(address), Escaped(&text[end..found.end()]));
        let _ = write!(out, "{before}<a target=\"_blank\" href=\"{prefix}#compose={}\">{link}</a>{after}", Escaped(&href));
        from = found.end();
    }
    let _ = write!(out, "{}", Escaped(&text[from..]));
}
