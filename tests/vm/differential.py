# vm-settings: kubicle against Roundcube, the webmail it copies, on combinations of the settings both have. Each
# webmail has its own account with the same seeded mailbox (mailu.nix, `roundcube`); every combination is set in
# both, one fixed script of actions runs in both pages at once, and after each step the two accounts' state on the
# mail server must match (mailstate.py).
#
# Nothing waits for time. Each action waits for its page to say the work is done (kubicle's toast, Roundcube's
# idle request queue) and for the page's requests to go quiet, which a script injected into every page logs
# (REQUEST_LOG); "mark as read after 5 s" waits for each page's own mark request. Both browsers are driven from
# here through chromedriver on a forwarded port, one thread each, and the mail state comes from a server in the VM
# that keeps one IMAP connection per account.
#
# Combinations are pairwise by default: every two values of every two settings meet in one. Set
# KUBICLE_SETTINGS_RANDOM=SEED:COUNT for COUNT random ones instead, and KUBICLE_SETTINGS_ONLY=3,7 to run only those
# of the list, as a failure names them. Every difference goes to $out/differences.txt, with each combination whole,
# and the time of every step to $out/timing.txt.
# mypy: disable-error-code="name-defined"
# base64, json, os, pathlib, re, shlex, time, traceback and urllib come imported from record.py and scenarios.py, which
# run first: the driver's lint refuses a second import.
import collections
import email.utils
import itertools
import random
import socket
import statistics
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

# Each setting both webmails have whose effect this script can see on the server, with the values it tries, as
# kubicle's Settings stores them. roundcube_input() maps each to Roundcube's preference. `identity`, `trash` and
# `sent` stand for kubicle's identities and special_folders maps.
FACTORS: list[tuple[str, list]] = [
    ("mark_read_after", [0, None, 5]),
    ("display_next", [True, False]),
    ("reply_mode", ["Below", "Above", "AboveNoIndent", "Empty"]),
    ("forward_attachment", [False, True]),
    ("mdn_default", [False, True]),
    ("dsn_default", [False, True]),
    ("reply_same_folder", [False, True]),
    ("reply_all_list", [True, False]),
    ("read_when_deleted", [True, False]),
    ("delete_junk", [False, True]),
    ("logout_purge", ["Never", "All", {"OlderThan": 30}]),
    ("read_on_archive", [False, True]),
    ("archive_type", ["None", "Year", "Month", "ThunderbirdMonth", "Sender", "Folder", "FolderYear", "FolderMonth"]),
    # Auto is Europe/Kyiv for both: the browser's zone and Roundcube's PHP zone are set to it (settings.nix, mailu.nix).
    ("time_zone", [None, "UTC", "Asia/Tokyo"]),
    ("display_name", ["", "Kdt Tester"]),
    ("identity", ["plain", "with fields"]),
    ("trash", ["server", None]),
    ("sent", ["server", None]),
]
IDENTITY = {"organization": "Kdt Org", "reply_to": "replies@example.net", "bcc": "audit@example.net"}

KDT = "KDT-TYPED"  # mailstate.py's mark for typed text
ROUNDCUBE = "http://localhost/roundcube/"


def pairwise(sizes: list[int], rng: random.Random, tries: int = 40) -> list[list[int]]:
    """Rows of value indices in which every two values of every two factors meet at least once, greedily: each row
    starts from a pair not yet covered and takes, factor by factor, the value that covers the most pairs still open;
    the best of `tries` such rows goes in."""
    factors = range(len(sizes))
    uncovered = {(i, a, j, b) for i, j in itertools.combinations(factors, 2) for a in range(sizes[i]) for b in range(sizes[j])}

    def pairs(row: dict[int, int]) -> set:
        return {(i, row[i], j, row[j]) for i, j in itertools.combinations(sorted(row), 2)}

    rows = []
    while uncovered:
        best: dict[int, int] = {}
        for _ in range(tries):
            i, a, j, b = rng.choice(sorted(uncovered))
            row = {i: a, j: b}
            rest = [f for f in factors if f not in row]
            rng.shuffle(rest)
            for f in rest:
                gains = [len(pairs({**row, f: v}) & uncovered) for v in range(sizes[f])]
                row[f] = rng.choice([v for v, g in enumerate(gains) if g == max(gains)])
            if len(pairs(row) & uncovered) > len(pairs(best) & uncovered):
                best = row
        rows.append([best[f] for f in factors])
        uncovered -= pairs(best)
    return rows


def combinations() -> list[dict]:
    sizes = [len(values) for _, values in FACTORS]
    if random_run := os.environ.get("KUBICLE_SETTINGS_RANDOM"):
        seed, count = random_run.split(":")
        rng = random.Random(int(seed))
        rows = [[rng.randrange(n) for n in sizes] for _ in range(int(count))]
    else:
        rows = pairwise(sizes, random.Random(1))
    return [{name: values[i] for (name, values), i in zip(FACTORS, row)} for row in rows]


def kubicle_settings(combo: dict, me: str) -> dict:
    settings = {name: combo[name] for name, _ in FACTORS if name not in ("identity", "trash", "sent")}
    fields = {
        "organization": IDENTITY["organization"],
        "reply_to": [{"name": None, "email": IDENTITY["reply_to"]}],
        "bcc": [{"name": None, "email": IDENTITY["bcc"]}],
    }
    settings["identities"] = {me: fields} if combo["identity"] == "with fields" else {}
    settings["special_folders"] = {role: None for role, name in [("Trash", "trash"), ("Sent", "sent")] if combo[name] is None}
    return settings


def roundcube_input(combo: dict) -> dict:
    """The preferences and identity fields Roundcube's Settings would store for `combo`, typed as prefs_save.php and
    identity_save.php store them."""
    with_fields = combo["identity"] == "with fields"
    prefs = {
        "mail_read_time": {0: 0, None: -1, 5: 5}[combo["mark_read_after"]],
        "display_next": combo["display_next"],
        "reply_mode": {"Empty": -1, "Below": 0, "Above": 1, "AboveNoIndent": 2}[combo["reply_mode"]],
        "forward_attachment": combo["forward_attachment"],
        "mdn_default": combo["mdn_default"],
        "dsn_default": combo["dsn_default"],
        "reply_same_folder": combo["reply_same_folder"],
        "reply_all_mode": 1 if combo["reply_all_list"] else 0,
        "read_when_deleted": combo["read_when_deleted"],
        "delete_junk": combo["delete_junk"],
        "logout_purge": {"Never": False, "All": True}.get(str(combo["logout_purge"]), "30"),
        "read_on_archive": combo["read_on_archive"],
        "archive_type": {
            "None": "",
            "Year": "year",
            "Month": "month",
            "ThunderbirdMonth": "tbmonth",
            "Sender": "sender",
            "Folder": "folder",
            "FolderYear": "folderyear",
            "FolderMonth": "foldermonth",
        }[combo["archive_type"]],
        "timezone": combo["time_zone"] or "auto",
        # Special Folders' Save names every role and stops following SPECIAL-USE (prefs_save.php, section folders).
        "drafts_mbox": "Drafts",
        "sent_mbox": "" if combo["sent"] is None else "Sent",
        "junk_mbox": "Junk",
        "trash_mbox": "" if combo["trash"] is None else "Trash",
        "archive_mbox": "Archive",
        "lock_special_folders": True,
        "show_real_foldernames": False,
    }
    identity = {
        "name": combo["display_name"],
        "organization": IDENTITY["organization"] if with_fields else "",
        "reply-to": IDENTITY["reply_to"] if with_fields else "",
        "bcc": IDENTITY["bcc"] if with_fields else "",
    }
    return {"prefs": prefs, "identity": identity}


# Injected into every document of both browsers before its own scripts (CDP): each fetch and XMLHttpRequest goes
# into the top window's `kdtLog` with its method, address, text body, and when its answer came. Frames the top
# window cannot reach (kubicle's sandboxed mail) log nothing. kubicle's event stream stays out, as it never ends;
# its messages go into `kdtEvents`.
REQUEST_LOG = """(() => {
    let root;
    try { root = window.top; root.kdtLog ??= []; } catch (e) { return; }
    const log = root.kdtLog, now = () => root.performance.now();
    const track = (entry, answer) => { log.push(entry); answer.finally(() => { entry.done = true; entry.at = now(); }); };
    const fetch = window.fetch;
    window.fetch = function (input, init) {
        const answer = fetch.apply(this, arguments);
        const body = typeof init?.body === 'string' ? init.body : '';
        const entry = {m: init?.method ?? 'GET', u: String(input?.url ?? input), b: body};
        if (!entry.u.includes('api/events')) track(entry, answer.catch(() => {}));
        return answer;
    };
    const Source = window.EventSource;
    window.EventSource = Object.assign(function (url, options) {
        const source = new Source(url, options);
        source.addEventListener('message', e => (root.kdtEvents ??= []).push(String(e.data).slice(0, 300)));
        return source;
    }, {prototype: Source.prototype, CONNECTING: 0, OPEN: 1, CLOSED: 2});
    const open = XMLHttpRequest.prototype.open, send = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function (m, u) { this.kdt = {m, u: String(u)}; return open.apply(this, arguments); };
    XMLHttpRequest.prototype.send = function (b) {
        this.kdt.b = typeof b === 'string' ? b : '';
        track(this.kdt, new Promise(resolve => this.addEventListener('loadend', resolve)));
        return send.apply(this, arguments);
    };
})();"""
# Every request answered, the last one more than 150 ms ago: a change's answer starts the next request at once
# (the next message's body, its mark as read), so a gap that long means the page is done.
QUIET = """(l => !l.some(e => !e.done) && performance.now() - l.reduce((a, e) => Math.max(a, e.at ?? 0), 0) > 150)
    (window.kdtLog ?? [])"""
# kubicle's and Roundcube's request that marks a message read, logged since `{0}`.
KUBICLE_MARKED = """(window.kdtLog ?? []).slice({0}).some(e => e.done && e.m === 'POST' && e.u.includes('api/change')
    && e.b.includes('"flag":"Seen","set":true'))"""
ROUNDCUBE_MARKED = (
    "(window.kdtLog ?? []).slice({0}).some(e => e.done && e.u.includes('_action=mark') && e.b.includes('_flag=read'))"
)


def css(selector: str) -> str:
    return f"document.querySelector({json.dumps(selector)})"


def visible(selector: str) -> str:
    return f"[...document.querySelectorAll({json.dumps(selector)})].find(e => e.offsetParent !== null)"


class Browser:
    """One Chromium session through chromedriver, spoken to directly: a call costs a few ms, and each browser's
    calls can run in a thread of their own, as the driver's shell cannot."""

    def __init__(self, name: str, port: int, session: str) -> None:
        self.name = name
        self.base = f"http://127.0.0.1:{port}/session/{session}"
        self.call("POST", "/timeouts", {"script": 120_000})
        self.call("POST", "/goog/cdp/execute", {"cmd": "Page.addScriptToEvaluateOnNewDocument", "params": {"source": REQUEST_LOG}})

    def call(self, method: str, path: str, body: dict | None = None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data, {"content-type": "application/json"}, method=method)
        try:
            with urllib.request.urlopen(request, timeout=150) as answer:
                return json.load(answer)["value"]
        except urllib.error.HTTPError as e:
            error = json.load(e).get("value", {})
            raise RuntimeError(f"{self.name}: webdriver {error.get('error')}: {str(error.get('message'))[:300]}") from None

    def js(self, script: str):
        return self.call("POST", "/execute/sync", {"script": script, "args": []})

    def wait(self, expression: str, seconds: float = 30):
        """The value of `expression` once it is truthy, which the page itself polls every 10 ms."""
        deadline = time.monotonic() + seconds
        while True:
            left = max(0, int((deadline - time.monotonic()) * 1000))
            script = (
                f"const done = arguments[arguments.length - 1], end = Date.now() + {left}; (function poll() {{ let v = null;"
                + f" try {{ v = ({expression}) || null; }} catch (e) {{}}"
                + " if (v !== null || Date.now() > end) done(v); else setTimeout(poll, 10); })();"
            )
            try:
                value = self.call("POST", "/execute/async", {"script": script, "args": []})
            except RuntimeError:
                value = None  # the page went away under the script: a navigation, so ask the next one
            if value is not None:
                return value
            if time.monotonic() > deadline:
                raise TimeoutError(f"{self.name}: no {expression} within {seconds} s")

    def element(self, find: str) -> str:
        return next(iter(self.wait(find).values()))

    def go(self, url: str) -> None:
        self.call("POST", "/url", {"url": url})

    def click(self, find: str) -> None:
        """A real click, mouse down and up, on what the script expression `find` yields."""
        self.call("POST", f"/element/{self.element(find)}/click", {})

    def type(self, find: str, text: str) -> None:
        self.call("POST", f"/element/{self.element(find)}/value", {"text": text})

    def logged(self) -> int:
        return self.js("return (window.kdtLog ?? []).length")

    def quiet(self) -> None:
        self.wait(QUIET)


def mailu_sign_in(browser: Browser, user: str) -> None:
    browser.type(css("input[name=email]"), user)
    browser.type(css("input[name=pw]"), "fixture-password")
    browser.click(css("form [type=submit]"))


class Kubicle:
    name = "kubicle"
    ROWS = "[...document.querySelectorAll('.row')]"

    def __init__(self, browser: Browser) -> None:
        self.user = tier.user
        self.browser = browser
        self.marks_from = 0

    def sign_in(self) -> None:
        self.browser.go("http://localhost/login/")
        mailu_sign_in(self.browser, self.user)
        self.loaded()

    def loaded(self) -> None:
        self.browser.wait(
            "location.pathname === '/login/' && !!document.querySelector('.folder')"
            + f" && document.body.textContent.includes({json.dumps(self.user)})",
            60,
        )
        self.browser.quiet()

    def request(self, method: str, path: str, body: dict | None = None) -> list:
        """A request from the page with its CSRF token: the status and the answer."""
        send = "" if body is None else f", body: {json.dumps(json.dumps(body))}"
        script = f"""const done = arguments[arguments.length - 1];
            fetch('api/session').then(r => r.json()).then(s => fetch({json.dumps(path)}, {{method: '{method}',
                headers: {{'content-type': 'application/json', 'x-kubicle-csrf': s.csrf}}{send}}}))
            .then(async r => {{ const t = await r.text();
                try {{ done([r.status, JSON.parse(t)]); }} catch {{ done([r.status, t]); }} }})"""
        return self.browser.call("POST", "/execute/async", {"script": script, "args": []})

    def apply(self, combo: dict) -> None:
        status, before = self.request("GET", "api/settings")
        assert status == 200, (status, before)
        status, answer = self.request("POST", "api/settings", {**before, **kubicle_settings(combo, self.user)})
        assert status == 200, (status, answer)
        self.browser.go("http://localhost/login/")  # the page reads the settings as it starts
        self.loaded()

    def toast(self) -> int:
        return self.browser.js("return Number(document.querySelector('.toast')?.dataset.seq ?? 0)")

    def toasted(self, before: int, *texts: str) -> None:
        """Waits for a toast after the one numbered `before` that says one of `texts`: the server took the change.
        Then for what the change set off, such as the next message opening."""
        self.browser.wait(
            f"(t => !!t && Number(t.dataset.seq) > {before} && {json.dumps(texts)}.some(x => t.textContent.includes(x)))"
            + "(document.querySelector('.toast'))"
        )
        self.browser.quiet()

    def tool(self, label: str) -> None:
        self.browser.js(
            "[...document.querySelectorAll('.toolbar button.tool')]"
            + f".find(b => b.textContent.trim() === {json.dumps(label)}).click()"
        )

    def menu(self, label: str) -> None:
        item = f"document.querySelector('button.menu-item[name={json.dumps(label)}]')"
        self.browser.wait(f"!!{item}")
        self.browser.js(f"{item}.click()")

    def open_folder(self, label: str) -> None:
        folders = "[...document.querySelectorAll('.folder')]"
        folder = f"{folders}.find(f => f.querySelector('.folder-name').textContent === {json.dumps(label)})"
        self.browser.js(f"{folder}.click()")
        self.browser.wait("!!document.querySelector('.list')")

    def select(self, subject: str, delay: int | None) -> None:
        """Opens the message, and under "at once" waits until it was marked read."""
        row = f"{self.ROWS}.find(r => r.querySelector('.row-subject')?.textContent === {json.dumps(subject)})"
        self.browser.wait(f"!!{row}", 60)
        self.marks_from = self.browser.logged()
        self.browser.js(f"{row}.click()")
        self.browser.wait(
            f"document.querySelector('.message-subject')?.textContent === {json.dumps(subject)}"
            + " && !!document.querySelector('.mail-frame iframe')?.srcdoc"
        )
        if delay == 0:
            self.wait_marked(0)
        self.browser.quiet()

    def wait_marked(self, delay: int) -> bool:
        """Whether the page marked an opened message read within `delay` seconds and 3 more."""
        try:
            self.browser.wait(KUBICLE_MARKED.format(self.marks_from), delay + 3)
            return True
        except TimeoutError:
            return False

    def compose_ready(self, field: str) -> None:
        self.browser.wait(f"document.activeElement === {css(field)}")

    def send(self) -> None:
        """Sends, and waits until the server filed the copy and flagged the original, which it does after answering
        the send: the event stream then says a folder changed."""
        before, heard = self.toast(), self.browser.js("return (window.kdtEvents ?? []).length")
        self.browser.click(css(".compose .button.primary"))
        self.toasted(before, "Message sent successfully.")
        self.browser.wait(f"(window.kdtEvents ?? []).slice({heard}).some(e => e.includes('FolderChanged'))", 10)
        self.browser.quiet()

    def reply(self, button: str, text: str) -> None:
        self.tool(button)
        self.compose_ready("textarea.compose-body")
        self.browser.type(css("textarea.compose-body"), text)
        self.send()

    def forward(self, to: str, text: str) -> None:
        self.tool("Forward")
        self.compose_ready(".compose input[role=combobox]")
        self.browser.type(css(".compose input[role=combobox]"), to)
        self.browser.type(css("textarea.compose-body"), text)
        self.send()

    def compose(self, to: str, subject: str, text: str) -> None:
        self.browser.click(css(".compose-button"))
        self.compose_ready(".compose input[role=combobox]")
        self.browser.type(css(".compose input[role=combobox]"), to)
        self.browser.type(css(".compose input[aria-label=Subject]"), subject)
        self.browser.type(css("textarea.compose-body"), text)
        self.send()

    def mark(self, which: str) -> None:
        before = self.toast()
        self.tool("Mark")
        self.menu({"read": "As read", "unread": "As unread", "flagged": "As flagged"}[which])
        self.toasted(before, "Message(s) marked successfully.")

    def archive(self) -> None:
        before, self.marks_from = self.toast(), self.browser.logged()
        self.tool("Archive")
        self.toasted(before, "Successfully archived")

    def delete(self) -> None:
        before, self.marks_from = self.toast(), self.browser.logged()
        self.tool("Delete")
        self.toasted(before, "Message(s) moved to Trash successfully.", "Message(s) deleted successfully.")

    def move(self, folder: str) -> None:
        before, self.marks_from = self.toast(), self.browser.logged()
        self.tool("More")
        self.menu("Move to...")
        self.menu(folder)
        self.toasted(before, "Message(s) moved successfully.")

    def sign_out(self) -> None:
        self.browser.click(css(".tool.logout"))
        self.browser.wait("!!document.querySelector('input[name=pw]')", 60)


class Roundcube:
    name = "roundcube"
    # Nothing on its way, the preview of a selected message included: Roundcube's busy lock and jQuery's count of
    # open requests, which carries every request of Roundcube's own (`http_request`, `http_post`).
    IDLE = """window.rcmail && window.jQuery && jQuery.active === 0 && !rcmail.busy
        && (s => !s || (f => !!f && f.contentWindow.rcmail?.env.uid == s && f.contentDocument.readyState === 'complete'
            && f.contentWindow.jQuery?.active === 0)(document.getElementById('messagecontframe')))
            (rcmail.message_list?.get_single_selection())"""
    LISTED = "rcmail.env.task === 'mail' && rcmail.env.action === '' && !!rcmail.message_list && rcmail.message_list.rowcount > 0"
    COMPOSING = "rcmail.env.action === 'compose' && !!rcmail.env.compose_id && document.readyState === 'complete'"

    def __init__(self, browser: Browser) -> None:
        self.user = "roundcube@example.com"
        self.browser = browser
        self.marks_from = 0

    def press(self, command: str) -> None:
        """A click on the first button Roundcube registered for `command`: Elastic moves its toolbars around."""
        self.browser.click(f"document.getElementById(rcmail.buttons[{json.dumps(command)}][0].id)")

    def idle(self, also: str = "true", seconds: float = 30) -> None:
        self.browser.wait(f"({self.IDLE}) && ({also})", seconds)
        self.browser.quiet()

    def sign_in(self) -> None:
        self.browser.go(ROUNDCUBE)
        mailu_sign_in(self.browser, self.user)
        self.idle(self.LISTED, 60)

    def store(self, combo: dict) -> None:
        """Writes the preferences, through the driver's shell: call it from the main thread."""
        php = "docker exec -u mailu mailu-roundcube php83 /kdt/roundcube-prefs.php"
        print(machine.succeed(f"{php} {self.user} {shlex.quote(json.dumps(roundcube_input(combo)))}"), flush=True)

    def apply(self, combo: dict) -> None:
        self.open_folder("INBOX")  # a request reads the preferences store() wrote, and the list the new mail

    def open_folder(self, mailbox: str) -> None:
        self.browser.go(f"{ROUNDCUBE}?_task=mail&_mbox={urllib.parse.quote(mailbox)}")  # type: ignore[attr-defined]
        self.idle(f"{self.LISTED} && rcmail.env.mailbox === {json.dumps(mailbox)}")

    def select(self, subject: str, delay: int | None) -> None:
        """Opens the message. Under "at once" the preview's own request marks it read."""
        rows = "[...document.querySelectorAll('#messagelist tbody tr')]"
        row = f"{rows}.find(tr => tr.querySelector('span.subject a')?.textContent.trim() === {json.dumps(subject)})"
        self.marks_from = self.browser.logged()
        self.browser.click(row)
        self.idle(f"rcmail.message_list.get_single_selection() == ({row}).uid")

    def wait_marked(self, delay: int) -> bool:
        if delay == 0:
            return True  # the preview marked it, and idle() waited for the preview
        try:
            self.browser.wait(ROUNDCUBE_MARKED.format(self.marks_from), delay + 3)
            return True
        except TimeoutError:
            return False

    def send(self) -> None:
        self.press("send")
        self.idle(self.LISTED, 60)  # Roundcube lists the folder again once it sent

    def reply(self, button: str, text: str) -> None:
        self.press({"Reply": "reply", "Reply all": "reply-all"}[button])
        self.browser.wait(f"{self.COMPOSING} && document.activeElement?.id === 'composebody'")
        self.browser.type(css("#composebody"), text)
        self.send()

    def forward(self, to: str, text: str) -> None:
        self.press("forward")
        self.browser.wait(f"{self.COMPOSING} && document.activeElement?.matches('#compose_to input')")
        self.browser.type(css("#compose_to ul.recipient-input input"), to + ",")
        self.browser.type(css("#composebody"), text)
        self.send()

    def compose(self, to: str, subject: str, text: str) -> None:
        self.press("compose")
        self.browser.wait(f"{self.COMPOSING} && document.activeElement?.matches('#compose_to input')")
        self.browser.type(css("#compose_to ul.recipient-input input"), to + ",")
        self.browser.type(css("#compose-subject"), subject)
        self.browser.type(css("#composebody"), text)
        self.send()

    def mark(self, which: str) -> None:
        item = {"read": "a.read", "unread": "a.unread", "flagged": "a.flag"}[which]
        self.browser.click(css("#markmessagemenulink"))
        self.browser.click(visible(f"#markmessage-menu {item}"))
        self.idle()

    def archive(self) -> None:
        self.marks_from = self.browser.logged()
        self.press("plugin.archive")
        self.idle()

    def delete(self) -> None:
        self.marks_from = self.browser.logged()
        self.press("delete")
        self.idle()

    def move(self, folder: str) -> None:
        self.marks_from = self.browser.logged()
        self.browser.click(css("#messagemenulink"))
        self.browser.click(visible("#message-menu a.move"))
        folders = "[...document.querySelectorAll('#folder-selector a')]"
        self.browser.click(f"{folders}.find(a => a.textContent.trim() === {json.dumps(folder)})")
        self.idle()

    def sign_out(self) -> None:
        self.press("logout")
        self.browser.wait("!!document.querySelector('input[name=pw]')", 60)


def mail_text(tag: str, me: str, name: str, subject: str, sender: str, to: str, extra: str = "", date: str = "") -> str:
    date = date or email.utils.formatdate()
    return (
        f"From: {sender}\nTo: {to.replace('ME', me)}\nSubject: {tag} {subject}\nDate: {date}\n"
        + f"Message-ID: <{tag}-{name}@kdt.test>\n{extra}Content-Type: text/plain; charset=utf-8\n\n"
        + f"Hello,\nthis is {subject}.\n\nA second paragraph, so a reply has more than one line to quote.\n"
    )


def mail(tag: str, me: str) -> list[tuple[str, str]]:
    """The combination's mail as (folder, message), oldest first: in INBOX each message an action takes has an unread
    one below it, which Roundcube's display_next opens next."""
    alice, carl = "Alice <alice@example.net>", "Carl <carl@example.net>"
    return [
        ("INBOX", mail_text(tag, me, "p0", "pad 0", carl, "ME")),
        ("INBOX", mail_text(tag, me, "g", "move me", carl, "ME")),
        ("INBOX", mail_text(tag, me, "p1", "pad 1", carl, "ME")),
        ("INBOX", mail_text(tag, me, "e", "delete me", carl, "ME")),
        ("INBOX", mail_text(tag, me, "p2", "pad 2", carl, "ME")),
        # A year's last hour in UTC, the next year's first in Kyiv: "Divide archive by" files it by the user's zone.
        ("INBOX", mail_text(tag, me, "d", "archive me", "Dora <dora@example.org>", "ME", date="Thu, 31 Dec 2020 22:30:00 +0000")),
        ("INBOX", mail_text(tag, me, "p3", "pad 3", carl, "ME")),
        ("INBOX", mail_text(tag, me, "h", "mark me", carl, "ME")),
        ("INBOX", mail_text(tag, me, "c", "forward me", carl, "ME")),
        (
            "INBOX",
            mail_text(
                tag,
                me,
                "b",
                "list mail",
                "Dave <dave@example.org>",
                "list@lists.example.net",
                "Cc: Erin <erin@example.net>\nList-Post: <mailto:list@lists.example.net>\n",
            ),
        ),
        ("INBOX", mail_text(tag, me, "a", "open me", alice, "ME", "Cc: Bob <bob@example.net>\nX-Priority: 2 (High)\n")),
        ("Junk", mail_text(tag, me, "jp", "junk pad", "Spam <spam@example.biz>", "ME")),
        ("Junk", mail_text(tag, me, "f", "junk me", "Spam <spam@example.biz>", "ME")),
    ]


def deliver(tag: str, users: list[str]) -> None:
    """The combination's mail into every account in the same order, through `doveadm save`, so arrival times and
    the list's order match. The files go where the IMAP container sees them, under its /mail; one shell command."""
    script = [f"rm -rf /var/lib/mailu/mail/.kdt && mkdir -p /var/lib/mailu/mail/.kdt/{tag}"]
    saves = []
    for user in users:
        for n, (folder, text) in enumerate(mail(tag, user)):
            path = f".kdt/{tag}/{user}-{n:02d}.eml"
            script.append(f"cat > /var/lib/mailu/mail/{path} <<'EOF'\n{text}EOF")
            saves.append(f"doveadm save -u {user} -m {folder} < /mail/{path}")
    script.append(f"docker exec mailu-imap sh -c {shlex.quote(' && '.join(saves))}")
    machine.succeed("\n".join(script))


class Comparison:
    """The two accounts' state after each step, and every difference found, each reported once."""

    def __init__(self, port: int, kubicle: Kubicle, roundcube: Roundcube) -> None:
        self.url = f"http://127.0.0.1:{port}/?tag="
        self.users = (kubicle.user, roundcube.user)
        self.reported: set[str] = set()
        self.report = pathlib.Path(os.environ["out"]) / "differences.txt"
        self.title = ""
        self.tag = ""
        self.snapshots: list[float] = []

    def snapshot(self) -> tuple[dict, dict]:
        began = time.monotonic()
        with urllib.request.urlopen(self.url + self.tag, timeout=60) as answer:
            states = json.load(answer)
        self.snapshots.append(time.monotonic() - began)
        return states[self.users[0]], states[self.users[1]]

    @staticmethod
    def differences(k: dict, r: dict) -> set[str]:
        found = set()
        for part in ("folders", "subscribed"):
            for folder in sorted(set(k[part]) ^ set(r[part])):
                found.add(f"{part}: {folder} only in {'kubicle' if folder in k[part] else 'roundcube'}")
        by_key: dict[str, tuple[list, list]] = collections.defaultdict(lambda: ([], []))
        for side, state in enumerate((k, r)):
            for mailbox, key, flags in state["messages"]:
                by_key[key][side].append(f"{mailbox} [{flags}]")
        for key, (ks, rs) in by_key.items():
            if sorted(ks) != sorted(rs):
                found.add(f"{key}: kubicle {sorted(ks)} | roundcube {sorted(rs)}")
        details: dict[str, tuple[list, list]] = collections.defaultdict(lambda: ([], []))
        for side, state in enumerate((k, r)):
            for d in state["details"]:
                details[d["key"]][side].append(d)
        for key, (kd, rd) in details.items():
            for field in sorted({f for d in kd + rd for f in d} - {"key", "mailbox", "flags"}):
                kv = sorted(json.dumps(d.get(field)) for d in kd)
                rv = sorted(json.dumps(d.get(field)) for d in rd)
                if kv != rv:
                    found.add(f"{key} {field}: kubicle {', '.join(kv)} | roundcube {', '.join(rv)}")
        return found

    def check(self, step: str, settle: float) -> None:
        """Compares the two accounts, once more every 100 ms while they differ, for at most `settle` seconds. Each
        action waits for its page to finish, so only what lands later needs it: a send's copy and flags, which kubicle
        files after its answer, and the delivery report Postfix sends."""
        deadline = time.monotonic() + settle
        while True:
            new = self.differences(*self.snapshot()) - self.reported
            if not new or time.monotonic() > deadline:
                break
            time.sleep(0.1)
        self.reported |= new
        print(f"compared after {step}: {len(new)} new differences", flush=True)  # the driver's stdout is a pipe
        if new:
            with self.report.open("a") as out:
                out.write(f"\n{self.title}\n  after {step}:\n" + "".join(f"    {line}\n" for line in sorted(new)))
            for line in sorted(new):
                fail(f"{self.title.split(':')[0]}, after {step}: {line}")


def run_combination(combo: dict, tag: str, k: Kubicle, r: Roundcube, compare: Comparison, pool, timing: dict) -> None:
    delay = combo["mark_read_after"]

    def together(action, name: str = "") -> None:
        """`action` in both webmails at once; a step costs the slower one. Each one's time goes in `timing`."""

        def timed(client) -> None:
            began = time.monotonic()
            action(client)
            if name:
                timing[f"{name}, {client.name} alone"].append(time.monotonic() - began)

        for done in [pool.submit(timed, client) for client in (k, r)]:
            done.result()

    def step(name: str, action, settle: float = 0.5) -> None:
        began = time.monotonic()
        together(action, name)
        compare.check(name, settle)
        timing[name].append(time.monotonic() - began)

    began = time.monotonic()
    signing_in = [pool.submit(client.sign_in) for client in (k, r)]
    deliver(tag, [k.user, r.user])  # meanwhile, on the driver's shell, which the threads do not use
    for done in signing_in:
        done.result()
    r.store(combo)
    together(lambda c: c.apply(combo))
    compare.check("delivery", 0.5)
    timing["sign in, settings, delivery"].append(time.monotonic() - began)

    # Read at once, never, or 5 s later. The first check comes before those 5 s for both, so it may not wait long.
    step("opening a message", lambda c: c.select(f"{tag} open me", delay))
    if delay == 5:
        step("5 s with it open", lambda c: c.wait_marked(5))

    step("reply", lambda c: c.reply("Reply", f"{KDT} reply"), settle=1)
    step("reply all", lambda c: (c.select(f"{tag} list mail", delay), c.reply("Reply all", f"{KDT} reply all")), settle=1)
    step("forward", lambda c: (c.select(f"{tag} forward me", delay), c.forward("dave@example.net", f"{KDT} forward")), 1)
    # Sent to oneself: under "Always request a delivery status notification" Postfix's report follows on its own time.
    step("a new message to oneself", lambda c: c.compose(c.user, f"{tag} new", f"{KDT} new"), 4 if combo["dsn_default"] else 1)

    step("selecting a message", lambda c: (c.select(f"{tag} mark me", delay), delay == 5 and c.wait_marked(5)))
    step("mark as flagged", lambda c: c.mark("flagged"))
    states = dict(zip((k.name, r.name), compare.snapshot()))
    seen = {
        name: any(key == f"<{tag}-h@kdt.test>" and "\\Seen" in flags for _, key, flags in state["messages"])
        for name, state in states.items()
    }
    step("mark as read or unread", lambda c: c.mark("unread" if seen[c.name] else "read"))

    def then_next(action):
        """`action`, then under display_next the message below opens: wait for it to count as read."""

        def run(c) -> None:
            action(c)
            if combo["display_next"] and delay is not None:
                c.wait_marked(delay)

        return run

    step("archive", then_next(lambda c: (c.select(f"{tag} archive me", delay), c.archive())))
    step("delete in INBOX", then_next(lambda c: (c.select(f"{tag} delete me", delay), c.delete())))
    step("move to finance", then_next(lambda c: (c.select(f"{tag} move me", delay), c.move("finance"))))
    step("delete in Junk", then_next(lambda c: (c.open_folder("Junk"), c.select(f"{tag} junk me", delay), c.delete())))
    step("sign out", lambda c: c.sign_out())


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def forward(guest: int) -> int:
    """A port on the host's loopback that reaches `guest` on the VM, as the driver's forward_port does on every
    address."""
    host = free_port()
    machine.send_monitor_command(f"hostfwd_add tcp:127.0.0.1:{host}-:{guest}")
    return host


def summary(name: str, values: list[float]) -> str:
    return f"{name}: min {min(values):.2f} s, median {statistics.median(values):.2f} s, max {max(values):.2f} s ({len(values)})"


def run_differential() -> None:
    guarded("setup", tier_setup)
    if not VERDICT:
        guarded("setup", differential_setup)
    if not VERDICT:
        driver = forward(9516)  # settings.nix relays it to chromedriver
        k = Kubicle(Browser("kubicle", driver, machine.succeed("cat /tmp/wd-session").strip()))
        r = Roundcube(Browser("roundcube", driver, machine.succeed("cat /tmp/wd-rc").strip()))
        compare = Comparison(forward(8790), k, r)
        combos = combinations()
        only = os.environ.get("KUBICLE_SETTINGS_ONLY")
        chosen = [int(n) for n in only.split(",")] if only else range(len(combos))
        print(f"{len(combos)} combinations, running {len(chosen)}", flush=True)
        timing: dict[str, list[float]] = collections.defaultdict(list)
        out = pathlib.Path(os.environ["out"])
        with ThreadPoolExecutor(2) as pool:
            for n in chosen:
                compare.tag = f"kdt{n:04d}"
                compare.title = f"combination {n}: " + ", ".join(f"{name}={value}" for name, value in combos[n].items())
                print(compare.title, flush=True)
                began = time.monotonic()
                try:
                    with subtest(compare.title):
                        run_combination(combos[n], compare.tag, k, r, compare, pool, timing)
                    timing["combination"].append(time.monotonic() - began)
                except Exception:
                    fail(f"combination {n} stopped: {traceback.format_exc(limit=3).strip()}")
                    for client in (k, r):
                        (out / f"failed-{n}-{client.name}.png").write_bytes(
                            base64.b64decode(client.browser.call("GET", "/screenshot"))
                        )
                        (out / f"failed-{n}-{client.name}.html").write_text(
                            client.browser.js("return document.documentElement.outerHTML")
                        )
                        client.browser.go("http://localhost/sso/logout")  # signed out, for the next combination
                print(f"combination {n} took {time.monotonic() - began:.1f} s", flush=True)
        lines = [summary(name, values) for name, values in timing.items()] + [summary("one snapshot", compare.snapshots)]
        (out / "timing.txt").write_text("".join(f"{line}\n" for line in lines))
        print("\n".join(lines), flush=True)
    collect(machine, "/tmp/netlog.json", "/tmp/wd-rc.netlog.json")
    record()


def differential_setup() -> None:
    machine.wait_for_unit("docker-mailu-roundcube.service")
    machine.wait_until_succeeds("docker exec mailu-front curl -s -o /dev/null http://roundcube/", timeout=120)
    machine.succeed("WD_SESSION=/tmp/wd-rc wd start")
    machine.succeed("systemctl start mail-state")
    machine.wait_for_open_port(8790)
    machine.wait_for_open_port(9516)
