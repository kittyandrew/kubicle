# Architecture

The parts and the contracts that cross files. `docs/design.md` has the reasons; read a file in full before
changing it.

## Crates

- `crates/kubicle-api`: every type the server and the client exchange. A change here changes both sides in
  the same commit; never hand-write JSON that one of these types already describes.
- `crates/kubicle-imap`: the IMAP client on imap-codec's 2.0 alpha line, which has SORT and THREAD (pinned in its
  `Cargo.toml`). Commands are imap-types values; no IMAP text is ever built by string formatting.
- `crates/kubicle-mail`: MIME to the `Message` view, no I/O. `structure.rs`, `plain.rs`, `flowed.rs`,
  `render.rs`: Roundcube's part walk, plain text read into lines, format=flowed, sanitizing and the sandboxed
  document; `links.rs`: the routes its URLs name, and the `Links` trait the server's `sign.rs` implements.
- `crates/kubicle-mailbox`: a signed-in user's mailbox without the routes, and kubicle's `Error` with its HTTP status.
  - `sync.rs`, `threads.rs`: the engine, which answers every page and search from Dovecot (SORT, THREAD=REFS,
    SEARCH) and carries out every IMAP change over one IDLE connection per user; Dovecot's threads in Roundcube's
    order.
  - `compose.rs`: prefilled drafts, uploads, building the MIME and sending it over submission.
  - `uploads.rs`, `settings.rs`: the user's unsent uploads, the settings database.
- `crates/kubicle-server`: the axum server, on `kubicle-mailbox`.
  - `proxy.rs`: the image proxy for remote images the user allowed.
  - `http.rs`, `session.rs`, `sign.rs`: routes, Mailu SSO sessions, signed URLs for resources the sandboxed
    frame loads without cookies.
- `crates/kubicle-web`: the Leptos client, compiled to wasm32 and rendered in the browser only. Beside it, the
  parts that do not use `App`: `crates/kubicle-ui` (icons, menus, the modal dialog) and `crates/kubicle-browser`
  (requests to the server, text helpers, dates through `Intl`).

Every first-party crate keeps these, so its boundary stays the compiler's to check (Andrew, Oct 25, 2026):

- **At most 2,000 lines of Rust code** (tokei's Code column). `kubicle-web` alone is exempt, as its components
  all share `App`.
- **No re-exports** (`pub use`): a caller imports an item from the crate and module that define it.
- **No `#[path = "..."]` and no `include!`**: each module lives at the path its name gives.

## Contracts

- **Mailu identity is read on `/sso.php` only.** Front sets `X-Remote-User` and `X-Remote-User-Token` there and
  nowhere else, so on any other route those headers may come from the browser. Never read them elsewhere.
- **The session cookie is `roundcube_sessid`, `Path=/`**, so Mailu's logout clears it. The Mailu token lives in
  server memory only, never in SQLite or a log.
- **The mail frame has an opaque origin and sends no cookies.** Everything it loads (inline parts, proxied
  images) uses URLs signed by `sign.rs`, bound to the session and expiring; a route serving those checks the
  signature, never the cookie.
- **Mail HTML has three required layers.** The frame never gets `allow-scripts` or `allow-same-origin`, and
  every document carries the CSP from `render.rs`: together they stop scripts, which the hostile suite proves
  with sanitizing off. ammonia must stay on: only it stops DNS lookups, preconnects, Chromium's prerender
  fetch and relative requests to the app (`docs/design.md`, Security).
- **The server serves `/`**: front strips `WEB_WEBMAIL` before proxying. The client derives every URL from the
  base path it was loaded under; no route or asset path is absolute.
- **The event stream can always resync.** Every `api/events` stream, and a listener that fell behind, starts
  with `Event::Resync`, on which the client reloads what it shows; so a client may drop and reopen the stream
  at any time. The stream sends `Event::Keepalive` after `KEEPALIVE_SECONDS` of quiet and `X-Accel-Buffering: no`.
- **IMAP is the only copy of the mail.** The server keeps no headers on disk; in memory, each view's order until a
  message arrives in its folder or leaves it, and Sent's headers, for autocomplete and the From choices, until Sent
  changes. Only the settings database is backed up.
