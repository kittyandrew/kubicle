// A file rather than an inline script, so the page's CSP can stay at script-src 'self'.
import init from "./kubicle_web.js"
// A sign-in after the session ended comes back from sso.php without the hash, which api.rs `reauthenticate`
// kept in this tab's session storage: the same Compose, folder and page again.
try {
  const back = sessionStorage.getItem("kubicle.return")
  sessionStorage.removeItem("kubicle.return")
  if (back && !location.hash) history.replaceState(null, "", back)
} catch {
  // storage off: the app opens on INBOX, not where it was before the session ended
}
// The session leaves now, while the wasm starts, with the first page of the folder the address names in it; the
// app takes it over (state.rs, `load_session`). A standalone message window lists nothing; a guessed folder the app
// does not open is only fetched again. The address's page counts from 1, as Roundcube's `_page`; the API's from 0.
const hash = location.hash
const [folder, page] = hash.startsWith("#folder=") ? hash.slice(8).split("&page=") : ["INBOX", "1"]
// Encoded again, as kubicle-browser's `api::enc` writes it: raw, a link in a mail could add parameters, a search.
const name = (() => {
  try {
    return decodeURIComponent(folder)
  } catch {
    return folder
  }
})()
const opening = `?folder=${encodeURIComponent(name)}&page=${Math.max(0, (parseInt(page, 10) || 1) - 1)}`
globalThis.kubicleStarted = { session: fetch(`api/session${hash.startsWith("#open=") ? "" : opening}`) }
globalThis.kubicleBuild = "dev" // nix/package.nix writes the wasm's content-named file here: the app's build
init()
navigator.serviceWorker?.register("sw.js")
