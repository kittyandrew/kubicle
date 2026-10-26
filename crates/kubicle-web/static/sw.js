// The service worker: a start takes the page and its content-named files from here, so it waits on no network
// before the app runs (docs/design.md, Speed budgets). Mail, the API and everything else go to the network.
const CACHE = "kubicle-shell"

// At once, not after the old version's tabs close: the app loads all of its files at start, so a page can never
// mix two versions' files, and a page that started on the old build reloads itself (state.rs, `reload_onto`).
self.addEventListener("install", () => self.skipWaiting())
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()))

self.addEventListener("fetch", (event) => {
  const request = event.request
  const url = new URL(request.url)
  const path = url.pathname.slice(new URL(self.registration.scope).pathname.length)
  if (request.method !== "GET" || url.origin !== location.origin) return
  if (request.mode === "navigate" && path === "") {
    event.respondWith(page(event))
  } else if (
    /^[\w-]+\.[0-9a-f]{16}\.(js|wasm|css)$/.test(path) ||
    (/^themes\/[\w-]+\.css$/.test(path) && url.searchParams.has("v"))
  ) {
    event.respondWith(fixed(request))
  }
})

// The stored page at once, and the current one fetched beside it for the next start. A deploy's new page empties
// the store first: the old build's files are never asked for again.
async function page(event) {
  const cache = await caches.open(CACHE)
  const stored = await cache.match("./")
  const was = stored?.clone().text() // read now: the stored page answers below, which uses up its body
  const fresh = fetch(event.request).then(async (response) => {
    if (kept(response)) {
      if (was && (await was) !== (await response.clone().text())) {
        await caches.delete(CACHE)
      }
      await (await caches.open(CACHE)).put("./", response.clone())
    }
    return response
  })
  event.waitUntil(fresh.catch(() => {}))
  return stored || fresh
}

// A file named after its content never changes, so the first copy serves for good.
async function fixed(request) {
  const cache = await caches.open(CACHE)
  const stored = await cache.match(request)
  if (stored) return stored
  const response = await fetch(request)
  if (kept(response)) await cache.put(request, response.clone())
  return response
}

// Not a redirected answer: once Mailu's session ended, front answers with its sign-in page, which stored under the
// app's name stood in for the app until a deploy.
function kept(response) {
  return response.ok && !response.redirected
}
