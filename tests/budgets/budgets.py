"""DEVELOPMENT ONLY, run by run.sh beside it: the speed budgets of docs/design.md on this machine. Never a flake
check: in a VM every step ran two to five times slower, so its numbers only ever failed (Oct 25, 2026).

usage: budgets.py SERVER_BIN WEB_DIR CA_FILE

Each budget prints its median and fails above its limit. Browser times run from the input event to the change on
screen, after a forced layout; a median of several runs keeps one slow frame from deciding. The fresh server's
sign-in runs five times, sso.php to the first INBOX page, each on a new server; the browser's budgets then run
against the fifth."""

import http.client
import os
import subprocess
import sys
import tempfile
import time

from playwright.sync_api import sync_playwright

SERVER, WEB, CA = sys.argv[1:4]
PORT = 8381  # a port no other kubicle on this machine uses, so a dev server can keep running
IDENTITY = {"X-Remote-User": "andrew@example.test", "X-Remote-User-Token": "fixture-password"}  # what Mailu front adds on sso.php
DATA = tempfile.TemporaryDirectory(prefix="kubicle-budgets-")  # holds each server's data directory; deleted at exit
failed: list[str] = []

HELPERS = """
const tick = () => new Promise(r => { const c = new MessageChannel(); c.port1.onmessage = r; c.port2.postMessage(0); });
const pause = ms => new Promise(r => setTimeout(r, ms));
const timeline = async (act, ...conditions) => {  // ms until each condition first holds; null if never within 5 s
  const at = conditions.map(() => null), t0 = performance.now();
  act();
  while (at.includes(null) && performance.now() - t0 < 5000) {
    await tick();
    conditions.forEach((c, i) => {
      if (at[i] === null && c()) { document.body.offsetHeight; at[i] = performance.now() - t0; }
    });
  }
  return at;
};
const key = (k, target = window) => target.dispatchEvent(new KeyboardEvent('keydown', {key: k, bubbles: true}));
const rows = () => [...document.querySelectorAll('.row')];
const openRow = () => document.querySelector('.row.open');
const subjectOf = r => r.querySelector('.row-subject').textContent;
const reader = () => document.querySelector('.message-subject')?.textContent;
const paging = () => document.querySelector('.paging .muted')?.title;
const title = () => document.querySelector('.list-pane .bar-title')?.textContent;
const folder = label => [...document.querySelectorAll('.folder')]
  .find(b => b.querySelector('.folder-name').textContent === label);
"""


def measure(body: str) -> dict:
    result = page.evaluate("async () => {" + HELPERS + body + "}")
    assert "error" not in result, result
    return result


def within(name: str, values: list, budget: float) -> None:
    if None in values:
        print(f"FAIL {name}: a change never showed: {values}")
        return failed.append(name)
    took = sorted(values)[len(values) // 2]
    ok = took < budget
    print(
        f"{'ok  ' if ok else 'FAIL'} {name:34} median {took:7.1f} ms   budget {budget:5.0f} ms   {[round(v, 1) for v in values]}"
    )
    if not ok:
        failed.append(name)


def arrows() -> None:
    m = measure("""
        folder('Inbox').click(); await pause(1000);
        rows()[0].click(); await pause(1000);
        const moved = [], shown = [];
        for (let i = 0; i < 10; i++) {
          const next = rows()[rows().indexOf(openRow()) + 1], id = next.id, subject = subjectOf(next);
          const [m, s] = await timeline(() => key('ArrowDown'), () => openRow()?.id === id, () => reader() === subject);
          moved.push(m); shown.push(s); await pause(500);
        }
        return {moved, shown};""")
    within("arrow keys", m["moved"], 16)
    within("open a prefetched message", m["shown"], 16)


def open_not_prefetched() -> None:
    m = measure("""
        const opened = [];
        for (let page = 0; page < 5; page++) {
          await pause(1500);
          const subject = subjectOf(rows().at(-1));
          opened.push((await timeline(() => key('End'), () => reader() === subject))[0]);
          document.querySelector('[aria-label="Next page"]').click();
        }
        return {opened};""")
    within("open a message not prefetched", m["opened"], 50)


def paging() -> None:
    m = measure("""
        const turned = [];
        for (let i = 0; i < 5; i++) {
          await pause(1500);
          const before = paging();
          turned.push((await timeline(() => document.querySelector('[aria-label="Next page"]').click(),
            () => paging() !== before && rows().length > 0))[0]);
        }
        return {turned};""")
    within("paging", m["turned"], 16)


def folder_switch() -> None:
    # Only folders no change touched since sign-in, so each switch shows the first page the client prefetched: a
    # change drops that page (state.rs `refresh`), and the switch would wait for the server.
    m = measure("""
        const switched = [];
        for (const label of ['github', 'finance', 'Junk', 'Archive', 'Inbox']) {
          await pause(1000);
          const before = paging();
          switched.push((await timeline(() => folder(label).click(),
            () => title() === label && paging() !== before && rows().length > 0))[0]);
        }
        return {switched};""")
    within("folder switch", m["switched"], 16)


def sorting() -> None:
    m = measure("""
        const sorted = [];
        for (const column of ['From', 'Subject', 'Arrival date']) {
          await pause(1500);
          document.querySelector('[aria-label="List options"]').click(); await pause(200);
          const select = document.querySelector('select[aria-label="Sorting column"]');
          select.value = [...select.options].find(o => o.textContent === column).value;
          select.dispatchEvent(new Event('change', {bubbles: true}));
          const before = rows()[0].id;
          sorted.push((await timeline(() => document.querySelector('.dialog .button.primary').click(),
            () => rows().length > 0 && rows()[0].id !== before))[0]);
        }
        return {sorted};""")
    within("sorting", m["sorted"], 50)


def search() -> None:
    m = measure("""
        const input = document.getElementById('search'), searched = [];
        for (const text of ['invoice', 'kernel', 'Olena']) {
          await pause(1000);
          input.value = text; input.dispatchEvent(new Event('input', {bubbles: true}));
          searched.push((await timeline(() => key('Enter', input),
            () => document.title.endsWith(':: Search result') && /^(Messages|No results)/.test(paging() ?? '')))[0]);
          key('Escape', input);
        }
        return {searched};""")
    within("search on 50,000 messages", m["searched"], 100)


def app_start() -> None:
    watch = """new MutationObserver((_, o) => {
        if (document.querySelector('.row')) { window.listVisible = performance.now(); o.disconnect(); }
        }).observe(document, {childList: true, subtree: true});"""
    page.add_init_script(watch)  # before the app's own code
    started = []
    for _ in range(3):
        page.goto(f"http://127.0.0.1:{PORT}/")
        page.wait_for_function("window.listVisible", timeout=10000)
        started.append(page.evaluate("window.listVisible"))
    within("app start to list visible", started, 50)


def start_server(data: str) -> subprocess.Popen:
    env = {
        "PATH": os.environ["PATH"],
        "RUST_LOG": "warn",
        "KUBICLE_LISTEN": f"127.0.0.1:{PORT}",
        "KUBICLE_TRUSTED_PROXY": "127.0.0.1/32",
        "KUBICLE_WEB_DIR": WEB,
        "KUBICLE_DATA_DIR": data,
        "KUBICLE_PUBLIC_ORIGIN": f"http://127.0.0.1:{PORT}",
        "KUBICLE_BASE_PATH": "/",  # no Mailu front strips /login/ here: under it inline images would 404
        "KUBICLE_MAIL_HOST": "127.0.0.1",
        "KUBICLE_MAIL_TLS_NAME": "imap.example.test",
        "KUBICLE_MAIL_EXTRA_CA": CA,
        "KUBICLE_MAIL_DOMAINS": "example.test",
    }
    server = subprocess.Popen([SERVER], env=env)
    for _ in range(100):
        try:
            http.client.HTTPConnection("127.0.0.1", PORT, timeout=1).request("GET", "/healthz")
            return server
        except OSError:
            time.sleep(0.05)
    raise SystemExit("the server did not start")


def fresh_server() -> subprocess.Popen:
    """A fresh kubicle holds nothing: sso.php starts the engine, and the first INBOX page is Dovecot's SORT."""
    took, server = [], None
    for _ in range(5):  # runs spread by about 30 ms: in a median of three, two slow ones would set the result
        if server:
            server.terminate()
            server.wait()
        server = start_server(tempfile.mkdtemp(dir=DATA.name))
        t0 = time.perf_counter()
        conn = http.client.HTTPConnection("127.0.0.1", PORT)
        conn.request("GET", "/sso.php", headers=IDENTITY)
        signed_in = conn.getresponse()
        signed_in.read()
        cookie = signed_in.getheader("Set-Cookie")
        assert cookie, f"sso.php set no cookie: {signed_in.status}"
        cookie = cookie.split(";")[0]
        conn.request(
            "GET", "/api/list?folder=INBOX&page=0&size=50&sort=Arrival&order=Descending&threads=false", headers={"Cookie": cookie}
        )
        listed = conn.getresponse()
        listed.read()
        assert listed.status == 200, listed.status
        took.append((time.perf_counter() - t0) * 1000)
    within("fresh server, sign-in to the first page of 50,000 messages", took, 300)
    assert server is not None, "the loop above starts one"
    return server


server = fresh_server()
try:
    with sync_playwright() as p:
        context = p.chromium.launch().new_context(viewport={"width": 1230, "height": 900}, bypass_csp=True)
        page = context.new_page()
        page.set_extra_http_headers(IDENTITY)  # for sso.php only: cleared once it has signed in
        page.goto(f"http://127.0.0.1:{PORT}/sso.php")
        page.set_extra_http_headers({})
        page.wait_for_function("document.querySelectorAll('.row').length === 50", timeout=60000)
        page.wait_for_timeout(3000)  # until the other folders' first pages are prefetched, so none waits ahead of a click
        for budget in (arrows, open_not_prefetched, paging, folder_switch, sorting, search, app_start):
            budget()
finally:
    server.terminate()
    server.wait()
print("FAILED: " + ", ".join(failed) if failed else "all budgets met")
sys.exit(1 if failed else 0)
