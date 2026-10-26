//! RFC 3676 format=flowed as Roundcube's `rcube_mime` reads and writes it: `unfold` joins a received part's soft
//! line breaks back into paragraphs, for a quote or a reopened draft; `wrap` writes sent text at 72 columns with
//! soft breaks, which flowed readers join again.

/// Roundcube's `unfold_flowed`: a line ending in a space continues on the next line of the same quote depth, and
/// each line loses one space of stuffing. With `delsp` (DelSp=yes) the space at the join goes too. The `-- `
/// signature line and a line of bare quote marks never join.
#[must_use]
pub fn unfold(text: &str, delsp: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0; // the previous line's
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        let quotes = line.bytes().take_while(|&b| b == b'>').count();
        let soft = out.last().is_some_and(|last| last.ends_with(' '));
        let joins = if quotes > 0 {
            let bare = out.last().is_some_and(|last| matches!(last.trim_start_matches('>'), "" | " "));
            let rest = &line[quotes..];
            (quotes == depth && soft && !bare).then(|| rest.strip_prefix(' ').unwrap_or(rest))
        } else if line == "-- " {
            None
        } else {
            let rest = line.strip_prefix(' ').unwrap_or(line);
            (depth == 0 && soft && !rest.is_empty() && out.last().is_some_and(|last| last != "-- ")).then_some(rest)
        };
        match (joins, out.last_mut()) {
            (Some(rest), Some(last)) => {
                if delsp {
                    last.pop();
                }
                last.push_str(rest);
            }
            _ if quotes == 0 && line != "-- " => out.push(line.strip_prefix(' ').unwrap_or(line).to_owned()),
            _ => out.push(line.to_owned()), // a quoted line keeps its marks and stuffing until a join
        }
        depth = quotes;
    }
    out.join("\n")
}

/// Roundcube's `format_flowed` at its width of 74: a line breaks at its last space within 72 characters, the space
/// left at the line end as the soft break; a quoted line rewraps at 72 less its depth under the same `>` marks; a
/// line that would start with `From`, a space or `>` gets a stuffing space. The `-- ` line stays as it is.
#[must_use]
pub fn wrap(text: &str) -> String {
    let line = |line: &str| {
        if line == "-- " || line.is_empty() {
            return line.to_owned();
        }
        let depth = line.bytes().take_while(|&b| b == b'>').count();
        if depth == 0 {
            let stuff = |l: String| if l.starts_with("From") || l.starts_with([' ', '>']) { format!(" {l}") } else { l };
            return words(line.trim_end(), 72).into_iter().map(stuff).collect::<Vec<_>>().join(" \r\n");
        }
        let rest = line[depth..].trim_end_matches(' ');
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        let marks = ">".repeat(depth);
        if rest.is_empty() {
            return marks; // Roundcube writes "> ", a soft break that would join the quote's paragraphs in Thunderbird
        }
        format!("{marks} {}", words(rest, 72_usize.saturating_sub(depth)).join(&format!(" \r\n{marks} ")))
    };
    text.split('\n').map(|l| line(l.strip_suffix('\r').unwrap_or(l))).collect::<Vec<_>>().join("\r\n")
}

/// Roundcube's `wordwrap` for one line, counting characters: pieces of at most `width` broken at spaces, the
/// breaking space dropped. A word longer than `width` stays whole, and a line starting with `>` is not wrapped.
fn words(line: &str, width: usize) -> Vec<String> {
    let mut rest: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    while !rest.is_empty() {
        if rest[0] == '>' || rest.len() <= width {
            out.push(rest.iter().collect());
            break;
        }
        let (piece, cut) = if rest[width] == ' ' {
            if rest.len() == width + 1 { (width + 1, width + 1) } else { (width, width + 1) }
        } else if let Some(space) = rest[..width].iter().rposition(|&c| c == ' ').or_else(|| rest.iter().position(|&c| c == ' ')) {
            (space, space + 1)
        } else {
            out.push(rest.iter().collect());
            break;
        };
        out.push(rest[..piece].iter().collect());
        rest.drain(..cut);
    }
    out
}
