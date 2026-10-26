// The corpora that are JavaScript modules, as JSON lines on stdout: {corpus, id, html}, or for ept3 its raw block.
// usage: node js-corpora.mjs <DOMPurify> <H5SC> <ept3>
import { createRequire } from "node:module"
import path from "node:path"

const [dompurify, h5sc, ept3] = process.argv.slice(2)
const require = createRequire(import.meta.url)
const emit = (corpus, id, fields) => console.log(JSON.stringify({ corpus, id, ...fields }))

const expect = (await import(path.join(dompurify, "test/fixtures/expect.mjs"))).default
expect.forEach((c, i) => emit("dompurify", String(i).padStart(3, "0"), { html: c.payload }))

const payloads = require(path.join(h5sc, "payloads.js"))
for (const item of require(path.join(h5sc, "items.js"))) {
  const html = String(item.data).replace(/%([a-z0-9_]+)%/g, (m, key) => payloads[key] ?? m)
  emit("h5sc", String(item.id).padStart(3, "0"), { html })
}

// Each test describes its snippet in prose; the snippet is the first paragraph that names the tracking host.
// cases.py decides how it goes into a mail (HTML, a header, a calendar or vCard part, a nested message).
const tests = await import(path.join(ept3, "backend/lib/tests.js"))
for (const [name, test] of Object.entries(tests)) {
  const { type, desc } = test("en")
  const block = desc.split(/\n\s*\n/).find((b) => b.includes("TRACKING_"))
  if (block) emit("ept3", name, { type, block: block.trim() })
}
