# Hostile mail's cost: our own cases, each a mail built to crash kubicle or make it spend time or memory out of
# proportion to its size. A case lands in the Cost folder byte for byte (doveadm save: no Postfix limit in
# between, as an import or another client could store it), then goes through the routes it attacks. It fails when
# kubicle's process changed (it died), a route answered outside its statuses or took over `seconds`, or kubicle's
# peak RSS during the case rose more than `mb` over its RSS before. Appended to scenarios.py by full.nix.
# mypy: disable-error-code="name-defined"
# `machine`, `tier`, `request`, `search`, `reload`, `kubicle_kb`, `tempfile` and `time` come from scenarios.py and
# the driver.
import binascii
from collections.abc import Callable
from typing import NamedTuple

COST_FOLDER = "Cost"


def list_status(path: str) -> int:
    """The status of a GET from the signed-in page, its body not transited: the Cost folder's list page grows as
    cases accumulate mail, and a large body through the driver truncates and fails to parse (`measured`)."""
    return int(json.loads(wd("async", f"fetch({json.dumps(path)}).then(r => done(r.status)).catch(() => done(0))")))


def list_has_uid(path: str, uid: int) -> bool:
    """Whether the list at `path` holds `uid`, decided in the page so its (large) body is not transited whole."""
    script = (
        f"fetch({json.dumps(path)}).then(r => r.json())"
        f".then(d => done(d.rows.some(h => h.row.uid === {uid}))).catch(() => done(false))"
    )
    return bool(json.loads(wd("async", script)))


def cost_ready() -> None:
    """kubicle up with this tab signed in, even after an earlier case killed it, and the Cost folder listed."""
    machine.wait_until_succeeds("docker exec mailu-front curl -sf http://webmail/healthz", timeout=60)
    if request("GET", "api/session")[0] != 200:
        reload()
    machine.execute(f"{tier.doveadm} mailbox create -u {tier.user} {COST_FOLDER}")
    deadline = time.monotonic() + 90
    while list_status(f"api/list?folder={COST_FOLDER}&page=0&size=50&sort=Arrival&order=Descending&threads=false") != 200:
        assert time.monotonic() < deadline, "kubicle never listed the Cost folder"
        time.sleep(2)


def cost_save(name: str, raws: bytes | list[bytes]) -> int:
    """Stores each mail in the Cost folder as it is, under an X-Cost-Case header naming the case, and answers the
    last one's UID."""
    for raw in [raws] if isinstance(raws, bytes) else raws:
        with tempfile.NamedTemporaryFile(suffix=".eml", delete=False) as f:
            f.write(f"X-Cost-Case: {name}\r\n".encode() + raw)
        machine.copy_from_host(f.name, "/tmp/cost.eml")
        machine.succeed(f"docker exec -i mailu-imap doveadm save -u {tier.user} -m {COST_FOLDER} </tmp/cost.eml")
    uid = search(COST_FOLDER, f"header X-Cost-Case {name}", 30)[-1]
    # A user opens only mail the list showed: wait until kubicle knows of it, as its folder round tells it.
    deadline, page = (
        time.monotonic() + 60,
        f"api/list?folder={COST_FOLDER}&page=0&size=200&sort=Arrival&order=Descending&threads=false",
    )
    while not list_has_uid(page, uid):
        assert time.monotonic() < deadline, f"kubicle never listed {name}'s UID {uid}"
        time.sleep(1)
    return uid


def kubicle_pid() -> str:
    return machine.succeed("docker inspect -f '{{.State.Pid}}' kubicle").strip()


def compose(mode: str):
    return lambda uid: request("POST", "api/compose", {"mode": mode, "messages": [[COST_FOLDER, {"Uids": [uid]}]], "zone": ""})


class Measured(NamedTuple):
    size: int
    head: str


def measured(path: str, query: str = ""):
    """A GET whose answer the browser hands back only the size and first 300 characters of: a 2.4 MB body through
    the driver once came back cut short, and `request`'s full-body transit truncates a large answer (a JSON parse
    then failing mid-string), so every route that may answer large is measured this way, not read whole."""
    script = "fetch(URL).then(async r => { const t = await r.text(); done([r.status, t.length, t.slice(0, 300)]); })"

    def route(uid: int) -> tuple[int, Measured]:
        url = json.dumps(f"{path}?folder={COST_FOLDER}&uid={uid}{query}")
        status, size, head = json.loads(wd("async", script.replace("URL", url)))
        return status, Measured(size, head)

    return route


# A 1x1 PNG, for cases with many images.
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")


def inline_images(uid: int) -> tuple[int, object]:
    """The message read, then every inline image it shows fetched at once, as the frame loads them: 200 when each
    is the PNG, the sizes otherwise."""
    status, m = request("GET", f"api/message?folder={COST_FOLDER}&uid={uid}")
    if status != 200:
        return status, m
    urls = [u.replace("&amp;", "&") for u in re.findall(r'src="([^"]*/inline\?[^"]*)"', m["body"]["document"])]
    fetch_all = (
        f"Promise.all({json.dumps(urls)}.map(u => fetch(u).then(r => r.arrayBuffer()))).then(b => done(b.map(x => x.byteLength)))"
    )
    sizes = json.loads(wd("async", fetch_all, "120"))
    return (200 if sizes and set(sizes) == {len(PNG)} else 500), sizes


def post_built(path: str, build: str) -> tuple[int, object]:
    """POSTs to `path` the JSON that the script expression `build` makes in the page: a body too large for the
    driver's command line, which `request` passes it on."""
    script = f"""fetch('api/session').then(r => r.json()).then(s => fetch({json.dumps(path)}, {{method: 'POST',
        headers: {{'content-type': 'application/json', 'x-kubicle-csrf': s.csrf}}, body: JSON.stringify({build})}}))
        .then(async r => {{ const t = await r.text();
          try {{ done([r.status, JSON.parse(t)]); }} catch {{ done([r.status, t]); }} }})"""
    return json.loads(wd("async", script))


def draft_repeats() -> tuple[int, object]:
    """A draft that lists one 1 MiB upload 1,000 times, which a build read once per listing."""
    status, uploaded = upload("new Blob([new Uint8Array(1 << 20)])", "repeated.bin")
    if status != 200:
        return status, uploaded
    attachment = json.loads(uploaded)
    draft = request("POST", "api/compose", {"mode": "New", "messages": [], "zone": ""})[1]["draft"]
    answer = post_built(
        "api/draft", f"Object.assign({json.dumps(draft)}, {{attachments: Array(1000).fill({json.dumps(attachment)})}})"
    )
    request("POST", "api/upload/remove", [attachment["id"]])
    return answer


def settings_over_cap() -> tuple[int, object]:
    """24 themes of 60 KiB each, every one within its own limit, which every settings read then parsed."""
    settings = request("GET", "api/settings")[1]
    css = json.dumps('/* kubicle-theme name="Big" dark=false */\n:root{--x:')
    themes = f"Object.fromEntries([...Array(24).keys()].map(n => ['big-' + n, {css} + 'a'.repeat(60000) + '}}']))"
    return post_built("api/settings", f"Object.assign({json.dumps(settings)}, {{themes: {themes}}})")


def zip_has(subject: str) -> tuple[int, object]:
    """Select all of the Cost folder as one mbox, which is stored uncompressed: 200 when it holds `subject`."""
    url = "api/zip?mode=Mbox&sets=" + urllib.parse.quote(json.dumps([[COST_FOLDER, "*"]]))
    script = "fetch(URL).then(async r => done([r.status, (await r.text()).includes(SUBJECT)]))"
    status, found = json.loads(wd("async", script.replace("URL", json.dumps(url)).replace("SUBJECT", json.dumps(subject))))
    return (status if found else 500), found


def replies_beside_healthz(uid: int) -> tuple[int, object]:
    """One Reply more than kubicle has CPUs at once, and /healthz meanwhile: quoted on the async workers, the
    replies held every one of them, and with them every other user's requests."""
    replies = int(machine.succeed("nproc")) + 1
    body = json.dumps(json.dumps({"mode": "Reply", "messages": [[COST_FOLDER, {"Uids": [uid]}]], "zone": ""}))
    js(
        "window.costReplies = fetch('api/session').then(r => r.json()).then(s => Promise.all(Array.from({length: N}, () =>"
        " fetch('api/compose', {method: 'POST', headers: {'content-type': 'application/json', 'x-kubicle-csrf': s.csrf},"
        " body: BODY}).then(r => r.status))))".replace("N", str(replies)).replace("BODY", body)
    )
    time.sleep(0.5)
    took = float(machine.succeed("docker exec mailu-front curl -s -o /dev/null -w '%{time_total}' http://webmail/healthz"))
    statuses = json.loads(wd("async", "window.costReplies.then(done)", "120"))
    return (200 if took < 0.5 and set(statuses) == {200} else 500), {"healthz s": took, "replies": statuses}


def whole_body(uid: int) -> tuple[int, object]:
    """Edit as new, which reads the body as a reopened draft does: 200 when all of it came back. A quote's size cut
    once emptied a large draft, and its next save replaced the copy that still held the text. The browser hands back
    the length alone, as `measured` does."""
    body = json.dumps(json.dumps({"mode": "EditAsNew", "messages": [[COST_FOLDER, {"Uids": [uid]}]], "zone": ""}))
    script = (
        "fetch('api/session').then(r => r.json()).then(s => fetch('api/compose', {method: 'POST', headers:"
        " {'content-type': 'application/json', 'x-kubicle-csrf': s.csrf}, body: BODY}))"
        ".then(async r => done([r.status, r.ok ? (await r.json()).draft.body.length : 0]))"
    ).replace("BODY", body)
    status, size = json.loads(wd("async", script))
    return (200 if status == 200 and size >= 2_500_000 else 500), {"status": status, "body": size}


def lists_complete() -> tuple[int, object]:
    """The Cost folder flat and threaded: 200 when both show every message the folder counts. Threaded, `total` counts
    threads, so the rows are what must add up."""
    page = f"api/list?folder={COST_FOLDER}&page=0&size=200&sort=Arrival&order=Descending&threads="
    (s1, flat), (s2, threaded) = request("GET", page + "false"), request("GET", page + "true")
    if 200 not in (s1, s2) or s1 != s2:
        return max(s1, s2), (flat, threaded)
    same = flat["total"] == len(flat["rows"]) == len(threaded["rows"])
    return (200 if same else 500), {"total": flat["total"], "flat rows": len(flat["rows"]), "threaded rows": len(threaded["rows"])}


def list_row_recipients(uid: int) -> tuple[int, int]:
    """How many recipients kubicle puts in a list row for `uid`, read in the page. The list shows three and a
    "..." (web `people`), so a row carries at most four (sync.rs `ROW_RECIPIENTS`); without the cap a mail to
    tens of thousands makes one list page megabytes, which wedged the browser here. 200 when at most four."""
    page = f"api/list?folder={COST_FOLDER}&page=0&size=200&sort=Arrival&order=Descending&threads=false"
    script = (
        f"fetch({json.dumps(page)}).then(r => r.json())"
        f".then(d => {{ const h = d.rows.find(h => h.row.uid === {uid}); done(h ? h.row.to.length : -1); }})"
        ".catch(() => done(-1))"
    )
    n = int(json.loads(wd("async", script)))
    return (200 if 0 <= n <= 4 else 500), n


# The routes a case can attack, one request each; a case names the statuses it accepts from each.
ROUTES = {
    "read": measured("api/message"),
    "read as text": measured("api/message", "&text=true"),
    "print": measured("api/print"),
    "download": measured("api/download"),
    "source": measured("api/source"),
    "reply": compose("Reply"),
    "reply all": compose("ReplyAll"),
    "forward": compose("Forward"),
    "forward as attachment": compose("ForwardAttachment"),
    "edit as new": compose("EditAsNew"),
    "inline images": inline_images,
    "zip of repeated sets": lambda uid: request(
        "GET", "api/zip?mode=Maildir&sets=" + urllib.parse.quote(json.dumps([[COST_FOLDER, "1:99999"]] * 60))
    ),
    "lists complete": lambda uid: lists_complete(),
    "replies beside healthz": replies_beside_healthz,
    "edit as new keeps the body": whole_body,
    "zip of Select all": lambda uid: zip_has("cost deep-structure"),
    "draft listing one upload 1,000 times": lambda uid: draft_repeats(),
    "settings over the cap": lambda uid: settings_over_cap(),
    "forward Select all 400 times": lambda uid: request(
        "POST", "api/compose", {"mode": "ForwardAttachment", "messages": [[COST_FOLDER, "All"]] * 400, "zone": ""}
    ),
    # Past uploads' 1,000 files: the forward is refused part way, and what it stored must go with it.
    "forward 1,100 of INBOX": lambda uid: request(
        "POST", "api/compose", {"mode": "ForwardAttachment", "messages": [["INBOX", {"Uids": list(range(1, 1101))}]], "zone": ""}
    ),
    # Past the last page of a sorted search over every folder, which once fetched every hit of every folder.
    "deep sorted search": lambda uid: request(
        "GET",
        "api/list?"
        + urllib.parse.urlencode(
            {
                "folder": "INBOX",
                "page": 999,
                "size": 200,
                "sort": "Date",
                "order": "Descending",
                "threads": "false",
                "search": json.dumps(
                    {"text": "", "fields": {}, "filter": "Unanswered", "interval": "Any", "scope": "All", "full": True}
                ),
            }
        ),
    ),
    "upload": lambda uid: upload("new Blob(['x'])", "x.txt"),
    "threads": lambda uid: request(
        "GET", f"api/list?folder={COST_FOLDER}&page=0&size=50&sort=Arrival&order=Descending&threads=true"
    ),
    "list row bounded": lambda uid: list_row_recipients(uid),
}


class Case(NamedTuple):
    name: str
    mail: Callable[[], bytes | list[bytes]]
    routes: dict[str, set[int]]  # each route it goes through, with the statuses that route may answer
    mb: int  # how far kubicle's RSS may peak over its RSS before the case
    seconds: float = 5  # for each route
    kb: int = 0  # the largest answer a route may give, when not 0


def cost_case(case: Case) -> None:
    cost_ready()
    uid = cost_save(case.name, case.mail())
    pid = kubicle_pid()
    for route, statuses in case.routes.items():
        # Each route's own peak: glibc keeps what a route freed for the next, so one reset before all three of
        # address-lists' routes read 100 MB where each peaked at 45-55 on its own.
        machine.succeed(f"echo 5 > /proc/{pid}/clear_refs")  # resets VmHWM to the RSS now
        before = kubicle_kb("VmRSS")
        started = time.monotonic()
        status, answer = ROUTES[route](uid)
        took = time.monotonic() - started
        assert kubicle_pid() == pid, f"kubicle died on {route}"
        assert status in statuses, f"{route} answered {status}, not one of {sorted(statuses)}: {str(answer)[:300]}"
        assert took < case.seconds, f"{route} took {took:.1f} s"
        size = (
            answer.size if isinstance(answer, Measured) else len(answer if isinstance(answer, str) else json.dumps(answer))
        ) // 1024
        assert not case.kb or size <= case.kb, f"{route} answered {size} KB"
        grew = (kubicle_kb("VmHWM") - before) // 1024
        assert grew <= case.mb, f"on {route}, kubicle's RSS peaked {grew} MB over its {before // 1024} MB before"


def nested_rfc822() -> bytes:
    """mail-parser built a chain of unencoded nested messages to any depth, 4 KB a level, and dropping it took a stack
    frame per level: 90,000 levels (3 MB) aborted the process. Past 1,000 it is not parsed, and downloads whole."""
    return (
        b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost nested-rfc822\r\nMIME-Version: 1.0\r\n"
        + b"Content-Type: message/rfc822\r\n\r\n" * 100_000
        + b"Content-Type: text/plain\r\n\r\ndeep\r\n"
    )


def wide_table() -> bytes:
    """HTML as text laid tables out in columns, padding every row to the widest cell: these 68 KB made 20 MB of text
    and a 171 MB peak, for the reader's Plain text and for every reply's quote."""
    rows = "<tr><td>a</td><td>b</td></tr>" * 2000
    return (
        b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost wide-table\r\nMIME-Version: 1.0\r\n"
        b"Content-Type: text/html; charset=utf-8\r\n\r\n"
        + f"<table><tr><td>{'x' * 10_000}</td><td>b</td></tr>{rows}</table>".encode()
    )


def wide_table_max() -> bytes:
    """The same wide table at the 2 MB input ceiling: one 10,000-char cell over ~65,000 rows made html2text build
    290 MB in the shared process. The admission gate refuses it (cells times the widest cell) and shows a notice."""
    row = "<tr><td>a</td><td>b</td></tr>"
    rows = row * ((1_900_000 - 10_000) // len(row))
    return (
        b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost wide-table-max\r\nMIME-Version: 1.0\r\n"
        b"Content-Type: text/html; charset=utf-8\r\n\r\n"
        + f"<table><tr><td>{'x' * 10_000}</td><td>b</td></tr>{rows}</table>".encode()
    )


def html_mail(subject: str, html: str, headers: str = "") -> bytes:
    return (
        f"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost {subject}\r\nMIME-Version: 1.0\r\n{headers}"
        f"Content-Type: text/html; charset=utf-8\r\n\r\n{html}"
    ).encode()


def image_references() -> bytes:
    """Every image source turned into a signed URL or the blocked SVG, 200 bytes or so for each 18-byte reference: 2 MB
    of them made an 18 MB answer and a 109 MB peak. Past 1,000 in a part they are dropped."""
    return html_mail("image-references", "<img src=http://a>" * 30_000)


def huge_html() -> bytes:
    """An HTML part costs the sanitizer about 35 times its size; past 2 MB the reader shows a notice and a download."""
    return html_mail("huge-html", "<p>x</p>" * 400_000)


def enriched_excerpts() -> bytes:
    """Each text/enriched excerpt rebuilt the whole body: 1,800 nested in 43 KB took 12 s and 149 MB. Past 16 they stay
    as tags."""
    body = "<excerpt>" * 1800 + "x\r\n\r\n" * 1800 + "</excerpt>" * 1800
    return (
        "From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost enriched-excerpts\r\nMIME-Version: 1.0\r\n"
        f"Content-Type: text/enriched; charset=utf-8\r\n\r\n{body}"
    ).encode()


def many_images() -> bytes:
    """Each inline image fetched and parsed the whole message, here with a 4 MB attachment: 200 images moved 800 MB
    through the user's one IMAP connection. Now each reads its own part."""
    images = "".join(
        f"--r\r\nContent-Type: image/png\r\nContent-ID: <i{n}@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        f"{base64.b64encode(PNG).decode()}\r\n"
        for n in range(200)
    )
    html = "".join(f'<img src="cid:i{n}@x">' for n in range(200))
    pdf = base64.encodebytes(b"%PDF-1.4 " + b"0" * 4_000_000).decode().replace("\n", "\r\n")
    return (
        "From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost many-images\r\nMIME-Version: 1.0\r\n"
        "Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: multipart/related; boundary=r\r\n\r\n"
        f"--r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n{images}--r--\r\n"
        "--b\r\nContent-Type: application/pdf; name=big.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        f"{pdf}--b--\r\n"
    ).encode()


def small() -> bytes:
    """A plain mail, for cases whose cost is in the request, not the mail."""
    return b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost small\r\n\r\nhello\r\n"


def deep_thread() -> list[bytes]:
    """imap-codec decodes a THREAD answer only 8 lists deep, and the dropped answer emptied the threaded folder: 17
    replies branching at each of 9 levels. kubicle reads THREAD itself, and threads flat."""
    mails: list[bytes] = []
    chain: list[str] = []
    for n in range(17):
        mid = f"<cost-thread-{n}@example.org>"
        parent = chain[-1] if chain else None
        refs = f"In-Reply-To: {parent}\r\nReferences: {' '.join(chain)}\r\n" if parent else ""
        mails.append(
            f"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost deep-thread {n}\r\nMessage-ID: {mid}\r\n"
            f"{refs}\r\nreply {n}\r\n".encode()
        )
        if n % 2 == 0:  # every other one is a leaf beside the chain, so each level branches into a list of its own
            chain.append(mid)
    return mails


def deep_structure() -> bytes:
    """Nested 9 levels, past the 8 at which imap-codec decodes a BODYSTRUCTURE: a list FETCH that asked for one would
    drop this row from every list while the folder still counted it. The list asks for none (docs/design.md, IMAP)."""
    head, tail = "", ""
    for n in range(9):
        head += f"Content-Type: multipart/mixed; boundary=b{n}\r\n\r\n--b{n}\r\n"
        head += f"Content-Type: text/plain\r\n\r\nlevel {n}\r\n--b{n}\r\n"
        tail = f"\r\n--b{n}--\r\n" + tail
    return (
        "From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost deep-structure\r\nMIME-Version: 1.0\r\n"
        f"{head}Content-Type: text/plain\r\n\r\ndeep{tail}"
    ).encode()


def address_lists() -> bytes:
    """The reader and Reply all compared address lists pairwise, quadratic in what the sender lists: 60,000 each in
    From, Reply-To and To (25,000 still passed in 5 s). Now a set of each list."""
    many = ", ".join(f"a{n}@example.org" for n in range(60_000))
    return (f"From: {many}\r\nReply-To: {many}\r\nTo: {many}\r\nSubject: cost address-lists\r\n\r\nhello\r\n").encode()


def deep_html() -> bytes:
    """The HTML parser walks the open elements for each start tag, so nesting made it quadratic: 80,000 <div>s took
    7.6 s. Past 256 levels the gate shows a notice instead."""
    return html_mail("deep-html", "<div>" * 60_000 + "x" + "</div>" * 60_000)


def unclosed_formatting() -> bytes:
    """html5ever's adoption agency re-opens each unclosed formatting element into every later block, so 100 <b>s
    left open inside a <p> (under the 256 depth cap, to exercise the work cap not the depth one) reconstruct into
    each of 200,000 closed paragraphs: 1.6 MB of mail built 370 MB of DOM and a 5 GB peak. The gate sums that work
    and shows a notice. The paragraphs are closed so the gate's own parser keeps a small stack, leaving the work
    cap, not its memory backstop, to catch it; and the leading <p> is load-bearing, as <b>s at body level do not
    reconstruct."""
    return html_mail("unclosed-formatting", "<p>" + "".join(f"<b class=c{n}>" for n in range(100)) + "<p>x</p>" * 200_000)


def uu_files() -> bytes:
    """Each file Forward carried out of a uuencoded text decoded the whole text again: 900 files of 4.5 KB, 5.7 MB of
    text (450-byte files decoded too fast to tell). Now one decode per part."""
    data = bytes(range(256)) * 17 + bytes(148)
    block = b"".join(binascii.b2a_uu(data[i : i + 45]) for i in range(0, len(data), 45))
    files = b"".join(b"begin 644 f%d.bin\n" % n + block + b"`\nend\n" for n in range(900))
    return b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost uu-files\r\n\r\nfiles:\n" + files


def uu_flood() -> bytes:
    """Uuencoded files escaped the 50,000-part cap, 31 bytes each: 60,000 listed made 27.8 MB of JSON and a 92 MiB
    peak. Past 1,000 a message's blocks stay in its text."""
    files = b"".join(b"begin 644 f%d.html\n`\nend\n" % n for n in range(60_000))
    return b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost uu-flood\r\n\r\nfiles:\n" + files


def big_text() -> bytes:
    """2.5 MB of plain text, past the reader's 2 MiB: text costs only its size, so Compose takes all of it."""
    return b"From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost big-text\r\n\r\n" + b"words words\r\n" * 210_000


def many_parts() -> bytes:
    """Forward carried every part, each upload summing all the user's uploads, and a half-carried one stayed behind:
    1,500 one-byte files. Uploads stop at 1,000 files, and a refused forward removes what it stored."""
    parts = "".join(f"--b\r\nContent-Type: application/octet-stream; name=p{n}.bin\r\n\r\nx\r\n" for n in range(1500))
    return (
        "From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost many-parts\r\nMIME-Version: 1.0\r\n"
        f"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nfiles\r\n{parts}--b--\r\n"
    ).encode()


def part_flood() -> bytes:
    """Each part cost the parse, the list and the JSON 1.5 KB, with no bound on parts: 160,000 in 6 MB peaked at 307 MB.
    Past 50,000 delimiter lines the message is not parsed, and downloads whole."""
    parts = "--b\r\nContent-Type: application/x-a\r\n\r\nx\r\n" * 60_000
    return (
        "From: a@example.org\r\nTo: andrew@example.com\r\nSubject: cost part-flood\r\nMIME-Version: 1.0\r\n"
        f"Content-Type: multipart/mixed; boundary=b\r\n\r\n{parts}--b--\r\n"
    ).encode()


COST_CASES = [
    Case("nested-rfc822", nested_rfc822, {"read": {422}, "print": {422}, "reply": {422}, "download": {200}}, mb=48),
    Case("wide-table", wide_table, {"read as text": {200}, "reply": {200}}, mb=32),
    Case("wide-table-max", wide_table_max, {"read as text": {200}, "reply": {200}}, mb=64),
    Case("image-references", image_references, {"read": {200}, "print": {200}}, mb=64, kb=1024),
    Case("enriched-excerpts", enriched_excerpts, {"read": {200}, "reply": {200}}, mb=32, seconds=3),
    Case("many-images", many_images, {"inline images": {200}}, mb=96, seconds=10),
    # Each set was bounded and the request was not: 60 sets of one folder's 1:99999 in 8 KB asked for 6 million UIDs.
    Case("zip-sets", small, {"zip of repeated sets": {400}}, mb=16),
    Case("deep-thread", deep_thread, {"lists complete": {200}}, mb=16),
    # mb headroom for the zip: it streams a 4 MiB batch (`BATCH`) plus a copy of the current message in its buffer,
    # so the peak is ~2x the folder's largest mail (the ~8 MB uu-flood one), not the folder total.
    Case("deep-structure", deep_structure, {"lists complete": {200}, "read": {200}, "zip of Select all": {200}}, mb=48),
    Case("uu-flood", uu_flood, {"read": {200}, "print": {200}}, mb=48, kb=8192),
    Case("draft-repeats", small, {"draft listing one upload 1,000 times": {400}}, mb=16),
    Case("settings-cap", small, {"settings over the cap": {413}}, mb=16),
    Case("forward-repeats", small, {"forward Select all 400 times": {400}}, mb=16),
    Case("forward-file-cap", small, {"forward 1,100 of INBOX": {413}, "upload": {200}}, mb=96, seconds=60),
    Case("deep-search", small, {"deep sorted search": {200}}, mb=24, seconds=30),
    Case("address-lists", address_lists, {"read": {200}, "reply all": {200}, "forward": {200}, "list row bounded": {200}}, mb=96),
    Case("deep-html", deep_html, {"read": {200}, "read as text": {200}, "reply": {200}}, mb=48, seconds=3),
    Case("unclosed-formatting", unclosed_formatting, {"read": {200}, "read as text": {200}, "reply": {200}}, mb=48, seconds=5),
    Case("uu-files", uu_files, {"forward": {200}}, mb=64, seconds=3),
    Case("many-parts", many_parts, {"forward": {413}, "upload": {200}}, mb=64, seconds=5),
    Case("part-flood", part_flood, {"read": {422}, "download": {200}}, mb=24),
    Case("huge-html", huge_html, {"read": {200}, "read as text": {200}, "print": {200}}, mb=24, kb=64),
    # A reply's quote converted its part on an async worker, 1.4 s for 4 MB: now on the render pool, in a turn.
    Case("big-text", big_text, {"edit as new keeps the body": {200}}, mb=64, seconds=10),
    Case("huge-html-replies", huge_html, {"replies beside healthz": {200}}, mb=192, seconds=120),
]


def cost_scenarios() -> list[tuple]:
    return [(f"hostile cost: {case.name}", lambda case=case: cost_case(case)) for case in COST_CASES]
