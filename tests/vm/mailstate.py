"""usage: mail-state PORT CA_FILE USER...

Serves `GET /?tag=TAG` on PORT: what each USER holds on the mail server, as one JSON object per user, normalized so
that two accounts that went through the same actions compare equal (vm-settings, differential.py). The user's own
address reads ME; UIDs and the Message-IDs a webmail made up are left out, so a message is known by its Message-ID
when the test delivered it and by its subject otherwise; flags lose \\Recent. Messages whose subject holds TAG also
carry the headers a setting can change and the shape of their text.

Each user keeps one IMAP connection, read-only (EXAMINE), through front's IMAPS with the account's password. A
message's headers and text never change, so each is fetched once; an answer costs a LIST, an LSUB and the flags of
every folder."""

import base64
import email
import email.policy
import imaplib
import itertools
import json
import re
import ssl
import sys
from concurrent.futures import ThreadPoolExecutor
from email.header import decode_header, make_header
from email.message import Message
from email.parser import HeaderParser
from email.utils import getaddresses, parsedate_tz
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

ADDRESSES = ["from", "to", "cc", "bcc", "reply-to", "mail-reply-to", "mail-followup-to", "disposition-notification-to"]
KNOWN_IDS = ("@fixture.example>", "@kdt.test>")  # seed.py's and differential.py's
TYPED = "KDT-TYPED"  # what differential.py types into a message


def decoded(value: str) -> str:
    return " ".join(str(make_header(decode_header(value))).split())


def mutf7(name: str) -> str:
    """IMAP's modified UTF-7 folder name (RFC 3501, 5.1.3) as text."""

    def utf16(m: re.Match) -> str:
        return base64.b64decode(m[1].replace(",", "/") + "=" * (-len(m[1]) % 4)).decode("utf-16-be") if m[1] else "&"

    return re.sub(r"&([^-]*)-", utf16, name)


def key(message_id: str, subject: str) -> str:
    message_id = message_id.strip()
    return message_id if message_id.endswith(KNOWN_IDS) else "subject: " + decoded(subject)


def flags(value: str) -> str:
    return " ".join(sorted(f for f in value.split() if f != "\\Recent"))


def addresses(message: Message, name: str) -> list[str]:
    found = getaddresses(message.get_all(name, []))
    return sorted(f"{decoded(n)} <{a.lower()}>" if n else a.lower() for n, a in found)


def text_part(message: Message) -> Message | None:
    return next((p for p in message.walk() if p.get_content_type() == "text/plain" and not p.get_filename()), None)


def shape(part: Message | None) -> str:
    """The text's lines in runs: T what the test typed, Q quoted, O anything else; "OQT" is a reply below the quote."""
    if part is None:
        return ""
    payload = part.get_payload(decode=True)
    text = payload.decode(part.get_content_charset() or "utf-8", "replace") if isinstance(payload, bytes) else ""
    kinds = ["T" if TYPED in line else "Q" if line.startswith(">") else "O" for line in text.splitlines() if line.strip()]
    return "".join(k for k, _ in itertools.groupby(kinds))


def detail(message: Message) -> dict:
    found: dict = {name: addresses(message, name) for name in ADDRESSES}
    zone = parsedate_tz(message.get("date", ""))
    part = text_part(message)
    found.update(
        {
            "subject": decoded(message.get("subject", "")),
            "in-reply-to": re.findall(r"<[^>]+>", message.get("in-reply-to", "")),
            "references": re.findall(r"<[^>]+>", message.get("references", "")),
            "x-priority": message.get("x-priority", "").split()[:1],
            "organization": decoded(message.get("organization", "")),
            "date-zone": zone[9] if zone else None,
            "content-type": message.get_content_type(),
            "format": [part.get_param(p) for p in ("format", "delsp")] if part else [],
            "parts": [f"{p.get_content_type()} {p.get_filename() or ''}".strip() for p in message.walk() if not p.is_multipart()],
            "shape": shape(part),
        }
    )
    return found


class Account:
    def __init__(self, user: str, context: ssl.SSLContext) -> None:
        self.user, self.context = user, context
        self.me = re.compile(re.escape(user), re.IGNORECASE)
        self.headers: dict[tuple[str, str, str], tuple[str, str]] = {}  # (folder, UIDVALIDITY, UID): Message-ID, subject
        self.texts: dict[tuple[str, str, str], dict] = {}  # the same: `detail`
        self.connect()

    def connect(self) -> None:
        self.imap = imaplib.IMAP4_SSL("mail.example.test", ssl_context=self.context)
        self.imap.login(self.user, "fixture-password")

    @staticmethod
    def ok(answer: tuple) -> list:
        typ, data = answer
        if typ != "OK":
            raise RuntimeError(f"{typ} {data}")
        return data

    def folders(self, command: str) -> list[tuple[str, bool]]:
        """The folders LIST or LSUB names, each with whether it can be opened."""
        found = []
        for line in self.ok(getattr(self.imap, command)()):
            match = re.match(rb'\((?P<flags>[^)]*)\) (?:"[^"]*"|NIL) (?P<name>.+)$', line)
            assert match, line
            name = match["name"].decode()
            name = json.loads(name) if name.startswith('"') else name
            found.append((name, b"\\Noselect" not in match["flags"] and b"\\NonExistent" not in match["flags"]))
        return found

    def fetch(self, uids: list[str], items: str) -> list[tuple[str, bytes]]:
        found = []
        for start in range(0, len(uids), 500):
            for part in self.ok(self.imap.uid("FETCH", ",".join(uids[start:][:500]), items)):
                if isinstance(part, tuple):
                    found.append((re.search(rb"UID (\d+)", part[0])[1].decode(), part[1]))  # type: ignore[index]
        return found

    def folder(self, name: str, tag: str) -> tuple[list, list]:
        exists = int(self.ok(self.imap.select(f'"{name}"', readonly=True))[0])
        validity = self.imap.response("UIDVALIDITY")[1][0].decode()  # type: ignore[index]
        if not exists:
            return [], []
        marks = {}
        for line in self.ok(self.imap.uid("FETCH", "1:*", "(FLAGS)")):
            marks[re.search(rb"UID (\d+)", line)[1].decode()] = re.search(rb"FLAGS \(([^)]*)\)", line)[1].decode()  # type: ignore[index]
        new = [uid for uid in marks if (name, validity, uid) not in self.headers]
        for uid, raw in self.fetch(new, "(BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])"):
            parsed = HeaderParser(policy=email.policy.compat32).parsestr(raw.decode("utf-8", "replace"))
            self.headers[(name, validity, uid)] = (parsed.get("message-id", ""), parsed.get("subject", ""))
        tagged = [uid for uid in marks if tag in decoded(self.headers[(name, validity, uid)][1])]
        for uid, raw in self.fetch([uid for uid in tagged if (name, validity, uid) not in self.texts], "(BODY.PEEK[])"):
            message = email.message_from_bytes(raw, policy=email.policy.compat32)
            self.texts[(name, validity, uid)] = {"key": key(message.get("message-id", ""), message.get("subject", ""))} | detail(
                message
            )
        label = self.me.sub("ME", mutf7(name))
        messages = [[label, self.me.sub("ME", key(*self.headers[(name, validity, uid)])), flags(f)] for uid, f in marks.items()]
        details = [{"mailbox": label, **self.texts[(name, validity, uid)], "flags": flags(marks[uid])} for uid in tagged]
        return messages, [json.loads(self.me.sub("ME", json.dumps(d))) for d in details]

    def state(self, tag: str) -> dict:
        try:
            return self.read(tag)
        except (imaplib.IMAP4.abort, OSError):
            self.connect()  # the server dropped the connection: once more on a new one
            return self.read(tag)

    def read(self, tag: str) -> dict:
        folders = self.folders("list")
        messages, details = [], []
        for name, selectable in folders:
            if selectable:
                found, more = self.folder(name, tag)
                messages += found
                details += more
        return {
            "folders": sorted(self.me.sub("ME", mutf7(n)) for n, _ in folders),
            "subscribed": sorted(self.me.sub("ME", mutf7(n)) for n, _ in self.folders("lsub")),
            "messages": sorted(messages),
            "details": details,
        }


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        tag = parse_qs(urlparse(self.path).query).get("tag", [""])[0]
        try:
            with ThreadPoolExecutor(len(ACCOUNTS)) as pool:
                states = dict(zip(ACCOUNTS, pool.map(lambda a: a.state(tag), ACCOUNTS.values())))
            body, status = json.dumps(states).encode(), 200
        except Exception as e:
            body, status = repr(e).encode(), 500
        self.send_response(status)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        pass  # a line per request is noise in the VM's journal


port, ca, *users = sys.argv[1:]
context = ssl.create_default_context(cafile=ca)
ACCOUNTS = {user: Account(user, context) for user in users}
HTTPServer(("0.0.0.0", int(port)), Handler).serve_forever()
