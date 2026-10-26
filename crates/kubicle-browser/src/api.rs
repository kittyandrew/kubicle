//! Requests to the kubicle server. Every URL is relative, so it resolves against the base path the app was
//! loaded under (Mailu's `WEB_WEBMAIL`); the browser sends the session cookie itself.

use std::{cell::Cell, cell::RefCell};

use kubicle_api::ApiError;
use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{FormData, RequestInit, Response};

thread_local! {
    static CSRF: RefCell<String> = const { RefCell::new(String::new()) };
    /// Set by the first answer that showed the session gone; the page then sends no requests of its own.
    static SIGNED_OUT: Cell<bool> = const { Cell::new(false) };
    /// What the app does then (`App::signed_out`); without it, sign in again at once.
    static ON_SIGNED_OUT: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// The session-storage key under which `reauthenticate` keeps the address for after the sign-in (`boot.js`).
const RETURN_KEY: &str = "kubicle.return";

/// JSON text into `T`: the browser parses it, serde only maps the result onto `T`.
pub fn parse<T: DeserializeOwned>(text: &str) -> Result<T, Error> { from_js(js_sys::JSON::parse(text)?) }

fn from_js<T: DeserializeOwned>(value: JsValue) -> Result<T, Error> {
    serde_wasm_bindgen::from_value(value).map_err(|e| Error(format!("bad answer: {e}"), 0))
}

/// `value` as JSON text. `json_compatible` makes maps plain objects, since JSON.stringify writes a JS Map as `{}`.
/// Both steps refuse only an integer past 2^53, which no kubicle type the client sends holds.
pub fn stringify(value: &impl Serialize) -> String {
    let value = value.serialize(&serde_wasm_bindgen::Serializer::json_compatible()).expect("kubicle's types serialize");
    js_sys::JSON::stringify(&value).expect("a serialized kubicle type is plain JSON").into()
}

pub fn set_csrf(token: &str) { CSRF.with(|c| token.clone_into(&mut c.borrow_mut())); }

/// What went wrong, and the HTTP status of the answer that said so: 0 when none came.
#[derive(Debug, Clone)]
pub struct Error(pub String, pub u16);

impl From<JsValue> for Error {
    fn from(value: JsValue) -> Self { Self(value.as_string().unwrap_or_else(|| "network error".into()), 0) }
}

#[must_use]
pub fn enc(s: &str) -> String { String::from(js_sys::encode_uri_component(s)) }

/// Mailu's session ended: `sso.php` asks front for a fresh one and comes back here, without the address's hash,
/// so the hash waits in this tab's session storage, which `boot.js` puts back: the same Compose, folder and page.
pub fn reauthenticate() {
    let window = leptos::prelude::window();
    if let (Ok(Some(storage)), Ok(hash)) = (window.session_storage(), window.location().hash()) {
        let _ = storage.set_item(RETURN_KEY, &hash);
    }
    let _ = window.location().set_href("sso.php");
}

pub fn on_signed_out(then: impl Fn() + 'static) { ON_SIGNED_OUT.set(Some(Box::new(then))); }

#[must_use]
pub fn is_signed_out() -> bool { SIGNED_OUT.get() }

/// An answer or the server's `SessionExpired` showed the session gone: the app hears it once.
pub fn signed_out() {
    if SIGNED_OUT.replace(true) {
        return;
    }
    let told = ON_SIGNED_OUT.with_borrow(|then| then.as_ref().map(|then| then()));
    if told.is_none() {
        reauthenticate();
    }
}

async fn send(url: &str, init: &RequestInit) -> Result<Response, Error> { answer(fetch(url, init)).await }

/// The request leaves here, when `fetch` is called, not when its future is first awaited.
fn fetch(url: &str, init: &RequestInit) -> JsFuture { JsFuture::from(leptos::prelude::window().fetch_with_str_and_init(url, init)) }

async fn answer(sent: JsFuture) -> Result<Response, Error> {
    let resp: Response = sent.await?.unchecked_into();
    // A redirected answer is front's sign-in page: once Mailu's session ends, front sends every webmail request
    // there (302, then 200), and kubicle's API never redirects. Taken as success, a Send would have looked sent.
    if resp.status() == 401 || resp.redirected() {
        signed_out();
        return Err(Error("signed out".into(), 401));
    }
    if !resp.ok() {
        let text = body_text(&resp).await.unwrap_or_default();
        let message = parse::<ApiError>(&text).map_or_else(|_| format!("HTTP {}", resp.status()), |e| e.message);
        return Err(Error(message, resp.status()));
    }
    Ok(resp)
}

async fn body_text(resp: &Response) -> Result<String, Error> { Ok(JsFuture::from(resp.text()?).await?.as_string().unwrap_or_default()) }

async fn json<T: DeserializeOwned>(resp: &Response) -> Result<T, Error> { from_js(JsFuture::from(resp.json()?).await?) }

pub async fn get<T: DeserializeOwned>(url: &str) -> Result<T, Error> { read(request(url)).await }

/// A GET that `signal` can abort, as a newer search does to an older one's requests.
pub async fn get_aborting<T: DeserializeOwned>(url: &str, signal: &web_sys::AbortSignal) -> Result<T, Error> {
    let init = RequestInit::new();
    init.set_signal(Some(signal));
    json(&send(url, &init).await?).await
}

/// A GET on its way when this returns, for requests that go out together; `read` takes its answer.
#[must_use]
pub fn request(url: &str) -> JsFuture { fetch(url, &RequestInit::new()) }

/// A GET boot.js sent before the app ran (`kubicleStarted.<name>`), taken so no second caller gets it too.
#[must_use]
pub fn started(name: &str) -> Option<JsFuture> {
    let started = js_sys::Reflect::get(&js_sys::global(), &"kubicleStarted".into()).ok()?;
    let promise = js_sys::Reflect::get(&started, &name.into()).ok()?.dyn_into::<js_sys::Promise>().ok()?;
    let _ = js_sys::Reflect::set(&started, &name.into(), &JsValue::NULL);
    Some(JsFuture::from(promise))
}

pub async fn read<T: DeserializeOwned>(sent: JsFuture) -> Result<T, Error> { json(&answer(sent).await?).await }

pub async fn get_text(url: &str) -> Result<String, Error> { body_text(&send(url, &RequestInit::new()).await?).await }

fn post_init(content_type: Option<&str>) -> Result<RequestInit, Error> {
    let init = RequestInit::new();
    init.set_method("POST");
    let headers = web_sys::Headers::new()?;
    CSRF.with(|c| headers.set("X-Kubicle-Csrf", &c.borrow()))?;
    if let Some(ct) = content_type {
        headers.set("Content-Type", ct)?;
    }
    init.set_headers_headers(&headers);
    Ok(init)
}

/// A POST of `body` as JSON on its way, as a promise others can wait on too; `read` takes its answer, once.
pub fn post_shared(url: &str, body: &impl Serialize) -> Result<js_sys::Promise, Error> {
    let init = post_init(Some("application/json"))?;
    init.set_body(&JsValue::from_str(&stringify(body)));
    Ok(leptos::prelude::window().fetch_with_str_and_init(url, &init))
}

async fn post_raw(url: &str, body: &impl Serialize) -> Result<Response, Error> { answer(JsFuture::from(post_shared(url, body)?)).await }

/// A POST whose answer carries nothing the client reads.
pub async fn post(url: &str, body: &impl Serialize) -> Result<(), Error> { post_raw(url, body).await.map(drop) }

pub async fn post_json<T: DeserializeOwned>(url: &str, body: &impl Serialize) -> Result<T, Error> { json(&post_raw(url, body).await?).await }

/// One file on its way to `POST api/upload` over `XMLHttpRequest`, which reports the bytes sent where `fetch` does
/// not, as Roundcube's upload does. Dropping it before `answer` comes stops the progress calls; `cancel` stops the
/// upload.
pub struct Upload {
    xhr: web_sys::XmlHttpRequest,
    url: String,
    sent: js_sys::Promise,
    _progress: wasm_bindgen::closure::Closure<dyn Fn(web_sys::ProgressEvent)>,
}

impl Upload {
    /// Starts the upload; `progress` hears the bytes sent and the total as they go.
    pub fn start(url: &str, file: &web_sys::File, progress: impl Fn(f64, f64) + 'static) -> Result<Self, Error> {
        let xhr = web_sys::XmlHttpRequest::new()?;
        xhr.open("POST", url)?;
        CSRF.with(|c| xhr.set_request_header("X-Kubicle-Csrf", &c.borrow()))?;
        let progress = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::ProgressEvent)>::new(move |e: web_sys::ProgressEvent| {
            if e.length_computable() {
                progress(e.loaded(), e.total());
            }
        });
        xhr.upload()?.set_onprogress(Some(progress.as_ref().unchecked_ref()));
        // Settles once the answer is in; an abort or a network error rejects it.
        let sent = js_sys::Promise::new(&mut |resolve, reject| {
            xhr.set_onload(Some(&resolve));
            xhr.set_onerror(Some(&reject));
            xhr.set_onabort(Some(&reject));
        });
        let form = FormData::new()?;
        form.append_with_blob_and_filename("file", file, &file.name())?;
        xhr.send_with_opt_form_data(Some(&form))?;
        Ok(Self { xhr, url: url.to_owned(), sent, _progress: progress })
    }

    /// Stops the upload; `answer` then fails.
    pub fn cancel(&self) { let _ = self.xhr.abort(); }

    /// The server's answer, read as `answer` reads a `fetch` one.
    pub async fn answer<T: DeserializeOwned>(&self) -> Result<T, Error> {
        JsFuture::from(self.sent.clone()).await.map_err(|_| Error("the upload was stopped".into(), 0))?;
        let status = self.xhr.status()?;
        let text = self.xhr.response_text()?.unwrap_or_default();
        // Front's sign-in page, as in `answer`: XHR follows the redirect too, and has no `redirected` to say so.
        if status == 401 || !self.xhr.response_url().ends_with(&self.url) {
            signed_out();
            return Err(Error("signed out".into(), 401));
        }
        if !(200..300).contains(&status) {
            let message = parse::<ApiError>(&text).map_or_else(|_| format!("HTTP {status}"), |e| e.message);
            return Err(Error(message, status));
        }
        parse(&text)
    }
}

/// `POST api/import`: the files as multipart fields `file`.
pub async fn post_files<T: DeserializeOwned>(url: &str, files: &[web_sys::File]) -> Result<T, Error> {
    let form = FormData::new()?;
    for file in files {
        form.append_with_blob_and_filename("file", file, &file.name())?;
    }
    let init = post_init(None)?;
    init.set_body(&form);
    json(&send(url, &init).await?).await
}
