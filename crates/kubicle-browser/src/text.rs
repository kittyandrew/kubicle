//! Small text helpers: addresses and counts.

use kubicle_api::{Address, Draft};

/// The name when there is one, else the address.
#[must_use]
pub fn who(a: &Address) -> &str { a.name.as_deref().unwrap_or(&a.email) }

/// The name the reader shows for an address, as Roundcube's `address_string`: none when it holds an `@`, unless
/// `show_email` puts the address beside it, as `"support@bank.example" <x@phish.example>` would pass for the bank.
#[must_use]
pub fn reader_name(a: &Address, show_email: bool) -> Option<&str> {
    let looks_like_address = |n: &str| n != a.email && n.contains(['@', '\u{FF20}', '\u{FE6B}']);
    a.name.as_deref().filter(|n| show_email || !kubicle_api::valid_email(&a.email) || !looks_like_address(n))
}

/// What the reader writes for an address: the name, or the address, or both as `Name <email>` with `show_email` and
/// for an address that is not valid, whose name alone, `boss@example.com` say, would hide it. Compose's chips too.
#[must_use]
pub fn reader_label(a: &Address, show_email: bool) -> String {
    let invalid = !a.email.is_empty() && !kubicle_api::valid_email(&a.email);
    match reader_name(a, show_email) {
        Some(name) if show_email || invalid => format!("{name} <{}>", a.email),
        Some(name) => name.to_owned(),
        None => a.email.clone(),
    }
}

pub fn address_list(list: &[Address]) -> String { list.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ") }

/// Splits on commas and semicolons outside double quotes, trimmed, with empty entries dropped.
fn split_addresses(text: &str) -> Vec<&str> {
    let (mut out, mut start, mut quoted) = (Vec::new(), 0, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// What the user typed into a To, Cc or Bcc field. Lenient: a draft keeps half-typed text as it is.
#[must_use]
pub fn parse_addresses(text: &str) -> Vec<Address> {
    let address = |item: &str| match (item.rfind('<'), item.rfind('>')) {
        (Some(open), Some(close)) if open < close => {
            let name = item[..open].trim().trim_matches('"').trim();
            Address { name: (!name.is_empty()).then(|| name.to_owned()), email: item[open + 1..close].trim().to_owned() }
        }
        _ => Address { name: None, email: item.to_owned() },
    };
    split_addresses(text).into_iter().map(address).collect()
}

/// The text up to the last separator outside quotes, and the token being typed after it.
#[must_use]
pub fn last_token(text: &str) -> (&str, &str) {
    let (mut quoted, mut cut) = (false, 0);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' | ';' if !quoted => cut = i + 1,
            _ => {}
        }
    }
    (&text[..cut], text[cut..].trim_start())
}

#[must_use]
pub fn count(n: u32) -> String { js_sys::Number::from(n).to_locale_string("en-US").into() }

/// The first letter or digit of the name the reader shows, for the avatar.
#[must_use]
pub fn initial(a: &Address) -> String {
    let shown = reader_name(a, false).unwrap_or(&a.email);
    shown.chars().find(|c| c.is_alphanumeric()).map_or_else(|| "?".into(), |c| c.to_uppercase().collect())
}

/// Fills a new draft from a `mailto:` URI (RFC 6068): the addresses before `?`, then the fields Roundcube's
/// compose takes, `to`, `cc`, `bcc`, `reply-to`, `in-reply-to`, `references`, `subject` and `body`; others are
/// ignored. `+` stays a plus, as the RFC says. Addresses join the fields, so the identity's Bcc and Reply-To stay.
pub fn fill_from_mailto(draft: &mut Draft, uri: &str) {
    let decode = |s: &str| js_sys::decode_uri_component(s).ok().map_or_else(|| s.to_owned(), String::from);
    let rest = uri.get(7..).filter(|_| uri.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:"))).unwrap_or("");
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    draft.to = parse_addresses(&decode(to));
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        let value = decode(value);
        match key.to_ascii_lowercase().as_str() {
            "to" => join(&mut draft.to, &value),
            "cc" => join(&mut draft.cc, &value),
            "bcc" => join(&mut draft.bcc, &value),
            "reply-to" => join(&mut draft.reply_to, &value),
            "in-reply-to" => draft.in_reply_to = message_ids(&value).next(),
            "references" => draft.references = message_ids(&value).collect(),
            "subject" => draft.subject = value,
            "body" => draft.body = value,
            _ => {}
        }
    }
}

fn join(field: &mut Vec<Address>, value: &str) {
    for address in parse_addresses(value) {
        if !field.iter().any(|a| a.email.eq_ignore_ascii_case(&address.email)) {
            field.push(address);
        }
    }
}

/// Message-IDs as a header lists them, without their angle brackets.
fn message_ids(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split_whitespace().map(|id| id.trim_matches(['<', '>']).to_owned()).filter(|id| !id.is_empty())
}

/// An address field's text as Elastic's recipient input splits it (`recipient_input_parser`): new lines count as
/// commas. An entry of space-separated addresses gives each one; else words before a last address are its name.
/// Anything else is handed back as text, for the field to keep as typed.
#[must_use]
pub fn recipients(text: &str) -> (Vec<Address>, String) {
    let text = text.replace("\r\n", "\n").replace('\n', ",");
    let (mut found, mut rest) = (Vec::new(), Vec::new());
    let address = |word: &str| {
        let word = word.trim_matches(['<', '>', ',', ';']);
        word.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty()).then(|| word.to_owned())
    };
    for item in split_addresses(&text) {
        if item.contains('<') && item.contains('>') {
            let parsed = parse_addresses(item);
            if parsed.iter().all(|a| address(&a.email).is_some()) {
                found.extend(parsed);
                continue;
            }
        }
        let words: Vec<&str> = item.split_whitespace().collect();
        if words.iter().all(|w| address(w).is_some()) {
            found.extend(words.iter().filter_map(|w| address(w)).map(|email| Address { name: None, email }));
        } else if let Some(email) = words.last().and_then(|w| address(w)) {
            let name = words[..words.len() - 1].join(" ").trim_matches('"').trim().to_owned();
            found.push(Address { name: (!name.is_empty()).then_some(name), email });
        } else {
            rest.push(item);
        }
    }
    (found, rest.join(", "))
}
