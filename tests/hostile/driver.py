"""The hostile suite's browser driver. It signs in through the front stand-in and opens the reader set's cases in the
real client, one by one, the way a user clicks a message, and each one's print page (`PRINT`). Then it renders its
share of all cases in batches on a page from the app origin (`BATCH`), and after each case or batch reads what it did:
- a script ran: payloads post a message to the page (cases.py), counted per frame;
- an HTTP request reached the sink, which takes every outbound TCP connection of the browser (sink.py);
- the frame lost its sandbox, the page navigated, a window opened, or the message never rendered.

Those fail a case in both modes. `strict`, for the sanitized server, also fails on any DNS query (dnsmasq's log), sink
connection without a request, other packet (the firewall's kernel log), or request from the mail frame to the app
origin outside the signed inline and image routes (front's log). `sandbox`, for the server with the sanitizer off,
counts each distinct one as a note: sandbox and CSP do not stop those (docs/design.md, Security). A batch that leaks
splits in fresh browsers until the case is named.

usage: kubicle-hostile <chromium|firefox> <strict|sandbox> <session> <batch size> <reader.json> <part.json>
       <report.json>
The reader set (cases.py) goes through the reader; the part is this session's share of the batch pass. Session 1 runs
the reader pass; the others open one message in the reader only to copy its frame. Session i's WebDriver listens on
port 4443+i; its front, wrapper and logs are where ../stack.nix puts them.
"""

import errno
import json
import os
import sys
import time
from pathlib import Path

from selenium import webdriver
from selenium.common.exceptions import WebDriverException

# The wait after a frame or batch loaded. Firefox gets longer for its `srcset` race in the reader (docs/design.md,
# Security): with the app's `img-src` opened, 300 ms caught it in the `srcset` cases there.
SETTLE_MS = {"chromium": 50, "firefox": 300}

# The app page's side of the observation, installed after every page load: message events (the tripwire) and iframe
# load events counted in attributes on `<html>`, which outlive the WebDriver script that set them up.
INSTALL = """
const html = document.documentElement;
const bump = (key) => { html.dataset[key] = String(Number(html.dataset[key] || 0) + 1); };
window.addEventListener('message', () => bump('kubicleExec'));
document.addEventListener('load', (e) => { if (e.target.tagName === 'IFRAME') bump('kubicleLoads'); }, true);
"""

# Opens one message by clicking its row and answers once it rendered and settled. Leptos patches the reader in place,
# so the signs of a new message are the list's active row, the reader out of "Loading", and a new load of the frame
# when its document changed. The frame gets PRINT's 15 s: unsanitized in Chromium, httpleaks-all takes 2 to 5 s from
# the click to its frame's load on a quiet host, and took over 8 s on a starved one.
OPEN = """
const [uid, settle, done] = arguments;
const html = document.documentElement;
const count = (key) => Number(html.dataset[key] || 0);
const row = document.getElementById('row-' + uid);
if (!row) return done({ error: 'no row for uid ' + uid });
const before = document.querySelector('.reader iframe')?.srcdoc;
const loads = count('kubicleLoads');
const start = performance.now();
row.click();
let opened = null;
(function poll() {
  const active = document.querySelector('.list')?.getAttribute('aria-activedescendant') === 'row-' + uid;
  const article = active && document.querySelector('.reader article.message');
  const frame = article && article.querySelector('iframe');
  if (article && opened === null) opened = performance.now() - start;
  if (article && (!frame || frame.srcdoc === before || count('kubicleLoads') > loads)) {
    return setTimeout(() => done({
      sandbox: frame ? frame.getAttribute('sandbox') : null,
      attributes: frame ? [...frame.attributes].filter((a) => a.name !== 'srcdoc').map((a) => [a.name, a.value]) : null,
      size: frame ? [frame.getBoundingClientRect().width, frame.getBoundingClientRect().height] : null,
      exec: count('kubicleExec'),
      url: location.href,
      opened,
      ready: performance.now() - start,
    }), settle);
  }
  if (performance.now() - start > (article ? 15000 : 8000)) {
    const reader = document.querySelector('.reader')?.innerText.replace(/\\s+/g, ' ').slice(0, 120);
    return done({ error: (article ? 'the frame never loaded: ' : 'the message never opened: ') + reader });
  }
  setTimeout(poll, 10);
})();
"""

# Loads one message's print page in a frame on the app page and answers once it loaded and settled. The print page is
# the mail's HTML as a document of its own, under its own policy header and sandbox (render.rs, `print_response`)
# where the reader frame has the app's; the frame adds no sandbox, so those alone stand.
PRINT = """
const [folder, uid, settle, done] = arguments;
const url = `api/print?folder=${encodeURIComponent(folder)}&uid=${uid}`;
fetch(url).then((r) => {
  if (!r.ok) return done({ error: `api/print answered ${r.status}` });
  const frame = document.createElement('iframe');
  frame.style.cssText = 'position:absolute;top:0;left:0;width:1000px;height:800px;border:0';
  const finish = (answer) => { frame.remove(); done(answer); };
  const timer = setTimeout(() => finish({ error: 'the print page never loaded' }), 15000);
  frame.addEventListener('load', () => {
    clearTimeout(timer);
    setTimeout(() => finish({ exec: Number(document.documentElement.dataset.kubicleExec || 0) }), settle);
  }, { once: true });
  frame.src = url;
  document.body.append(frame);
}, (e) => done({ error: String(e) }));
"""

# Renders a batch of cases at once on the batch page, a page from the app origin, so the app's CSP header governs it
# as it does the reader. The page's own script (batch.js) does the rendering; this hands it the request and waits for
# the answer, both through attributes on `<html>`.
BATCH = """
const [cases, attributes, size, settle, done] = arguments;
const html = document.documentElement;
const nonce = String(Math.random());
delete html.dataset.result;
html.dataset.batch = JSON.stringify({ nonce, cases, attributes, size, settle });
(function poll() {
  const result = html.dataset.result && JSON.parse(html.dataset.result);
  if (result && result.nonce === nonce) return done(result);
  setTimeout(poll, 20);
})();
"""

# DNS answers the browser's own vendor hosts with this address (../stack.nix); their queries and packets are dropped,
# so a case is charged only with what its page asked for.
VENDOR_SINK = "198.51.100.2"


class Tail:
    """New lines of a growing text file since the last call."""

    def __init__(self, path):
        self.file = open(path, encoding="utf-8")  # noqa: SIM115 - kept open, read on from where it stopped
        self.file.seek(0, os.SEEK_END)

    def lines(self):
        return self.file.read().splitlines()


class Kmsg:
    """The kernel log from now on: the firewall's LOG rule writes one record per refused packet."""

    def __init__(self, prefix):
        self.fd = os.open("/dev/kmsg", os.O_RDONLY | os.O_NONBLOCK)  # needs root
        os.lseek(self.fd, 0, os.SEEK_END)
        self.prefix = prefix

    def leaks(self):
        out = []
        while True:
            try:
                line = os.read(self.fd, 8192).decode(errors="replace")
            except BlockingIOError:
                return out
            except OSError as e:
                if e.errno == errno.EPIPE:
                    continue  # records overwritten before we read them
                raise
            at = line.find(self.prefix)
            if at >= 0 and f"DST={VENDOR_SINK} " not in line:
                out.append(line[at:].rstrip())


def after(line, word):
    """The word after the first one `word` accepts."""
    words = line.split()
    for i, w in enumerate(words[:-1]):
        if word(w):
            return words[i + 1]
    return None


class Logs:
    """The logs one session's leaks land in (../stack.nix), read from where the last read stopped."""

    def __init__(self, i):
        self.dns = Tail(f"/var/log/dnsmasq{i}.log")
        self.front = Tail(f"/var/log/caddy/front{i}.log")
        self.sink = Tail(f"/var/log/sink{i}.log")
        self.kmsg = Kmsg(f"kubicle-leak{i} ")
        self.vendor = set()  # the names DNS answered with the vendor sink

    def queries(self, lines):
        """The queries a page made: dnsmasq logs each as `query[TYPE] name from ...`, then its answer."""
        for line in lines:
            if line.endswith(f" is {VENDOR_SINK}") and (name := after(line, lambda w: w == "config")):
                self.vendor.add(name)
        return [line for line in lines if (n := after(line, lambda w: w.startswith("query["))) and n not in self.vendor]

    def read(self):
        """HTTP requests that reached the sink, and everything else: DNS queries, connections without a request,
        other packets, and the mail frame's requests to the app origin."""
        requests, hints = [], []
        for line in self.sink.lines():
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if str(event.get("dst", "")).startswith(f"{VENDOR_SINK}:"):
                continue
            if event.get("event") != "request":
                hints.append(f"connect: {line}")
            elif event.get("purpose") == ["prefetch", "navigate"]:
                # Chromium's NoStatePrefetch of a `<link rel=prerender>`, which ignores CSP and the sandbox, so only
                # ammonia stops it (docs/design.md, Security): a hint, which fails only the sanitized run.
                hints.append(f"prefetch: {line}")
            else:
                requests.append(f"http request: {line}")
        hints += [f"dns: {line}" for line in self.queries(self.dns.lines())]
        hints += [f"packet: {line}" for line in self.kmsg.leaks()]
        hints += [f"app request: {line}" for line in frame_requests(self.front.lines())]
        return requests, hints


def frame_requests(lines):
    """Front's log lines for requests the mail frame made to the app: the frame's origin is opaque, so its requests are
    cross-site, and only the signed inline and image routes may answer them."""
    out = []
    for line in lines:
        try:
            request = json.loads(line)["request"]
        except (ValueError, KeyError, TypeError):
            continue
        site = (request.get("headers", {}).get("Sec-Fetch-Site") or ["none"])[0]
        uri = request.get("uri", "")
        if site not in ("same-origin", "none") and not uri.startswith(("/login/inline?", "/login/image?")):
            out.append(f"{site} {request.get('method', '')} {uri}")
    return out


def hint_key(hint):
    """A hint without its timestamps and ports, so the same leak counts once: the name a DNS query asked for, where a
    connection went, what a packet was sent to, which app path the frame asked for."""
    kind, _, rest = hint.partition(": ")
    if kind == "dns":
        what = after(rest, lambda w: w.startswith("query[")) or rest
    elif kind == "connect":
        try:
            what = json.loads(rest).get("dst") or rest
        except ValueError:
            what = rest
    elif kind == "packet":
        what = " ".join(w for w in rest.split() if w.startswith(("DST=", "DPT=", "PROTO=")))
    else:
        what = rest
    return f"{kind} {what}"


def connect(browser, i):
    """A WebDriver session; the binary is ../stack.nix's wrapper, which runs the browser as the user the firewall
    watches."""
    binary = f"/run/current-system/sw/bin/browser{i}"
    if browser == "chromium":
        options = webdriver.ChromeOptions()
        options.binary_location = binary
        # Past the window size, the flags stop Chromium's own background fetches, which would show as leaks.
        for arg in [
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--window-size=1280,900",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-default-apps",
            "--disable-domain-reliability",
            "--no-service-autorun",
            "--disable-client-side-phishing-detection",
            "--disable-features=OptimizationHints,AutofillServerCommunication,MediaRouter,Translate",
        ]:
            options.add_argument(arg)
    elif browser == "firefox":
        options = webdriver.FirefoxOptions()
        options.binary_location = binary
        options.add_argument("-headless")
        # Selenium asks for the remote agent by default, which every Firefox opens on port 9222: a VM's second session
        # then never starts ("Failed to decode response from marionette").
        options.set_capability("moz:debuggerAddress", False)
        for name, value in {  # Firefox's own background fetches, which would show as leaks
            "services.settings.server": "data:,#remote-settings-dummy/v1",
            "network.connectivity-service.enabled": False,
            "network.captive-portal-service.enabled": False,
            "app.normandy.enabled": False,
            "messaging-system.rsexperimentloader.enabled": False,
            "network.trr.mode": 5,
        }.items():
            options.set_preference(name, value)
    else:
        sys.exit(f"unknown browser {browser}")
    options.accept_insecure_certs = True  # the sink's certificate, so an HTTPS request shows (sink.py)
    start = time.monotonic()
    while True:
        # The previous session's browser may still be closing: geckodriver refuses a second session until it has.
        try:
            driver = webdriver.Remote(command_executor=f"http://127.0.0.1:{4443 + i}", options=options)
            break
        except WebDriverException as e:
            if time.monotonic() - start > 30:
                raise
            print(f"WebDriver session not ready yet: {e.msg}", file=sys.stderr)
            time.sleep(0.5)
    driver.set_script_timeout(300)
    return driver


def sign_in(driver, app):
    driver.get(f"{app}sso.php")
    # Every corpus folder fits one page at 200, the largest page size the server takes (Roundcube's 2 to 200).
    status = driver.execute_async_script("""const done = arguments[0];
        fetch('api/session').then((r) => r.json()).then((s) => fetch('api/settings', {
          method: 'POST', headers: {'content-type': 'application/json', 'x-kubicle-csrf': s.csrf},
          body: JSON.stringify({...s.settings, page_size: 200}),
        })).then((r) => done(r.status), (e) => done(String(e)));""")
    assert status == 200, f"page size not saved: {status}"  # not 204: the answer carries the settings' new revision
    driver.get(app)
    driver.execute_script(INSTALL)
    # Check the checker: a message posted to the page must be counted.
    driver.execute_script("window.postMessage(1, '*')")
    time.sleep(0.2)
    counted = driver.execute_script("return document.documentElement.dataset.kubicleExec")
    assert counted == "1", "the tripwire listener does not count messages"
    driver.execute_script("document.documentElement.dataset.kubicleExec = '0'")


def open_folder(driver, folder, rows):
    start = time.monotonic()
    # After a reload the folder pane fills in once the session answers.
    while not driver.execute_script(
        """const f = [...document.querySelectorAll('.folder')].find((e) => e.querySelector('.folder-name')?.textContent
           === arguments[0]); if (f) f.click(); return !!f;""",
        folder,
    ):
        assert time.monotonic() - start < 30, f"no folder {folder} in the folder pane"
        time.sleep(0.1)
    while driver.execute_script("return document.querySelectorAll('.row').length") != rows:
        assert time.monotonic() - start < 60, f"folder {folder} never listed {rows} rows"
        time.sleep(0.1)


def open_batch_page(driver, app):
    driver.get(f"{app}hostile-batch.html")
    start = time.monotonic()
    while driver.execute_script("return document.documentElement.dataset.ready") != "1":
        assert time.monotonic() - start < 30, "the batch page's script never started"
        time.sleep(0.05)


def windows(driver):
    try:
        return len(driver.window_handles)
    except WebDriverException:
        return 0


class Run:
    def __init__(self, browser, session, strict):
        self.browser, self.session, self.strict = browser, session, strict
        self.settle = SETTLE_MS[browser]
        self.app = f"http://localhost:{8087 + session}/login/"
        self.driver = connect(browser, session)
        self.logs = Logs(session)
        self.frame = (None, None)  # the live reader frame's attributes and size, copied onto every batch frame
        self.failures = []
        # Distinct hints of the sandbox-mode run (see `hint_key`): a count that grows when a new kind of leak appears.
        self.notes = set()
        self.batches = 0

    def restart(self):
        """A fresh browser, signed in: bisecting a leak needs empty DNS and connection caches."""
        try:
            self.driver.quit()  # geckodriver holds one session at a time
        except WebDriverException:
            pass
        self.driver = connect(self.browser, self.session)
        sign_in(self.driver, self.app)
        open_batch_page(self.driver, self.app)
        self.logs.read()

    def fail(self, case, problem):
        failure = {"case": case, "problem": problem}
        if failure in self.failures:
            return  # a bisection reran the case
        print(f"FAIL {case}: {problem}", file=sys.stderr)
        self.failures.append(failure)

    def note_or_fail(self, problems, hints):
        if self.strict:
            problems += hints
        else:
            self.notes.update(hint_key(h) for h in hints)

    def check(self, cases):
        """Opens `cases` in one batch. Per-frame problems are named at once; a leak, which the logs cannot pin to a
        frame, fails the batch, and the halves rerun in fresh browsers until it names its case."""
        self.batches += 1
        listed = [{"key": c["id"], "folder": c["folder"], "uid": c["uid"]} for c in cases]
        try:
            seen = self.driver.execute_async_script(BATCH, listed, self.frame[0], self.frame[1], self.settle)
        except WebDriverException as e:
            seen, error = None, e.msg
        time.sleep(0.1)  # log writers flush after the last event
        leaks, hints = self.logs.read()
        if seen is None:
            leaks.append(f"the page stopped answering: {error}")
        else:
            print(
                f"batch of {len(cases)}: {seen['frames']} frames, fetched in {seen.get('fetched') or 0:.0f} ms, loaded"
                f" in {seen.get('loaded') or 0:.0f} ms, then {self.settle} ms settle",
                file=sys.stderr,
            )
            for key, count in (seen.get("hits") or {}).items():
                self.fail(key, f"a script ran and posted {count} message(s)")
            for key, problem in seen.get("errors") or []:
                self.fail(key, problem)
            if (seen.get("stray") or 0) > 0 or not str(seen.get("url", "")).endswith("hostile-batch.html"):
                leaks.append(f"a script ran outside the frames, or the page navigated to {json.dumps(seen.get('url'))}")
        if windows(self.driver) != 1:
            leaks.append("a window opened")
        self.note_or_fail(leaks, hints)
        if not leaks:
            return
        if len(cases) == 1:
            return self.fail(cases[0]["id"], "; ".join(leaks))
        before = len(self.failures)
        half = len(cases) // 2
        for part in (cases[:half], cases[half:]):
            self.restart()
            self.check(part)
        if len(self.failures) == before:
            ids = " ".join(c["id"] for c in cases)
            self.fail(ids, f"{'; '.join(leaks)} (and neither half alone repeats it)")


def reader_pass(run, folders):
    """The reader pass: `folders`' cases opened one by one through the real client, the way a user clicks them, and
    each one's print page. It proves those paths end to end and gives the batch pass the live frame's attributes."""

    # Without the fragment: the client keeps the open folder there (`#folder=`), and the frame's sandbox has no
    # allow-top-navigation, so only a change before the `#` is the page leaving.
    def page(url):
        return url.split("#")[0]

    home, exec_seen = page(run.driver.current_url), 0
    for folder, ids in sorted(folders.items()):
        open_folder(run.driver, folder, len(ids))
        for index, case in enumerate(ids):
            if case is None:
                continue
            problems = []
            try:
                seen = run.driver.execute_async_script(OPEN, index + 1, run.settle)
                if seen.get("error"):
                    problems.append(seen["error"])
                sandbox = seen.get("sandbox")
                if sandbox and any(t in ("allow-scripts", "allow-same-origin") for t in sandbox.split()):
                    problems.append(f"frame sandbox: {sandbox}")
                if seen.get("attributes") is not None and run.frame[0] is None:
                    run.frame = (seen["attributes"], seen["size"])
                if (seen.get("exec") or 0) > exec_seen:
                    problems.append(f"a script ran and posted {seen['exec'] - exec_seen} message(s)")
                    exec_seen = seen["exec"]
                if seen.get("url") and page(seen["url"]) != home:
                    problems.append(f"the page navigated to {json.dumps(seen['url'])}")
            except WebDriverException as e:
                problems.append(f"the page stopped answering: {e.msg}")
            try:
                seen = run.driver.execute_async_script(PRINT, folder, index + 1, run.settle)
                if seen.get("error"):
                    problems.append(f"print page: {seen['error']}")
                if (seen.get("exec") or exec_seen) > exec_seen:
                    problems.append(f"a script ran on the print page and posted {seen['exec'] - exec_seen} message(s)")
                    exec_seen = seen["exec"]
            except WebDriverException as e:
                problems.append(f"the print page stopped answering: {e.msg}")
            if windows(run.driver) != 1:
                problems.append("a window opened")
            requests, hints = run.logs.read()
            problems += requests
            run.note_or_fail(problems, hints)
            if problems:
                run.fail(case, f"in the reader: {'; '.join(problems)}")
                run.driver.get(home)
                run.driver.execute_script(INSTALL)
                exec_seen = 0
                open_folder(run.driver, folder, len(ids))


def main():
    if len(sys.argv) != 8:
        sys.exit("usage: kubicle-hostile <browser> <mode> <session> <batch size> <reader.json> <part.json> <report.json>")
    _, browser, mode, session, batch, reader_path, part_path, report = sys.argv
    if mode not in ("strict", "sandbox"):
        sys.exit(f"unknown mode {mode}")
    session, batch = int(session), int(batch)
    # Case ids in UID order; a subset (the quick set) has null where a case is left out.
    reader, part = (json.loads(Path(p).read_text()) for p in (reader_path, part_path))
    run = Run(browser, session, mode == "strict")
    started = time.monotonic()
    sign_in(run.driver, run.app)
    requests, hints = run.logs.read()
    if requests or hints:
        run.fail("(sign-in)", "; ".join(requests + hints))  # nothing opened yet: the app itself leaked

    if session > 1:
        # Only the first own case, an HTML mail, to copy the frame from; session 1 checks the reader path.
        reader = {f: [case if n == 0 else None for n, case in enumerate(ids)] for f, ids in reader.items() if f == "own"}
    reader_pass(run, reader)
    reader_seconds = time.monotonic() - started
    assert run.frame[0] is not None, "no HTML mail in the reader pass to copy the frame from"

    open_batch_page(run.driver, run.app)
    # The batch page and the app page get the same CSP header, from the same server middleware.
    csp = run.driver.execute_async_script("""const done = arguments[0]; Promise.all(['./', 'hostile-batch.html']
        .map((u) => fetch(u))).then((rs) => done(rs.map((r) => r.headers.get('content-security-policy'))));""")
    assert isinstance(csp[0], str) and csp[0] == csp[1], f"the batch page's CSP differs from the app's: {csp}"
    cases = [
        {"folder": folder, "uid": n + 1, "id": case}
        for folder, ids in sorted(part.items())
        for n, case in enumerate(ids)
        if case is not None
    ]
    for at in range(0, len(cases), batch):
        run.check(cases[at : at + batch])

    seconds = time.monotonic() - started
    summary = {
        "browser": browser,
        "mode": mode,
        "cases": len(cases),
        "batch": batch,
        "batches": run.batches,
        "reader_seconds": reader_seconds,
        "seconds": seconds,
        "failures": run.failures,
        "notes": sorted(run.notes),
    }
    with open(report, "w") as f:
        json.dump(summary, f, indent=2)
    for note in sorted(run.notes):
        print(f"NOTE {note}", file=sys.stderr)
    print(
        f"{browser} {mode}: {len(cases)} cases in batches of {batch} ({run.batches} batches run), {len(run.failures)}"
        f" failing, {len(run.notes)} distinct notes; reader pass {reader_seconds:.0f} s, all {seconds:.0f} s",
        file=sys.stderr,
    )
    try:
        run.driver.quit()
    except WebDriverException:
        pass
    sys.exit(1 if run.failures else 0)


if __name__ == "__main__":
    main()
