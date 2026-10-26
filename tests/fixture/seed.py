"""Write the fixture mailbox as a Maildir++ tree: the same bytes on every run, no network data.

usage: python3 -I seed.py <maildir-root>

Folders follow the owner's Mailu account: a large INBOX, threaded INBOX.github notifications, the special-use
folders and a folder with a Cyrillic name. Mail mixes plain text, HTML, attachments, encoded-word subjects in
several charsets and malformed headers. Dovecot numbers new files in file-name order, so UIDs are stable too.
"""

import base64
import os
import random
import sys
from datetime import datetime, timedelta, timezone
from email.utils import format_datetime
from pathlib import Path

ROOT = Path(sys.argv[1])
rng = random.Random(20261021)

START = datetime(2020, 1, 1, tzinfo=timezone.utc)
END = datetime(2026, 10, 20, tzinfo=timezone.utc)
SPAN = (END - START).total_seconds()

ME = ("Andrew", "andrew@example.test")
WORDS = (
    "nix flake build deploy server backup restic grafana alert outage invoice meeting tomorrow review patch "
    "release kernel driver update security token caddy docker container mail dovecot postfix question answer "
    "thanks schedule power battery grid fridge report weekly monthly order shipping receipt payment account "
    "reset welcome newsletter discount event"
).split()
UK_WORDS = "привіт рахунок зустріч завтра звіт оновлення сервер електрика батарея посилка оплата тиждень".split()
PEOPLE = [
    ("Olena Kovalenko", "olena@example.net"),
    ("Taras Shevchuk", "taras@example.com"),
    ("Мирослава Гнатюк", "myroslava@example.org"),
    ("Hetzner Online", "billing@hetzner.example"),
    ("Grafana", "alerts@grafana.example"),
    ("Nix Discourse", "discourse@nixos.example"),
    ("Mykola P.", "mykola@example.org"),
    ("Nova Poshta", "info@novaposhta.example"),
    ("山田 太郎", "taro@example.jp"),
    ("Café Ünïcode", "cafe@example.de"),
    (None, "noreply@service.example"),
]
CORRESPONDENTS = [(f"Contact {i}", f"contact{i}@example.com") for i in range(60)] + PEOPLE[:3]
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
GITHUB_REPOS = ["nixos/nixpkgs", "kittyandrew/kubicle", "duesee/imap-codec", "dovecot/core", "mailu/mailu"]

serial = 0  # file names sort in creation order, which is the order Dovecot assigns UIDs in


def mutf7(name):
    """IMAP modified UTF-7 (RFC 3501, 5.1.3), which Maildir++ uses for folder directory names."""
    out, run = [], []

    def flush():
        if run:
            data = "".join(run).encode("utf-16-be")
            out.append("&" + base64.b64encode(data).decode().rstrip("=").replace("/", ",") + "-")
            run.clear()

    for ch in name:
        if 0x20 <= ord(ch) <= 0x7E:
            flush()
            out.append("&-" if ch == "&" else ch)
        else:
            run.append(ch)
    flush()
    return "".join(out)


def words(n, pool=WORDS):
    return " ".join(rng.choice(pool) for _ in range(n))


def encoded_word(text):
    """A header value as RFC 2047 encoded words, in a charset and encoding picked at random."""
    kind = rng.random()
    if kind < 0.5:
        return f"=?UTF-8?B?{base64.b64encode(text.encode()).decode()}?="
    if kind < 0.7:
        q = "".join(
            c if c.isascii() and c.isalnum() else "_" if c == " " else "".join(f"={b:02X}" for b in c.encode()) for c in text
        )
        return f"=?utf-8?q?{q}?="
    try:
        charset = rng.choice(["windows-1251", "koi8-u"])
        return f"=?{charset}?B?{base64.b64encode(text.encode(charset)).decode()}?="
    except UnicodeEncodeError:
        return f"=?UTF-8?B?{base64.b64encode(text.encode()).decode()}?="


def subject_line():
    kind = rng.random()
    if kind < 0.80:
        return words(rng.randint(3, 8)).capitalize()
    if kind < 0.92:
        return encoded_word(words(rng.randint(2, 6), UK_WORDS).capitalize())
    if kind < 0.96:
        return encoded_word(rng.choice(["Поїзд 🚆 о 8:15", "会議の議事録", "Résumé — naïve façade", "Grüße 👋"]))
    return words(2) + " " + encoded_word("звіт") + " " + words(2)


def address(person):
    name, email = person
    if name is None:
        return email
    if name.isascii():
        return f'"{name}" <{email}>' if "." in name else f"{name} <{email}>"
    return f"{encoded_word(name)} <{email}>"


def paragraphs():
    return [words(rng.randint(8, 30)).capitalize() + "." for _ in range(rng.randint(1, 6))]


def body(head):
    """A complete message: `head` plus a MIME body of a randomly chosen shape."""
    paras = paragraphs()
    text = "\r\n\r\n".join(paras)
    html = (
        "<html><body style='font-family:sans-serif'>"
        + "".join(f"<p>{p}</p>" for p in paras)
        + ("<p><img src='https://tracker.example/pixel.gif?id=1'></p></body></html>")
    )
    kind = rng.random()
    head += "MIME-Version: 1.0\r\n"
    if kind < 0.55:
        return head + "Content-Type: text/plain; charset=utf-8\r\n\r\n" + text + "\r\n"
    if kind < 0.80:
        return (
            head + 'Content-Type: multipart/alternative; boundary="alt"\r\n\r\n'
            "--alt\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n" + text + "\r\n"
            "--alt\r\nContent-Type: text/html; charset=utf-8\r\n\r\n" + html + "\r\n--alt--\r\n"
        )
    if kind < 0.85:
        return head + "Content-Type: text/html; charset=utf-8\r\n\r\n" + html + "\r\n"
    if kind < 0.89:
        return (
            head + 'Content-Type: multipart/related; boundary="rel"\r\n\r\n'
            "--rel\r\nContent-Type: text/html; charset=utf-8\r\n\r\n"
            + html.replace("</body>", "<img src='cid:logo@fixture'></body>")
            + "\r\n"
            "--rel\r\nContent-Type: image/png\r\nContent-ID: <logo@fixture>\r\nContent-Disposition: inline\r\n"
            "Content-Transfer-Encoding: base64\r\n\r\n" + base64.b64encode(PNG).decode() + "\r\n--rel--\r\n"
        )
    pdf = base64.encodebytes(b"%PDF-1.1\n" + text.encode() * 20 + b"\n%%EOF").decode().replace("\n", "\r\n")
    return (
        head + 'Content-Type: multipart/mixed; boundary="mix"\r\n\r\n'
        "--mix\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n" + text + "\r\n"
        '--mix\r\nContent-Type: image/png; name="pixel.png"\r\nContent-Disposition: attachment; filename="pixel.png"\r\n'
        "Content-Transfer-Encoding: base64\r\n\r\n" + base64.b64encode(PNG).decode() + "\r\n"
        '--mix\r\nContent-Type: application/pdf; name="=?UTF-8?B?0YDQsNGF0YPQvdC+0LoucGRm?="\r\n'
        "Content-Disposition: attachment; filename*=UTF-8''%D1%80%D0%B0%D1%85%D1%83%D0%BD%D0%BE%D0%BA.pdf\r\n"
        "Content-Transfer-Encoding: base64\r\n\r\n" + pdf + "--mix--\r\n"
    )


def folder_dir(name):
    d = ROOT if name == "INBOX" else ROOT / ("." + mutf7(name))
    for sub in ("cur", "new", "tmp"):
        (d / sub).mkdir(parents=True, exist_ok=True)
    return d


def store(d, message, when, flags=""):
    global serial
    serial += 1
    data = message.encode()
    vsize = len(data) + data.count(b"\n") - data.count(b"\r\n")  # the size with every line ending as CRLF
    # S= and W= as Dovecot's own delivery writes them: without them it stats and reads every file for its sizes.
    path = d / "cur" / f"{1600000000 + serial}.M{serial}P1.fixture,S={len(data)},W={vsize}:2,{flags}"
    path.write_bytes(data)
    stamp = when.timestamp()
    os.utime(path, (stamp, stamp))  # the file's mtime is the message's INTERNALDATE in maildir


def flags_for(i, n, unseen_tail):
    flags = ""
    if rng.random() < 0.03:
        flags += "F"
    if rng.random() < 0.02:
        flags += "R"
    if i < n - unseen_tail:
        flags += "S"
    return "".join(sorted(flags))


def times(n):
    return sorted(START + timedelta(seconds=rng.random() * SPAN) for _ in range(n))


def gen_folder(name, n, unseen_tail, sender=None, reply_ratio=0.3):
    d = folder_dir(name)
    sent = []  # (message-id, subject, references) of earlier messages, for replies
    for i, when in enumerate(times(n)):
        mid = f"<{mutf7(name)}.{i}.{rng.getrandbits(40):x}@fixture.example>"
        frm, to = (ME, [rng.choice(CORRESPONDENTS) for _ in range(rng.randint(1, 3))]) if sender else (rng.choice(PEOPLE), [ME])
        head = f"From: {address(frm)}\r\nTo: {', '.join(address(p) for p in to)}\r\n"
        if rng.random() < 0.15:
            head += f"Cc: {address(rng.choice(CORRESPONDENTS))}\r\n"
        if sent and rng.random() < reply_ratio:
            parent = rng.choice(sent[-300:])
            subject = "Re: " + parent[1].removeprefix("Re: ")
            refs = (parent[2] + " " + parent[0]).strip()
            head += f"In-Reply-To: {parent[0]}\r\nReferences: {refs}\r\n"
        else:
            subject, refs = subject_line(), ""
        sent.append((mid, subject, refs))
        head += f"Subject: {subject}\r\nDate: {format_datetime(when)}\r\nMessage-ID: {mid}\r\n"
        store(d, body(head), when, flags_for(i, n, unseen_tail))


def gen_github(n, unseen_tail):
    """GitHub's notification shape: every mail References a thread root that never arrives itself."""
    d = folder_dir("INBOX.github")
    threads = [(rng.choice(GITHUB_REPOS), k + 1, words(rng.randint(3, 7)).capitalize()) for k in range(n // 8)]
    for i, when in enumerate(times(n)):
        repo, number, title = threads[
            min(int(rng.paretovariate(1.2)) - 1, len(threads) - 1) if rng.random() < 0.5 else rng.randrange(len(threads))
        ]
        root = f"<{repo}/pull/{number}@github.com>"
        mid = f"<{repo}/pull/{number}/c{i}@github.com>"
        head = (
            f'From: "{rng.choice(["octocat", "dependabot[bot]", "Mic92", "duesee"])}" <notifications@github.com>\r\n'
            f"To: {repo} <{repo.split('/')[1]}@noreply.github.com>\r\nCc: Andrew <andrew@example.test>\r\n"
            f"Subject: Re: [{repo}] {title} (PR #{number})\r\nDate: {format_datetime(when)}\r\nMessage-ID: {mid}\r\n"
            f"In-Reply-To: {root}\r\nReferences: {root}\r\nList-ID: {repo} <{repo.replace('/', '.')}.github.com>\r\n"
        )
        store(d, body(head), when, flags_for(i, n, unseen_tail))


def gen_edge_cases():
    """Malformed and large mail that real mailboxes hold, at the end of INBOX so the newest page shows them."""
    d = folder_dir("INBOX")
    when = END + timedelta(hours=1)
    cases = [
        "From: No Date <nodate@example.com>\r\nTo: andrew@example.test\r\nSubject: No Date header\r\n"
        "Message-ID: <nodate@fixture.example>\r\n",
        "From: Bad Date <baddate@example.com>\r\nTo: andrew@example.test\r\nSubject: Unparseable date\r\n"
        "Date: sometime last week\r\nMessage-ID: <baddate@fixture.example>\r\n",
        f"From: <nomid@example.com>\r\nTo: andrew@example.test\r\nSubject: No Message-ID\r\nDate: {format_datetime(when)}\r\n",
        "From: Empty Subject <empty@example.com>\r\nTo: andrew@example.test\r\nSubject: \r\n"
        f"Date: {format_datetime(when)}\r\nMessage-ID: <empty@fixture.example>\r\n",
        "From: Long <long@example.com>\r\nTo: andrew@example.test\r\nSubject: " + words(120) + "\r\n"
        f"Date: {format_datetime(when)}\r\nMessage-ID: <long@fixture.example>\r\n",
        "From: =?UTF-8?B?0J/QtdGC0YDQvg==?= <petro@example.com>\r\nTo: undisclosed-recipients:;\r\n"
        f"Subject: =?UTF-8?Q?=D0=9F=D1=80=D0=B8=D0=B2=D1=96=D1=82?= =?UTF-8?Q?_=F0=9F=91=8B?=\r\n"
        f"Date: {format_datetime(when)} (EEST)\r\nMessage-ID: <groups@fixture.example>\r\n",
        "From: Raw UTF-8 Ім'я <raw@example.com>\r\nTo: andrew@example.test\r\nSubject: Сирий UTF-8 без кодування\r\n"
        f"Date: {format_datetime(when)}\r\nMessage-ID: <raw8bit@fixture.example>\r\n",
    ]
    for head in cases:
        store(d, head + "Content-Type: text/plain; charset=utf-8\r\n\r\nEdge case body.\r\n", when)
    big = base64.encodebytes(bytes(rng.getrandbits(8) for _ in range(4 * 1024 * 1024))).decode().replace("\n", "\r\n")
    store(
        d,
        (
            "From: Big Attachment <big@example.com>\r\nTo: andrew@example.test\r\nSubject: Four megabytes attached\r\n"
            f"Date: {format_datetime(when)}\r\nMessage-ID: <big@fixture.example>\r\nMIME-Version: 1.0\r\n"
            'Content-Type: multipart/mixed; boundary="big"\r\n\r\n--big\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n'
            '--big\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename="random.bin"\r\n'
            "Content-Transfer-Encoding: base64\r\n\r\n" + big + "--big--\r\n"
        ),
        when,
    )


gen_folder("INBOX", 50_000, unseen_tail=300)
gen_edge_cases()
gen_github(6_000, unseen_tail=1_000)
gen_folder("INBOX.finance", 800, unseen_tail=5)
gen_folder("Проєкти", 40, unseen_tail=3)
gen_folder("Sent", 400, unseen_tail=0, sender=ME, reply_ratio=0.5)
gen_folder("Archive", 500, unseen_tail=0)
gen_folder("Junk", 300, unseen_tail=120, reply_ratio=0)
gen_folder("Trash", 200, unseen_tail=0)
gen_folder("Drafts", 3, unseen_tail=0, sender=ME, reply_ratio=0)
(ROOT / "subscriptions").write_text(
    "\n".join(["INBOX", "INBOX.github", "INBOX.finance", mutf7("Проєкти"), "Sent", "Archive", "Junk", "Trash", "Drafts"]) + "\n"
)
