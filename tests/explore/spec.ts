// What kubicle's client keeps true in every state Bombadil reaches while it clicks, types and presses keys at
// random against the fixture mailbox (docs/design.md, Testing). Every extractor is named: test_trace.py reads the
// values from the trace and pins them with inline-snapshot.
import { always, extract, next, now } from "@antithesishq/bombadil"
import { actions, weighted } from "@antithesishq/bombadil/browser"
import { isAction } from "@antithesishq/bombadil/actions"
import { clicks, inputs, navigation, scroll } from "@antithesishq/bombadil/browser/defaults/actions"
// Of Bombadil's default properties only this one: a wasm panic shows as an uncaught exception.
export { noUncaughtExceptions } from "@antithesishq/bombadil/browser/defaults/properties"

// Requests the page made since the previous state; the buffer is cleared so it never fills and stops recording.
// Properties run in Bombadil's own JS engine, which has no `URL`, so the origin check happens here.
const requests = extract((s) => {
  const urls = s.window.performance.getEntriesByType("resource").map((e) => e.name)
  s.window.performance.clearResourceTimings()
  return { urls, offsite: urls.filter((u) => new URL(u).origin !== s.window.location.origin) }
}).named("requests")

export const noRequestOutsideTheApp = always(() => requests.current.offsite.length === 0)

// The mail frame: no script, no app origin, its own CSP among the first tags of its document.
const frames = extract((s) =>
  [...s.document.querySelectorAll("iframe")].map((f) => ({
    sandbox: f.getAttribute("sandbox"),
    referrerpolicy: f.getAttribute("referrerpolicy"),
    cspInHead: /^<!DOCTYPE html><html><head>(<meta [^>]*>)*<meta http-equiv="Content-Security-Policy"/.test(f.srcdoc)
  }))
).named("frames")

export const mailFrameSandboxed = always(() =>
  frames.current.every((f) => f.sandbox !== null && !/\ballow-(scripts|same-origin)\b/.test(f.sandbox) && f.cspInHead)
)

// The folder pane's unread badges and the tab title, which shows the inbox's count.
const folders = extract((s) =>
  [...s.document.querySelectorAll(".folder")].map((f) => ({
    name: f.querySelector(".folder-name")?.textContent ?? "",
    unseen: f.querySelector(".badge")?.textContent ?? ""
  }))
).named("folders")
const title = extract((s) => s.document.title).named("title")

export const unreadCountsAgree = always(() => {
  const counts = folders.current.map((f) => (f.unseen === "" ? 0 : Number(f.unseen.replaceAll(",", ""))))
  if (!counts.every((n) => Number.isInteger(n) && n >= 0)) return false
  const inbox = folders.current.find((f) => f.name === "Inbox")
  // Only the mail view counts in the title, as Roundcube's; Compose and Settings name themselves alone.
  if (!inbox || /:: (Compose|Settings)$/.test(title.current)) return true
  const shown = /^\((\d+)\) /.exec(title.current)?.[1] ?? "0"
  return shown === (inbox.unseen.replaceAll(",", "") || "0")
})

// The stack's Dovecot has no QUOTA, and an account without a limit gets no quota widget (docs/design.md, Account).
const quota = extract((s) => s.document.querySelector(".quota") !== null).named("quota")
export const noQuotaWidget = always(() => !quota.current)

// The open message, the focused row where the list's keys act and the list's idea of it, and the selection.
const list = extract((s) => {
  const rows = [...s.document.querySelectorAll(".row")]
  return {
    active: s.document.querySelector(".list")?.getAttribute("aria-activedescendant") ?? null,
    open: rows.filter((r) => r.classList.contains("open")).map((r) => r.id),
    focused: rows.filter((r) => r.classList.contains("focused")).map((r) => r.id),
    selected: rows.filter((r) => r.getAttribute("aria-selected") === "true").map((r) => r.id)
  }
}).named("list")
const lastAction = extract((s) => s.lastAction).named("lastAction")
const keyPress = (a: unknown) => typeof a === "object" && a !== null && "PressKey" in a
const arrow = (a: unknown) => keyPress(a) && [38, 40].includes((a as any).PressKey.code)

// The focused row is the open one unless a checkbox or a modifier moved it, as Roundcube's `last_selected`.
export const focusedRowIsTheActiveOne = always(() => {
  const { active, open, focused } = list.current
  return open.length <= 1 && focused.length <= 1 && (focused.length === 0 || focused[0] === active)
})

// An arrow key that moves the open message moves the selection with it. Only the list's own: on a focused
// `<select>` an arrow changes its value.
const listFocused = extract((s) => s.document.activeElement?.classList.contains("list") ?? false).named("listFocused")

export const arrowKeysSelectTheOpenRow = always(
  now(() => {
    const [before, listed] = [list.current.open[0], listFocused.current]
    return next(() => {
      const { open, selected } = list.current
      return (
        !listed ||
        !arrow(lastAction.current) ||
        open.length === 0 ||
        open[0] === before ||
        (selected.length === 1 && selected[0] === open[0])
      )
    })
  })
)

// A key press never drops keyboard focus to the page body when it was on something. A new page, as a sign-out
// that navigates while the key goes down, starts with focus on the body.
const focus = extract((s) => {
  const e = s.document.activeElement
  return !e || e === s.document.body ? null : e.id || e.className || e.tagName
}).named("focus")
const documentStart = extract((s) => s.window.performance.timeOrigin).named("documentStart")

export const keyboardFocusKept = always(
  now(() => {
    const [before, loaded] = [focus.current, documentStart.current]
    return next(
      () => before === null || !keyPress(lastAction.current) || focus.current !== null || documentStart.current !== loaded
    )
  })
)

// No dead clicks: a click, or Enter on a button, link or option, changes something a user can see. Changes are
// the page's DOM outside menus (theme attributes and stylesheets included), form values, a menu or popup opening,
// a request or download, the URL, or focus moving to an element; a menu entry that only closes its menu did
// nothing. After a crash nothing responds, and noUncaughtExceptions has said so.
const surface = extract((s) => {
  const w = s.window as any
  if (s.errors.uncaughtExceptions.length > 0) w.kubicleCrashed = true // until a reload starts a new page
  const body = s.document.documentElement.cloneNode(true) as HTMLElement
  body.querySelectorAll(".menu, .backdrop").forEach((e) => e.remove())
  body.querySelectorAll("[aria-expanded]").forEach((e) => e.removeAttribute("aria-expanded"))
  // Comments too: Leptos leaves a placeholder comment where a closed menu was, which no one sees.
  const comments = s.document.createTreeWalker(body, NodeFilter.SHOW_COMMENT)
  const found: Node[] = []
  while (comments.nextNode()) found.push(comments.currentNode)
  found.forEach((c) => c.parentNode?.removeChild(c))
  const fields = [...s.document.querySelectorAll("input, textarea, select")].map((e) => {
    const f = e as HTMLInputElement
    return f.type === "checkbox" || f.type === "radio" ? String(f.checked) : f.value
  })
  // Where every scrolled box stands: a link that scrolls to a section changes nothing else.
  const scrolled = [...s.document.querySelectorAll("*")].filter((e) => e.scrollTop > 0).map((e) => e.scrollTop)
  let hash = 0
  for (const c of body.outerHTML + fields.join("\u0000") + scrolled.join(",") + s.window.location.href) {
    hash = (Math.imul(hash, 31) + c.charCodeAt(0)) | 0
  }
  // Requests started, not only finished ones: a click whose answer is slower than the next state is not dead.
  if (w.kubicleFetches === undefined) {
    const fetch = w.fetch
    w.kubicleFetches = 0
    w.fetch = (...args: unknown[]) => {
      w.kubicleFetches++
      return fetch.apply(w, args)
    }
  }
  // The browser's own dialogs (confirm, prompt, alert) and the file picker draw outside the page: count them.
  if (w.kubicleDialogs === undefined) {
    w.kubicleDialogs = 0
    for (const name of ["confirm", "prompt", "alert"]) {
      const original = w[name]
      w[name] = (...args: unknown[]) => {
        w.kubicleDialogs++
        return original.apply(w, args)
      }
    }
    // A tab of its own (Open, Print, Show source) would leave this one in the background, where every later step
    // waits out Bombadil's quiescence timeout: it counts as opened and opens nothing. A click meant for something
    // else can land on such an entry when a menu covers its point.
    w.open = () => {
      w.kubicleDialogs++
      return null
    }
    // Registering kubicle for mailto: links asks in the browser's own bar, also outside the page.
    const register = w.navigator.registerProtocolHandler
    w.navigator.registerProtocolHandler = (...args: unknown[]) => {
      w.kubicleDialogs++
      return register.apply(w.navigator, args)
    }
    const click = w.HTMLInputElement.prototype.click
    w.HTMLInputElement.prototype.click = function (this: HTMLInputElement) {
      if (this.type === "file") w.kubicleDialogs++
      return click.call(this)
    }
  }
  // A download starts as a navigation and leaves the page as it was: count the unloads it announces.
  if (w.kubicleLeaving === undefined) {
    w.kubicleLeaving = 0
    w.addEventListener("beforeunload", () => w.kubicleLeaving++)
  }
  // What the open menus show: a submenu taking its parent's place changes this and nothing else.
  const menus = [...s.document.querySelectorAll(".menu")].map((m) => m.textContent ?? "").join("\u0000")
  return {
    hash,
    menus,
    opened: s.document.querySelectorAll('.menu, [aria-expanded="true"]').length,
    fetches: w.kubicleFetches + w.kubicleLeaving + w.kubicleDialogs,
    crashed: !!w.kubicleCrashed
  }
}).named("surface")
const pressable = extract((s) => {
  const e = s.document.activeElement
  return !!e && e.matches("button, a[href], [role=button], [role=option], [role=menuitem], [role=menuitemcheckbox]")
}).named("pressable")
const enter = (a: unknown) => keyPress(a) && (a as any).PressKey.code === 13
const click = (a: unknown) => typeof a === "object" && a !== null && "Click" in a

// The no-ops a click may genuinely be, one named rule each, judged on the element the click (or Enter) landed on
// at the moment it landed: afterwards a menu entry is gone with its menu. Capture listeners record that element;
// without one (no pointer event on a disabled control) the element now under the point stands in. Extractors run
// in the page, so this one repeats its checks rather than calling the helpers above.
const clicked = extract((s) => {
  const w = s.window as any
  const d = s.document
  const judge = (target: Element) => {
    const control = target.closest("label")?.control ?? target // a label's field, by nesting or by `for`
    const rules: [string, boolean][] = [
      ["already the active one", target.closest('[aria-selected="true"][role=tab], [aria-current="page"], .active') !== null],
      ["checked radio", control instanceof w.HTMLInputElement && control.type === "radio" && control.checked],
      ["disabled control", target.closest(":disabled, [aria-disabled=true]") !== null],
      [
        "row already open alone",
        target.closest(".row.open[aria-selected=true]") !== null && d.querySelectorAll(".row[aria-selected=true]").length === 1
      ],
      [
        // Saves queue one behind another (`save_draft`): a Save while one is on its way changes nothing yet.
        "save already on its way",
        target.closest('button[title="Save as draft now"]') !== null &&
          d.querySelector(".compose-status")?.textContent === "Saving..."
      ],
      [
        "field already focused",
        !!d.activeElement?.matches("input, textarea") &&
          (control === d.activeElement || target === d.activeElement || target.contains(d.activeElement))
      ]
    ]
    return { target, before: d.activeElement, noop: rules.find(([, applies]) => applies)?.[0] ?? null }
  }
  if (w.kubicleHit === undefined) {
    w.kubicleHit = null
    w.addEventListener("pointerdown", (e: Event) => (w.kubicleHit = judge(e.target as Element)), true)
    w.addEventListener(
      "keydown",
      (e: KeyboardEvent) => e.keyCode === 13 && d.activeElement && (w.kubicleHit = judge(d.activeElement)),
      true
    )
  }
  const a = s.lastAction as any
  const isClick = typeof a === "object" && a !== null && "Click" in a
  const under = isClick ? d.elementFromPoint(a.Click.point.x, a.Click.point.y) : d.activeElement
  const seen = w.kubicleHit !== null
  const hit = seen ? w.kubicleHit : under ? judge(under) : null
  w.kubicleHit = null
  if (!hit) return { reached: null, noop: null, focusMoved: null, focusOnTarget: false, toggle: false }
  const target: Element = hit.target
  // Bombadil clicks a point; when something else covers its target there (a toast, a backdrop), the click never
  // reached the control it meant.
  const meant = isClick ? a.Click.fingerprint : null
  const reached =
    !meant ||
    (target.closest(meant.tag) !== null &&
      (meant.id ? target.closest(`[id="${w.CSS.escape(meant.id)}"]`) !== null : true) &&
      (meant.text_content ? (target.closest(meant.tag)?.textContent ?? "").includes(meant.text_content.slice(0, 20)) : true) &&
      (meant.accessible_name
        ? target.closest(`[aria-label="${w.CSS.escape(meant.accessible_name)}"], ${meant.tag}`) !== null
        : true))
  const focused = d.activeElement
  const focusOnTarget =
    !!focused &&
    !focused.matches("input, textarea, select") &&
    (focused === target || focused.contains(target) || target.contains(focused) || focused.matches("[aria-haspopup]"))
  return {
    reached: `${target.tagName.toLowerCase()}${target.id ? "#" + target.id : ""}.${[...target.classList].join(".")}`,
    noop: reached ? hit.noop : "click landed on something else",
    // Focus landing on a clicked button is no effect of its own; on a clicked text field it is the whole effect.
    // Focus going back to a menu's button is the menu closing, nothing more. Compared by element, since two
    // fields can share the id-less class the focus extractor reports; null when the listeners saw nothing.
    focusMoved: seen ? !!focused && focused !== d.body && focused !== hit.before && !focusOnTarget : null,
    focusOnTarget,
    toggle: target.closest("[aria-haspopup]") !== null // a menu's own button, which closes its open menu as well as opening it
  }
}).named("clicked")

export const noDeadClicks = always(
  now(() => {
    const before = { ...surface.current, focus: focus.current, pressable: pressable.current }
    return next(() => {
      const action = lastAction.current
      if (surface.current.crashed || (!click(action) && !(enter(action) && before.pressable))) return true
      const focusMoved =
        clicked.current.focusMoved ?? (focus.current !== null && focus.current !== before.focus && !clicked.current.focusOnTarget)
      return (
        surface.current.hash !== before.hash ||
        surface.current.opened > before.opened ||
        (surface.current.menus !== "" && surface.current.menus !== before.menus) ||
        surface.current.fetches > before.fetches ||
        requests.current.urls.length > 0 ||
        focusMoved ||
        clicked.current.noop !== null ||
        (clicked.current.toggle && surface.current.opened < before.opened)
      )
    })
  })
)

// Toasts the app showed: their texts are pinned, so a new error message shows as a diff.
extract((s) => s.document.querySelector(".toast")?.textContent ?? null).named("toast")

// kubicle's keys (docs/design.md, Keys) whenever focus is outside a text field, and text into its search box.
const typing = extract((s) => {
  const e = s.document.activeElement
  return !!e && (e.tagName === "INPUT" || e.tagName === "TEXTAREA" || e.closest("[contenteditable]") !== null)
}).named("typing")
const searchFocused = extract((s) => s.document.activeElement?.id === "search").named("searchFocused")
const page = extract((s) => s.document.contentType === "text/html").named("html")

// Focus on an entry that opens a tab of its own (see inTab below), where Enter would open it as a click does: a
// menu opens with focus on its first entry, which in More is Print this message.
const opensTab = extract((s) => {
  const e = s.document.activeElement
  const name = e?.getAttribute("name") ?? e?.getAttribute("aria-label") ?? e?.textContent ?? ""
  return /^(Print this message|Show source|Open|Open in new window)$/.test(name.trim())
}).named("opensTab")
const KEY_CODES = [38, 40, 13, 46, 27, 36, 35, 37, 39, 65, 67, 70, 82, 191] // Up Down Enter Del Esc Home End Left Right a c f r /
const keys = actions(() =>
  page.current && !typing.current
    ? KEY_CODES.filter((code) => code !== 13 || !opensTab.current).map((code) => ({ PressKey: { code } }))
    : []
)
const search = actions(() =>
  searchFocused.current ? [{ TypeText: { text: { Regexp: "[a-z]{2,6}" } } }, { PressKey: { code: 13 } }] : []
)

// Print this message, Show source and both Opens open a tab of their own, which leaves the app's tab in the
// background: there its timers slow down, every later action waits out Bombadil's quiescence timeout, and the
// walk stalls.
const inTab = clicks.filter((a) => {
  const f = isAction(a, "Click") ? (a.Click.fingerprint as any) : {}
  return !/^(Print this message|Show source|Open|Open in new window)$/.test(f.textContent || f.accessibleName || "")
})

// Menu entries, whenever a menu is open: a menu that does nothing (the dead List options entries) needs a walk
// that gets as far as clicking one.
const menuEntries = inTab.filter((a) => isAction(a, "Click") && ((a.Click.fingerprint as any).role ?? "").startsWith("menuitem"))

export const explore = weighted([
  [100, inTab],
  [200, menuEntries],
  [60, keys],
  [40, inputs],
  [20, search],
  [20, scroll],
  [5, navigation]
])
