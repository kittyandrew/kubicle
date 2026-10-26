//! IMAP's modified UTF-7 for mailbox names (RFC 3501, 5.1.3): printable ASCII stands for itself, `&` is `&-`,
//! and everything else is UTF-16 in base64 with `,` for `/`, between `&` and `-`.

use base64::{Engine, alphabet::IMAP_MUTF7, engine::DecodePaddingMode, engine::GeneralPurpose, engine::GeneralPurposeConfig};

const MUTF7: GeneralPurpose = GeneralPurpose::new(
    &IMAP_MUTF7,
    GeneralPurposeConfig::new().with_encode_padding(false).with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

pub fn encode(name: &str) -> String {
    let (mut out, mut run) = (String::with_capacity(name.len()), Vec::<u16>::new());
    for ch in name.chars() {
        if (' '..='~').contains(&ch) {
            flush(&mut run, &mut out);
            out.push(ch);
            if ch == '&' {
                out.push('-');
            }
        } else {
            run.extend(ch.encode_utf16(&mut [0; 2]).iter());
        }
    }
    flush(&mut run, &mut out);
    out
}

fn flush(run: &mut Vec<u16>, out: &mut String) {
    if run.is_empty() {
        return;
    }
    let bytes: Vec<u8> = run.drain(..).flat_map(u16::to_be_bytes).collect();
    out.push('&');
    MUTF7.encode_string(bytes, out);
    out.push('-');
}

/// `None` for a name that is not valid modified UTF-7, which Dovecot never sends.
pub fn decode(raw: &[u8]) -> Option<String> {
    let (mut out, mut rest) = (String::with_capacity(raw.len()), raw);
    while let Some((&byte, tail)) = rest.split_first() {
        if !(0x20..=0x7e).contains(&byte) {
            return None;
        }
        if byte != b'&' {
            out.push(char::from(byte));
            rest = tail;
            continue;
        }
        let end = tail.iter().position(|&b| b == b'-')?;
        if end == 0 {
            out.push('&');
        } else {
            let bytes = MUTF7.decode(&tail[..end]).ok()?;
            let (pairs, []) = bytes.as_chunks::<2>() else { return None };
            let units = pairs.iter().map(|pair| u16::from_be_bytes(*pair));
            for unit in char::decode_utf16(units) {
                let ch = unit.ok()?;
                if (' '..='~').contains(&ch) {
                    return None; // printable ASCII must stand for itself, so this name has two spellings
                }
                out.push(ch);
            }
        }
        rest = &tail[end + 1..];
    }
    Some(out)
}
