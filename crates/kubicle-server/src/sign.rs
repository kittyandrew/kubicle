//! Signed URLs for everything loaded without the session cookie: the sandboxed mail frame has an opaque origin,
//! so its image requests carry no cookie, and attachment links work the same way. A URL names one resource, one
//! session and an expiry, under an HMAC-SHA256 tag keyed by a secret generated at start; a restart voids them all.
//! The tag covers the route and every parameter before it, in order, so a URL carries no parameter it does not sign.

use axum::http::Uri;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jiff::Timestamp;
use kubicle_mail::{links::IMAGE, links::INLINE, links::PartRoute, links::Section, links::enc, structure::PartPath};
use ring::{hmac, rand::SystemRandom};
use serde::{Deserialize, Deserializer, de::Error as _};
use url::form_urlencoded;

const TTL_SECS: i64 = 12 * 3600; // a message left open half a day still shows its images

#[derive(Clone)]
pub struct Signer(hmac::Key);

/// The session handle of a URL `Signer::verify` accepted: `Sessions::find_handle` takes nothing else, so no route
/// finds a session by a handle alone, which the frame's markup shows.
pub struct Handle(String);

impl Handle {
    pub fn as_str(&self) -> &str { &self.0 }
}

impl Signer {
    #[must_use]
    pub fn new() -> Self { Self(hmac::Key::generate(hmac::HMAC_SHA256, &SystemRandom::new()).expect("the OS random source works")) }

    /// The URLs of one message's resources. `prefix` is the app's public origin plus base path
    /// (`https://host/login/`); `session` is the session's public handle, never its cookie value: it ends up in the
    /// frame's markup.
    pub fn links(&self, prefix: &str, session: &str, folder: &str, uid: u32, validity: u32) -> Links {
        let (prefix, session, folder) = (prefix.to_owned(), session.to_owned(), folder.to_owned());
        Links { signer: self.clone(), prefix, session, folder, uid, validity }
    }

    /// `prefix` and `route` with `params` as its query, then the expiry `e`, then the tag `t` over all of them.
    fn url(&self, prefix: &str, route: &str, params: &[(&str, &str)]) -> String {
        let expires = (Timestamp::now().as_second() + TTL_SECS).to_string();
        let params: Vec<(&str, &str)> = params.iter().copied().chain([("e", expires.as_str())]).collect();
        let tag = URL_SAFE_NO_PAD.encode(hmac::sign(&self.0, &mac_input(route, &params)));
        let query: Vec<String> = params.iter().map(|(name, value)| format!("{name}={}", enc(value))).collect();
        format!("{prefix}{route}?{}&t={tag}", query.join("&"))
    }

    /// The session handle `uri` names, when its last parameter is a tag over its route and every parameter before it,
    /// decoded (constant-time), and its expiry has not passed.
    pub fn verify(&self, uri: &Uri) -> Option<Handle> {
        let mut params: Vec<(String, String)> = form_urlencoded::parse(uri.query()?.as_bytes()).into_owned().collect();
        let tag = params.pop().filter(|(name, _)| name == "t").and_then(|(_, tag)| URL_SAFE_NO_PAD.decode(tag).ok())?;
        let signed: Vec<(&str, &str)> = params.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        hmac::verify(&self.0, &mac_input(uri.path().trim_start_matches('/'), &signed), &tag).ok()?;
        let value = |key: &str| signed.iter().find(|(name, _)| *name == key).map(|(_, value)| *value);
        let (expires, handle) = (value("e")?.parse::<i64>().ok()?, value("s")?);
        (expires >= Timestamp::now().as_second()).then(|| Handle(handle.to_owned()))
    }
}

/// Signed URLs for the parts and remote images of one message.
#[derive(Clone)]
pub struct Links {
    signer: Signer,
    prefix: String,
    session: String,
    folder: String,
    uid: u32,
    /// The folder's UIDVALIDITY: a UID names another message once the folder is recreated.
    validity: u32,
}

impl Links {
    /// A part's URL for `route`, with `more` after the fields every part URL names.
    fn part_url(&self, route: &str, part: &PartPath, more: &[(&str, &str)]) -> String {
        let (uid, validity, part) = (self.uid.to_string(), self.validity.to_string(), part.to_string());
        let named = [("s", self.session.as_str()), ("f", &self.folder), ("u", &uid), ("v", &validity), ("p", &part)];
        self.signer.url(&self.prefix, route, &[&named[..], more].concat())
    }
}

impl kubicle_mail::links::Links for Links {
    fn prefix(&self) -> &str { &self.prefix }

    fn part(&self, route: PartRoute, part: &PartPath) -> String { self.part_url(route.path(), part, &[]) }

    fn inline(&self, part: &PartPath, section: Option<&Section>) -> String {
        let Some(Section { number, size }) = section else { return self.part(PartRoute::Inline, part) };
        self.part_url(INLINE, part, &[("n", number), ("z", &size.to_string())])
    }

    fn image(&self, remote: &str) -> String { self.signer.url(&self.prefix, IMAGE, &[("s", &self.session), ("url", remote)]) }
}

/// The query of an `inline`, `download` or `view` request, read once `Signer::verify` accepted it.
#[derive(Debug, Deserialize)]
pub struct PartQuery {
    pub f: String,
    pub u: u32,
    /// The folder's UIDVALIDITY when signed: the part is served only while it still holds.
    pub v: u32,
    #[serde(deserialize_with = "part_path")]
    pub p: PartPath,
    /// An inline image's IMAP section number and decoded size. The route is signed too, and only `inline` signs a
    /// section, so a section verifies nowhere else.
    n: Option<String>,
    z: Option<usize>,
}

impl PartQuery {
    pub fn section(&self) -> Option<Section> { self.n.clone().zip(self.z).map(|(number, size)| Section { number, size }) }
}

fn part_path<'de, D: Deserializer<'de>>(path: D) -> Result<PartPath, D::Error> {
    String::deserialize(path)?.parse().map_err(|()| D::Error::custom("not a part path"))
}

/// The query of an `image` (proxy) request, read once `Signer::verify` accepted it.
#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    pub url: String,
}

/// Length-prefixed fields, so no two different field lists produce the same bytes.
fn mac_input(route: &str, params: &[(&str, &str)]) -> Vec<u8> {
    let mut input = Vec::new();
    for field in std::iter::once(route).chain(params.iter().flat_map(|(name, value)| [*name, *value])) {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field.as_bytes());
    }
    input
}
