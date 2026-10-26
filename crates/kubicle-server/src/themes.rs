//! Theme files: built in (shipped with the client), system (the deployment's themes directory) and user
//! (pasted in settings). Every file starts with `/* kubicle-theme name="..." dark=true|false */` and only sets
//! tokens (docs/design.md, Themes).

use std::path::Path;

use kubicle_api::{ThemeInfo, ThemeSource};

/// The theme a file's first-line header describes. `None` for a file without one, which is then not listed.
pub fn info(id: String, css: &str, source: ThemeSource) -> Option<ThemeInfo> {
    let line = css.lines().next()?.trim();
    let inner = line.strip_prefix("/* kubicle-theme ")?.strip_suffix("*/")?.trim();
    let (name, rest) = inner.strip_prefix("name=\"")?.split_once('"')?;
    let dark: bool = rest.trim().strip_prefix("dark=")?.parse().ok()?; // `true` or `false`, nothing else
    (!name.is_empty()).then(|| ThemeInfo { id, name: name.into(), dark, source, version: crate::assets::content_hash(css.as_bytes()) })
}

/// Why a pasted theme is refused. Its id doubles as a URL segment. Anything that loads a resource is out: in a
/// mail app remote CSS can read what is on screen through attribute selectors and `url()`, and a theme only needs
/// custom properties.
pub fn reject_reason(id: &str, css: &str) -> Option<&'static str> {
    if !(1..=40).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Some("theme ids are 1-40 characters of a-z, 0-9 and -");
    }
    if info(String::new(), css, ThemeSource::User).is_none() {
        return Some("the first line must be the kubicle-theme header");
    }
    if css.len() > 64 * 1024 {
        return Some("a theme is limited to 64 KiB");
    }
    let lower = css.to_ascii_lowercase();
    // A backslash is refused too: a CSS escape spells `url(` as `\75rl(`, which the browser reads as `url(`.
    let loads = ["@import", "url(", "@font-face", "image-set(", "expression(", "\\"].into_iter().any(|needle| lower.contains(needle));
    loads.then_some("a theme may not load anything: no @import, url(), @font-face, image-set() or backslash escapes")
}

/// Every `<id>.css` with a valid header in `dir`, sorted by id. A missing directory lists nothing.
pub fn list_dir(dir: &Path, source: ThemeSource) -> Vec<ThemeInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut themes: Vec<ThemeInfo> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".css")?.to_owned();
            info(id, &std::fs::read_to_string(&path).ok()?, source)
        })
        .collect();
    themes.sort_by(|a, b| a.id.cmp(&b.id));
    themes
}
