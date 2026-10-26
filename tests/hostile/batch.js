// The batch page's own script: it builds the mail frames in the page's realm, as the client's code does. Firefox's
// srcset race never shows here, with frames built this way or by a WebDriver script; cases.py sends those cases
// through the reader.
// The driver writes a request into <html data-batch> and reads the answer from <html data-result>.
const html = document.documentElement

async function render({ nonce, cases, attributes, size, settle }) {
  const start = performance.now()
  document.body.replaceChildren()
  const frames = []
  const hits = {}
  let stray = 0
  const listener = (e) => {
    const frame = frames.find(([w]) => w === e.source)
    if (frame) hits[frame[1]] = (hits[frame[1]] || 0) + 1
    else stray++
  }
  window.addEventListener("message", listener)
  const errors = []
  const loads = []
  let texts = 0
  let next = 0
  const worker = async () => {
    while (next < cases.length) {
      const c = cases[next++]
      try {
        const r = await fetch(`api/message?folder=${encodeURIComponent(c.folder)}&uid=${c.uid}&images=false&text=false`)
        if (!r.ok) {
          errors.push([c.key, `api/message answered ${r.status}`])
          continue
        }
        const body = (await r.json()).body
        if (body.text) {
          texts++
          continue
        }
        const frame = document.createElement("iframe")
        for (const [name, value] of attributes) frame.setAttribute(name, value)
        // On top of each other inside the viewport at the reader's size, so lazy loads start as they do there.
        frame.style.cssText = `position:absolute;top:0;left:0;width:${size[0]}px;height:${size[1]}px;border:0`
        loads.push(
          new Promise((resolve) => {
            frame.addEventListener("load", resolve, { once: true })
            setTimeout(resolve, 15000)
          })
        )
        frame.srcdoc = body.document
        document.body.append(frame)
        frames.push([frame.contentWindow, c.key])
      } catch (e) {
        errors.push([c.key, String(e)])
      }
    }
  }
  await Promise.all(Array.from({ length: 16 }, worker))
  const fetched = performance.now() - start
  await Promise.all(loads)
  const loaded = performance.now() - start
  await new Promise((resolve) => setTimeout(resolve, settle))
  window.removeEventListener("message", listener)
  return { nonce, hits, stray, errors, texts, frames: frames.length, fetched, loaded, url: location.href }
}

new MutationObserver(() => {
  const request = html.dataset.batch
  if (!request) return
  delete html.dataset.batch
  render(JSON.parse(request)).then((result) => {
    html.dataset.result = JSON.stringify(result)
  })
}).observe(html, { attributes: true, attributeFilter: ["data-batch"] })
html.dataset.ready = "1"
