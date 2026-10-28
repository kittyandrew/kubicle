# The scenarios the Mailu VM checks run, after tests/record.py. A check's test script defines tier_setup(), which
# fills `tier` (its user, its mail server's size limit, and how to sign in, deliver mail, run doveadm and forge
# sso.php on its stack), and ends in run_tier(). Every scenario runs even after one failed, so one run names every
# broken scenario.
# mypy: disable-error-code="name-defined"
# `machine` and `subtest` come from the test driver, tier_setup() from the check's script, `fail`, `guarded` and the
# rest from record.py: the driver type-checks (ty) and lints (ruff F) the joined script, where every name is defined.
import base64
import hashlib
import html
import json
import re
import shlex
import tempfile
import time
import urllib.parse
from types import SimpleNamespace

# Folder label -> unread badge, from seed.py: each folder's unseen_tail, plus INBOX's 8 edge cases left unread.
FOLDERS = {
    "Inbox": "308",
    "finance": "5",
    "github": "1,000",
    "Drafts": "",
    "Sent": "",
    "Junk": "120",
    "Trash": "",
    "Archive": "",
    "Проєкти": "3",
}
NEWEST = ["Four megabytes attached", "Unparseable date", "No Date header", "(no subject)"]  # seed.py's edge cases
SUBJECTS = "[...document.querySelectorAll('.row-subject')].map(e => e.textContent)"

tier = SimpleNamespace()
failed: list[str] = []


def wd(*args: str) -> str:
    return machine.succeed(" ".join(["wd", *map(shlex.quote, args)]))


def js(script: str):
    return json.loads(wd("js", script))


def wait(expression: str, seconds: int = 30):
    return json.loads(wd("wait", expression, str(seconds)))


def scenario(name: str, check) -> None:
    try:
        with subtest(name):
            check()
    except Exception:
        failed.append(f"{name}: {traceback.format_exc(limit=3).strip()}")
        machine.execute(f"wd shot /tmp/failed-{len(failed)}.png")  # run_tier() collects it
    finally:
        close_tabs()


def reload() -> None:
    """The app anew in this tab, still signed in."""
    wd("go", "http://localhost/login/")
    wait(f"document.querySelector('.folder') && document.body.textContent.includes('{tier.user}')", 60)


def sign_in() -> None:
    tier.sign_in()
    wait(
        "location.pathname === '/login/' && document.querySelector('.folder')"
        f" && document.body.textContent.includes('{tier.user}')"
    )


def folders() -> None:
    shown = """return Object.fromEntries([...document.querySelectorAll('.folder')].map(b =>
        [b.querySelector('.folder-name').textContent, b.querySelector('.badge')?.textContent ?? '']))"""
    # The badges come from the engine's STATUS of each folder: the check runs after INBOX's page loaded and wants
    # five equal readings in a row.
    deadline, matches = time.monotonic() + 120, 0
    while matches < 5:
        found = js(shown)
        matches = matches + 1 if found == FOLDERS else 0
        assert time.monotonic() < deadline, f"folders shown: {found}"
        time.sleep(1)
    # Roundcube's sort_folder_list: Inbox, the special folders in their order, each with its subfolders, then the rest.
    order = js("return [...document.querySelectorAll('.folder .folder-name')].map(e => e.textContent)")
    assert order == list(FOLDERS), f"folder order: {order}"


def inbox_page() -> None:
    wait(
        f"""document.querySelectorAll('.row').length === 50
        && document.querySelector('.paging .muted')?.title === 'Messages 1 to 50 of 50,008'
        && {json.dumps(NEWEST)}.every(s => {SUBJECTS}.includes(s))""",
        120,
    )


def open_message() -> None:
    js("""[...document.querySelectorAll('.row')]
        .find(r => r.querySelector('.row-subject').textContent === 'Four megabytes attached').click()""")
    wait("""document.querySelector('.message-subject')?.textContent === 'Four megabytes attached'
        && document.querySelector('.mail-frame iframe')?.srcdoc.includes('See attached.')
        && document.querySelector('.attachments .chip-name')?.textContent === 'random.bin'""")


def csrf() -> None:
    # The CSRF check runs before kubicle reads the body, so any state-changing route answers 403 first.
    status = json.loads(
        wd(
            "async",
            """fetch('api/settings', {method: 'POST',
        headers: {'content-type': 'application/json'}, body: '{}'}).then(r => done(r.status))""",
        )
    )
    assert status == 403, f"a POST without the CSRF token got {status}"


def forged_sso() -> None:
    status = machine.succeed(tier.forged_sso).strip()
    assert status == "403", f"sso.php with a forged token got {status}"


def healthcheck() -> None:
    # The image's own Docker healthcheck, `kubicle-server --health` in exec form, as the image has no shell: it asks
    # /healthz over loopback, which KUBICLE_TRUSTED_PROXY's range does not hold.
    status = "docker inspect -f '{{.State.Health.Status}}' kubicle"
    machine.wait_until_succeeds(f'test "$({status})" = healthy', timeout=90)


def send() -> None:
    subject = "Sent through submission"
    wd("click", ".compose-button")
    wd("type", ".compose input[role=combobox]", tier.user)  # To, the first address field
    wd("type", ".compose input[aria-label=Subject]", subject)
    wd("type", "textarea.compose-body", "Written in the VM test.")
    wd("click", ".compose .button.primary")
    wait("document.querySelector('.toast')?.textContent === 'Message sent successfully.'")
    for folder in ["INBOX", "Sent"]:
        arrived(folder, subject)


def size_limit() -> None:
    # The mail server's SIZE (Postfix's message_size_limit; Mailu's MESSAGE_SIZE_LIMIT), read by kubicle, refuses
    # a file too large alone at upload, a message too large in all before SMTP, and in Compose the file that would
    # push the message past it. A file takes 78 bytes per 57 in the message (kubicle_api::encoded_size).
    limit = tier.size_limit
    session = json.loads(wd("async", "fetch('api/session').then(r => r.json()).then(s => done(s.max_message_size))"))
    assert session == limit, f"the session says {session}, the mail server {limit}"

    def zeros(size: int) -> str:
        return f"new Blob([new Uint8Array({size})], {{type: 'application/octet-stream'}})"

    # Just past what fits alone, under kubicle's own request ceiling.
    status, text = upload(zeros(min(36 * 2**20 - 4096, limit // 78 * 57 + 100_000)), "alone.bin")
    assert status == 413 and "exceeds the maximum size" in text, (status, text)
    # Two that fit alone and not together.
    part = limit // 78 * 57 * 6 // 10
    sent = []
    for name in ["first.bin", "second.bin"]:
        status, text = upload(zeros(part), name)
        assert status == 200, (status, text)
        sent.append(json.loads(text))
    draft = new_draft()
    draft.update(to=[{"name": None, "email": tier.user}], subject="Too large in all", attachments=sent)
    status, answer = request("POST", "api/send", draft)
    assert status == 413 and "more than the" in str(answer), (status, answer)
    # Compose: the second file is refused before it uploads, in Roundcube's words. Compose is discarded however
    # this ends, so a failure here does not leave it in the way of the scenarios after.
    machine.succeed(f"head -c {part} /dev/zero > /tmp/first.bin && head -c {part} /dev/zero > /tmp/second.bin")
    wd("click", ".compose-button")
    try:
        wait("document.querySelector('.compose input[type=file]')")
        wd("type", ".compose input[type=file]", "/tmp/first.bin")
        wait("document.querySelectorAll('.attachment-item:not(.uploading)').length === 1", 60)
        wd("type", ".compose input[type=file]", "/tmp/second.bin")
        wait("document.querySelector('.toast')?.textContent.startsWith('Failed to attach a file. Maximum size of a message')")
        assert js("return document.querySelectorAll('.attachment-item').length") == 1
    finally:
        wd("click", ".compose .tool[title='Discard this draft']")
        wait("!document.querySelector('.compose') || document.querySelector('.dialog .button.primary')")
        if js("return !!document.querySelector('.dialog .button.primary')"):
            wd("click", ".dialog .button.primary")
        wait("!document.querySelector('.compose')")


def resend() -> None:
    # Resend (bounce) puts Resent-* lines on top of the message and sends it on unchanged, its own Bcc line
    # included, as Roundcube's does after its warning; and it goes out only from the user's own address. The filed
    # copy in Sent holds the bytes that went out, which the copy delivered to INBOX does not show: delivery drops
    # a Bcc line.
    subject = "Resent with its blind copy"
    machine.succeed(tier.deliver(subject, "--h-Bcc secret@example.org"))
    me = {"name": None, "email": tier.user}
    uid = arrived("INBOX", subject)
    original = fetch("hdr", "INBOX", uid)
    assert re.search(r"(?im)^bcc:", original), f"the original has no Bcc line: {original}"
    ask = {"folder": "INBOX", "uid": uid, "to": [me], "cc": [], "bcc": [], "sent_copy": "Sent"}

    def post(body: dict) -> int:
        return request("POST", "api/redirect", body)[0]

    assert post({**ask, "from": {"name": None, "email": "mallory@elsewhere.example"}}) == 400, "resent from a foreign address"
    assert post({**ask, "from": me}) == 204, "resend refused"
    head = fetch("hdr", "Sent", arrived("Sent", subject, f"header Resent-From {shlex.quote(tier.user)}"))
    assert "Resent-Message-ID:" in head and re.search(r"(?im)^bcc: secret@example.org", head), head


def request(method: str, path: str, body=None):
    """A request from the signed-in page, with its CSRF token: the status and the answer, as JSON when it is."""
    send = "" if body is None else f", body: {json.dumps(json.dumps(body))}"
    return json.loads(
        wd(
            "async",
            f"""fetch('api/session').then(r => r.json()).then(s => fetch({json.dumps(path)}, {{
        method: '{method}', headers: {{'content-type': 'application/json', 'x-kubicle-csrf': s.csrf}}{send}}}))
        .then(async r => {{ const t = await r.text();
          try {{ done([r.status, JSON.parse(t)]); }} catch {{ done([r.status, t]); }} }})""",
        )
    )


# Every `doveadm search` here passes `-o plugin/fts=`: through Mailu's full-text index it waits for the indexer
# to catch up (18 s after a delivery, Oct 25, 2026) and never finds a one-letter word; without it Dovecot reads
# the headers it caches. These searches only read the server's state; kubicle's own search still uses the index.
def search(folder: str, criteria: str, wait: int = 0) -> list[int]:
    """The UIDs `doveadm search` finds in `folder` for `criteria`; with `wait`, once it finds one within that many s."""
    command = f"{tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox {shlex.quote(folder)} {criteria}"
    if wait:
        machine.wait_until_succeeds(f"{command} | grep -q .", timeout=wait)
    return [int(uid) for uid in machine.succeed(command).split()[1::2]]  # each line: the mailbox GUID, the UID


def arrived(folder: str, subject: str, also: str = "", wait: int = 30) -> int:
    """The UID of the newest mail with `subject` (and `also`, more criteria) in `folder`, once there is one."""
    return search(folder, f"subject {shlex.quote(subject)} {also}", wait)[-1]


def fetch(field: str, folder: str, uid) -> str:
    """`doveadm fetch` of `field` (hdr, text, flags) for a UID or a set."""
    return machine.succeed(f"{tier.doveadm} fetch -u {tier.user} {field} mailbox {shlex.quote(folder)} uid {uid}")


def upload(blob: str, name: str, path: str = "api/upload") -> tuple[int, str]:
    """POSTs a file to api/upload, or `path`, `blob` a script expression for its Blob: the status and the answer's text."""
    return json.loads(
        wd(
            "async",
            f"""fetch('api/session').then(r => r.json()).then(s => {{
        const form = new FormData(); form.append('file', {blob}, {json.dumps(name)});
        return fetch({json.dumps(path)}, {{method: 'POST', headers: {{'x-kubicle-csrf': s.csrf}}, body: form}});
        }}).then(async r => done([r.status, await r.text()]))""",
        )
    )


def deliver_raw(subject: str, message: str) -> int:
    """Delivers `message`, whole with its headers, and answers its UID in INBOX."""
    machine.succeed(f"cat > /tmp/raw.eml <<'EOF'\n{message}\nEOF")
    machine.succeed(tier.deliver(subject, "--data @/tmp/raw.eml"))  # without the @, swaks sends the path as text
    return arrived("INBOX", subject)


def settings_change(**fields) -> dict:
    """Changes settings and answers the ones before."""
    status, before = request("GET", "api/settings")
    assert status == 200, (status, before)
    status, answer = request("POST", "api/settings", {**before, **fields})
    assert status == 200, (status, answer)
    return before


def prefilled(mode: str, uid: int, folder: str = "INBOX") -> dict:
    status, prefill = request("POST", "api/compose", {"mode": mode, "messages": [[folder, {"Uids": [uid]}]], "zone": ""})
    assert status == 200, (mode, status, prefill)
    return prefill["draft"]


def new_draft() -> dict:
    status, prefill = request("POST", "api/compose", {"mode": "New", "messages": [], "zone": ""})
    assert status == 200, (status, prefill)
    return prefill["draft"]


def emails(addresses: list) -> list:
    return [a["email"] for a in addresses]


def reply_prefill() -> None:
    # Roundcube's replies and forwards (rcmail_sendmail::compose_header_value, compose_part_body): recipients by
    # Mail-Followup-To, Mail-Reply-To, Reply-To and From; a flowed original unfolded; HTML quoted as text with its
    # line breaks; the signature cut from a reply and an inline forward only when short; the original's priority
    # and thread kept.
    me = tier.user
    flowed = deliver_raw(
        "Flowed from a list",
        """From: Alice <alice@example.net>
To: list@lists.example.net
Reply-To: list@lists.example.net
Subject: Flowed from a list
Message-ID: <m1@example.com>
References: <r0@example.com>
X-Priority: 1 (Highest)
Content-Type: text/plain; charset=utf-8; format=flowed

This paragraph was written as one line and sent with soft breaks, so it 
continues here.

-- 
Alice""",  # noqa: W291 - format=flowed's soft break and the "-- " signature line both end in a space
    )
    reply = prefilled("Reply", flowed)
    assert emails(reply["to"]) == ["list@lists.example.net"] and reply["cc"] == [], reply
    assert reply["priority"] == "Highest", reply["priority"]
    assert "> This paragraph was written as one line and sent with soft breaks, so it continues here." in reply["body"], reply[
        "body"
    ]
    assert "Alice\n" not in reply["body"].split("wrote:", 1)[1], reply["body"]  # the signature went
    everyone = prefilled("ReplyAll", flowed)
    # Reply-To names every To address but not From: a list pointing replies at itself, so From goes to Cc.
    assert emails(everyone["to"]) == ["list@lists.example.net"] and emails(everyone["cc"]) == ["alice@example.net"], everyone
    forward = prefilled("Forward", flowed)
    assert forward["in_reply_to"] == "m1@example.com" and forward["references"] == ["r0@example.com", "m1@example.com"], forward
    assert forward["priority"] == "Highest" and forward["body"].endswith("so it continues here."), forward  # the signature went

    html = deliver_raw(
        "Gmail-style lines",
        """From: Bob <bob@example.net>
To: someone@example.net
Mail-Reply-To: support@vendor.example
Reply-To: bob@example.net
Mail-Followup-To: team@example.net
Subject: Gmail-style lines
Content-Type: text/html; charset=utf-8

<div>First line</div><div>Second line</div><ul><li>one</li><li>two</li></ul><p><a href="https://example.com/x">a link</a></p>""",
    )
    reply = prefilled("Reply", html)
    assert emails(reply["to"]) == ["support@vendor.example"], reply  # Mail-Reply-To before Reply-To
    for line in ["> First line\n> Second line", "one\n", "two\n", "https://example.com/x"]:
        assert line in reply["body"], (line, reply["body"])
    assert emails(prefilled("ReplyAll", html)["to"]) == ["team@example.net"]  # Mail-Followup-To, for reply all

    tail = "\n".join(f"Body line {n}" for n in range(20))
    long = deliver_raw(
        "A dash line mid-body",
        f"""From: Carol <carol@example.net>
To: {me}
Subject: A dash line mid-body
Content-Type: text/plain; charset=utf-8

Above the dashes.
-- 
{tail}""",  # noqa: W291 - the "-- " signature line ends in a space
    )
    assert "> Body line 19" in prefilled("Reply", long)["body"]  # 20 lines after it: not a signature

    own = deliver_raw(
        "Sent by myself",
        f"""From: {me}
To: Bob <bob@example.net>
Cc: carol@example.net, {me}
Subject: Sent by myself
Content-Type: text/plain; charset=utf-8

Mine.""",
    )
    everyone = prefilled("ReplyAll", own)
    assert emails(everyone["to"]) == ["bob@example.net"] and emails(everyone["cc"]) == ["carol@example.net"], everyone

    # A colleague on the user's own domain, first in To, is not one of the user's identities: the reply goes out
    # from the user's address, as Roundcube's identity_select picks it.
    colleague = "colleague@" + me.split("@")[1]
    shared = deliver_raw(
        "To a colleague, copied to me",
        f"""From: Heidi <heidi@example.net>
To: {colleague}
Cc: {me}
Subject: To a colleague, copied to me
Content-Type: text/plain; charset=utf-8

For both of you.""",
    )
    everyone = prefilled("ReplyAll", shared)
    assert everyone["from"]["email"] == me and emails(everyone["cc"]) == [colleague], everyone


def forward_and_quote() -> None:
    # Roundcube's forward block (Date and To even when empty, Reply-To when it is not From, the body trimmed), its
    # quote shape (no leading empty lines, the caret line right below), and a reply dropping "(was: ...)".
    uid = deliver_raw(
        "Ticket seven (was: Earlier thread name)",
        """From: Alice <alice@example.net>
Reply-To: tickets@vendor.example
Subject: Ticket seven (was: Earlier thread name)
Content-Type: text/plain; charset=utf-8


Can you check the invoice?

Thanks

""",
    )
    forward = prefilled("Forward", uid)
    assert forward["subject"] == "Fwd: Ticket seven (was: Earlier thread name)", forward["subject"]
    body = forward["body"]
    assert body.startswith("\n\n\n-------- Original Message --------\nSubject: Ticket seven (was: Earlier thread name)\nDate: "), (
        body
    )
    assert "\nTo: " in body and "\nReply-To: tickets@vendor.example\n" in body, body
    assert body.endswith("\n\nCan you check the invoice?\n\nThanks"), repr(body)
    reply = prefilled("Reply", uid)
    assert reply["subject"] == "Re: Ticket seven", reply["subject"]
    assert reply["body"].split("wrote:\n", 1)[1] == "> Can you check the invoice?\n>\n> Thanks\n", repr(reply["body"])


def addresses_out() -> None:
    # Cc alone goes out as To, Bcc alone under "undisclosed-recipients:;"; an IDN domain is sent as punycode; a
    # letter beyond ASCII before the @ is refused with its reason.
    me = {"name": None, "email": tier.user}
    for subject, field, want in [("Cc only", "cc", f"To: <{tier.user}>"), ("Bcc only", "bcc", "To: undisclosed-recipients:;")]:
        draft = new_draft()
        draft.update(subject=subject, body="x", **{field: [me]})
        assert request("POST", "api/send", draft)[0] == 204, subject
        head = fetch("hdr", "Sent", arrived("Sent", subject))
        assert want in head and not re.search(r"(?im)^cc:", head), head
    draft = new_draft()
    draft.update(subject="To an IDN domain", to=[{"name": None, "email": "kontakt@müller.de"}])
    status, copy = request("POST", "api/draft", draft)
    assert status == 200, (status, copy)
    head = fetch("hdr", "Drafts", copy["uid"])
    assert "kontakt@xn--mller-kva.de" in head, head
    draft.update(to=[{"name": None, "email": "андрій@example.net"}])
    status, text = request("POST", "api/send", draft)
    assert status == 400 and "beyond ASCII before the @" in text["message"], (status, text)


def stale_sent_choice() -> None:
    # A Sent folder chosen in Settings and then deleted (by kubicle here, which leaves the choice as another mail
    # app would, without the 15 s poll that app's change waits for) leaves the server's own Sent in charge, and
    # Settings still save.
    me = {"name": None, "email": tier.user}
    assert request("POST", "api/change", {"CreateFolder": {"name": "Sent Items"}})[0] == 204
    before = settings_change(special_folders={"Sent": "Sent Items"})
    try:
        assert request("POST", "api/change", {"DeleteFolder": {"name": "Sent Items"}})[0] == 204
        assert not any(f["name"] == "Sent Items" for f in request("GET", "api/session")[1]["folders"])
        settings_change(page_size=before["page_size"])  # any save, with the stale choice still stored
        subject = "Filed after a stale choice"
        draft = new_draft()
        draft.update(subject=subject, body="x", to=[me])
        assert request("POST", "api/send", draft)[0] == 204
        arrived("Sent", subject)
    finally:
        settings_change(special_folders=before["special_folders"])


def flowed_send() -> None:
    # Sent text goes out wrapped at 72 as format=flowed and comes back whole when unfolded; Keep formatting sends
    # it as typed. The copy in Sent is the one that went out.
    import email
    from email import policy

    paragraph = " ".join(["Flowed words make a long paragraph typed on one line"] * 5) + " From the end."
    for subject, keep in [("Sent flowed", False), ("Sent as typed", True)]:
        draft = new_draft()
        draft.update(to=[{"name": None, "email": tier.user}], subject=subject, body=paragraph, keep_formatting=keep)
        status, answer = request("POST", "api/send", draft)
        assert status == 204, (status, answer)
        raw = fetch("text", "Sent", arrived("Sent", subject)).split("\n", 1)[1]
        part = email.message_from_string(raw, policy=policy.default)
        text = part.get_content().replace("\r\n", "\n").rstrip("\n")
        if keep:
            assert part.get_param("format") is None and text == paragraph, (part["content-type"], text)
        else:
            lines = text.split("\n")
            assert part.get_param("format") == "flowed" and all(len(line) <= 74 for line in lines), (part["content-type"], lines)
            assert all(line.endswith(" ") for line in lines[:-1]) and len(lines) > 3, lines  # soft breaks
            assert "".join(line[1:] if line.startswith(" ") else line for line in lines) == paragraph, lines  # stuffed "From"


def forwarded(uid: int) -> bool:
    return "$Forwarded" in fetch("flags", "INBOX", uid)


def forward_several() -> None:
    # Forward as attachment of two messages, as Roundcube's write_forward_attachments: each named after its subject,
    # a repeated name numbered, "Fwd: " and the first subject, References for both and In-Reply-To for neither. The
    # draft's X-Draft-Info names both as a UID set, as Roundcube writes it, so the draft opened again still flags both.
    # A subject no seeded message has: a search that matches thousands dies of SIGPIPE in `grep -q`.
    a = deliver_raw("Bicycle parts", "From: a@example.com\nSubject: Bicycle parts\nMessage-ID: <a1@example.com>\n\nA")
    b = deliver_raw("Bicycle parts", "From: b@example.com\nSubject: Bicycle parts\nMessage-ID: <b1@example.com>\n\nB")
    status, prefill = request(
        "POST", "api/compose", {"mode": "ForwardAttachment", "messages": [["INBOX", {"Uids": [a, b]}]], "zone": ""}
    )
    assert status == 200, prefill
    draft = prefill["draft"]
    assert [x["filename"] for x in draft["attachments"]] == ["Bicycle parts.eml", "Bicycle parts_2.eml"], draft["attachments"]
    assert draft["subject"] == "Fwd: Bicycle parts" and draft["in_reply_to"] is None, draft
    assert draft["references"] == ["a1@example.com", "b1@example.com"] and prefill["notice"] is None, prefill
    draft.update(to=[{"name": None, "email": tier.user}], body="Both attached.")
    status, saved = request("POST", "api/draft", draft)
    assert status == 200, (status, saved)
    reopened = prefilled("EditDraft", saved["uid"], "Drafts")
    assert [(x["folder"], x["uid"]) for x in reopened["sources"]] == [("INBOX", a), ("INBOX", b)], reopened["sources"]
    assert not forwarded(a) and not forwarded(b)
    assert request("POST", "api/send", reopened)[0] == 204
    machine.wait_until_succeeds(
        f"{tier.doveadm} fetch -u {tier.user} flags mailbox INBOX uid {a},{b} | grep -c Forwarded | grep -qx 2", timeout=30
    )


def forward_left_out() -> None:
    # Past the mail server's size limit a forwarded message is left out, and Compose says how many, in Roundcube's
    # words: 10 KB for the text, then each message whole. Two of 60% of the limit each: the second does not fit.
    size = tier.size_limit * 6 // 10 // 4 * 3  # base64 grows it by 4/3
    uids = []
    for n in [1, 2]:
        subject = f"Large message {n}"
        machine.succeed(
            f"{{ printf 'From: big@example.com\\nSubject: {subject}\\n\\n';"
            f" head -c {size} /dev/zero | base64 -w 76; }} > /tmp/big.eml"
        )
        machine.succeed(tier.deliver(subject, "--data @/tmp/big.eml"))
        uids.append(arrived("INBOX", subject, wait=60))
    body = {"mode": "ForwardAttachment", "messages": [["INBOX", {"Uids": uids[:2]}]], "zone": ""}
    status, prefill = request("POST", "api/compose", body)
    assert status == 200, prefill
    assert [x["filename"] for x in prefill["draft"]["attachments"]] == ["Large message 1.eml"], prefill["draft"]["attachments"]
    assert "exceeded. 1 message(s) have not been attached." in (prefill["notice"] or ""), prefill["notice"]


def send_checks() -> None:
    # Roundcube's checks at Send, in its order: the empty subject asked once with a field to type it in, the empty
    # body, then more than five public recipients, whose Use Bcc moves them all to Bcc.
    subject = "Six public recipients"
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('.compose input[aria-label=To]')")
        six = " ".join(f"public{n}@example.net" for n in range(5)) + f" {tier.user} "
        wd("type", ".compose input[aria-label=To]", six)
        wait(f"{CHIPS}.length === 6")
        wd("click", ".compose .button.primary")
        wait("document.querySelector('.dialog .ask input')")
        # Enter answers in the field, which goes with its question: the next one takes the focus to its first button.
        wd("type", ".dialog .ask input", subject + "")
        wait("document.querySelector('.dialog')?.textContent.includes('Send this message without text?')")
        wait("document.activeElement?.closest('.dialog-buttons') && document.activeElement.textContent.trim() === 'Send message'")
        dialog_button("Send this message without text?", "Cancel")
        wait("document.activeElement === document.querySelector('textarea.compose-body')")
        wd("type", "textarea.compose-body", "To six people.")
        wd("click", ".compose .button.primary")
        dialog_button("Too many public recipients", "Use Bcc")
        wait("document.querySelector('.toast')?.textContent === 'Message sent successfully.'")
    finally:
        js("document.querySelector('.dialog .dialog-head button')?.click()")  # a question left open covers Discard
        if js("return !!document.querySelector('.compose')"):
            discard()
    head = fetch("hdr", "Sent", arrived("Sent", subject))
    assert "To: undisclosed-recipients:;" in head and "public4@example.net" in head and not re.search(r"(?im)^cc:", head), head


def send_locks_compose() -> None:
    # While a send is on its way Compose is locked, as Roundcube's `set_busy` and `lock_form`: every field and tool
    # disabled and the corner on "Sending message...". A Save taken meanwhile leaves a copy in Drafts that the send
    # does not drop. The page holds api/send until the form has been tried.
    subject = "Locked while it sends"
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('.compose input[aria-label=Subject]')")
        wd("type", ".compose input[role=combobox]", tier.user)
        wd("type", ".compose input[aria-label=Subject]", subject)
        wd("type", "textarea.compose-body", "Sent while the form waits.")
        js(r"""const plain = window.fetch, hold = new Promise(r => window.kubicleRelease = r);
            window.fetch = (url, ...rest) => url === 'api/send' ? hold.then(() => plain(url, ...rest)) : plain(url, ...rest);""")
        wd("click", ".compose .button.primary")
        wait("document.querySelector('.compose-status')?.textContent === 'Sending message...'")
        js("document.querySelector(\".compose .tool[title='Save as draft now']\").click()")
        time.sleep(1)  # a save is answered by now
        state = js("""return {
            status: document.querySelector('.compose-status').textContent,
            buttons: [...document.querySelectorAll('.compose button')]
                .filter(b => !b.matches(':disabled') && b.getAttribute('aria-disabled') !== 'true')
                .map(b => b.title || b.textContent.trim()),
            fields: [...document.querySelectorAll('.compose :is(input, textarea, select)')]
                .filter(f => !f.matches(':disabled')).map(f => f.getAttribute('aria-label') || f.type),
        }""")
        js("window.kubicleRelease()")
        wait("document.querySelector('.toast')?.textContent === 'Message sent successfully.'")
        arrived("Sent", subject)
        time.sleep(2)  # a save's APPEND has landed by now
        state["drafts"] = drafts_with(subject)
        assert state == {"status": "Sending message...", "buttons": [], "fields": [], "drafts": 0}, state
    finally:
        js("window.kubicleRelease?.()")
        if js("return !!document.querySelector('.compose')"):
            discard()


def question_goes_with_compose() -> None:
    # A dialog belongs to the view it opened over, as Roundcube's to its page. A Forward answered late replaces the
    # Compose whose subject question is open: the question goes with it, and no answer reaches the Compose that is
    # gone, whose state is disposed (a wasm panic). The page holds the Forward's start until the question is up.
    subject = "Forwarded over a question"
    deliver_raw(subject, f"From: Max <max@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click(subject)
    js(r"""window.kubicleErrors = []; window.addEventListener('error', e => kubicleErrors.push(e.message));
        const plain = window.fetch, hold = new Promise(r => window.kubicleRelease = r);
        window.fetch = (url, init, ...rest) => url === 'api/compose' && /"mode":"Forward"/.test(init?.body)
            ? hold.then(() => plain(url, init, ...rest)) : plain(url, init, ...rest);""")
    try:
        tool("Forward")
        wd("click", ".compose-button")
        wait("!!document.querySelector('.compose textarea.compose-body')")
        wd("click", ".compose .button.primary")
        wait("document.querySelector('.dialog')?.textContent.includes('Would you like to enter one now?')")
        js("window.kubicleRelease()")
        wait(f"document.querySelector('.compose input[aria-label=Subject]')?.value === {json.dumps('Fwd: ' + subject)}")
        time.sleep(0.5)
        left = js("return document.querySelector('.dialog')?.textContent ?? null")
        js(
            "document.querySelector('.dialog')?.dispatchEvent(new MouseEvent('click', {bubbles: true, clientX: 1, clientY: 1}))"
        )  # outside the box: Cancel
        time.sleep(0.5)
        errors = js("return kubicleErrors")
        assert (left, errors) == (None, []), f"dialog left open: {left!r}; page errors: {errors}"
    finally:
        reload()  # a panicked client takes nothing more; the reload drops the held fetch too
        open_folder("Inbox")


def compose_fields() -> None:
    # Elastic's recipient input: an address and a separator make a chip, Backspace in the empty input takes the last
    # back, a pasted list splits on its new lines. The browser's spelling check runs on Subject and the text only.
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('.compose input[aria-label=To]')")
        wd("type", ".compose input[aria-label=To]", "Ann Lee <ann@example.net> bob@example.net,carl@example.net")
        wait(f"JSON.stringify({CHIPS}) === JSON.stringify(['Ann Lee <ann@example.net>', 'bob@example.net'])")
        assert js("return document.querySelector('.compose input[aria-label=To]').value") == "carl@example.net"
        wd("type", ".compose input[aria-label=To]", "\ue003" * len("carl@example.net"))  # Backspace
        wd("type", ".compose input[aria-label=To]", "\ue003")
        wait(f"JSON.stringify({CHIPS}) === JSON.stringify(['Ann Lee <ann@example.net>'])")
        spelling = js("""return ['Subject', 'From', 'To']
            .map(l => document.querySelector(`.compose input[aria-label=${l}]`).spellcheck)
            .concat(document.querySelector('textarea.compose-body').spellcheck)""")
        assert spelling == [True, False, False, True], spelling
    finally:
        discard()


def attachment_rename() -> None:
    # A file renamed in Compose goes out under its new name, and Compose's Open and Download serve the stored file:
    # an image inline, anything else as a download under the name the draft gives it.
    stored = json.loads(upload("new Blob(['%PDF-1.4 scan'], {type: 'application/pdf'})", "scan0001.pdf")[1])
    served = json.loads(
        wd(
            "async",
            f"""fetch('api/upload?id={stored["id"]}&name=invoice-march.pdf&open=true')
        .then(r => done([r.headers.get('content-type'), r.headers.get('content-disposition')]))""",
        )
    )
    assert served[0] == "application/octet-stream" and "invoice-march.pdf" in served[1], served
    subject = "Renamed attachment"
    draft = new_draft()
    stored["filename"] = "invoice-march.pdf"
    draft.update(to=[{"name": None, "email": tier.user}], subject=subject, body="See the file.", attachments=[stored])
    assert request("POST", "api/send", draft)[0] == 204
    body = fetch("text", "INBOX", arrived("INBOX", subject))
    assert "invoice-march.pdf" in body and "scan0001.pdf" not in body, body


def upload_cancel() -> None:
    # Roundcube's upload entry: the progress as the browser sends the file, "N% (x of y)", and Cancel, which stops
    # it and leaves no attachment. The upload is slowed to 64 KB/s so it is caught on its way.
    machine.succeed("head -c 2000000 /dev/zero > /tmp/slow.bin")
    wd("cdp", "Network.enable", "{}")
    slow = {"offline": False, "latency": 0, "downloadThroughput": -1, "uploadThroughput": 65536}
    wd("cdp", "Network.emulateNetworkConditions", json.dumps(slow))
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('.compose input[type=file]')")
        wd("type", ".compose input[type=file]", "/tmp/slow.bin")
        wait("/^[1-9]\\d*% \\(/.test(document.querySelector('.attachment-item.uploading .muted')?.textContent ?? '')")
        wd("click", ".attachment-item.uploading .icon-button")
        wait(
            "!document.querySelector('.attachment-item')"
            " && document.querySelector('.compose-status').textContent !== 'Uploading...'"
        )
        time.sleep(1)
        assert js("return document.querySelectorAll('.attachment-item').length") == 0
    finally:
        wd("cdp", "Network.emulateNetworkConditions", json.dumps({**slow, "uploadThroughput": -1}))
        discard()


def attach_small() -> str:
    """Attaches /tmp/small.txt in the open Compose and answers its upload's id, from the browser copy."""
    machine.succeed("echo 'A small file.' > /tmp/small.txt")
    wd("type", ".compose input[type=file]", "/tmp/small.txt")
    files = (
        "Object.keys(localStorage).filter(k => k.startsWith('kubicle.draft.'))"
        ".flatMap(k => JSON.parse(localStorage[k]).draft.attachments)"
    )
    return wait(f"{files}.find(a => a.filename === 'small.txt')?.id")


def upload_kept(upload_id: str) -> bool:
    return json.loads(wd("async", f"fetch('api/upload?id={upload_id}&name=x').then(r => done(r.status === 200))"))


def wait_upload_gone(upload_id: str) -> None:
    deadline = time.monotonic() + 30
    while upload_kept(upload_id):
        assert time.monotonic() < deadline, f"upload {upload_id} stays"
        time.sleep(0.5)


def uploads_freed() -> None:
    # A draft's uploads go once nothing needs them: on Remove, on Discard and once Compose is left with the draft saved
    # to Drafts, whose copy holds the file; never while a send on its way still reads them. The page holds the send
    # while Compose is left by a folder.
    subject = "Files freed once sent"
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('.compose input[type=file]')")
        removed = attach_small()
        wd("click", ".compose .attachment-item button[aria-label='Remove small.txt']")
        wait_upload_gone(removed)
        discarded = attach_small()
        discard()
        wait_upload_gone(discarded)

        wd("click", ".compose-button")
        wait("!!document.querySelector('.compose input[type=file]')")
        wd("type", ".compose input[aria-label=Subject]", "Files freed once saved")
        saved = attach_small()
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        wait("document.querySelector('.toast')?.textContent === 'Draft saved.'")
        wait_upload_gone(saved)
        assert "small.txt" in fetch("text", "Drafts", arrived("Drafts", "Files freed once saved"))

        wd("click", ".compose-button")
        wait("!!document.querySelector('.compose input[type=file]')")
        wd("type", ".compose input[role=combobox]", tier.user)
        wd("type", ".compose input[aria-label=Subject]", subject)
        wd("type", "textarea.compose-body", "Sent after Compose was left.")
        attach_small()
        js(r"""const plain = window.fetch, hold = new Promise(r => window.kubicleRelease = r);
            window.fetch = (url, ...rest) => url === 'api/send' ? hold.then(() => plain(url, ...rest)) : plain(url, ...rest);""")
        wd("click", ".compose .button.primary")
        wait("document.querySelector('.compose-status')?.textContent === 'Sending message...'")
        open_folder("Inbox")
        time.sleep(1)  # a drop of the uploads would be answered by now
        js("window.kubicleRelease()")
        wait("document.querySelector('.toast')?.textContent === 'Message sent successfully.'")
        assert "small.txt" in fetch("text", "Sent", arrived("Sent", subject))
    finally:
        js("window.kubicleRelease?.()")
        if js("return !!document.querySelector('.compose')"):
            discard()
        reload()  # without the held fetch


def sign_out_keeps_open_files() -> None:
    # Sign-out frees the uploads of this browser's drafts, but not those of a Compose open in another tab, which
    # carries on there after the sign-in.
    main = windows()[0]
    js("window.open('#compose=' + encodeURIComponent('mailto:bob@example.net?subject=Open%20elsewhere'), '_blank')")
    wait_windows(2)
    try:
        wd("window", [h for h in windows() if h != main][0])
        wait("!!document.querySelector('.compose input[type=file]')", 60)
        kept = attach_small()
        wd("window", main)
        wd("click", "button.tool.logout")
        wait("location.pathname !== '/login/'", 60)
        signed_in_again()
        assert upload_kept(kept), "sign-out freed the files of a Compose open in another tab"
    finally:
        wd("window", main)
        signed_in_again()


def windows() -> list:
    return json.loads(wd("windows"))


def close_tabs() -> None:
    """Closes every tab a scenario left open but the first, where the next one starts: a tab left open, as Print's,
    stays in front and hides the first, whose requestAnimationFrame callbacks then never run."""
    first, *others = windows()
    for handle in others:
        wd("window", handle)
        wd("close")
    wd("window", first)


def discard_window() -> None:
    """Discard in a compose window, which then closes: nothing waits on its page after the question."""
    wd("click", ".compose .tool[title='Discard this draft']")
    dialog_button("Discard this draft?", "Discard", then_wait=False)
    wait_windows(1)


def wait_windows(count: int) -> None:
    deadline = time.monotonic() + 30
    while len(windows()) != count:
        assert time.monotonic() < deadline, f"windows open: {windows()}, not {count}"
        time.sleep(0.2)


def compose_windows() -> None:
    # "Open in new window" hands the browser copy to a tab of its own, which goes on with it while this one goes back
    # to the mail, saving nothing to Drafts; "Compose in a new window" opens every Compose in a tab, which closes
    # itself once its Compose is closed, as Roundcube's popup window does.
    main = windows()[0]
    subject = "Carried to a window"
    wd("click", ".compose-button")
    wait("!!document.querySelector('.compose input[aria-label=Subject]')")
    wd("type", ".compose input[aria-label=Subject]", subject)
    wd("click", ".compose .extwin")
    wait("!document.querySelector('.compose')")
    opened = [h for h in windows() if h != main]
    assert len(opened) == 1, opened
    try:
        wd("window", opened[0])
        wait(f"document.querySelector('.compose input[aria-label=Subject]')?.value === {json.dumps(subject)}", 60)
        assert js("return location.hash").endswith("&window=1")
        assert search("Drafts", f"subject {shlex.quote(subject)}") == [], "handing the draft over saved it to Drafts"
        discard_window()
    finally:
        wd("window", main)
    before = settings_change(compose_extwin=True)
    try:
        reload()  # so the page has the setting
        wd("click", ".compose-button")
        wait_windows(2)
        assert not js("return !!document.querySelector('.compose')"), "Compose opened here, not in a new tab"
        wd("window", [h for h in windows() if h != main][0])
        wait("!!document.querySelector('.compose')", 60)
        wd("click", ".compose .tool[title^='Close']")
        wait_windows(1)
    finally:
        wd("window", main)
        settings_change(compose_extwin=before["compose_extwin"])
        reload()


def identity_fields() -> None:
    # An identity's Reply-To goes into a new message from it and its Organization goes out with it, as Roundcube's.
    domain = tier.user.split("@")[1]
    support = {"name": None, "email": f"support@{domain}"}
    before = settings_change(identities={tier.user: {"organization": "Kitty Ltd", "reply_to": [support], "bcc": []}})
    try:
        draft = new_draft()
        assert emails(draft["reply_to"]) == [support["email"]], draft["reply_to"]
        subject = "From an identity"
        draft.update(to=[{"name": None, "email": tier.user}], subject=subject, body="x")
        assert request("POST", "api/send", draft)[0] == 204
        head = fetch("hdr", "Sent", arrived("Sent", subject))
        assert "Organization: Kitty Ltd" in head and f"Reply-To: <support@{domain}>" in head, head
    finally:
        settings_change(identities=before["identities"])


def composing_settings() -> None:
    # Roundcube's Composing switches: a return receipt and a delivery status notification asked for by default, a
    # reply filed in the original's folder, and the reply above the original without quoting it.
    before = settings_change(mdn_default=True, dsn_default=True, reply_same_folder=True, reply_mode="AboveNoIndent")
    try:
        uid = deliver_raw("Filed beside it", f"From: Bob <bob@example.com>\nTo: {tier.user}\nSubject: Filed beside it\n\nHi there")
        draft = prefilled("Reply", uid)
        assert draft["return_receipt"] and draft["delivery_status"], draft
        assert draft["sent_copy"] == {"Folder": "INBOX"}, draft["sent_copy"]
        body = draft["body"]
        assert body.startswith("\n\n\n\n\n\n-------- Original Message --------\nSubject: Filed beside it\n"), repr(body)
        assert body.endswith("\n\nHi there") and "> " not in body and "Cc:" not in body, repr(body)
    finally:
        settings_change(**{k: before[k] for k in ["mdn_default", "dsn_default", "reply_same_folder", "reply_mode"]})


def mailto_fields() -> None:
    # A mailto: link's Bcc, Reply-To and In-Reply-To reach the draft, as Roundcube's compose takes them, beside the
    # identity's own Bcc, which a link's `bcc=` once replaced. Opened by a script in a tab of its own, it is a compose
    # window: Discard closes it.
    main = windows()[0]
    boss = {"name": None, "email": "boss@example.net"}
    before = settings_change(identities={tier.user: {"organization": "", "reply_to": [], "bcc": [boss]}})
    uri = (
        "mailto:bob@example.net?subject=From%20a%20link&reply-to=help%40example.net&bcc=carol%40example.net"
        "&in-reply-to=%3Clinked%40example.net%3E"
    )
    js(f"window.open('#compose=' + encodeURIComponent({json.dumps(uri)}), '_blank')")
    wait_windows(2)
    try:
        wd("window", [h for h in windows() if h != main][0])
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === 'From a link'", 60)
        chips = js(f"return {CHIPS}")
        assert sorted(chips) == ["bob@example.net", "boss@example.net", "carol@example.net", "help@example.net"], chips
        wd("click", ".compose .tool[title='Save as draft now']")
        wait("document.querySelector('.compose-status').textContent === 'Draft saved'")
        head = fetch("hdr", "Drafts", arrived("Drafts", "From a link"))
        assert "In-Reply-To: <linked@example.net>" in head and "Reply-To: <help@example.net>" in head, head
        bcc = re.search(r"(?im)^bcc:(.*)$", head)
        assert bcc and "boss@example.net" in bcc.group(1) and "carol@example.net" in bcc.group(1), head
        discard_window()
    finally:
        wd("window", main)
        settings_change(identities=before["identities"])


def about() -> None:
    # Roundcube's About, from the folder actions menu: the name, the licence and Mailu's own pages.
    wd("click", "button[aria-label='Folder actions']")
    js("[...document.querySelectorAll('.menu .menu-item')].find(b => b.textContent.trim() === 'About').click()")
    wait(
        "document.querySelector('.dialog')?.textContent.includes('kubicle')"
        " && document.querySelector('.dialog')?.textContent.includes('AGPL-3.0')"
    )
    assert js("return [...document.querySelectorAll('.dialog-buttons button')].map(b => b.textContent.trim())") == [
        "Get support",
        "Close",
    ]
    assert request("GET", "api/session")[1]["admin_url"] == "/admin"
    dialog_button("kubicle", "Close")


def row_click(subject: str) -> None:
    wait(f"{SUBJECTS}.includes({json.dumps(subject)})", 60)
    js(f"""[...document.querySelectorAll('.row')]
        .find(r => r.querySelector('.row-subject').textContent === {json.dumps(subject)}).click()""")
    wait(f"document.querySelector('.message-subject')?.textContent === {json.dumps(subject)}")


def tool(label: str) -> None:
    js(f"[...document.querySelectorAll('.toolbar button.tool')].find(b => b.textContent.trim() === {json.dumps(label)}).click()")


def discard() -> None:
    wd("click", ".compose .tool[title='Discard this draft']")
    wait("!document.querySelector('.compose') || document.querySelector('.dialog .button.primary')")
    if js("return !!document.querySelector('.dialog .button.primary')"):
        wd("click", ".dialog .button.primary")
    wait("!document.querySelector('.compose')")


# The recipient chips of Compose's address fields, each as "Name <address>" or the address alone.
CHIPS = "[...document.querySelectorAll('.compose .recipient')].map(c => c.title || c.textContent.trim())"


def dialog_button(text: str, button: str, then_wait: bool = True) -> None:
    """Waits for the dialog that says `text` and presses its button labelled `button`, then waits for it to go: the
    next question may take its place at once."""
    says = f"document.querySelector('.dialog')?.textContent.includes({json.dumps(text)})"
    wait(says)
    js(f"""[...document.querySelectorAll('.dialog-buttons button')]
        .find(b => b.textContent.trim() === {json.dumps(button)}).click()""")
    if then_wait:
        wait(f"!({says})")


def open_folder(name: str) -> None:
    js(
        "[...document.querySelectorAll('.folder')]"
        f".find(f => f.querySelector('.folder-name').textContent === {json.dumps(name)}).click()"
    )
    wait("!!document.querySelector('.list')")


def compose_actions() -> None:
    # The Reply all button writes to the list on list mail, as Mailu's reply_all_mode = 1; a reply turns its
    # original read as Compose opens; Discard moves a draft opened from Drafts to Trash. Back in Inbox however it
    # ends, for the scenarios after.
    try:
        compose_steps()
    finally:
        if js("return !!document.querySelector('.compose')"):
            discard()
        open_folder("Inbox")


def compose_steps() -> None:
    deliver_raw(
        "Posted to a list",
        """From: Dave <dave@example.com>
To: list@lists.example.com
Cc: erin@example.com
List-Post: <mailto:list@lists.example.com>
Subject: Posted to a list
Content-Type: text/plain; charset=utf-8

To everyone on the list.""",
    )
    row_click("Posted to a list")
    tool("Reply all")
    wait(f"{CHIPS}.includes('list@lists.example.com')")
    assert not any("dave@" in c for c in js(f"return {CHIPS}")), js(f"return {CHIPS}")
    assert not js("return !!document.querySelector('.compose input[aria-label=Cc]')"), "Reply list has no Cc"
    discard()
    # "Default action of [Reply all] button" set to reply to all: the same button answers everyone.
    before = settings_change(reply_all_list=False)
    try:
        reload()  # so the page has the setting
        row_click("Posted to a list")
        tool("Reply all")
        wait(f"{CHIPS}.includes('Dave <dave@example.com>') && {CHIPS}.includes('erin@example.com')")
        discard()
    finally:
        settings_change(reply_all_list=before["reply_all_list"])

    before = settings_change(mark_read_after=None)
    try:
        subject = "Unread until answered"
        uid = deliver_raw(subject, f"From: Frank <frank@example.com>\nTo: {tier.user}\nSubject: {subject}\n\nAnswer me.")
        row_click(subject)
        flags = f"{tier.doveadm} fetch -u {tier.user} flags mailbox INBOX uid {uid}"
        assert "\\Seen" not in fetch("flags", "INBOX", uid), "opened with mark-read off, yet read"
        tool("Reply")
        wait("!!document.querySelector('.compose')")
        machine.wait_until_succeeds(f"{flags} | grep -qF '\\Seen'", timeout=30)
        discard()
    finally:
        settings_change(mark_read_after=before["mark_read_after"])

    # No subject: Roundcube asks once, so Cancel lets the next Send through to the recipient check, whose alert
    # gives To the focus. A new message starts in To.
    wd("click", ".compose-button")
    wait("document.activeElement === document.querySelector('.compose input[role=combobox]')")
    wd("type", "textarea.compose-body", "Nobody to send this to.")
    wd("click", ".compose .button.primary")
    dialog_button("Would you like to enter one now?", "Cancel")
    wait("document.activeElement === document.querySelector('.compose input[aria-label=Subject]')")
    wd("click", ".compose .button.primary")
    dialog_button("Please enter at least one recipient.", "OK")
    wait("document.activeElement === document.querySelector('.compose input[role=combobox]')")
    discard()

    subject = "Opened then discarded"
    draft = new_draft()
    draft.update(subject=subject, body="Kept for later.", to=[{"name": None, "email": "someone@example.net"}])
    status, uid = request("POST", "api/draft", draft)
    assert status == 200, (status, uid)
    open_folder("Drafts")
    row_click(subject)
    tool("Edit")
    wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))
    wait("document.activeElement === document.querySelector('textarea.compose-body')")  # To and Subject are filled
    discard()
    arrived("Trash", subject)
    assert search("Drafts", f"subject {shlex.quote(subject)}") == [], "the opened draft stayed in Drafts"


def arrow_burst() -> None:
    # Arrow keys in a quick run show each message at once but mark read only where they stop, as Roundcube's
    # 400 ms preview delay leaves the rows passed unread.
    subjects = [f"Burst {word}" for word in ["alpha", "bravo", "charlie", "delta", "echo"]]
    uids = {s: deliver_raw(s, f"From: Ivan <ivan@example.net>\nTo: {tier.user}\nSubject: {s}\n\n{s}.") for s in subjects}
    wait(f"{json.dumps(subjects)}.every(s => {SUBJECTS}.includes(s))", 60)
    shown = [s for s in js(f"return {SUBJECTS}") if s in uids]
    row_click(shown[0])  # its body is fetched now, so the click below shows it at once
    # The click and the keys run in one script, so the first key comes within SETTLE_MS of the click. As two WebDriver
    # calls on a loaded VM they came 430 ms apart, and the first row passed was rightly marked as shown after a pause.
    js(
        row_of(shown[0])
        + """.click();
        const keys = n => { if (n) { document.dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowDown', bubbles: true}));
        setTimeout(() => keys(n - 1), 40); } }; setTimeout(() => keys(4), 40);"""
    )
    wait(f"document.querySelector('.message-subject')?.textContent === {json.dumps(shown[4])}")
    time.sleep(1.5)
    seen = {s: "\\Seen" in fetch("flags", "INBOX", uids[s]) for s in shown}
    assert [seen[s] for s in shown] == [True, False, False, False, True], seen


def delete_key(target: str = "document") -> None:
    """Delete pressed with the focus on `target`, a script expression; the app's keys listen on the window."""
    js(f"{target}.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Delete', bubbles: true}}))")


def no_stale_page() -> None:
    # A delete takes the row off at once, before the server moved it. A page asked meanwhile, here by opening another
    # folder and coming back while the MOVE is on its way, still holds the message and must not bring it back.
    subject = "Deleted while its page loads"
    deliver_raw(subject, f"From: Lena <lena@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click(subject)
    # One script: the row goes in the first redraw after it, which the watch already sees.
    js(f"""document.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Delete', bubbles: true}}));
        for (const name of ['finance', 'Inbox'])
            [...document.querySelectorAll('.folder')].find(f => f.querySelector('.folder-name').textContent === name).click();
        window.kubicleBack = false;
        new MutationObserver(() => window.kubicleBack ||= {SUBJECTS}.includes({json.dumps(subject)}))
            .observe(document.querySelector('.list'), {{childList: true, subtree: true}});""")
    wait("document.querySelector('.toast')?.textContent === 'Message(s) moved to Trash successfully.'")
    wait("location.hash === '#folder=INBOX' && document.querySelectorAll('.row').length === 50")
    time.sleep(1)  # the event after the move reloads the page once more
    assert not js("return window.kubicleBack"), "a page asked before the move was answered showed the message again"


def list_waits_for_move() -> None:
    # A page asked while a move the server took is still on its way, as by a tab reloaded meanwhile, which knows
    # nothing of the move, must not list what it moves: the server answers it once the move is applied.
    subject = "Moved while a reloaded tab lists"
    uid = deliver_raw(subject, f"From: Rita <rita@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    wait(f"{SUBJECTS}.includes({json.dumps(subject)})", 60)
    body = json.dumps({"Move": {"folder": "INBOX", "messages": {"Uids": [uid]}, "to": "Trash"}})
    machine.succeed("pkill -STOP -x imap")
    try:
        js(f"""fetch('api/session').then(r => r.json()).then(s => fetch('api/change', {{method: 'POST',
            headers: {{'content-type': 'application/json', 'x-kubicle-csrf': s.csrf}}, body: {json.dumps(body)}}}));""")
        time.sleep(1)  # the move is the engine's job now, held at Dovecot
        js("""window.kubicleListed = fetch('api/list?folder=INBOX&page=0&size=50&sort=Unsorted&order=Descending&threads=false')
            .then(r => r.json()).then(p => p.rows.map(r => r.row.uid));""")
        time.sleep(1)
    finally:
        machine.succeed("pkill -CONT -x imap")
    listed = json.loads(wd("async", "window.kubicleListed.then(done)"))
    assert uid not in listed, f"the list asked during the move still holds UID {uid}"


def forward_then_delete() -> None:
    # Forward, then Delete before Compose opened: the move waits for the Compose start, so the server reads the
    # message first, as the user asked, and Compose opens on it. kubicle's IMAP connection is held still meanwhile,
    # as a long body search holds it, so the two requests would otherwise reach the server together. Roundcube's
    # busy notice shows the move was taken while it waits.
    subject = "Forwarded, then deleted"
    deliver_raw(subject, f"From: Max <max@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click(subject)
    js("""window.kubicleSent = []; const fetch = window.fetch;
        window.fetch = (url, init) => {
            kubicleSent.push(`${init?.method ?? 'GET'} ${url}`);
            return fetch.call(window, url, init);
        };""")
    machine.succeed("pkill -STOP -x imap")
    try:
        tool("Forward")
        delete_key()
        time.sleep(1)
        sent = js("return kubicleSent")
        assert "POST api/compose" in sent, sent
        assert "POST api/change" not in sent, f"the move left before the Compose start was answered: {sent}"
        toast = js("return document.querySelector('.toast')?.textContent ?? null")
        assert toast == "Moving message(s)...", f"the waiting move showed {toast!r}, so the click looked dead"
    finally:
        machine.succeed("pkill -CONT -x imap")
    wait(f"document.querySelector('.compose input[aria-label=Subject]')?.value === {json.dumps('Fwd: ' + subject)}")
    discard()
    found = f"{tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Trash subject {shlex.quote(subject)}"
    machine.wait_until_succeeds(f"{found} | grep -q .", timeout=30)


def delete_keeps_focus() -> None:
    # Delete from the toolbar on the one message of a search leaves nothing open, so the toolbar turns disabled; the
    # focus it held goes to the list, not to the page.
    subject = "Alone in its search"
    deliver_raw(subject, f"From: Ola <ola@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    wd("type", "#search", subject)
    wait(f"{SUBJECTS}.length === 1 && {SUBJECTS}[0] === {json.dumps(subject)}")
    row_click(subject)
    button = "[...document.querySelectorAll('.toolbar button.tool')].find(b => b.textContent.trim() === 'Delete')"
    js(f"{button}.focus()")
    delete_key(button)
    try:
        wait("document.querySelector('.toast')?.textContent === 'Message(s) moved to Trash successfully.'")
        wait("document.activeElement === document.querySelector('.list')", 5)
    finally:
        js("document.querySelector('.list-search .reset')?.click()")


def last_page_keeps_focus() -> None:
    # "Last page" turns disabled once its page shows, or while it loads; the focus it held goes to the list, not to
    # the page.
    open_folder("Inbox")
    button = "document.querySelector('.paging button[aria-label=\"Last page\"]')"
    wait(f"!{button}.disabled")
    js(f"{button}.focus(); {button}.click()")
    try:
        wait(f"{button}.disabled && document.activeElement === document.querySelector('.list')", 10)
    finally:
        js("document.querySelector('.paging button[aria-label=\"First page\"]').click()")


def search_delete_stays_off() -> None:
    # Delete on a search over several folders takes the row off at once. Nothing asked before the move, such as the
    # search's answer from before it, may bring it back while pages turn; Reply after it answers.
    open_folder("Inbox")
    wd("type", "#search", "subject:flake")
    paging = "document.querySelector('.paging .muted')?.title"
    wait(
        f"""[...document.querySelectorAll('.row-folder')].some(f => f.textContent === 'INBOX.github')
        && {paging}?.startsWith('Messages 1 to 50 of')
        && !document.querySelector('.paging button[aria-label="Next page"]').disabled""",
        60,
    )
    rows = "[...document.querySelectorAll('.row')].filter(r => r.querySelector('.row-folder')?.textContent === 'INBOX.github')"
    target = js(f"return {rows}[0].id")
    js(f"{rows}.find(r => r.id === {json.dumps(target)}).click()")
    wait(f"document.querySelector('.list').getAttribute('aria-activedescendant') === {json.dumps(target)}")
    time.sleep(1)  # its mark as read is answered and the search ran again for it
    js(f"""window.kubicleBack = false; window.kubicleCompose = [];
        new MutationObserver(() => window.kubicleBack ||= {rows}.some(r => r.id === {json.dumps(target)}))
            .observe(document.querySelector('.list'), {{childList: true, subtree: true}});
        window.kubicleFetches = [];
        const fetch = window.fetch;
        window.fetch = (url, init) => {{
            const seen = [Math.round(performance.now()), String(url), String(init?.body ?? '').match(/"page":\\d+/)?.[0]];
            kubicleFetches.push(seen);
            return fetch.call(window, url, init).then(r => {{
                if (String(url).startsWith('api/compose')) kubicleCompose.push(r.status);
                seen.push(r.status);
                return r;
            }}, e => {{ seen.push(e.name); throw e; }});
        }};""")
    delete_key()
    try:
        wait("document.querySelector('.toast')?.textContent === 'Message(s) moved to Trash successfully.'")
        time.sleep(1)  # the move's event runs the search again
        back = f"{target} of INBOX.github showed again after its move"
        assert not js("return window.kubicleBack"), back

        def turn(button: str, first: int) -> None:
            pager = f"document.querySelector('.paging button[aria-label=\"{button}\"]')"
            wait(f"!{pager}.disabled", 60)
            js(f"kubicleFetches = []; {pager}.click()")
            stays(first, 30)

        def stays(first: int, seconds: int) -> None:
            try:
                wait(f"{paging}?.startsWith('Messages {first} to')", seconds)
            except Exception as e:
                raise AssertionError(
                    f"not on 'Messages {first} to' but {js(f'return {paging}')!r}; requests since: {js('return kubicleFetches')}"
                ) from e

        turn("Next page", 51)
        # A change on the second page runs the search again for it, on the same page, as the folder's list keeps it.
        js("kubicleFetches = []; document.querySelector('.row').click()")
        wait("!!document.querySelector('.message-subject')")
        js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 's', bubbles: true}))")
        time.sleep(3)  # the flag is answered and its event runs the search again
        stays(51, 1)
        js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 's', bubbles: true}))")  # unflagged again
        time.sleep(1)
        turn("First page", 1)
        time.sleep(1)
        assert not js("return window.kubicleBack"), back
        js("document.querySelector('.row').click()")
        wait("!!document.querySelector('.message-subject')")
        tool("Reply")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value.startsWith('Re: ')")
        assert js("return kubicleCompose") == [200], js("return kubicleCompose")
        discard()
    finally:
        js("document.querySelector('.list-search .reset')?.click()")


def trust_keeps_focus() -> None:
    # "Always for" a sender draws the message again with its images, which takes away the button that held the
    # focus. The list takes it as the button goes, not a frame later: a key pressed meanwhile found the page body.
    # The page's own observer, made after the app's, reads the focus in the same turn as the removal.
    subject = "Remote image from a sender not yet trusted"
    deliver_raw(
        subject,
        f"""From: Ivan <ivan@example.net>
To: {tier.user}
Subject: {subject}
Content-Type: text/html; charset=utf-8

<p>Look: <img src="https://images.example.com/pixel.png"></p>""",
    )
    before = settings_change(remote_images="Trusted")
    try:
        reload()
        open_folder("Inbox")
        row_click(subject)
        trust = "document.querySelector('.notice.images .trust')"
        wait(f"!!{trust}")
        js(f"""const button = {trust};
            window.focusAfter = undefined;
            new MutationObserver((_, observer) => {{
                if (button.isConnected) return;
                const active = document.activeElement;
                window.focusAfter = !active || active === document.body ? 'body' : active.className;
                observer.disconnect();
            }}).observe(document.body, {{childList: true, subtree: true}});
            button.focus();
            button.click();""")
        wait("window.focusAfter !== undefined && !document.querySelector('.notice.images')")
        time.sleep(1)
        focus, later = js(
            "const a = document.activeElement; return [window.focusAfter, a === document.body ? 'body' : a.className]"
        )
        assert focus == "list", f"the focus went to {focus!r} as the trust button went, and was on {later!r} a second later"
    finally:
        settings_change(trusted_senders=before["trusted_senders"], remote_images=before["remote_images"])


# The remote images notice and every part of it that ends past the screen's edge, or whose text spills out of it.
OUTSIDE = """const n = document.querySelector('.notice.images'), edge = document.documentElement.clientWidth;
    return [n, ...n.children].filter(e => e.getBoundingClientRect().right > edge + 0.5 || e.scrollWidth > e.clientWidth)
        .map(e => `${e.tagName.toLowerCase()}.${e.getAttribute('class') ?? ''}: ${e.textContent.trim()}`)"""
# Whether the notice's sentence starts on the icon's row, rather than wrapping below it.
BESIDE_ICON = """const n = document.querySelector('.notice.images'), icon = n.querySelector('.icon').getBoundingClientRect();
    return n.querySelector(':scope > span').getBoundingClientRect().top < icon.bottom"""


def images_notice_fits_a_phone() -> None:
    # On a phone the notice wraps: "Always for" takes its own line, its address's local part giving way only when that
    # line is too narrow too, and the sentence for a trusted sender's unverified mail wraps beside the icon, its Show
    # still on screen. Held on one line, both ran past the screen and the button's text past the button.
    mails = {
        "Remote image from a long address": "Example <no-reply-member-login@mail.example-corp.com>",
        "Remote image from a trusted sender, unverified": "Ivan <ivan@example.net>",
    }
    for subject, sender in mails.items():
        deliver_raw(
            subject,
            f"""From: {sender}
To: {tier.user}
Subject: {subject}
Content-Type: text/html; charset=utf-8

<p>Look: <img src="https://images.example.com/pixel.png"></p>""",
        )
    before = settings_change(remote_images="Trusted", trusted_senders=["ivan@example.net"])
    try:
        metrics = {"width": 360, "height": 800, "deviceScaleFactor": 1, "mobile": True}
        wd("cdp", "Emulation.setDeviceMetricsOverride", json.dumps(metrics))
        reload()
        for subject in mails:
            open_folder("Inbox")
            row_click(subject)
            wait("!!document.querySelector('.notice.images')")
            outside = js(OUTSIDE)
            assert not outside, f"{subject}: past the screen or spilling: {outside}"
            assert js(BESIDE_ICON), f"{subject}: the sentence went below the icon"
    finally:
        wd("cdp", "Emulation.clearDeviceMetricsOverride", "{}")
        settings_change(trusted_senders=before["trusted_senders"], remote_images=before["remote_images"])
        reload()


# A box's bottom, and the lowest edge of anything visible inside it (its own top when it holds only text).
BOTTOM_EDGES = """const box = document.querySelector(%s), r = box.getBoundingClientRect();
    const inside = [...box.querySelectorAll('button, input, select, label, span, svg')].filter(e => e.getClientRects().length)
        .map(e => e.getBoundingClientRect().bottom);
    return {top: r.top, bottom: r.bottom, content: Math.max(r.top, ...inside), screen: innerHeight}"""


def bottom_bars_clear_the_gesture_bar() -> None:
    # Chrome on a phone draws a page with viewport-fit=cover under its gesture bar, whose height it gives as
    # safe-area-inset-bottom. Each bar at the bottom then reaches the screen's edge with its buttons above the gesture
    # bar, and Compose's last option, a toast and the floating Compose sit above it too. Before, all sat in it.
    inset = 48  # dp, Android's three-button bar: the gesture bar's 24 equals a toast's own 1.5rem and hides one in it
    wrong: list[str] = []

    def bar(state: str, selector: str) -> dict:
        m = js(BOTTOM_EDGES % json.dumps(selector))
        if abs(m["bottom"] - m["screen"]) > 0.5 or m["content"] > m["screen"] - inset + 0.5:
            wrong.append(f"{state} {selector}: {m}")
        return m

    subject = "Bottom bars on a phone"
    deliver_raw(subject, f"From: Pat <pat@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nA line.")
    phone = {"width": 360, "height": 800, "deviceScaleFactor": 1, "mobile": True}
    wd("cdp", "Emulation.setDeviceMetricsOverride", json.dumps(phone))
    wd("cdp", "Emulation.setSafeAreaInsetsOverride", json.dumps({"insets": {"bottom": inset, "bottomMax": inset}}))
    try:
        reload()
        open_folder("Inbox")
        paging = bar("list", ".paging")
        fab = js(BOTTOM_EDGES % json.dumps(".fab"))
        if fab["bottom"] > paging["top"] - 14 + 0.5:  # Elastic's 14 px above the footer (base.css, .fab)
            wrong.append(f"list .fab: {fab}, the paging bar's top at {paging['top']}")
        js("document.querySelector('.folders-button').click()")
        wait("document.querySelector('.app').dataset.pane === 'folders'")
        bar("folders", ".folders-bottom")
        open_folder("Inbox")
        row_click(subject)
        bar("reader", ".reader-nav")
        js("""const t = document.createElement('div'); t.className = 'toast'; t.popover = 'manual';
            t.textContent = 'A toast as main.rs shows one'; document.body.append(t); t.showPopover()""")
        toast = js(BOTTOM_EDGES % json.dumps(".toast"))
        if toast["bottom"] > toast["screen"] - inset + 0.5:
            wrong.append(f"toast: {toast}")
        js("document.querySelector('.toast').remove()")
        js("[...document.querySelectorAll('.reader-nav button')].find(b => b.textContent.trim() === 'Reply').click()")
        wait("!!document.querySelector('.compose-side')")
        js("const m = document.querySelector('.compose-main'); m.scrollTop = m.scrollHeight")
        bar("compose, scrolled to its end", ".compose-side")
        js("document.querySelector(\".compose .tool[title='Discard this draft']\").click()")
        wait("!document.querySelector('.compose') || document.querySelector('.dialog .button.primary')")
        if js("return !!document.querySelector('.dialog .button.primary')"):
            js("document.querySelector('.dialog .button.primary').click()")
        wait("!document.querySelector('.compose')")
        assert not wrong, "in the gesture bar or short of the screen's edge:\n" + "\n".join(wrong)
    finally:
        wd("cdp", "Emulation.setSafeAreaInsetsOverride", json.dumps({"insets": {"bottom": 0, "bottomMax": 0}}))
        wd("cdp", "Emulation.clearDeviceMetricsOverride", "{}")
        reload()


def installable() -> None:
    # Chrome fetches the web app manifest without cookies unless its link asks for them, and front answers a request
    # without Mailu's session with its sign-in page, which Chrome then parsed as the manifest and would not install.
    reload()
    errors = json.loads(wd("cdp", "Page.getInstallabilityErrors", "{}"))["installabilityErrors"]
    assert not errors, [e["errorId"] for e in errors]


def folder_column_arrows() -> None:
    # From 769 to 1200 px the Folders button shows the folder column in the list's place. An arrow key from the page
    # then goes to the folders, where the next one moves between them, and the hidden list's open message stays.
    open_folder("Inbox")
    metrics = {"width": 1024, "height": 768, "deviceScaleFactor": 1, "mobile": False}
    wd("cdp", "Emulation.setDeviceMetricsOverride", json.dumps(metrics))
    try:
        wait("getComputedStyle(document.querySelector('.folders-button')).display !== 'none'")
        js("document.querySelector('.folders-button').click()")
        wait("document.querySelector('.app').dataset.pane === 'folders'")
        before = js("return document.querySelector('.row.open')?.id ?? null")
        js("document.activeElement.blur()")
        for key in ["ArrowUp", "ArrowDown"]:
            js(f"document.activeElement.dispatchEvent(new KeyboardEvent('keydown', {{key: '{key}', bubbles: true}}))")
            time.sleep(0.2)
        after = js("return [document.querySelector('.row.open')?.id ?? null, document.activeElement.classList.contains('folder')]")
        assert after == [before, True], (before, after)
    finally:
        wd("cdp", "Emulation.clearDeviceMetricsOverride", "{}")
        open_folder("Inbox")


def discard_keeps_focus() -> None:
    # Discard's question answered closes Compose a task after the dialog, and the focus goes to the list in that
    # same task: a key pressed next finds it there, not on the page.
    open_folder("Inbox")
    wd("click", ".compose-button")
    wait("!!document.querySelector('.compose input[aria-label=Subject]')")
    wd("type", ".compose input[aria-label=Subject]", "Discarded at once")
    wd("click", ".compose .tool[title='Discard this draft']")
    wait("!!document.querySelector('.dialog .button.primary')")
    focus = json.loads(
        wd(
            "async",
            """const yes = document.querySelector('.dialog .button.primary');
        yes.focus(); yes.click();
        const at = () => [!document.querySelector('.compose'), document.activeElement.matches('.list'),
          document.activeElement.className];
        setTimeout(() => done(at()), 0);""",
        )
    )
    wait("!document.querySelector('.compose')")
    assert focus[:2] == [True, True], focus


def remote_images() -> None:
    # Allow remote resources: a trusted sender's images load only when the From domain vouched for the mail, which
    # this mail, with no Authentication-Results of the server's, cannot; Always loads them for anyone.
    subject = "Remote image from a trusted sender"
    uid = deliver_raw(
        subject,
        f"""From: Grace <grace@example.com>
To: {tier.user}
Subject: {subject}
Content-Type: text/html; charset=utf-8

<p>Look: <img src="https://images.example.com/pixel.png"></p>""",
    )
    before = settings_change(trusted_senders=["grace@example.com"], remote_images="Trusted")
    try:

        def blocked() -> bool:
            status, message = request("GET", f"api/message?folder=INBOX&uid={uid}")
            assert status == 200, (status, message)
            return message["body"]["remote_blocked"]

        assert blocked(), "a trusted sender's images loaded on an unverified From"
        settings_change(remote_images="Always")
        assert not blocked(), "Always left the images out"
        settings_change(remote_images="Never")
        assert blocked(), "Never loaded them"
    finally:
        settings_change(trusted_senders=before["trusted_senders"], remote_images=before["remote_images"])


def imported_verdict() -> None:
    # An import stores the file's mail as it is, so a verdict planted with the server's authserv-id once read as the
    # server's own: each Authentication-Results is renamed X-Imported-Authentication-Results on the way in.
    subject = "An imported verdict"
    eml = (
        "Authentication-Results: mail.example.test; dkim=pass header.d=paypal.com; dmarc=pass header.from=paypal.com\r\n"
        f"From: PayPal <service@paypal.com>\r\nTo: {tier.user}\r\nSubject: {subject}\r\n\r\nText.\r\n"
    )
    status, answer = upload(f"new Blob([{json.dumps(eml)}])", "verdict.eml", "api/import?folder=INBOX")
    assert status == 200, (status, answer)
    uid = arrived("INBOX", subject)
    status, message = request("GET", f"api/message?folder=INBOX&uid={uid}")
    assert status == 200 and message["authenticity"] is None, message.get("authenticity")
    head = fetch("hdr", "INBOX", uid)
    assert "X-Imported-Authentication-Results: mail.example.test" in head, head


def authentication_results() -> None:
    # The reader trusts the topmost Authentication-Results with the server's authserv-id (KUBICLE_AUTHSERV_ID in
    # mailu.nix): the mail server adds its own above every header the mail came with, so a lower one with that id
    # is the sender's, and one above it with another id is not the server's. A `;` inside a quoted string or a
    # comment, nested ones too, separates nothing.
    server = "mail.example.test"

    def verdict(subject: str, results: list[str], sender: str) -> dict:
        head = "".join(f"Authentication-Results: {r}\n" for r in results)
        uid = deliver_raw(subject, f"{head}From: {sender}\nTo: {tier.user}\nSubject: {subject}\n\nText.")
        status, message = request("GET", f"api/message?folder=INBOX&uid={uid}")
        assert status == 200, (status, message)
        return message["authenticity"]

    found = {
        "planted": verdict(
            "A verdict planted below the server's",
            [
                "elsewhere.example; dmarc=pass header.from=paypal.com",
                f"{server}; dkim=none; spf=fail smtp.mailfrom=evil.example; dmarc=fail header.from=paypal.com (policy=reject)",
                f"{server}; dkim=pass header.d=paypal.com; spf=pass smtp.mailfrom=paypal.com; dmarc=pass header.from=paypal.com",
            ],
            "PayPal <service@paypal.com>",
        ),
        "quoted": verdict(
            "A semicolon inside a quoted local part",
            [f'{server}; spf=pass smtp.mailfrom="x;dkim=pass header.d=example.org x"@evil.example; dkim=none'],
            "Support <support@example.org>",
        ),
        "other domain": verdict(
            "A DMARC pass for another domain",
            [f"{server}; dkim=none; dmarc=pass header.from=evil.example"],
            "PayPal <service@paypal.com>",
        ),
        # rspamd quotes the envelope sender in its SPF comment and value, escaping only `"`: a `)` in the comment once
        # ended the quote, and the dkim=pass after it counted.
        "sender in comment": verdict(
            "A parenthesis in the quoted sender",
            [f'{server}; spf=pass (mx: domain of "x) ; dkim=pass header.d=paypal.com ; (y"@evil.example designates 1.2.3.4)'],
            "PayPal <service@paypal.com>",
        ),
        # The same writer gives a value ending in `\` the bytes of an escaped quote, so `\"` may end the string or not:
        # read either way, a dkim=pass could count or a dmarc=fail vanish, so the header shows nothing but a notice.
        "sender backslash": verdict(
            "A backslash in the quoted sender",
            [f'{server}; spf=pass smtp.mailfrom="a\\\\"; dkim=pass header.d=paypal.com; b"@evil.example'],
            "PayPal <service@paypal.com>",
        ),
        "trailing backslash": verdict(
            "A quoted sender ending in a backslash",
            [f'{server}; spf=pass smtp.mailfrom="x\\"@evil.example; dmarc=fail header.from=paypal.com (policy=reject)'],
            "PayPal <service@paypal.com>",
        ),
        # Any account may sign in and write any From; Postfix ties only the envelope sender to the login.
        "other account": verdict(
            "Signed in as another account",
            [f"{server}; auth=pass smtp.mailfrom=mallory@example.com"],
            "Bob <bob@example.com>",
        ),
        "own account": verdict(
            "Signed in as the From",
            [f"{server}; auth=pass smtp.mailfrom=Bob@example.com"],
            "Bob <bob@example.com>",
        ),
        "commented": verdict(
            "A semicolon inside a comment",
            [
                f"{server}; spf=fail (not permitted; auth=pass ) smtp.mailfrom=bob@example.com;"
                " dmarc=none (no policy (none); dkim=pass header.d=example.com )"
            ],
            "Bob <bob@example.com>",
        ),
    }
    nobody: dict = {"signed_by": [], "mailed_by": None, "warning": None, "from_verified": False}
    want = {
        "planted": nobody | {"warning": {"Dmarc": "paypal.com"}},
        "quoted": nobody | {"mailed_by": "evil.example"},
        "other domain": nobody,
        "sender in comment": nobody,
        "sender backslash": nobody | {"warning": "Unclear"},
        "trailing backslash": nobody | {"warning": "Unclear"},
        "other account": nobody | {"warning": {"Account": "mallory@example.com"}},
        "own account": nobody,
        "commented": nobody | {"warning": {"Own": "example.com"}},
    }
    wrong = {case: (found[case], wanted) for case, wanted in want.items() if found[case] != wanted}
    assert not wrong, f"(found, wanted): {wrong}"


def blocked_image_boxes() -> None:
    # A blocked image that declares no size, as a tracking pixel, takes no space; one with a width and a height keeps
    # that box. The page measures a copy of the frame's document: the reader's frame has an opaque origin.
    subject = "Blocked image boxes"
    deliver_raw(
        subject,
        f"""From: Pat <pat@example.net>
To: {tier.user}
Subject: {subject}
Content-Type: text/html; charset=utf-8

<p>Text<img id="pixel" src="https://tracker.example.net/p.gif">
<img id="banner" src="https://images.example.net/b.png" width="300" height="100"></p>""",
    )
    before = settings_change(remote_images="Never")
    try:
        open_folder("Inbox")
        row_click(subject)
        wait("document.querySelector('.mail-frame iframe')?.srcdoc.includes('banner')")
        boxes = json.loads(
            wd(
                "async",
                """const copy = document.createElement('iframe');
            copy.style.width = '600px';
            copy.onload = () => {
              const box = id => copy.contentDocument.getElementById(id).getBoundingClientRect();
              const size = id => [box(id).width, box(id).height];
              done([size('pixel'), size('banner')]);
              copy.remove();
            };
            copy.srcdoc = document.querySelector('.mail-frame iframe').srcdoc;
            document.body.append(copy);""",
            )
        )
        assert boxes == [[0, 0], [300, 100]], boxes
    finally:
        settings_change(remote_images=before["remote_images"])


def image_proxy_bounded() -> None:
    # vm-full only: each proxied image buffers up to 10 MiB for up to 10 s, so at most 8 run at once. Sixteen slow
    # images fetched together reach the image host 8 at a time at most, and more than one at a time. curl sends them
    # straight to kubicle: Chromium opens 6 connections to a host, one of them held, so it alone kept the count at 5.
    before = settings_change(remote_images="Always")
    try:
        subject = "Slow images"
        imgs = "".join(f'<img src="http://images.example.net/slow{n}.png">' for n in range(16))
        uid = deliver_raw(
            subject, f"From: S <s@example.net>\nTo: {tier.user}\nSubject: {subject}\nContent-Type: text/html\n\n{imgs}"
        )
        urls = [html.unescape(u) for u in re.findall(r'src="([^"]*/image\?[^"]*)"', message(uid)["body"]["document"])]
        assert len(urls) == 16, urls
        address, session, _ = kubicle_direct()
        paths = " ".join(shlex.quote(u[u.index("image?") :]) for u in urls)
        fetch = f"curl -s -o /dev/null -w '%{{http_code}}\\n' -b roundcube_sessid={session} http://{address}/{{}}"
        codes = machine.succeed(f"printf '%s\\n' {paths} | xargs -P 16 -I{{}} {fetch}").split()
        assert codes == ["200"] * 16, codes
        lines = machine.succeed("grep inflight /var/log/image-server.log").splitlines()
        most = max(json.loads(line)["inflight"] for line in lines)
        assert 1 < most <= 8, f"{most} fetches reached the image host at once"
    finally:
        settings_change(remote_images=before["remote_images"])


def image_proxy() -> None:
    # vm-full only: its image host (image-server.nix) stands for a sender's. Each signed image URL in the mail frame
    # brings the image through kubicle with its own User-Agent and none of the browser's Cookie or Referer, follows a
    # redirect, and answers 502 for a page that is no image. The frame's own loads pass front without a cookie.
    host = "http://images.example.net"
    original = machine.succeed(
        f"ip netns exec images curl -sf {host}/pixel.png --resolve images.example.net:80:11.0.0.80 | base64 -w0"
    )
    before = settings_change(remote_images="Always")
    try:
        subject = "Images through the proxy"
        deliver_raw(
            subject,
            f"""From: Pat <pat@example.net>
To: {tier.user}
Subject: {subject}
Content-Type: multipart/related; boundary=b

--b
Content-Type: text/html; charset=utf-8

<p><img src="{host}/pixel.png"><img src="{host}/moved.png"><img src="{host}/page.png"><img src="cid:dot@example.net"></p>
--b
Content-Type: image/png
Content-ID: <dot@example.net>
Content-Transfer-Encoding: base64

{original}
--b--""",
        )
        since = machine.succeed("date +%s").strip()
        open_folder("Inbox")
        row_click(subject)
        srcdoc = wait(
            "document.querySelector('.mail-frame iframe')?.srcdoc.includes('/image?')"
            " && document.querySelector('.mail-frame iframe').srcdoc"
        )

        # What front answered the frame's loads, before this test asks for anything itself. The frame has an opaque
        # origin and sends no cookie, so front must pass the signed routes without Mailu's sign-in check, which would
        # answer each with a 302 to its sign-in page and the frame would show broken images.
        def framed() -> set:
            log = machine.succeed(f"docker logs --since {since} mailu-front 2>&1")
            return set(re.findall(r'"GET /login/(image|inline)\?\S* HTTP/[\d.]+" (\d+)', log))

        deadline = time.monotonic() + 30
        while not ({route for route, _ in framed()} == {"image", "inline"} or time.monotonic() > deadline):
            time.sleep(0.5)
        time.sleep(1)  # the image loads that answer last
        statuses = framed()
        assert statuses == {("image", "200"), ("image", "502"), ("inline", "200")}, statuses
        signed = {}
        for url in re.findall(r'src="([^"]*/image\?[^"]*)"', srcdoc):
            url = html.unescape(url)
            signed[urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)["url"][0]] = url
        assert sorted(signed) == [f"{host}/moved.png", f"{host}/page.png", f"{host}/pixel.png"], signed

        # From the signed-in page, so the browser sends kubicle its cookies and a Referer.
        def fetched(url: str) -> list:
            return json.loads(
                wd(
                    "async",
                    f"""fetch({json.dumps(url)}, {{referrerPolicy: 'unsafe-url'}})
            .then(async r => done([r.status, r.headers.get('content-type'),
                btoa(String.fromCharCode(...new Uint8Array(await r.arrayBuffer())))]))""",
                )
            )

        for name in ["pixel.png", "moved.png"]:
            assert fetched(signed[f"{host}/{name}"]) == [200, "image/png", original], name
        assert fetched(signed[f"{host}/page.png"])[0] == 502, "a page that is no image came through"
        seen = [json.loads(line) for line in machine.succeed("cat /var/log/image-server.log").splitlines()]
        seen = [(r["path"], {name.lower(): value for name, value in r["headers"].items()}) for r in seen]
        proxied = {path for path, headers in seen if headers.get("user-agent") == "Mozilla/5.0 (compatible; kubicle image proxy)"}
        assert proxied == {"/pixel.png", "/moved.png", "/page.png"}, seen
        assert not [headers for _, headers in seen if {"cookie", "referer"} & set(headers)], seen
    finally:
        settings_change(remote_images=before["remote_images"])


def too_large_part() -> None:
    # vm-full only: a part too large to show is listed with the attachments, and its Download saves the whole part.
    # The frame must not link to it: a navigation the frame starts carries no cookie, so Mailu front's sign-in check
    # answers it with a 302 to its sign-in page, and the frame's sandbox allows no download.
    subject = "A part too large to show"
    body = "head -c 2200000 /dev/zero | tr '\\0' a | fold -w 76"  # past render.rs's MAX_SHOWN, 2 MiB
    machine.succeed(f"{{ printf 'From: Pat <pat@example.net>\\nTo: {tier.user}\\n\\n'; {body}; }} > /tmp/raw.eml")
    machine.succeed(tier.deliver(subject, "--data @/tmp/raw.eml"))
    arrived("INBOX", subject)
    open_folder("Inbox")
    row_click(subject)
    srcdoc = wait(
        "[...document.querySelectorAll('.notice')].some(n => n.textContent.includes('too large to show'))"
        " && document.querySelector('.mail-frame iframe')?.srcdoc"
    )
    assert "/download?" not in srcdoc, "the mail frame links to the part"
    chips = js("return [...document.querySelectorAll('.attachments .chip')].map(c => c.textContent)")
    assert len(chips) == 1, chips
    since = machine.succeed("date +%s").strip()
    machine.succeed("rm -rf /tmp/downloads")
    wd("cdp", "Browser.setDownloadBehavior", json.dumps({"behavior": "allow", "downloadPath": "/tmp/downloads"}))
    wd("click", ".attachments .chip")
    js("[...document.querySelectorAll('button.menu-item')].find(b => b.textContent.trim() === 'Download').click()")
    machine.wait_until_succeeds("ls /tmp/downloads/* && ! ls /tmp/downloads/*.crdownload", timeout=30)
    saved = int(machine.succeed("stat -c %s /tmp/downloads/*").strip())
    log = machine.succeed(f"docker logs --since {since} mailu-front 2>&1")
    answered = re.findall(r'"GET /login/download\?\S* HTTP/[\d.]+" (\d+) (\d+)', log)
    # Front's answer whole, and the file saved as large: not a transfer the browser cut short.
    assert answered == [("200", str(saved))] and saved > 2 << 20, (answered, saved)


def too_large_forwarded() -> None:
    # vm-full only: a forwarded message opened as a page of its own says that a part was too large to show there,
    # and links to no download, which its sandbox would refuse.
    subject = "A forward with a part too large to show"
    head = f"From: Pat <pat@example.net>\\nTo: {tier.user}\\nContent-Type: multipart/mixed; boundary=b\\n\\n--b\\n"
    head += "Content-Type: message/rfc822\\n\\nFrom: Shop <shop@example.net>\\nSubject: Huge log\\n\\n"
    body = "head -c 2200000 /dev/zero | tr '\\0' a | fold -w 76"  # past render.rs's MAX_SHOWN, 2 MiB
    machine.succeed(f"{{ printf '{head}'; {body}; printf '\\n--b--\\n'; }} > /tmp/raw.eml")
    machine.succeed(tier.deliver(subject, "--data @/tmp/raw.eml"))
    views = {a["filename"]: a["view_url"] for a in message(arrived("INBOX", subject))["attachments"]}
    page = fetched(views["Huge log.eml"])  # as Open loads it
    shown = page["text"][:2000]
    assert page["status"] == 200 and "too large to show here" in page["text"], shown
    assert "/download?" not in page["text"], shown


def invoice(subject: str) -> int:
    """Delivers a mail with every line the print page shows and one attachment; answers its UID."""
    return deliver_raw(
        subject,
        f"""From: Shop <shop@example.com>
To: {tier.user}
Bcc: archive@example.org
Reply-To: billing@example.com
Date: Tue, 21 Oct 2026 09:15:00 +0000
Subject: {subject}
X-Priority: 2 (High)
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: text/html; charset=utf-8

<p>See attached.</p>
--b
Content-Type: application/pdf; name=invoice-0423.pdf
Content-Disposition: attachment; filename=invoice-0423.pdf
Content-Transfer-Encoding: base64

JVBERi0xLjQK
--b--""",
    )


def print_fetch(subject: str) -> tuple:
    """The print page of a fresh invoice(subject), dated in UTC: its CSP header and HTML."""
    url = f"api/print?folder=INBOX&uid={invoice(subject)}&tz=UTC"
    status, csp, page = json.loads(
        wd(
            "async",
            f"""fetch({json.dumps(url)})
        .then(async r => done([r.status, r.headers.get('content-security-policy'), await r.text()]))""",
        )
    )
    assert status == 200, (status, page)
    return csp, page


def print_page() -> None:
    # Roundcube's print view: the subject as a heading, its header lines with the date in the zone the client names,
    # the attachments.

    subject = "Invoice 0423"
    csp, page = print_fetch(subject)
    lines = [(k, html.unescape(v)) for k, v in re.findall(r"<tr><th[^>]*>([^<]+)</th><td>([^<]*)</td></tr>", page)]
    want = [("From", "Shop <shop@example.com>"), ("To", tier.user), ("Bcc", "archive@example.org")]
    want += [("Reply-To", "billing@example.com"), ("Date", "2026-10-21 09:15"), ("Priority", "High")]
    assert lines == want, lines
    assert f">{subject}</h2>" in page and "<li>invoice-0423.pdf (9 B)</li>" in page, page[:2000]


def print_no_script() -> None:
    # The print page is the mail as a page of its own: a sandbox that allows nothing and no script source, as the
    # mail frame has. The user prints it with the browser's own Print.
    csp, page = print_fetch("Invoice 0425")
    policy = {d.split()[0]: d.split()[1:] for d in csp.split("; ")}
    assert policy.get("sandbox") == [] and "script-src" not in policy and policy["default-src"] == ["'none'"], csp
    assert "<script" not in page, page[:2000]


def print_marks_read() -> None:
    # Print marks the message read whatever the mark-read setting, as Roundcube's does.
    subject = "Invoice 0424"
    uid = invoice(subject)
    before = settings_change(mark_read_after=None)
    try:
        row_click(subject)
        flags = f"{tier.doveadm} fetch -u {tier.user} flags mailbox INBOX uid {uid}"
        assert "\\Seen" not in fetch("flags", "INBOX", uid), "opened with mark-read off, yet read"
        wd("click", ".toolbar button.tool[aria-label=More]")
        wd("click", "button.menu-item[name='Print this message']")  # the page opens in a tab of its own
        machine.wait_until_succeeds(f"{flags} | grep -qF '\\Seen'", timeout=30)
    finally:
        settings_change(mark_read_after=before["mark_read_after"])


def search_api(text: str, **fields) -> dict:
    """A search of INBOX through api/list with Roundcube's defaults, `fields` over them; the page, its rows as `hits`
    and whether it came threaded as `threads`."""
    view = {"folder": "INBOX", "page": 0, "size": 50, "sort": "Unsorted", "order": "Descending", "threads": False}
    search = {"text": text, "fields": {}, "filter": "All", "interval": "Any", "scope": "Folder", "full": False}
    for name, value in fields.items():
        (view if name in view else search)[name] = value
    query = urllib.parse.urlencode({**view, "threads": str(view["threads"]).lower(), "search": json.dumps(search)})
    status, found = request("GET", f"api/list?{query}")
    assert status == 200, (status, found)
    return {**found, "hits": found["rows"], "threads": found["query"]["threads"]}


def uid_in(folder: str, subject: str) -> str:
    """The UID of the message with `subject` in `folder`, or "" while there is none."""
    found = search(folder, f"subject {shlex.quote(subject)}")
    return str(found[-1]) if found else ""


def flags_of(folder: str, uid: str) -> str:
    return fetch("flags", folder, uid)


def move(uid: int, to: str, folder: str = "INBOX") -> None:
    status, answer = request("POST", "api/change", {"Move": {"folder": folder, "messages": {"Uids": [uid]}, "to": to}})
    assert status == 204, (status, answer)


def create(name: str, parent=None) -> None:
    status, answer = request("POST", "api/change", {"CreateFolder": {"name": name, "parent": parent}})
    assert status == 204, (name, status, answer)


def menu_click(label: str) -> None:
    js(f"[...document.querySelectorAll('button.menu-item')].find(b => b.textContent.includes({json.dumps(label)})).click()")


def folder_button(label: str) -> str:
    """A script expression for the folder pane's button labelled `label`."""
    return (
        f"[...document.querySelectorAll('.folder')].find(f => f.querySelector('.folder-name').textContent === {json.dumps(label)})"
    )


def junk_keywords() -> None:
    # markasjunk's keywords go with the move in one job: Junk stores `Junk`, Not junk `NonJunk`; Roundcube's notice
    # confirms each. A report from the other side (Not junk outside Junk, Junk inside it) moves nothing, so a search
    # over several folders started from Junk cannot send good mail there.
    subject = "Junk keyword probe"
    deliver_raw(subject, f"From: Spam <s@example.biz>\nTo: {tier.user}\nSubject: {subject}\n\nBuy.")
    open_folder("Inbox")
    row_click(subject)
    js("document.dispatchEvent(new KeyboardEvent('keydown', {key: '!', bubbles: true}))")
    wait("document.querySelector('.toast')?.textContent === 'Successfully reported as junk'")
    junk = str(arrived("Junk", subject))
    assert re.search(r"\bJunk\b", flags_of("Junk", junk)) and "NonJunk" not in flags_of("Junk", junk), flags_of("Junk", junk)

    def report(folder: str, uid: str, spam: bool) -> list:
        return request("POST", "api/change", {"Report": {"folder": folder, "messages": {"Uids": [int(uid)]}, "spam": spam}})

    assert report("Junk", junk, True)[0] == 204
    assert uid_in("Junk", subject) == junk, "Junk on mail in Junk moved it"
    assert report("Junk", junk, False)[0] == 204
    back = str(arrived("INBOX", subject))
    assert "NonJunk" in flags_of("INBOX", back) and not re.search(r"\bJunk\b", flags_of("INBOX", back)), flags_of("INBOX", back)
    assert report("INBOX", back, False)[0] == 204
    time.sleep(1)
    assert uid_in("INBOX", subject) == back and uid_in("Junk", subject) == "", "Not junk on mail outside Junk moved it"


def archive_rules() -> None:
    # Archive leaves mail already in Archive or below it (Roundcube's archive plugin skips that subtree and disables
    # its button there); read_on_archive and "mark read on delete" mark inside the server's move, whatever moved it.
    create("2023", "Archive")
    subject = "Archived by hand"
    uid = deliver_raw(subject, f"From: Old <old@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nKept.")
    move(uid, "Archive.2023")
    inner = arrived("Archive.2023", subject)
    archived = {"Archive": {"folder": "Archive.2023", "messages": {"Uids": [inner]}, "zone": "UTC"}}
    assert request("POST", "api/change", archived)[0] == 204
    time.sleep(1)
    assert uid_in("Archive.2023", subject) and not uid_in("Archive", subject), "Archive moved mail out of Archive.2023"
    open_folder("2023")
    row_click(subject)
    assert js(
        "return [...document.querySelectorAll('.toolbar button.tool')].find(b => b.textContent.trim() === 'Archive').disabled"
    )
    open_folder("Inbox")
    before = settings_change(read_on_archive=True, read_when_deleted=True)
    try:
        for subject, change in [("Read on archive", "Archive"), ("Read on a move to Trash", "Trash")]:
            uid = deliver_raw(subject, f"From: Kim <kim@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nUnread.")
            body = (
                {"Archive": {"folder": "INBOX", "messages": {"Uids": [uid]}, "zone": "UTC"}}
                if change == "Archive"
                else {"Move": {"folder": "INBOX", "messages": {"Uids": [uid]}, "to": "Trash"}}
            )
            assert request("POST", "api/change", body)[0] == 204
            arrived(change, subject, "seen")
    finally:
        settings_change(read_on_archive=before["read_on_archive"], read_when_deleted=before["read_when_deleted"])


def search_order() -> None:
    # Search results follow the folder's list order: a sort column as the list sorts, none as the list's UID order;
    # the date filter reads the arrival, as Roundcube's IMAP SINCE; several folders without a sort come grouped by
    # folder, the last folder first; a threaded folder's results come as threads of the hits.
    probes = [
        ("Order probe b", "Sat, 01 Feb 2031 10:00:00 +0000"),
        ("Order probe c", "Mon, 01 Jan 2001 10:00:00 +0000"),
        ("Order probe a", "Thu, 01 Jan 2026 10:00:00 +0000"),
    ]
    uids = {s: deliver_raw(s, f"From: P <p@example.net>\nTo: {tier.user}\nSubject: {s}\nDate: {d}\n\nx") for s, d in probes}

    def subjects(found: dict) -> list[str]:
        return [h["row"]["subject"] for h in found["hits"]]

    found = search_api("order probe", fields={"subject": True}, sort="Subject", order="Ascending")
    assert subjects(found) == ["Order probe a", "Order probe b", "Order probe c"], subjects(found)
    assert subjects(search_api("order probe", fields={"subject": True})) == ["Order probe a", "Order probe c", "Order probe b"]
    young = search_api("order probe", fields={"subject": True}, interval={"YoungerThan": "Week"})
    assert "Order probe c" in subjects(young), subjects(young)  # written in 2001, arrived now
    move(uids["Order probe c"], "INBOX.finance")
    arrived("INBOX.finance", "Order probe c")
    grouped = search_api("order probe", fields={"subject": True}, scope="All")
    assert [h["folder"] for h in grouped["hits"]] == ["INBOX.finance", "INBOX", "INBOX"], grouped["hits"]
    root = deliver_raw(
        "Thread probe", f"From: P <p@example.net>\nTo: {tier.user}\nSubject: Thread probe\nMessage-ID: <tp1@example.net>\n\nx"
    )
    deliver_raw(
        "Re: Thread probe",
        f"From: Q <q@example.net>\nTo: {tier.user}\nSubject: Re: Thread probe\nMessage-ID: <tp2@example.net>\n"
        "In-Reply-To: <tp1@example.net>\nReferences: <tp1@example.net>\n\ny",
    )
    threaded = search_api("thread probe", fields={"subject": True}, threads=True)
    assert threaded["threads"] and threaded["total"] == 1, threaded
    assert [(h["row"]["uid"], h["row"]["depth"]) for h in threaded["hits"]][0] == (root, 0), threaded["hits"]
    assert [h["row"]["depth"] for h in threaded["hits"]] == [0, 1], threaded["hits"]


def sort_keeps_page() -> None:
    # A sort change keeps the page shown, as Roundcube's list keeps `_page`; a list mode change goes to page 1, and
    # the address follows (no stale &page= left for a reload).
    open_folder("github")
    for page in [2, 3]:
        wait("!document.querySelector('.paging button[aria-label=\"Next page\"]').disabled", 60)
        js("document.querySelector('.paging button[aria-label=\"Next page\"]').click()")
        wait(
            f"location.hash.endsWith('page={page}')"
            f" && document.querySelector('.paging .muted')?.title.startsWith('Messages {(page - 1) * 50 + 1} to')",
            60,
        )
    before_rows = js(f"return {SUBJECTS}")
    settings = settings_change()
    prefs = settings["folders"].get("INBOX.github", {})
    try:
        settings_change(folders={**settings["folders"], "INBOX.github": {**prefs, "sort": "Subject", "order": "Ascending"}})
        wait(f"JSON.stringify({SUBJECTS}) !== {json.dumps(json.dumps(before_rows))}", 30)
        time.sleep(1)
        assert js("return document.querySelector('.paging .muted').title").startswith("Messages 101 to 150")
        assert js("return location.hash").endswith("page=3"), js("return location.hash")
        settings_change(
            folders={**settings["folders"], "INBOX.github": {**prefs, "sort": "Subject", "order": "Ascending", "threads": True}}
        )
        wait("document.querySelector('.paging .muted')?.title.startsWith('Threads 1 to')", 60)
        assert js("return location.hash") == "#folder=INBOX.github", js("return location.hash")
    finally:
        latest = settings_change()
        settings_change(folders={**latest["folders"], "INBOX.github": prefs})
        open_folder("Inbox")


def sorted_view_after_read() -> None:
    # A read keeps a view's order on the server (sync.rs, `Engine::orders`), so the next page needs no SORT: the
    # sorted view comes back in the same order with the new flag, an Unread search drops the message read, and mail
    # that arrives joins the view (`around` finds it only in an order made after it came).
    view = {"page": 0, "size": 50, "sort": "Subject", "order": "Ascending", "threads": "false"}

    def page(**asked) -> dict:
        status, found = request("GET", f"api/list?{urllib.parse.urlencode({'folder': 'INBOX', **view, **asked})}")
        assert status == 200, (status, found)
        return found

    def mark(uid: int, seen: bool) -> None:
        status, answer = request(
            "POST", "api/change", {"Flag": {"folder": "INBOX", "messages": {"Uids": [uid]}, "flag": "Seen", "set": seen}}
        )
        assert status == 204, (status, answer)

    before = page()
    uids = [h["row"]["uid"] for h in before["rows"]]
    first, was_read = uids[0], bool(before["rows"][0]["row"]["flags"] & 1)
    unread = search_api("", filter="Unread", sort="Subject", order="Ascending")
    target = unread["hits"][0]["row"]["uid"]
    try:
        mark(first, not was_read)
        after = page()
        assert [h["row"]["uid"] for h in after["rows"]] == uids, (uids, after["rows"])
        assert bool(after["rows"][0]["row"]["flags"] & 1) != was_read, after["rows"][0]
        mark(target, True)
        still = search_api("", filter="Unread", sort="Subject", order="Ascending")
        assert target not in [h["row"]["uid"] for h in still["hits"]] and still["total"] == unread["total"] - 1, still["total"]
        uid = deliver_raw("Sorted view probe", f"From: S <s@example.net>\nTo: {tier.user}\nSubject: Sorted view probe\n\nx")
        for _ in range(20):  # until IDLE has reported it
            found = page(around=uid)
            if uid in [h["row"]["uid"] for h in found["rows"]]:
                break
            time.sleep(0.5)
        assert found["total"] == before["total"] + 1, (found["total"], before["total"])
        assert uid in [h["row"]["uid"] for h in found["rows"]], found["query"]
        move(uid, "Trash")
    finally:
        mark(first, was_read)
        mark(target, False)


def open_select_menu() -> None:
    # Wide layout has a 'Select' toolbar dropdown; narrow folds it under 'List actions' as a 'Select' submenu.
    js("""document.querySelector('button.tool[aria-label=Select], button[aria-label="List actions"]').click()""")
    if js("""return [...document.querySelectorAll('button.menu-item')].some(b => b.textContent.trim() === 'Select')"""):
        menu_click("Select")
    wait("""[...document.querySelectorAll('button.menu-item')].some(b => b.textContent.trim() === 'Unread')""")


def select_menu_no_dead_entry() -> None:
    # Bombadil's noDeadClicks caught 'Select > Unread' staying live when the unread rows were already selected, so
    # clicking it changed nothing. The Select menu disables an entry that would not change the selection (list.rs);
    # with the whole page selected, Unread (and Current page) must be disabled, else the click is dead.
    open_folder("Inbox")
    wait("document.querySelectorAll('.row').length > 1")
    assert js("return !!document.querySelector('.row.unread')"), "fixture page has no unread row to exercise the guard"
    open_select_menu()
    menu_click("Current page")
    wait("""[...document.querySelectorAll('.row')].every(r => r.getAttribute('aria-selected') === 'true')""")
    open_select_menu()
    disabled = js("""return Object.fromEntries(['All', 'Current page', 'Unread', 'None'].map(label =>
        [label, [...document.querySelectorAll('button.menu-item')].find(b => b.textContent.trim() === label)
            ?.getAttribute('aria-disabled')]))""")
    js("document.body.click()")  # close the menu
    assert disabled["Unread"] == "true", f"Select > Unread is a dead click with the whole page selected: {disabled}"
    assert disabled["Current page"] == "true" and disabled["None"] is None, disabled


def mark_all_scope() -> None:
    # Mark all as read asks Roundcube's three scopes when another folder has unread mail; "with subfolders" marks the
    # folder and those below it, and nothing else.
    create("Marks")
    create("Sub", "Marks")
    for folder in ["Marks", "Marks.Sub"]:
        uid = deliver_raw(f"Unread in {folder}", f"From: M <m@example.net>\nTo: {tier.user}\nSubject: Unread in {folder}\n\nx")
        move(uid, folder)
    inbox_unread = len(search("INBOX", "unseen"))
    open_folder("Marks")
    wait(f"{folder_button('Marks')}?.querySelector('.badge')")
    js("document.querySelector('.folders .bar-end button').click()")
    menu_click("Mark all as read")
    wait("document.querySelector('.dialog input[name=ask-choice][value=cur]')?.checked")
    js("document.querySelector('.dialog input[name=ask-choice][value=sub]').click()")
    js("document.querySelector('.dialog .button.primary').click()")
    wait("document.querySelector('.toast')?.textContent === 'Message(s) marked successfully.'")
    for folder in ["Marks", "Marks.Sub"]:
        machine.wait_until_succeeds(
            f'test -z "$({tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox {folder} unseen)"', timeout=30
        )
    assert len(search("INBOX", "unseen")) == inbox_unread
    open_folder("Inbox")


def recent_accent() -> None:
    # Roundcube's "recent" mark: a folder other than the open one whose unread count and newest UID grew, until it
    # is opened.
    open_folder("Archive")
    deliver_raw("Recent probe", f"From: R <r@example.net>\nTo: {tier.user}\nSubject: Recent probe\n\nNew.")
    wait(f"{folder_button('Inbox')}.classList.contains('recent')", 60)
    assert not js(f"return {folder_button('Archive')}.classList.contains('recent')")
    open_folder("Inbox")
    wait(f"!{folder_button('Inbox')}.classList.contains('recent')")


def quota_bar() -> None:
    # Roundcube's quota widget, only for an account with a limit: Mailu's Dovecot reports the fixture user's 1 GB. The
    # explore suite's Dovecot has no QUOTA, and its noQuotaWidget property holds the other side.
    quota = request("GET", "api/session")[1]["quota"]
    assert quota and quota["limit"] == -(-(10**9) // 1024) * 1024, quota  # IMAP's KiB, rounded up from 10^9 bytes
    wait("document.querySelector('.quota')?.title.startsWith('Disk usage: ')", 60)
    percent = (quota["used"] * 200 + quota["limit"]) // (2 * quota["limit"])
    assert js("return document.querySelector('.quota-count').textContent") == f"{min(percent, 100)}%"


def search_prefixes() -> None:
    # Roundcube's prefixes: one field in place of those ticked, the rest of the text after it, any case.
    deliver_raw("Prefix probe", f"From: Zed Quux <zed@example.org>\nTo: {tier.user}\nSubject: Prefix probe\n\nbody words here")

    def hits(text: str) -> list[str]:
        return [h["row"]["subject"] for h in search_api(text, fields={"subject": True}, full=True)["hits"]]

    assert "Prefix probe" in hits("from:zed")
    assert "Prefix probe" not in hits("subject:zed")
    assert "Prefix probe" in hits("SUBJECT:prefix probe")
    assert "Prefix probe" in hits("body:words")
    assert search_api("from:", fields={"subject": True})["hits"] == []  # a prefix alone narrows nothing: blank


def search_server_answer() -> None:
    # Search shows the server's answer alone, never a guess at it: Mailu's Dovecot stems words and drops stopwords, so
    # a longer query is no prefix filter of a shorter one's hits. Until the answer comes, the rows on screen stay.
    for subject in ["Weekly Quokka digest", "Quokka week ahead"]:
        deliver_raw(subject, f"From: Q <q@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    paging = "document.querySelector('.paging .muted')?.title"
    try:
        wd("type", "#search", "quokka")
        wait(f"{paging} === 'Messages 1 to 2 of 2'", 60)
        before = js(f"return {SUBJECTS}")
        machine.succeed("pkill -STOP -x imap")
        try:
            wd("type", "#search", " weekl")
            time.sleep(1)
            assert js(f"return {SUBJECTS}") == before, "rows the server had not answered showed"
        finally:
            machine.succeed("pkill -CONT -x imap")
        answer = search_api("quokka weekl", scope="Subfolders", full=True)
        want = [h["row"]["subject"] for h in answer["hits"]]
        wait(f"JSON.stringify({SUBJECTS}) === {json.dumps(json.dumps(want))} && !/searching/i.test({paging})", 60)
    finally:
        js("document.querySelector('.list-search .reset')?.click()")


def search_closes_message() -> None:
    # A new query closes the open message, as Roundcube's qsearch clears the list; the same query asked again, for
    # mail that changed, keeps it.
    subject = "Zorbel ledger"
    deliver_raw(subject, f"From: Z <z@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click(subject)
    try:
        wd("type", "#search", "zorbel")
        wait(f"{SUBJECTS}.includes({json.dumps(subject)}) && !{OPEN_SUBJECT} && {SELECTED} === 0", 60)
        row_click(subject)
        deliver_raw("Zorbel ledger, second", f"From: Z <z@example.net>\nTo: {tier.user}\nSubject: Zorbel ledger, second\n\nx")
        wait(f"{SUBJECTS}.includes('Zorbel ledger, second')", 60)
        assert js(f"return {OPEN_SUBJECT}") == subject, "the same query asked again closed the open message"
    finally:
        js("document.querySelector('.list-search .reset')?.click()")


def search_reset() -> None:
    # The reset button shows while a search is pending and goes back to page 1 of the folder, as Roundcube's
    # reset-search; Chromium's own clear button is hidden, as it left the type and date behind.
    open_folder("Inbox")
    js("document.querySelector('.paging button[aria-label=\"Next page\"]').click()")
    wait("location.hash === '#folder=INBOX&page=2'")
    wd("type", "#search", "zzz no such words")
    wait("document.querySelector('.list-search .reset')")
    js("document.querySelector('.list-search .reset').click()")
    wait(
        "location.hash === '#folder=INBOX' && !document.querySelector('.list-search .reset')"
        " && document.querySelector('#search').value === ''"
    )


def folder_tree() -> None:
    # Roundcube's tree: a message drag resting on a collapsed folder opens it after a second, unsaved; Shift+Left
    # folds the folder and every folder below it, saved.
    create("Tree")
    create("Mid", "Tree")
    create("Leaf", "Tree.Mid")
    settings = settings_change()
    try:
        settings_change(folders={**settings["folders"], "Tree": {"collapsed": True}})
        open_folder("Inbox")
        wait(f"{folder_button('Tree')} && !{folder_button('Mid')}")
        js(f"""const row = document.querySelector('.row');
            row.dispatchEvent(new DragEvent('dragstart', {{bubbles: true, dataTransfer: new DataTransfer()}}));
            const li = {folder_button("Tree")}.closest('li');
            let n = 0; const t = setInterval(() => {{
                li.dispatchEvent(new DragEvent('dragover', {{bubbles: true, cancelable: true}}));
                if (++n === 15) clearInterval(t); }}, 100);""")
        wait(f"!!{folder_button('Mid')}", 10)
        js("document.querySelector('.row').dispatchEvent(new DragEvent('dragend', {bubbles: true}))")
        assert request("GET", "api/settings")[1]["folders"]["Tree"]["collapsed"], "the drag's opening was saved"
        js(f"{folder_button('Tree')}.focus()")
        js(
            f"{folder_button('Tree')}"
            ".dispatchEvent(new KeyboardEvent('keydown', {key: 'ArrowLeft', shiftKey: true, bubbles: true}))"
        )
        deadline = time.monotonic() + 30
        while not all(request("GET", "api/settings")[1]["folders"].get(f, {}).get("collapsed") for f in ["Tree", "Tree.Mid"]):
            assert time.monotonic() < deadline, request("GET", "api/settings")[1]["folders"]
            time.sleep(0.5)
    finally:
        latest = settings_change()
        settings_change(folders={k: v for k, v in latest["folders"].items() if not k.startswith("Tree")})
        request("POST", "api/change", {"DeleteFolder": {"name": "Tree"}})


def full_view() -> None:
    # A double-click opens Roundcube's message view in the tab: the reader alone, Previous and Next across the list,
    # Esc back to it.
    for subject in ["Full view one", "Full view two"]:
        deliver_raw(subject, f"From: F <f@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click("Full view two")
    js(
        "[...document.querySelectorAll('.row')].find(r => r.textContent.includes('Full view two'))"
        ".querySelector('.row-subject').dispatchEvent(new MouseEvent('dblclick', {bubbles: true}))"
    )
    wait(
        "document.querySelector('.app').dataset.pane === 'message'"
        " && getComputedStyle(document.querySelector('.list-pane')).display === 'none'"
    )
    js("document.querySelector('.nav-tools button[aria-label=\"Show next message\"]').click()")
    wait("document.querySelector('.message-subject')?.textContent === 'Full view one'")
    js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))")
    wait(
        "document.querySelector('.app').dataset.pane !== 'message'"
        " && getComputedStyle(document.querySelector('.list-pane')).display !== 'none'"
    )


def message_window() -> None:
    # The message window: its subject as the title, Previous and Next in the folder's order, no "Open in new window",
    # Esc leaves the message, a delete opens the next, and after the folder's last message the tab closes.
    create("Win")
    uids = {}
    for name in ["one", "two", "three"]:
        uid = deliver_raw(f"Window {name}", f"From: W <w@example.net>\nTo: {tier.user}\nSubject: Window {name}\n\nx")
        move(uid, "Win")
    for name in ["one", "two", "three"]:
        uids[name] = arrived("Win", f"Window {name}")
    try:
        js(f"location.hash = '#open={uids['two']}/Win'; location.reload()")
        wait("document.title === 'kubicle :: Window two'", 60)
        assert not js("return !!document.querySelector('.open-window')")
        for _ in range(2):
            js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))")
        time.sleep(0.5)
        assert js("return document.querySelector('.message-subject')?.textContent") == "Window two"
        wait("!document.querySelector('.nav-tools button[aria-label=\"Show next message\"]').disabled")
        js("document.querySelector('.nav-tools button[aria-label=\"Show next message\"]').click()")
        wait(f"document.title === 'kubicle :: Window one' && location.hash === '#open={uids['one']}/Win'")
        js("document.querySelector('.nav-tools button[aria-label=\"Show previous message\"]').click()")
        wait("document.title === 'kubicle :: Window two'")
        js("window.close = () => { window.kubicleClosed = true; }")
        tool("Delete")
        wait("document.title === 'kubicle :: Window one'")
        tool("Delete")
        wait("window.kubicleClosed === true")
    finally:
        js("location.hash = '#folder=INBOX'; location.reload()")
        wait("!!document.querySelector('.folder')", 60)
        request("POST", "api/change", {"DeleteFolder": {"name": "Win"}})


def message_window_compose() -> None:
    # A reply or forward in a message window opens Compose in that tab, which then is a compose window, as Roundcube
    # keeps `_extwin` for it: closing or discarding that Compose closes the tab.
    main = windows()[0]
    subject = "Answered from its window"
    uid = deliver_raw(subject, f"From: W <w@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")

    def discard_edited() -> None:
        wd("type", ".compose input[aria-label=Subject]", " edited")
        discard_window()

    try:
        for action, leave in [("Reply", lambda: wd("click", ".compose .tool[title^='Close']")), ("Forward", discard_edited)]:
            js(f"window.open('#open={uid}/INBOX', '_blank')")
            wait_windows(2)
            wd("window", [h for h in windows() if h != main][0])
            wait(f"document.title === {json.dumps('kubicle :: ' + subject)}", 60)
            tool(action)
            wait("!!document.querySelector('.compose input[aria-label=Subject]')")
            leave()
            wait_windows(1)
            wd("window", main)
    finally:
        wd("window", main)


def zip_download() -> None:
    # zipdownload's archives of a selection: Mbox (one mailbox file, `>From ` quoted) and Maildir (one .eml each,
    # named by UID and subject, under folder paths when several folders are in it), with CRCs that check out.
    import base64
    import io
    import zipfile

    create("Zipped")
    for subject, body in [("Zip one", "From here on"), ("Zip two: x/y", "two"), ("Zip three", ">From a quote")]:
        move(deliver_raw(subject, f"From: Z Z <z@example.net>\nTo: {tier.user}\nSubject: {subject}\n\n{body}"), "Zipped")
    machine.wait_until_succeeds(
        f"test $({tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Zipped all | wc -l) = 3", timeout=30
    )

    def download(mode: str, sets: list) -> zipfile.ZipFile:
        url = f"'api/zip?mode={mode}&sets=' + encodeURIComponent({json.dumps(json.dumps(sets))})"
        data = json.loads(
            wd(
                "async",
                f"""fetch({url}).then(async r => {{ const b = new Uint8Array(await r.arrayBuffer());
            let s = ''; for (const c of b) s += String.fromCharCode(c); done([r.status, btoa(s)]); }})""",
            )
        )
        assert data[0] == 200, data
        archive = zipfile.ZipFile(io.BytesIO(base64.b64decode(data[1])))
        assert archive.testzip() is None
        return archive

    try:
        mbox = download("Mbox", [["Zipped", "*"]])
        assert mbox.namelist() == ["Zipped.mbox"], mbox.namelist()
        text = mbox.read("Zipped.mbox").decode()
        assert len(re.findall(r"(?m)^From ", text)) == 3 and ">From here on" in text and ">>From a quote" in text, text
        maildir = download("Maildir", [["Zipped", "*"]])
        names = sorted(n.split(" ", 1)[1] for n in maildir.namelist())
        assert names == ["Zip one.eml", "Zip three.eml", "Zip two x-y.eml"], maildir.namelist()
        uid = uid_in("Zipped", "Zip one")
        several = download("Maildir", [["Zipped", uid], ["INBOX.finance", "1"]])
        assert sorted(n.split("/")[0] for n in several.namelist()) == ["INBOX", "Zipped"], several.namelist()
    finally:
        request("POST", "api/change", {"DeleteFolder": {"name": "Zipped"}})


def folder_names() -> None:
    # Roundcube's folder form: a name and a Parent folder; the hierarchy delimiter, `%` and `*` refused in its
    # words; sizes summed by the server.
    status, answer = request("POST", "api/change", {"CreateFolder": {"name": "Bad.Name"}})
    assert status == 400 and "forbidden character" in answer["message"], (status, answer)
    create("Child", "INBOX.finance")
    assert "INBOX.finance.Child" in machine.succeed(f"{tier.doveadm} mailbox list -u {tier.user}")
    status, answer = request(
        "POST", "api/change", {"RenameFolder": {"from": "INBOX.finance.Child", "name": "Moved", "parent": None}}
    )
    assert status == 204, (status, answer)
    listed = machine.succeed(f"{tier.doveadm} mailbox list -u {tier.user}").split()
    assert "Moved" in listed and "INBOX.finance.Child" not in listed, listed
    request("POST", "api/change", {"DeleteFolder": {"name": "Moved"}})
    sizes = machine.succeed(f"{tier.doveadm} fetch -u {tier.user} size.virtual mailbox INBOX.finance all")
    total = sum(int(n) for n in re.findall(r"size\.virtual: (\d+)", sizes))
    assert request("GET", "api/sizes")[1]["INBOX.finance"] == total


def logout_purge() -> None:
    # Clear Trash on logout: older than 30 days takes what arrived before then, in Trash and below it, and keeps a
    # subfolder with newer mail; all messages takes everything and the folders below Trash. Trash's icon hollows
    # once it holds nothing.
    trash_icon = f"{folder_button('Trash')}.querySelector('.icon path').getAttribute('d')"
    assert "M10 11v6" in js(f"return {trash_icon}"), "Trash holds mail, yet its icon is empty"
    create("Old", "Trash")
    move(search("INBOX", "before 2021-01-01")[-1], "Trash.Old")
    move(deliver_raw("Fresh in Trash", f"From: T <t@example.net>\nTo: {tier.user}\nSubject: Fresh in Trash\n\nx"), "Trash.Old")
    machine.wait_until_succeeds(
        f"test $({tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Trash.Old all | wc -l) = 2", timeout=30
    )

    def sign_out_and_in() -> None:
        assert request("POST", "api/logout")[0] == 204
        js("location.reload()")
        wait("!!document.querySelector('.folder')", 60)

    before = settings_change(logout_purge={"OlderThan": 30})
    try:
        sign_out_and_in()
        left = search("Trash.Old", "all")
        assert len(left) == 1 and uid_in("Trash.Old", "Fresh in Trash"), left
        settings_change(logout_purge="All")
        sign_out_and_in()
        listed = machine.succeed(f"{tier.doveadm} mailbox list -u {tier.user}").split()
        assert "Trash.Old" not in listed and search("Trash", "all") == []
        wait(f"{trash_icon} === 'M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13'", 60)
    finally:
        settings_change(logout_purge=before["logout_purge"])


def live_update(limit_ms: int) -> None:
    subject = "Delivered while the list is open"
    machine.succeed(f"(sleep 1; {tier.deliver(subject)}; date +%s%3N >/tmp/delivered) >/tmp/deliver.log 2>&1 &")
    seen = json.loads(
        wd(
            "async",
            f"""new MutationObserver((_, o) => {{
        if ({SUBJECTS}.includes({json.dumps(subject)})) {{ o.disconnect(); done(Date.now()); }}
        }}).observe(document.querySelector('.list'), {{childList: true, subtree: true}});""",
        )
    )
    took = seen - int(machine.succeed("cat /tmp/delivered"))
    print(f"new mail visible {took} ms after delivery")
    assert took < limit_ms, f"new mail took {took} ms to show, limit {limit_ms} ms"


def body_text(message: dict) -> str:
    """The words of a message's frame document, without its markup: a line for each line, row and paragraph."""
    paper = message["body"]["document"].split('<div id="kubicle-paper">', 1)[1]
    return html.unescape(re.sub(r"<[^>]+>", "", re.sub(r"<br>|</div>|</tr>|</p>", "\n", paper)))


# The reader's frame document, parsed: its opaque origin keeps the page from reading the frame itself.
FRAME = "new DOMParser().parseFromString(document.querySelector('.mail-frame iframe')?.srcdoc ?? '', 'text/html')"


def fetched(url: str) -> dict:
    """A URL fetched by the signed-in page, as a tab it opens would load it: behind Mailu front a request without the
    page's session is sent to sign in first."""
    return json.loads(
        wd(
            "async",
            f"""fetch({json.dumps(url)}).then(async r => done({{status: r.status,
        type: r.headers.get('content-type'), csp: r.headers.get('content-security-policy') || '', text: await r.text()}}))""",
        )
    )


def message(uid: int, query: str = "") -> dict:
    status, m = request("GET", f"api/message?folder=INBOX&uid={uid}{query}")
    assert status == 200, (status, m)
    return m


def html_as_text() -> None:
    # Roundcube's switch on every HTML mail (has_html_part): "Plain text" converts HTML-only mail with the html2text
    # replies use, and Display HTML off shows it so from the start.
    uid = deliver_raw(
        "HTML only",
        f"""From: News <news@example.com>
To: {tier.user}
Subject: HTML only
Content-Type: text/html; charset=utf-8

<table><tr><td><p>Hello <b>reader</b></p></td></tr><tr><td><a href="https://example.com/x">Open</a></td></tr></table>
<blockquote>quoted words</blockquote>""",
    )
    before = settings_change(prefer_html=False)
    try:
        assert message(uid)["body"]["text"], "Display HTML off still showed HTML"
    finally:
        settings_change(prefer_html=before["prefer_html"])
    assert not message(uid, "&text=false")["body"]["text"]
    m = message(uid, "&text=true")  # remembered for the session, so the reader opens it as text
    assert m["has_html"] and m["body"]["text"] and "Hello" in body_text(m), m["body"]
    # As Roundcube's rcube_html2text: no table border drawn, the link numbered and its address listed whole.
    text = body_text(m)
    assert "Open [1]" in text and "[1] https://example.com/x" in text and not re.search("[\u2500-\u257f]", text), text
    reload()  # else the HTML body the page prefetched when the mail arrived shows
    row_click("HTML only")
    wait("[...document.querySelectorAll('.header-links button')].some(b => b.textContent === 'HTML')")  # text, as chosen above
    js("[...document.querySelectorAll('.header-links button')].find(b => b.textContent === 'HTML').click()")
    wait(
        "[...document.querySelectorAll('.header-links button')].some(b => b.textContent === 'Plain text')"
        " && !!document.querySelector('.mail-frame iframe')"
    )


def plain_text_view() -> None:
    # Roundcube's text2html: format=flowed unfolded, quote levels nested, the signature muted, links for www., an
    # upper-case scheme and an address, which opens Compose to it.
    deliver_raw(
        "Flowed view",
        f"""From: Tom <tom@example.com>
To: {tier.user}
Subject: Flowed view
Content-Type: text/plain; charset=utf-8; format=flowed

This is one paragraph that a flowed sender wrapped 
at seventy-two columns.
> first level
>> second level
>>> third level
See www.example.com/menu and HTTPS://EXAMPLE.COM/A or write to anna@example.org.

-- 
Tom""",  # noqa: W291 - format=flowed's soft break and the "-- " signature line both end in a space
    )
    row_click("Flowed view")
    wait(f"{FRAME}.querySelector('.pre')?.textContent.includes('wrapped at seventy-two columns.')")
    assert js(f"return {FRAME}.querySelectorAll('.pre blockquote blockquote blockquote').length") == 1
    signature = js(f"return [...{FRAME}.querySelectorAll('.pre .sig')].map(e => e.textContent).join('|')")
    assert signature.rstrip("|") == "-- |Tom", signature  # the empty lines after it are muted too
    hrefs = js(f"return [...{FRAME}.querySelectorAll('.pre a')].map(a => a.getAttribute('href'))")
    assert hrefs[:2] == ["http://www.example.com/menu", "HTTPS://EXAMPLE.COM/A"], hrefs
    # The address opens Compose in a tab of its own, as a mailto: link in HTML mail does.
    assert urllib.parse.unquote(hrefs[2].split("#compose=", 1)[1]) == "mailto:anna@example.org", hrefs


def open_attachments() -> None:
    # Text, HTML and forwarded messages open as sandboxed pages through the signed view route: text decoded from
    # its charset, HTML sanitized, a message with its headers. A PDF stays a download.
    koi8 = "Привет".encode("koi8-r")
    uid = deliver_raw(
        "Open attachments",
        f"""From: Ops <ops@example.com>
To: {tier.user}
Subject: Open attachments
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: text/plain

See attached.
--b
Content-Type: text/plain; charset=koi8-r; name=log.txt
Content-Disposition: attachment; filename=log.txt
Content-Transfer-Encoding: base64

{base64.b64encode(koi8).decode()}
--b
Content-Type: text/html; name=receipt.html
Content-Disposition: attachment; filename=receipt.html

<p onclick="x()">Paid<script>alert(1)</script></p>
--b
Content-Type: message/rfc822

From: Shop <shop@example.net>
Subject: Your order

Thanks for the order.
--b
Content-Type: application/pdf; name=invoice.pdf
Content-Disposition: attachment; filename=invoice.pdf

%PDF-1.4
--b--""",
    )
    views = {a["filename"]: a["view_url"] for a in message(uid)["attachments"]}
    assert views["invoice.pdf"] is None and all(views[n] for n in ["log.txt", "receipt.html", "Your order.eml"]), views
    log = fetched(views["log.txt"])
    assert log["type"] == "text/plain; charset=utf-8" and "Привет" in log["text"], log
    receipt = fetched(views["receipt.html"])
    assert "sandbox" in receipt["csp"] and "Paid" in receipt["text"], receipt
    assert "<script" not in receipt["text"] and "onclick" not in receipt["text"], receipt
    order = fetched(views["Your order.eml"])
    assert "sandbox" in order["csp"] and "Thanks for the order." in order["text"] and "Shop" in order["text"], order


def forwarded_inline() -> None:
    # A forwarded message shows below the text that carries it, with its own headers, and its files join the list.
    uid = deliver_raw(
        "Fwd: complaint",
        f"""From: Colleague <col@example.com>
To: {tier.user}
Subject: Fwd: complaint
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: text/plain; charset=utf-8

Please look at this.
--b
Content-Type: message/rfc822

From: Customer <c@example.net>
Subject: Broken order
Content-Type: multipart/mixed; boundary=c

--c
Content-Type: text/plain; charset=utf-8

My order arrived broken.
--c
Content-Type: application/pdf; name=photo-report.pdf
Content-Disposition: attachment; filename=photo-report.pdf

%PDF-1.4
--c--
--b--""",
    )
    m = message(uid)
    assert [a["filename"] for a in m["attachments"]] == ["Broken order.eml", "photo-report.pdf"], m["attachments"]
    text = body_text(m)
    assert text.index("Please look") < text.index("Broken order") < text.index("My order arrived broken."), text
    row_click("Fwd: complaint")
    wait(f"{FRAME}.querySelector('.headers-table')?.textContent.includes('Broken order')")
    assert js(f"return {FRAME}.body.textContent.includes('My order arrived broken.')")


def inline_images_listed() -> None:
    # Roundcube's list: a photo shown inline without a Content-ID is a file with a preview and Download; a layout
    # image in multipart/related is not. An image/jpg one the mail shows is served, as Roundcube's fix_mimetype reads it.
    jpeg = (
        "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////"
        "wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q=="
    )
    uid = deliver_raw(
        "Photos",
        f"""From: Mum <mum@example.com>
To: {tier.user}
Subject: Photos
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: multipart/related; boundary=r

--r
Content-Type: text/html; charset=utf-8

<p>From the trip <img alt="logo" src="cid:logo@example"></p>
--r
Content-Type: image/jpg
Content-ID: <logo@example>
Content-Transfer-Encoding: base64

{jpeg}
--r--
--b
Content-Type: image/jpeg; name=IMG_4411.jpg
Content-Disposition: inline; filename=IMG_4411.jpg
Content-Transfer-Encoding: base64

{jpeg}
--b--""",
    )
    m = message(uid)
    assert [(a["filename"], bool(a["preview_url"])) for a in m["attachments"]] == [("IMG_4411.jpg", True)], m["attachments"]
    logo = re.search(r'<img[^>]*alt="logo"[^>]*>', m["body"]["document"])
    source = re.search(r'src="([^"]+)"', logo.group(0)) if logo else None
    assert source, m["body"]["document"]
    served = fetched(source.group(1).replace("&amp;", "&"))
    assert (served["status"], served["type"]) == (200, "image/jpeg"), served


def cache_policy() -> None:
    # No byte of mail is stored by the browser, as Roundcube's: a route the cookie authorizes names no account, so a
    # kept answer was the next account's for the same URL, and a signed page kept an hour reopened from history after
    # Sign out. The JSON answers too, the message's document and the session's CSRF token among them.
    png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
    uid = deliver_raw(
        "Cached",
        f"From: C <c@example.net>\nTo: {tier.user}\nSubject: Cached\nContent-Type: multipart/mixed; boundary=b\n\n"
        f"--b\nContent-Type: text/plain\n\nx\n--b\nContent-Type: image/png; name=a.png\nContent-Transfer-Encoding: base64\n\n"
        f"{png}\n--b--",
    )

    def kept(url: str) -> str:
        return json.loads(wd("async", f"fetch({json.dumps(url)}).then(r => done(r.headers.get('cache-control')))"))

    sets = urllib.parse.quote(json.dumps([["INBOX", str(uid)]]))
    for url in (
        f"api/download?folder=INBOX&uid={uid}",
        f"api/source?folder=INBOX&uid={uid}",
        f"api/print?folder=INBOX&uid={uid}",
        f"api/zip?mode=Maildir&sets={sets}",
        f"api/message?folder=INBOX&uid={uid}",
        "api/session",
    ):
        assert kept(url) == "private, no-store", (url, kept(url))
    attachment = message(uid)["attachments"][0]
    for url in (attachment["preview_url"], attachment["url"]):
        assert kept(url.replace("&amp;", "&")) == "private, no-store", (url, kept(url.replace("&amp;", "&")))


def signed_links_refused() -> None:
    # A signed URL serves only as signed: its tag covers the route and every parameter before it, so another UID, one
    # more parameter, another route or no tag gets 403. Each plant of a parameter left out of the tag fails one case.
    uid = deliver_raw(
        "Signed",
        f"From: S <s@example.net>\nTo: {tier.user}\nSubject: Signed\nContent-Type: multipart/mixed; boundary=b\n\n"
        f"--b\nContent-Type: text/plain\n\nx\n--b\nContent-Type: text/plain; name=a.txt\n\ny\n--b--",
    )
    url = message(uid)["attachments"][0]["url"].replace("&amp;", "&")
    assert fetched(url)["status"] == 200, url
    head, tag = url.rsplit("&t=", 1)
    for forged in (
        url.replace(f"&u={uid}&", f"&u={uid - 1}&"),
        f"{head}&n=1&z=1&t={tag}",
        url.replace("/download?", "/view?"),
        head,
    ):
        assert fetched(forged)["status"] == 403, forged


def shown_after_recreate() -> None:
    # Show remote images is kept per message for the session. A folder deleted and created again numbers its mail
    # from UID 1 anew, so the key holds the folder's UIDVALIDITY: the new message 1 starts with images blocked.
    folder, html = "Recreated", '<img src="http://img.example.org/x.png">'

    def fresh(subject: str) -> int:
        machine.succeed(f"{tier.doveadm} mailbox create -u {tier.user} {folder}")
        raw = f"From: R <r@example.net>\r\nTo: {tier.user}\r\nSubject: {subject}\r\nContent-Type: text/html\r\n\r\n{html}\r\n"
        machine.succeed(f"cat > /tmp/recreated.eml <<'EOF'\n{raw}\nEOF")
        machine.succeed(f"docker exec -i mailu-imap doveadm save -u {tier.user} -m {folder} </tmp/recreated.eml")
        return search(folder, "all", 30)[-1]

    def blocked(uid: int, images: str = "") -> bool:
        deadline = time.monotonic() + 60
        while (answer := request("GET", f"api/message?folder={folder}&uid={uid}{images}"))[0] != 200:
            assert time.monotonic() < deadline, answer
            time.sleep(2)
        return answer[1]["body"]["remote_blocked"]

    first = fresh("Before")
    assert not blocked(first, "&images=true"), "Show remote images did not show them"
    machine.succeed(f"{tier.doveadm} mailbox delete -u {tier.user} {folder}")
    second = fresh("After")
    assert second == first, (first, second)
    assert blocked(second), "the recreated folder's message inherited Show remote images"
    machine.succeed(f"{tier.doveadm} mailbox delete -u {tier.user} {folder}")


def failed_open() -> None:
    # A message the server cannot load says so in the reader, in Roundcube's words (messageopenerror), instead of
    # Loading... for good. The page's own fetch answers its body with a 404, as for a message gone meanwhile.
    subject = "Gone before it opened"
    js("""window.kubiclePlain = window.fetch;
        window.fetch = (url, init) => String(url).startsWith('api/message?') ? Promise.resolve(new Response(
            JSON.stringify({message: 'no such message'}), {status: 404, headers: {'content-type': 'application/json'}}))
            : kubiclePlain(url, init);""")
    try:
        open_folder("Inbox")
        deliver_raw(subject, f"From: G <g@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")  # never prefetched
        wait(f"{SUBJECTS}.includes({json.dumps(subject)})", 60)
        js(f"{row_of(subject)}.click()")
        wait("document.querySelector('.reader-empty p')?.textContent === 'Could not load message from server.'", 10)
    finally:
        js("window.fetch = window.kubiclePlain")


def flag_in_reader() -> None:
    # Roundcube's flag before the subject, following the mark live.
    deliver_raw("Flag me", f"From: Pat <pat@example.com>\nTo: {tier.user}\nSubject: Flag me\n\nImportant.")
    row_click("Flag me")
    press = "document.dispatchEvent(new KeyboardEvent('keydown', {key: 's', bubbles: true}))"
    js(press)
    wait("!!document.querySelector('.subject-line .flagged')")
    js(press)
    wait("!document.querySelector('.subject-line .flagged')")


def empty_attachment() -> None:
    # Roundcube's alert for an empty attachment, in place of its menu.
    deliver_raw(
        "Empty file inside",
        f"""From: Scanner <scan@example.com>
To: {tier.user}
Subject: Empty file inside
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: text/plain

Scanned.
--b
Content-Type: application/pdf; name=scan.pdf
Content-Disposition: attachment; filename=scan.pdf
Content-Transfer-Encoding: base64

--b--""",
    )
    row_click("Empty file inside")
    js("[...document.querySelectorAll('.attachments .chip')].find(c => c.textContent.includes('scan.pdf')).click()")
    dialog_button("This attachment appears to be empty.", "OK")


def draft_notice() -> None:
    # Roundcube's "This is a draft message." with Edit, which opens it in Compose.
    subject = "A draft to edit"
    draft = new_draft()
    draft.update(subject=subject, body="Not sent yet.", to=[{"name": None, "email": "someone@example.net"}])
    assert request("POST", "api/draft", draft)[0] == 200
    try:
        open_folder("Drafts")
        row_click(subject)
        wait("document.querySelector('.notice.draft')?.textContent.includes('This is a draft message.')")
        js("document.querySelector('.notice.draft button').click()")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))
    finally:
        if js("return !!document.querySelector('.compose')"):
            discard()
        open_folder("Inbox")


def draft_edit_close() -> None:
    # Each save of an edited draft replaces its copy in Drafts (APPEND, then EXPUNGE of the old UID), so once Compose
    # closes nothing may show the old copy, whose Headers and Show source would answer 404: back on the list with no
    # message open, as Roundcube's Compose returns to its list.
    subject = "A draft edited then closed"
    draft = new_draft()
    draft.update(subject=subject, body="First version.", to=[{"name": None, "email": tier.user}])
    status, copy = request("POST", "api/draft", draft)
    assert status == 200, (status, copy)
    old = copy["uid"]
    try:
        open_folder("Drafts")
        row_click(subject)
        tool("Edit")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))
        wd("type", ".compose input[aria-label=Subject]", " again")
        wait("document.querySelector('.compose-status')?.textContent === 'Draft saved'")
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        wait("!document.querySelector('.compose')")
        arrived("Drafts", f"{subject} again")
        machine.wait_until_succeeds(
            f"! {tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Drafts uid {old} | grep -q .", timeout=30
        )
        time.sleep(2)  # the reader would have to drop the old copy by now
        shown = js("return document.querySelector('.message-subject')?.textContent ?? null")
        assert shown is None, f"the reader still shows {shown!r}, UID {old} replaced"
        rows = js(f"return {SUBJECTS}")
        assert subject not in rows, f"the list still shows the replaced draft: {rows}"
        # Sent, the draft's copy goes too, and the reader with it.
        subject += " again"
        saved = arrived("Drafts", subject)
        row_click(subject)
        tool("Edit")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))
        wd("click", ".compose .button.primary")
        wait("document.querySelector('.toast')?.textContent === 'Message sent successfully.'")
        arrived("Sent", subject)
        machine.wait_until_succeeds(
            f"! {tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Drafts uid {saved} | grep -q .", timeout=30
        )
        time.sleep(2)
        shown = js("return document.querySelector('.message-subject')?.textContent ?? null")
        assert shown is None, f"the reader still shows {shown!r}, UID {saved} sent"
    finally:
        if js("return !!document.querySelector('.compose')"):
            discard()
        open_folder("Inbox")


def draft_in_drafts(subject: str) -> None:
    """A saved draft with `subject`, opened from Drafts in Compose."""
    draft = new_draft()
    draft.update(subject=subject, body="x", to=[{"name": None, "email": tier.user}])
    status, answer = request("POST", "api/draft", draft)
    assert status == 200, (status, answer)
    open_folder("Drafts")
    row_click(subject)
    tool("Edit")
    wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))


def drafts_with(subject: str) -> int:
    found = machine.succeed(f"{tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox Drafts subject {shlex.quote(subject)}")
    return len(found.split("\n")) - 1


# The subjects of the drafts this browser keeps, one per Compose.
KEPT = "Object.keys(localStorage).filter(k => k.startsWith('kubicle.draft.')).map(k => JSON.parse(localStorage[k]).draft.subject)"


def drafts_none() -> None:
    # Special Folders' None for Drafts, as Roundcube: no Save and nothing saved to the server, the text kept in this
    # browser; every way out of the edited Compose asks Roundcube's notsentwarning, whose Discard drops it.
    special = request("GET", "api/settings")[1]["special_folders"]
    settings_change(special_folders={**special, "Drafts": None})
    subject = "Kept in this browser"
    question = "The message has not been sent and has unsaved changes. Do you want to discard your changes?"
    try:
        reload()  # so the page has the setting
        js(r"""window.kubicleDraftPosts = 0; const plain = window.fetch;
            window.fetch = (url, ...rest) => (url === 'api/draft' && window.kubicleDraftPosts++, plain(url, ...rest));""")
        wd("click", ".compose-button")
        wait("!!document.querySelector('.compose input[aria-label=Subject]')")
        assert not js("return !!document.querySelector(\".compose .tool[title='Save as draft now']\")"), "Save is offered"
        wd("type", ".compose input[aria-label=Subject]", subject)
        wait("document.querySelector('.compose-status').textContent === 'Saved in this browser'")
        ways = {
            "a folder": lambda: js(f"{folder_button('Inbox')}.click()"),
            "Settings": lambda: js("document.querySelector('.folders-bottom .tool[title=Settings]').click()"),
            "Esc": lambda: js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))"),
            "Compose": lambda: wd("click", ".compose-button"),
            "Sign out": lambda: wd("click", "button.tool.logout"),
        }
        for way, leave in ways.items():
            leave()
            dialog_button(question, "Cancel")
            assert js("return document.querySelector('.compose input[aria-label=Subject]')?.value") == subject, (
                f"{way} left Compose"
            )
        time.sleep(11)  # past the 10 s idle after which a save would go
        assert js("return window.kubicleDraftPosts") == 0, "a draft went to the server"
        assert subject in js(f"return {KEPT}"), "the browser copy is gone"
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        dialog_button(question, "Discard")
        wait("!document.querySelector('.compose')")
        assert subject not in js(f"return {KEPT}"), "Discard kept the browser copy"
    finally:
        signed_in_again()  # had a way out not asked, Sign out ended the session
        settings_change(special_folders=special)
        reload()


def draft_saves_in_turn() -> None:
    # Close while a save is on its way: Close's save must wait for it and replace the copy it makes. The page holds
    # the first save's request, so without the wait Close's save reaches the server first and the held one, sent
    # later, replaces it: Drafts keeps the older text.
    subject = "A draft closed while it saves"
    draft_in_drafts(subject)
    try:
        js(r"""const plain = window.fetch; let first = true;
            const hold = new Promise(r => window.kubicleRelease = r);
            window.fetch = (url, ...rest) => url === 'api/draft' && first
                ? (first = false, hold.then(() => plain(url, ...rest))) : plain(url, ...rest);""")
        wd("type", ".compose input[aria-label=Subject]", " once")
        wd("click", ".compose .tool[title='Save as draft now']")
        wd("type", ".compose input[aria-label=Subject]", " twice")
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        time.sleep(1)
        js("window.kubicleRelease()")
        wait("document.querySelector('.toast')?.textContent === 'Draft saved.'", 60)
        time.sleep(2)
        copies = (drafts_with(subject), drafts_with(f"{subject} once twice"))
        assert copies == (1, 1), f"Drafts holds {copies[0]} copies, {copies[1]} with the last text"
    finally:
        if js("return !!document.querySelector('.compose')"):
            discard()
        reload()  # without the held fetch
        open_folder("Inbox")


def draft_page_during_save() -> None:
    # A page of Drafts asked before a save and answered with the copy the save then replaces must not list that copy
    # again. The page the first save's event asks for is held here until the second save, Close's, is answered.
    subject = "A draft saved while its page loads"
    draft_in_drafts(subject)
    try:
        js(r"""window.kubicleHeld = []; window.kubicleHold = true; const plain = window.fetch;
            window.fetch = (url, ...rest) => { const answer = plain(url, ...rest);
                return window.kubicleHold && /^api\/list\?folder=Drafts&page=0&/.test(url)
                    ? new Promise(r => window.kubicleHeld.push(() => r(answer))) : answer; };""")
        wd("type", ".compose input[aria-label=Subject]", " once")
        wd("click", ".compose .tool[title='Save as draft now']")
        wait("document.querySelector('.compose-status')?.textContent === 'Draft saved'")
        wait("window.kubicleHeld.length > 0")
        js("window.kubicleHold = false")
        wd("type", ".compose input[aria-label=Subject]", " twice")
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        wait("document.querySelector('.toast')?.textContent === 'Draft saved.'")
        wait(f"{SUBJECTS}.includes({json.dumps(subject + ' once twice')})")
        js("window.kubicleHeld.forEach(release => release())")
        time.sleep(1)
        rows = js(f"return {SUBJECTS}")
        assert f"{subject} once" not in rows, f"the list shows the copy the save replaced: {rows}"
    finally:
        reload()  # without the held fetch
        open_folder("Inbox")


def draft_in_subfolder() -> None:
    # A draft opened from a folder below Drafts stays there: a save replaces its copy in that folder and Discard
    # moves that copy to Trash. Drafts itself keeps every message, the one with the same UID as the copy included.
    subject = "A draft kept below Drafts"
    create("Older", "Drafts")
    try:
        draft = new_draft()
        draft.update(subject=subject, body="x", to=[{"name": None, "email": tier.user}])
        status, copy = request("POST", "api/draft", draft)
        assert status == 200 and copy["folder"] == "Drafts", (status, copy)
        move(copy["uid"], "Drafts.Older", "Drafts")
        uid = arrived("Drafts.Older", subject)
        drafts = search("Drafts", "all")
        open_folder("Older")
        row_click(subject)
        js("document.querySelector('.notice.draft button').click()")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(subject))
        wd("type", ".compose input[aria-label=Subject]", " again")
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        wait("document.querySelector('.toast')?.textContent === 'Draft saved.'")
        arrived("Drafts.Older", f"{subject} again")
        assert search("Drafts.Older", f"uid {uid}") == [], "the replaced copy stayed below Drafts"
        assert search("Drafts", "all") == drafts, "a save below Drafts changed Drafts"
        row_click(f"{subject} again")
        js("document.querySelector('.notice.draft button').click()")
        wait("document.querySelector('.compose input[aria-label=Subject]')?.value === " + json.dumps(f"{subject} again"))
        discard()
        arrived("Trash", f"{subject} again")
        assert search("Drafts.Older", "all") == [], "Discard left the copy below Drafts"
        assert search("Drafts", "all") == drafts, "Discard below Drafts changed Drafts"
    finally:
        if js("return !!document.querySelector('.compose')"):
            discard()
        open_folder("Inbox")
        request("POST", "api/change", {"DeleteFolder": {"name": "Drafts.Older"}})


def details_rows() -> None:
    # Roundcube's Details: Sender, Bcc, Followup-To and Priority rows, each address a link, ten to a row and "N more..."
    # opening the whole row in a dialog.
    cc = ", ".join(f"cc{n}@example.org" for n in range(12))
    deliver_raw(
        "Every header row",
        f"""From: Boss <boss@example.com>
Sender: Secretary <secretary@example.com>
To: {tier.user}
Cc: {cc}
Bcc: hidden@example.org
Mail-Followup-To: list@example.org
X-Priority: 1 (Highest)
Subject: Every header row

Rows.""",
    )
    before = settings_change(message_details=True)
    try:
        row_click("Every header row")
        wait("document.querySelectorAll('.details th').length > 0")
        titles = js("return [...document.querySelectorAll('.details th')].map(t => t.textContent)")
        assert titles[:8] == ["From", "Sender", "To", "Cc", "Bcc", "Followup-To", "Date", "Priority"], titles  # then sender checks
        cells = "[...document.querySelectorAll('.details tr')].find(r => r.querySelector('th').textContent === 'Cc')"
        assert js(f"return {cells}.querySelectorAll('.address').length") == 10
        assert js(f"return {cells}.querySelector('.more').textContent") == "2 more..."
        row = "[...document.querySelectorAll('.details tr')].find(r => r.querySelector('th').textContent === 'Priority')"
        assert js(f"return {row}.querySelector('td').textContent") == "Highest"
        js(f"{cells}.querySelector('.more').click()")
        wait("document.querySelectorAll('.dialog .address').length === 12")
        js("[...document.querySelectorAll('.dialog button')].find(b => b.textContent === 'Close').click()")
        # The link toggles, each click saved after the one before was answered: a second save once ran the
        # toggle twice and left the table open.
        details = "[...document.querySelectorAll('.header-links button')].find(b => b.textContent === 'Details')"
        for shown in (False, True):
            js(f"{details}.click()")
            wait(f"!!document.querySelector('.details') === {json.dumps(shown)}")
            deadline = time.monotonic() + 10
            while request("GET", "api/settings")[1]["message_details"] != shown:
                assert time.monotonic() < deadline, f"Details {shown} never saved"
                time.sleep(0.2)
    finally:
        settings_change(message_details=before["message_details"])


def address_names() -> None:
    # Roundcube's address_string: a display name holding an address gives way to the address, and "Show email address
    # with display name" shows both.
    deliver_raw(
        "Your bank account",
        f"""From: "security@bank.example" <alerts@phish.example>
To: {tier.user}
Subject: Your bank account

Please confirm.""",
    )
    row_click("Your bank account")
    wait("document.querySelector('.summary')?.textContent.includes('alerts@phish.example')")
    assert "security@bank" not in js("return document.querySelector('.summary').textContent")
    before = settings_change(show_email=True)
    try:
        wait("document.querySelector('.summary')?.textContent.includes('security@bank.example <alerts@phish.example>')")
    finally:
        settings_change(show_email=before["show_email"])
    # An address that is not valid keeps no name of its own: the name alone would hide it, as here.
    deliver_raw("Invalid sender", f'From: "security@bank.example" <alerts@phish>\nTo: {tier.user}\nSubject: Invalid sender\n\nx')
    row_click("Invalid sender")
    wait("document.querySelector('.summary')?.textContent.includes('security@bank.example <alerts@phish>')")
    # A reply's chip shows the address too when the Reply-To's name is one.
    deliver_raw(
        "Reply here",
        f'From: B <b@example.net>\nReply-To: "boss@example.com" <attacker@evil.example>\nTo: {tier.user}\n'
        "Subject: Reply here\n\nx",
    )
    row_click("Reply here")
    tool("Reply")
    names = "[...document.querySelectorAll('.compose .recipient .recipient-name')].map(n => n.textContent.trim())"
    wait(f"JSON.stringify({names}) === JSON.stringify(['attacker@evil.example'])", 30)
    discard()


def look_alike_domains() -> None:
    # Roundcube's notice for an address whose domain mixes scripts, as a look-alike does; one script alone is fine.
    look_alike = "xn--" + "раураl".encode("punycode").decode()  # Cyrillic letters with a Latin l
    cyrillic = ".".join("xn--" + label.encode("punycode").decode() for label in ["пошта", "укр"])
    deliver_raw(
        "Payment overdue",
        f"""From: Pay <service@{look_alike}.com>
To: {tier.user}
Subject: Payment overdue

Pay now.""",
    )
    deliver_raw(
        "From a Cyrillic domain",
        f"""From: Ivan <ivan@{cyrillic}>
To: {tier.user}
Subject: From a Cyrillic domain

Hello.""",
    )
    suspicious = "[...document.querySelectorAll('.notice')].some(n => n.textContent.includes('suspicious email addresses'))"
    row_click("Payment overdue")
    wait(suspicious)
    row_click("From a Cyrillic domain")
    assert not js(f"return {suspicious}")


def displaying_labels() -> None:
    # Roundcube's names in Displaying Messages.
    wd("click", "button[title='Settings']")
    try:
        wait("document.querySelectorAll('.setting > span').length > 0")
        labels = js("return [...document.querySelectorAll('.setting > span')].map(s => s.textContent)")
        assert "Display HTML" in labels and "Show email address with display name" in labels, labels
    finally:
        wd("click", "button[title='Close settings']")
        wait("!!document.querySelector('.list')")


def old_zone_name_shown() -> None:
    # Chromium's Intl still names Kyiv's zone Europe/Kiev: Settings shows Auto and the list with Europe/Kyiv instead.
    before = settings_change(time_zone=None)
    wd("cdp", "Emulation.setTimezoneOverride", json.dumps({"timezoneId": "Europe/Kiev"}))
    try:
        wd("click", "button[title='Settings']")
        options = wait(
            "document.querySelector('select[aria-label=\"Time zone\"]')?.options.length > 1"
            " && [...document.querySelector('select[aria-label=\"Time zone\"]').options].map(o => o.textContent)"
        )
        assert options[0] == "Auto (Europe/Kyiv)" and "Europe/Kyiv" in options and "Europe/Kiev" not in options, options[:3]
    finally:
        wd("cdp", "Emulation.setTimezoneOverride", json.dumps({"timezoneId": ""}))
        settings_change(time_zone=before["time_zone"])
        wd("click", "button[title='Close settings']")
        wait("!!document.querySelector('.list')")


def settings_saved_in_turn() -> None:
    # Two switches flipped in the same moment both stay: this tab's saves go one at a time, so the second is not
    # refused as if another tab had saved first ("Settings not saved: the settings were changed in another tab").
    status, before = request("GET", "api/settings")
    assert status == 200, before
    wd("click", "button[title='Settings']")
    try:
        wait("document.querySelectorAll('.setting > span').length > 0")
        js("""for (const label of ['Display HTML', 'Show email address with display name'])
            [...document.querySelectorAll('.setting')].find(s => s.querySelector('span')?.textContent === label)
                .querySelector('input').click();""")
        time.sleep(2)
        toast = js("return document.querySelector('.toast')?.textContent ?? ''")
        assert not toast.startswith("Settings not saved"), toast
        status, after = request("GET", "api/settings")
        flipped = {k: after[k] != before[k] for k in ["prefer_html", "show_email"]}
        assert all(flipped.values()), flipped
    finally:
        wd("click", "button[title='Close settings']")
        wait("!!document.querySelector('.list')")
        settings_change(prefer_html=before["prefer_html"], show_email=before["show_email"])


def refused_save_shows_stored() -> None:
    # A save the server refuses shows the stored settings again, as Roundcube's form after a failed save: a Special
    # Folders choice and a switch alike. The page's own fetch refuses every save.
    trash, switch = setting_field("Trash"), setting_field("Display HTML")
    shown = f"{trash}.options[{trash}.selectedIndex].text"
    wd("click", "button[title='Settings']")
    js("""window.kubiclePlain = window.fetch;
        window.fetch = (url, init) => url === 'api/settings' && init?.method === 'POST' ? Promise.resolve(new Response(
            JSON.stringify({message: 'refused'}), {status: 400, headers: {'content-type': 'application/json'}}))
            : kubiclePlain(url, init);""")
    try:
        wait(f"!!{trash} && !!{switch}")
        stored, checked = js(f"return [{shown}, {switch}.checked]")
        js(
            f"const s = {trash}; s.value = [...s.options].find(o => o.text === 'Junk').value;"
            " s.dispatchEvent(new Event('change', {bubbles: true}));"
        )
        wait(
            f"document.querySelector('.toast')?.textContent === 'Settings not saved: refused' && {shown} === {json.dumps(stored)}",
            10,
        )
        assert js(f"{switch}.click(); return {switch}.checked") != checked, "the switch did not flip"
        wait(f"{switch}.checked === {json.dumps(checked)}", 10)
    finally:
        js("window.fetch = window.kubiclePlain")
        wd("click", "button[title='Close settings']")
        wait("!!document.querySelector('.list')")


def encrypted_notice() -> None:
    # Roundcube's sentence in place of an encrypted part; only the payload is listed, not PGP/MIME's control part.
    uid = deliver_raw(
        "Encrypted PGP",
        f"""From: Spy <spy@example.com>
To: {tier.user}
Subject: Encrypted PGP
Content-Type: multipart/encrypted; protocol="application/pgp-encrypted"; boundary=b

--b
Content-Type: application/pgp-encrypted

Version: 1
--b
Content-Type: application/octet-stream; name=encrypted.asc

-----BEGIN PGP MESSAGE-----
hQEMA0x
-----END PGP MESSAGE-----
--b--""",
    )
    smime = deliver_raw(
        "Encrypted S/MIME",
        f"""From: Office <office@example.com>
To: {tier.user}
Subject: Encrypted S/MIME
Content-Type: application/pkcs7-mime; smime-type=enveloped-data; name=smime.p7m
Content-Disposition: attachment; filename=smime.p7m
Content-Transfer-Encoding: base64

MIAGCSqGSIb3DQEHA6CAMIACAQAx""",
    )
    for u, name in [(uid, "encrypted.asc"), (smime, "smime.p7m")]:
        m = message(u)
        notice = "This is an encrypted message and can not be displayed. Sorry!"
        assert body_text(m).strip() == notice and [a["filename"] for a in m["attachments"]] == [name], (
            m["body"],
            m["attachments"],
        )
    row_click("Encrypted PGP")
    wait(f"{FRAME}.querySelector('.part-notice')?.textContent === 'This is an encrypted message and can not be displayed. Sorry!'")


def report_parts() -> None:
    # A bounce's machine parts show as text below its human part, not as nameless files.
    bounce = deliver_raw(
        "Undelivered Mail",
        f"""From: MAILER-DAEMON@example.test
To: {tier.user}
Subject: Undelivered Mail
Content-Type: multipart/report; report-type=delivery-status; boundary=b

--b
Content-Type: text/plain

I could not deliver your message.
--b
Content-Type: message/delivery-status

Reporting-MTA: dns; mail.example.test

Final-Recipient: rfc822; nobody@example.net
Status: 5.1.1
--b
Content-Type: text/rfc822-headers

From: {tier.user}
Subject: The lost one
--b--""",
    )
    m = message(bounce)
    assert "Status: 5.1.1" in body_text(m) and "Subject: The lost one" in body_text(m) and m["attachments"] == [], m


def enriched_text() -> None:
    # text/enriched (RFC 1896) shows formatted, as Roundcube's rcube_enriched converts it.
    enriched = deliver_raw(
        "Enriched text",
        f"""From: Old <old@example.com>
To: {tier.user}
Subject: Enriched text
Content-Type: text/enriched

<bold>Hi</bold> there""",
    )
    assert "<b>Hi</b>" in message(enriched)["body"]["document"]


def uuencoded_files() -> None:
    # Roundcube's uu_decode: a uuencoded block in plain text becomes a file and leaves the text.
    uu = deliver_raw(
        "Uuencoded patch",
        f"""From: Hacker <hacker@example.com>
To: {tier.user}
Subject: Uuencoded patch
Content-Type: text/plain

Here it is.
begin 644 hello.txt
%:&5L;&\\`
`
end
Thanks""",
    )
    m = message(uu)
    assert [a["filename"] for a in m["attachments"]] == ["hello.txt"] and "begin 644" not in body_text(m), m
    assert fetched(m["attachments"][0]["url"])["text"] == "hello"


def summary_fallback() -> None:
    # Roundcube's summary in Drafts: To, else Cc, else Bcc; with nobody at all, "On <date>".
    for subject, field in [("Only a blind copy", "bcc"), ("Nobody yet", None)]:
        draft = new_draft()
        draft.update(subject=subject, body="x")
        if field:
            draft[field] = [{"name": None, "email": "blind@example.org"}]
        assert request("POST", "api/draft", draft)[0] == 200
    try:
        open_folder("Drafts")
        row_click("Only a blind copy")
        wait("document.querySelector('.summary')?.textContent.startsWith('To blind@example.org on ')")
        row_click("Nobody yet")
        wait("document.querySelector('.summary')?.textContent.startsWith('On ')")
    finally:
        open_folder("Inbox")


def key(name: str, target: str = "document", **mods) -> None:
    """A keydown of `name` on `target`, a script expression, with modifiers such as ctrlKey=True."""
    init = json.dumps({"key": name, "bubbles": True, **mods})
    js(f"{target}.dispatchEvent(new KeyboardEvent('keydown', {init}))")


def row_of(subject: str) -> str:
    """A script expression for the list row with `subject`."""
    return (
        f"[...document.querySelectorAll('.row')].find(r => r.querySelector('.row-subject').textContent === {json.dumps(subject)})"
    )


OPEN_SUBJECT = "document.querySelector('.message-subject')?.textContent"
SELECTED = "document.querySelectorAll('.row.selected').length"


def list_cursor() -> None:
    # Roundcube's list keeps the focused row apart from the selection: Ctrl+arrows move the focus alone, Ctrl+Space
    # toggles the focused row and clears the preview (`msglist_select`), Space opens it as the only selection,
    # Shift+End selects to the last row, and Ctrl+Shift+click adds a range to the selection.
    subjects = [f"Cursor {word}" for word in ["one", "two", "three", "four", "five"]]
    for s in subjects:
        deliver_raw(s, f"From: C <c@example.net>\nTo: {tier.user}\nSubject: {s}\n\nx")
    open_folder("Inbox")
    wait(f"{json.dumps(subjects)}.every(s => {SUBJECTS}.includes(s))", 60)
    shown = [s for s in js(f"return {SUBJECTS}") if s in subjects]
    row_click(shown[0])
    for _ in range(2):
        key("ArrowDown", ctrlKey=True)
    key(" ", ctrlKey=True)
    wait(f"{SELECTED} === 2 && {row_of(shown[2])}.classList.contains('selected')", 10)
    seen = js(f"return [{OPEN_SUBJECT}, document.querySelector('.row.focused .row-subject')?.textContent]")
    assert seen == [None, shown[2]], (seen, shown[:3])
    assert js("return document.querySelector('.reader-empty p')?.textContent") == "2 messages selected"
    key("End", shiftKey=True)
    rows = js("return document.querySelectorAll('.row').length")
    first = js(f"return [...document.querySelectorAll('.row')].indexOf({row_of(shown[2])})")
    wait(f"{SELECTED} === {rows - first}", 10)
    assert js(f"return [...document.querySelectorAll('.row')].slice({first}).every(r => r.classList.contains('selected'))")
    row_click(shown[0])

    def click(subject: str, **mods: bool) -> None:
        js(f"""{row_of(subject)}.querySelector('.row-subject').dispatchEvent(new MouseEvent('click',
        {json.dumps({"bubbles": True, **mods})}))""")

    click(shown[4], ctrlKey=True)
    click(shown[2], ctrlKey=True, shiftKey=True)
    wait(f"{SELECTED} === 4", 10)
    assert not js(f"return {row_of(shown[1])}.classList.contains('selected')"), "Ctrl+Shift+click lost the rows before"
    key("ArrowUp", ctrlKey=True)
    key(" ")
    wait(f"{OPEN_SUBJECT} === {json.dumps(shown[1])} && {SELECTED} === 1", 10)
    # Roundcube's `multi_selecting`: a selection Ctrl or Shift made opens no message, back to one row included.
    for name, ctrl in [(" ", True), ("ArrowDown", True), (" ", True), ("ArrowDown", False), ("ArrowUp", False)]:
        key(name, ctrlKey=ctrl, shiftKey=not ctrl)
    wait(f"{SELECTED} === 1 && {row_of(shown[2])}.classList.contains('selected')", 10)
    assert js(f"return {OPEN_SUBJECT} ?? null") is None, "a Ctrl or Shift selection opened a message"
    # With nothing selected an arrow moves the focus alone, as Roundcube's `use_arrow_key`; Enter opens that row.
    key("Escape")
    key("ArrowDown")
    focused = "document.querySelector('.row.focused .row-subject')?.textContent"
    wait(f"{focused} === {json.dumps(shown[3])} && {SELECTED} === 0", 10)
    assert js(f"return {OPEN_SUBJECT} ?? null") is None, "an arrow with nothing selected opened a message"
    key("Enter")
    wait(f"{OPEN_SUBJECT} === {json.dumps(shown[3])} && {SELECTED} === 1", 10)


def thread_message(subject: str, own: str, parents: list) -> str:
    refs = " ".join(f"<{p}@example.net>" for p in parents)
    head = f"\nIn-Reply-To: <{parents[-1]}@example.net>\nReferences: {refs}" if parents else ""
    return f"From: T <t@example.net>\nTo: {tier.user}\nSubject: {subject}\nMessage-ID: <{own}@example.net>{head}\n\nx"


def thread_folder(check) -> None:
    """Runs `check` on a threaded folder Thr holding "Thread root", "Thread reply one" below it and "Thread reply two"
    below that, all unread and collapsed, given their UIDs; then removes the folder."""
    create("Thr")
    settings = settings_change()
    try:
        settings_change(folders={**settings["folders"], "Thr": {"threads": True}})
        chain = [
            ("Thread root", "thr0", []),
            ("Thread reply one", "thr1", ["thr0"]),
            ("Thread reply two", "thr2", ["thr0", "thr1"]),
        ]
        uids = {}
        for subject, own, parents in chain:
            move(deliver_raw(subject, thread_message(subject, own, parents)), "Thr")
            uids[subject] = arrived("Thr", subject)
        open_folder("Thr")
        wait("document.querySelectorAll('.row').length === 1 && !!document.querySelector('.row .twisty')", 60)
        check(uids)
    finally:
        latest = settings_change()
        settings_change(folders={k: v for k, v in latest["folders"].items() if k != "Thr"})
        request("POST", "api/change", {"DeleteFolder": {"name": "Thr"}})
        open_folder("Inbox")


def unroot_underline() -> None:
    # Roundcube's `unroot`: a collapsed thread with unread replies underlines its first message only once that is read;
    # an unread one is bold instead.
    def check(uids: dict) -> None:
        assert not js(f"return {row_of('Thread root')}.classList.contains('unroot')"), "an unread root is underlined"
        machine.succeed(f"{tier.doveadm} flags add -u {tier.user} '\\Seen' mailbox Thr uid {uids['Thread root']}")
        wait(f"{row_of('Thread root')}?.classList.contains('unroot')", 30)

    thread_folder(check)


def thread_arrows() -> None:
    # Roundcube's Left and Right in a thread: Right expands, then goes to the first reply; Left goes to the message a
    # reply answers, Ctrl+Left to the thread's first, and collapses an expanded thread.
    def check(_uids: dict) -> None:
        row_click("Thread root")
        key("ArrowRight")
        wait("document.querySelectorAll('.row').length === 3", 10)
        key("ArrowRight")
        wait(f"{OPEN_SUBJECT} === 'Thread reply one'", 10)
        key("ArrowDown")
        wait(f"{OPEN_SUBJECT} === 'Thread reply two'", 10)
        key("ArrowLeft")
        wait(f"{OPEN_SUBJECT} === 'Thread reply one'", 10)
        key("ArrowDown")
        wait(f"{OPEN_SUBJECT} === 'Thread reply two'", 10)
        key("ArrowLeft", ctrlKey=True)
        wait(f"{OPEN_SUBJECT} === 'Thread root'", 10)
        key("ArrowLeft")
        wait(f"document.querySelectorAll('.row').length === 1 && {OPEN_SUBJECT} === 'Thread root' && {SELECTED} === 1", 10)

    thread_folder(check)


def folder_with_unread_child(parent: str, child: str) -> None:
    create(parent)
    create(child, parent)
    subject = f"Unread in {parent}.{child}"
    move(deliver_raw(subject, f"From: K <k@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx"), f"{parent}.{child}")
    arrived(f"{parent}.{child}", subject)


def collapse_opens_parent() -> None:
    # Roundcube's folder_collapsed: collapsing a folder that holds the open one opens the collapsed folder.
    folder_with_unread_child("Par", "Kid")
    try:
        wait(f"!!{folder_button('Kid')}", 30)
        open_folder("Kid")
        js(f"{folder_button('Par')}.closest('li').querySelector('.folder-twisty').click()")
        wait("document.querySelector('.folder.active .folder-name')?.textContent === 'Par' && location.hash === '#folder=Par'", 10)
    finally:
        latest = settings_change()
        settings_change(folders={k: v for k, v in latest["folders"].items() if not k.startswith("Par")})
        request("POST", "api/change", {"DeleteFolder": {"name": "Par"}})
        open_folder("Inbox")


def collapsed_parent_bold() -> None:
    # Roundcube's set_unread_count_display: a collapsed folder is bold while a subfolder has unread mail, its badge
    # still its own count.
    folder_with_unread_child("Bold", "Inner")
    settings = settings_change()
    try:
        settings_change(folders={**settings["folders"], "Bold": {"collapsed": True}})
        wait(f"!!{folder_button('Bold')} && !{folder_button('Inner')}", 30)
        wait(f"{folder_button('Bold')}.classList.contains('unread') && !{folder_button('Bold')}.querySelector('.badge')", 10)
    finally:
        latest = settings_change()
        settings_change(folders={k: v for k, v in latest["folders"].items() if not k.startswith("Bold")})
        request("POST", "api/change", {"DeleteFolder": {"name": "Bold"}})


def drop_modifiers() -> None:
    # Roundcube's drag menu: only Shift asks Move or Copy; a drop with Ctrl moves at once.
    subjects = ["Dropped with Ctrl", "Dropped with Shift"]
    for s in subjects:
        deliver_raw(s, f"From: D <d@example.net>\nTo: {tier.user}\nSubject: {s}\n\nx")
    open_folder("Inbox")

    def drop(subject: str, mod: str) -> None:
        js(f"""{row_of(subject)}.dispatchEvent(new DragEvent('dragstart', {{bubbles: true, dataTransfer: new DataTransfer()}}));
        {folder_button("Archive")}.dispatchEvent(new DragEvent('drop',
            {{bubbles: true, cancelable: true, {mod}: true, dataTransfer: new DataTransfer()}}))""")

    wait(f"{json.dumps(subjects)}.every(s => {SUBJECTS}.includes(s))", 60)
    drop(subjects[0], "ctrlKey")
    time.sleep(0.5)
    menu = js("return !!document.querySelector('.menu')")
    if menu:
        key("Escape")
    assert not menu, "a drop with Ctrl asked Move or Copy"
    arrived("Archive", subjects[0])
    drop(subjects[1], "shiftKey")
    wait("[...document.querySelectorAll('.menu .menu-item')].map(b => b.textContent.trim()).join() === 'Move here,Copy here'", 10)
    key("Escape")


def arrival_column() -> None:
    # Roundcube's list shows the arrival time in the date column under Arrival sort, the Date header otherwise.
    create("Arr")
    subject = "Written three days ago"
    written = time.strftime("%a, %d %b %Y %H:%M:%S +0000", time.gmtime(time.time() - 3 * 86400))
    move(deliver_raw(subject, f"From: A <a@example.net>\nTo: {tier.user}\nSubject: {subject}\nDate: {written}\n\nx"), "Arr")
    arrived("Arr", subject)
    settings = settings_change()
    date = f"{row_of(subject)}?.querySelector('.row-date').textContent"
    try:
        settings_change(folders={**settings["folders"], "Arr": {"sort": "Arrival"}})
        open_folder("Arr")
        wait(f"/^\\d\\d:\\d\\d$/.test({date})", 30)
        latest = settings_change()
        settings_change(folders={**latest["folders"], "Arr": {"sort": "Date"}})
        wait(f"/^[A-Z][a-z][a-z] \\d\\d:\\d\\d$/.test({date})", 30)
    finally:
        latest = settings_change()
        settings_change(folders={k: v for k, v in latest["folders"].items() if k != "Arr"})
        request("POST", "api/change", {"DeleteFolder": {"name": "Arr"}})
        open_folder("Inbox")


def sent_subfolder_recipients() -> None:
    # Roundcube's message_list_smart_column_name: Sent's and Drafts' subfolders list the recipients too, up to three
    # with each address as its tooltip and "..." after, and From/To sorts them by To.
    create("2024", "Sent")
    four = "Olena <olena@example.com>, Petro <petro@example.com>, Ivan <ivan@example.com>, Taras <taras@example.com>"
    mails = [
        ("Sent to four", "Aaron", four),
        ("Sent to Abe", "Zoe", "Abe <abe@example.com>"),
        ("Sent to Zed", "Abby", "Zed <zed@example.com>"),
    ]
    try:
        for subject, sender, to in mails:
            move(deliver_raw(subject, f"From: {sender} <{tier.user}>\nTo: {to}\nSubject: {subject}\n\nx"), "Sent.2024")
            arrived("Sent.2024", subject)
        open_folder("2024")
        cell = f"{row_of('Sent to four')}?.querySelector('.row-from')"
        wait(f"{cell}?.textContent === 'Olena, Petro, Ivan...'", 30)
        titles = js(f"return [...{cell}.querySelectorAll('span[title]')].map(s => s.title)")
        assert titles == ["olena@example.com", "petro@example.com", "ivan@example.com"], titles
        status, page = request("GET", "api/list?folder=Sent.2024&page=0&size=50&sort=FromTo&order=Ascending&threads=false")
        assert status == 200, (status, page)
        order = [r["row"]["subject"] for r in page["rows"]]
        assert order.index("Sent to Abe") < order.index("Sent to Zed"), order
    finally:
        request("POST", "api/change", {"DeleteFolder": {"name": "Sent.2024"}})
        open_folder("Inbox")


def list_sender_names() -> None:
    # Roundcube's address_string in the list: a name holding an @ gives way to the address, which every name has as
    # its tooltip.
    subject = "Name that looks like an address"
    deliver_raw(subject, f'From: "support@bank.example" <x@phish.example>\nTo: {tier.user}\nSubject: {subject}\n\nx')
    open_folder("Inbox")
    cell = f"{row_of(subject)}?.querySelector('.row-from span')"
    wait(f"{cell}?.textContent === 'x@phish.example' && {cell}.title === 'x@phish.example'", 30)


def search_title() -> None:
    # Roundcube's list_pagetitle: the tab says "Search result" while results show.
    deliver_raw("Title probe", f"From: T <t@example.net>\nTo: {tier.user}\nSubject: Title probe\n\nx")
    open_folder("Inbox")
    wd("type", "#search", "title probe")
    wait("document.title.endsWith(':: Search result')", 30)
    key("Escape")
    wait("document.title.endsWith(':: Inbox')", 10)


def special_resubscribe() -> None:
    # Roundcube's folder manager locks a special folder only while subscribed, so one unsubscribed elsewhere can come
    # back. Dovecot keeps its own `auto = subscribe` folders subscribed, so the Archive here is one chosen in Settings.
    # Kept shows as "Archive" (role names), as does the server's own Archive once demoted, so the pane counts
    # "Archive" rows and the folder manager's Kept is the unchecked one.
    create("Kept")
    before = settings_change(special_folders={"Archive": "Kept"})
    box = "input[aria-label='Subscribe to Archive']:not(:checked)"
    archives = "[...document.querySelectorAll('.folder .folder-name')].filter(n => n.textContent === 'Archive').length"
    try:
        wait(f"{archives} === 2", 30)
        assert request("POST", "api/change", {"Subscribe": {"name": "Kept", "subscribed": False}})[0] == 204
        wait(f"{archives} === 1", 30)
        js("[...document.querySelectorAll('.folders-bottom .tool')].find(b => b.textContent.trim() === 'Settings').click()")
        wait(f"!!document.querySelector({json.dumps(box)})", 30)
        assert not js(f"return document.querySelector({json.dumps(box)}).disabled"), (
            "an unsubscribed special folder cannot come back"
        )
        js(f"document.querySelector({json.dumps(box)}).click()")
        wait(f"{archives} === 2", 30)
    finally:
        settings_change(special_folders=before["special_folders"])
        request("POST", "api/change", {"DeleteFolder": {"name": "Kept"}})
        open_folder("Inbox")


def virtual_parents() -> None:
    # Roundcube's folder pane shows an unsubscribed parent of a subscribed folder as a row that does not open, so the
    # subfolder keeps its place under it.
    create("Vp")
    create("Leaf", "Vp")
    try:
        assert request("POST", "api/change", {"Subscribe": {"name": "Vp", "subscribed": False}})[0] == 204
        wait(f"{folder_button('Vp')}?.classList.contains('noselect') && !!{folder_button('Leaf')}", 30)
        assert js(f"return {folder_button('Vp')}.closest('li').nextElementSibling === {folder_button('Leaf')}.closest('li')")
    finally:
        request("POST", "api/change", {"DeleteFolder": {"name": "Vp"}})


def hidden_list_keys() -> None:
    # Where the folder column stands in the list's place, the list takes no keys, as Roundcube's list takes keys only
    # while focused: Enter, Delete and the letters leave the hidden list's message alone. Enter on a folder opens it.
    subject = "Hidden list keys"
    uid = deliver_raw(subject, f"From: H <h@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
    open_folder("Inbox")
    row_click(subject)
    metrics = {"width": 1024, "height": 768, "deviceScaleFactor": 1, "mobile": False}
    wd("cdp", "Emulation.setDeviceMetricsOverride", json.dumps(metrics))
    try:
        wait("getComputedStyle(document.querySelector('.folders-button')).display !== 'none'")
        js("document.querySelector('.folders-button').click()")
        wait("document.querySelector('.app').dataset.pane === 'folders'")
        js("document.activeElement.blur()")
        for name in ["s", "Enter", "Delete"]:
            key(name)
        time.sleep(1)
        assert js("return document.querySelector('.app').dataset.pane") == "folders", "a key left the folder column"
        assert "\\Flagged" not in flags_of("INBOX", str(uid)), "s flagged the hidden list's message"
        assert uid_in("INBOX", subject) == str(uid), "Delete moved the hidden list's message"
        wd("type", ".folder-list li[data-folder='INBOX.finance'] .folder", "\ue007")  # WebDriver's Enter, a real key
        wait("location.hash === '#folder=INBOX.finance'", 10)
    finally:
        wd("cdp", "Emulation.clearDeviceMetricsOverride", "{}")
        open_folder("Inbox")


def message_view_arrows() -> None:
    # In Roundcube's message view, a page of its own, the arrow keys belong to the message, which its frame scrolls:
    # Previous and Next stay on their buttons.
    subject = "A long letter"
    lines = "\n".join(f"Line {n} of the long letter." for n in range(300))
    deliver_raw(subject, f"From: L <l@example.net>\nTo: {tier.user}\nSubject: {subject}\n\n{lines}")
    open_folder("Inbox")
    row_click(subject)
    js(f"{row_of(subject)}.querySelector('.row-subject').dispatchEvent(new MouseEvent('dblclick', {{bubbles: true}}))")
    wait("document.querySelector('.app').dataset.pane === 'message' && !!document.querySelector('.mail-frame iframe')")
    try:
        for name in ["ArrowDown", "ArrowDown", "ArrowRight", "ArrowLeft", "ArrowUp"]:
            key(name)
        time.sleep(0.5)
        assert js(f"return {OPEN_SUBJECT}") == subject, js(f"return {OPEN_SUBJECT}")
    finally:
        key("Escape")
        wait("document.querySelector('.app').dataset.pane !== 'message'")


def headers_dialog() -> None:
    # Roundcube's Message headers dialog: each name and its colon bold, a folded line indented by four spaces
    # whatever it had, and no wrapping.
    deliver_raw(
        "Folded header", f"From: Fold <fold@example.com>\nTo: {tier.user}\nSubject: Folded header\nX-Test: one\n\t\t  two\n\nBody"
    )
    row_click("Folded header")
    js("[...document.querySelectorAll('.header-links button')].find(b => b.textContent === 'Headers').click()")
    wait("document.querySelector('.dialog h2')?.textContent === 'Message headers' && !!document.querySelector('.raw-headers div')")
    lines = js(
        "return [...document.querySelectorAll('.raw-headers div')]"
        ".map(d => [d.querySelector('b')?.textContent ?? '', d.textContent])"
    )
    at = [bold for bold, _ in lines].index("X-Test:")
    assert lines[at][1] == "X-Test: one" and lines[at + 1] == ["", " " * 4 + "two"], lines
    assert lines[0][0].endswith(":") and lines[-1][1] != "", lines
    assert js("return getComputedStyle(document.querySelector('.raw-headers')).whiteSpace") == "nowrap"
    js("document.querySelector('.dialog-head button[aria-label=Close]').click()")
    wait("!document.querySelector('.dialog')")


def mailto_links() -> None:
    # Roundcube's washtml_link_callback: a mailto: link keeps only the addresses kubicle can send to, has no link
    # without one, and a link without an @ goes to the browser as it is.

    uid = deliver_raw(
        "Mailto links",
        f"""From: Links <links@example.com>
To: {tier.user}
Subject: Mailto links
Content-Type: text/html; charset=utf-8

<p><a href="mailto:a@example.com?subject=Hi">valid</a> <a href="mailto:broken@,good@example.org">mixed</a>
<a href="mailto:broken@">invalid</a> <a href="mailto:?subject=Only">no-address</a></p>""",
    )
    document = message(uid)["body"]["document"]
    hrefs = {}
    for attributes, text in re.findall(r"<a([^>]*)>([^<]*)</a>", document):
        href = re.search(r'href="([^"]*)"', attributes)
        hrefs[text] = html.unescape(href.group(1)) if href else None

    def compose(href: str | None) -> str | None:
        return urllib.parse.unquote(href.split("#compose=", 1)[1]) if href and "#compose=" in href else href

    assert compose(hrefs["valid"]) == "mailto:a@example.com?subject=Hi", hrefs
    assert compose(hrefs["mixed"]) == "mailto:good@example.org", hrefs
    assert hrefs["invalid"] is None and hrefs["no-address"] == "mailto:?subject=Only", hrefs


def eml_names_and_source() -> None:
    # Roundcube's names: an .eml after filename_from_subject, an unnamed invite outside an alternative as
    # calendar.ics; Show source labelled with the charset the mail declares, UTF-8 for a multipart.
    def head(url: str) -> dict:
        return json.loads(
            wd(
                "async",
                f"""fetch({json.dumps(url)}).then(r => done({{
        type: r.headers.get('content-type'), disposition: r.headers.get('content-disposition')}}))""",
            )
        )

    uid = deliver_raw(
        "Re: Invoice 4/2026", f"From: Acc <acc@example.com>\nTo: {tier.user}\nSubject: Re: Invoice 4/2026\n\nSee it."
    )
    saved = head(f"api/download?folder=INBOX&uid={uid}")["disposition"]
    assert 'filename="Re Invoice 4 2026.eml"' in saved, saved
    invite = deliver_raw(
        "Team meeting invite",
        f"""From: Cal <cal@example.com>
To: {tier.user}
Subject: Team meeting invite
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: text/plain; charset=utf-8

You are invited.
--b
Content-Type: text/calendar; charset=utf-8; method=REQUEST

BEGIN:VCALENDAR
END:VCALENDAR
--b--""",
    )
    assert [a["filename"] for a in message(invite)["attachments"]] == ["calendar.ics"], message(invite)["attachments"]
    assert head(f"api/source?folder=INBOX&uid={invite}")["type"] == "text/plain; charset=utf-8"
    koi8 = deliver_raw(
        "Old mailer",
        f"From: Old <old@example.com>\nTo: {tier.user}\nSubject: Old mailer\nContent-Type: text/plain; charset=koi8-r\n\nhi",
    )
    assert head(f"api/source?folder=INBOX&uid={koi8}")["type"] == "text/plain; charset=koi8-r"


def forward_carries() -> None:
    # Roundcube's write_compose_attachments for a plain-text editor: a forward carries files marked attachment or
    # named, an inline PDF, never an unnamed invite in an alternative nor, in a mail with HTML, an inline photo.
    uid = deliver_raw(
        "Forward what counts",
        f"""From: Fw <fw@example.com>
To: {tier.user}
Subject: Forward what counts
Content-Type: multipart/mixed; boundary=b

--b
Content-Type: multipart/alternative; boundary=a

--a
Content-Type: text/plain; charset=utf-8

Meeting.
--a
Content-Type: text/html; charset=utf-8

<p>Meeting.</p>
--a
Content-Type: text/calendar; charset=utf-8; method=REQUEST

BEGIN:VCALENDAR
END:VCALENDAR
--a--
--b
Content-Type: image/jpeg; name=photo.jpg
Content-Disposition: inline; filename=photo.jpg
Content-Transfer-Encoding: base64

/9j/2Q==
--b
Content-Type: application/pdf; name=inline.pdf
Content-Disposition: inline; filename=inline.pdf

%PDF-1.4 inline
--b
Content-Type: application/pdf; name=report.pdf
Content-Disposition: attachment; filename=report.pdf

%PDF-1.4 report
--b--""",
    )
    carried = [a["filename"] for a in prefilled("Forward", uid)["attachments"]]
    assert carried == ["inline.pdf", "report.pdf"], carried


def sent_headers() -> None:
    # Roundcube's headers_input: the read-receipt request names the sender as From does, X-Sender is the bare
    # address; a file's name goes in Content-Type's name as RFC 2047 too; the Date is in the zone chosen in Settings,
    # else the server's (Europe/Kyiv here).
    stored = json.loads(upload("new Blob(['%PDF-1.4 report'], {type: 'application/pdf'})", "Звіт.pdf")[1])
    before = settings_change(time_zone="Asia/Tokyo")
    try:
        draft = new_draft()
        draft.update(
            to=[{"name": None, "email": tier.user}],
            subject="Receipt and zone",
            body="x",
            attachments=[stored],
            return_receipt=True,
            **{"from": {"name": "Receipt Tester", "email": tier.user}},
        )
        assert request("POST", "api/send", draft)[0] == 204
    finally:
        settings_change(time_zone=before["time_zone"])
    sent = arrived("Sent", "Receipt and zone")
    head = fetch("hdr", "Sent", sent)
    assert re.search(rf'(?im)^disposition-notification-to: "?Receipt Tester"? <{re.escape(tier.user)}>', head), head
    assert re.search(rf"(?im)^x-sender: {re.escape(tier.user)}\s*$", head), head
    assert re.search(r"(?im)^date: [^\n]*\+0900\s*$", head), head
    body = fetch("text", "Sent", sent)
    assert 'name="=?UTF-8?B?' in body and "filename*=UTF-8''%D0%97" in body, body
    draft = new_draft()
    draft.update(to=[{"name": None, "email": tier.user}], subject="Zone of the server", body="x")
    assert request("POST", "api/send", draft)[0] == 204
    head = fetch("hdr", "Sent", arrived("Sent", "Zone of the server"))
    assert re.search(r"(?im)^date: [^\n]*\+0[23]00\s*$", head), head


def setting_field(label: str) -> str:
    """A script expression for the input of the Settings row labelled `label`."""
    return (
        "[...document.querySelectorAll('.setting')]"
        f".find(s => s.querySelector('span')?.textContent === {json.dumps(label)})?.querySelector('input, select')"
    )


def rows_per_page() -> None:
    # Roundcube's "Rows per page" in Mailbox View: any number saves, clamped to 2..200 as prefs_save.php does, and
    # the field shows what was saved.
    before = request("GET", "api/settings")[1]
    wd("click", "button[title='Settings']")
    try:
        for asked, kept in [(1000, 200), (1, 2)]:
            settings = request("GET", "api/settings")[1]
            status, answer = request("POST", "api/settings", {**settings, "page_size": asked})
            assert status == 200, (asked, status, answer)
            assert request("GET", "api/settings")[1]["page_size"] == kept, asked
        field = setting_field("Rows per page")
        wait(f"!!{field}")
        js(f"const f = {field}; f.value = '1000'; f.dispatchEvent(new Event('change', {{bubbles: true}}));")
        wait(f"{field}.value === '200'")
        for _ in range(50):  # the save is on its way
            if request("GET", "api/settings")[1]["page_size"] == 200:
                break
            time.sleep(0.1)
        assert request("GET", "api/settings")[1]["page_size"] == 200
    finally:
        wd("click", "button[title='Close settings']")
        wait("!!document.querySelector('.list')")
        settings_change(page_size=before["page_size"])


def role_names() -> None:
    # Roundcube names a special folder by its role while show_real_foldernames is off (rcmail_action.php), as
    # Mailu leaves it: "Sent Items" chosen for Sent shows as Sent.
    assert request("POST", "api/change", {"CreateFolder": {"name": "Sent Items"}})[0] == 204
    names = "[...document.querySelectorAll('.folder-name')].map(e => e.textContent)"
    before = settings_change(special_folders={"Sent": "Sent Items"})
    try:
        wait(f"!{names}.includes('Sent Items') && {names}.filter(n => n === 'Sent').length === 2")
    finally:
        settings_change(special_folders=before["special_folders"])
        assert request("POST", "api/change", {"DeleteFolder": {"name": "Sent Items"}})[0] == 204


def special_folder_choices() -> None:
    # Roundcube's choices per role: never INBOX or a folder that is not there; None for Sent makes Don't save the
    # Compose default, None for Trash makes Delete delete for good at once, unasked (app.js delete_messages).
    before = request("GET", "api/settings")[1]
    special = before["special_folders"]
    try:
        for refused in [{"Trash": "INBOX"}, {"Junk": "No such folder"}]:
            status, answer = request(
                "POST", "api/settings", {**request("GET", "api/settings")[1], "special_folders": {**special, **refused}}
            )
            assert status == 400, (refused, status, answer)
        settings_change(special_folders={**special, "Trash": None, "Sent": None})
        assert new_draft()["sent_copy"] == "NotSaved"
        open_folder("Inbox")
        subject = "Deleted for good without Trash"
        deliver_raw(subject, f"From: Kim <kim@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nGone.")
        trash_before = len(search("Trash", "all"))
        row_click(subject)
        delete_key()
        machine.wait_until_succeeds(
            f"! {tier.doveadm} -o plugin/fts= search -u {tier.user} mailbox INBOX subject {shlex.quote(subject)} | grep -q .",
            timeout=30,
        )
        assert len(search("Trash", "all")) == trash_before, "a message went to Trash"
    finally:
        settings_change(special_folders=special)


def two_roles() -> None:
    # One folder may hold several roles, as Roundcube's <role>_mbox preferences may name one: Trash chosen for
    # Archive, Trash left as the server marks it, stays Trash, so Delete still moves there, and Archive files there.
    # It keeps Trash's name, as Roundcube's folder_classname names it before the archive plugin would.
    special = request("GET", "api/settings")[1]["special_folders"]
    settings_change(special_folders={**special, "Archive": "Trash"})
    try:
        reload()  # the page lists the folders' roles anew
        names = js("return [...document.querySelectorAll('.folder .folder-name')].map(e => e.textContent)")
        assert names.count("Trash") == 1 and names.count("Archive") == 1, names  # Archive, without its role, by its own name
        open_folder("Inbox")
        for subject, press, toast in [
            ("Deleted into a Trash that archives", "Delete", "Message(s) moved to Trash successfully."),
            ("Archived into Trash", "a", "Successfully archived"),
        ]:
            deliver_raw(subject, f"From: Uma <uma@example.net>\nTo: {tier.user}\nSubject: {subject}\n\nx")
            row_click(subject)
            key(press)
            wait(f"document.querySelector('.toast')?.textContent === {json.dumps(toast)}")
            arrived("Trash", subject)
    finally:
        settings_change(special_folders=special)
        reload()


def archive_division() -> None:
    # The archive plugin's "Divide archive by": by month (Archive/2021/06) and by sender email, the delimiter in the
    # address as "-", each folder created on first use.
    before = request("GET", "api/settings")[1]
    created = []
    try:
        for kind, subject, sender, folder in [
            ("Month", "Archived by month", "Kim <kim@example.net>", "Archive.2021.06"),
            ("Sender", "Archived by sender", "Kim Lee <kim.lee@example.net>", "Archive.kim-lee@example-net"),
        ]:
            settings_change(archive_type=kind)
            uid = deliver_raw(
                subject, f"From: {sender}\nTo: {tier.user}\nDate: Tue, 15 Jun 2021 10:00:00 +0000\nSubject: {subject}\n\nx"
            )
            body = {"Archive": {"folder": "INBOX", "messages": {"Uids": [uid]}, "zone": "UTC"}}
            assert request("POST", "api/change", body)[0] == 204
            created.append(".".join(folder.split(".")[:2]))
            arrived(folder, subject)
            assert folder in machine.succeed(f"{tier.doveadm} mailbox list -s -u {tier.user}").split(), folder
    finally:
        settings_change(archive_type=before.get("archive_type", "None"))
        for name in set(created):
            request("POST", "api/change", {"DeleteFolder": {"name": name}})


def suggested(text: str, email: str) -> bool:
    """Whether `api/autocomplete` offers `email` for `text`, within 30 s: a send files its copy in Sent after it
    answers, in the engine's queue."""
    for _ in range(60):
        status, answer = request("GET", f"api/autocomplete?q={text}")
        assert status == 200, (status, answer)
        if email in emails(answer):
            return True
        time.sleep(0.5)
    return False


def autocomplete_sources() -> None:
    # Roundcube's sources: recipients in Sent, Bcc included, and the trusted senders; from one letter (the name's
    # "Ґ", which no other address in Sent holds); Tab takes the suggestion and lets the browser move the focus on
    # (app.js ksearch_keydown).
    subject = "Only a Bcc recipient"
    draft = new_draft()
    draft.update(subject=subject, body="x", bcc=[{"name": "Ґава", "email": "bcc-only@example.com"}])
    assert request("POST", "api/send", draft)[0] == 204
    arrived("Sent", subject)
    assert suggested("bcc-only", "bcc-only@example.com"), "a Bcc recipient is not suggested"
    before = settings_change()
    try:
        settings_change(trusted_senders=[*before["trusted_senders"], "trusted-one@example.org"])
        assert suggested("trusted-one", "trusted-one@example.org"), "a trusted sender is not suggested"
    finally:
        settings_change(trusted_senders=before["trusted_senders"])
    wd("click", ".compose-button")
    try:
        to = ".compose input[aria-label=To]"
        wait(f"!!document.querySelector('{to}')")
        wd("type", to, "ґ")
        wait("document.querySelector('.suggest-list')?.textContent.includes('bcc-only@example.com')")
        tab = """const e = new KeyboardEvent('keydown', {key: 'Tab', bubbles: true, cancelable: true});
            document.querySelector('.compose input[aria-label=To]').dispatchEvent(e); return e.defaultPrevented"""
        assert not js(tab), "Tab kept the focus in To"
        wait(f"{CHIPS}.some(c => c.includes('bcc-only@example.com'))")
    finally:
        discard()


def drop_mailu_session() -> None:
    """Ends Mailu's session as its timeout does: every cookie but kubicle's own goes, so front sends each request
    to its sign-in page."""
    for cookie in json.loads(wd("cdp", "Network.getAllCookies", "{}"))["cookies"]:
        if cookie["name"] != "roundcube_sessid":
            params = {"name": cookie["name"], "domain": cookie["domain"], "path": cookie["path"]}
            wd("cdp", "Network.deleteCookies", json.dumps(params))


FRONT = "curl -s -o /dev/null -c {0} -b {0}"  # through Mailu front, with the cookie jar {0}


def mailu_session(jar: str) -> str:
    """A Mailu sign-in from curl, and kubicle's session cookie its sso.php gave."""
    machine.succeed(f"rm -f {jar}; {FRONT.format(jar)} http://localhost/sso/login")
    login = f"--data-urlencode email={tier.user} --data-urlencode pw=fixture-password -d submitWebmail=1"
    machine.succeed(
        f"{FRONT.format(jar)} {login} http://localhost/sso/login && {FRONT.format(jar)} -L http://localhost/login/sso.php"
    )
    return machine.succeed(f"awk '$6 == \"roundcube_sessid\" {{print $7}}' {jar}").strip()


def one_session_per_token() -> None:
    # vm-full only: one Mailu session has one token, so its sso.php without kubicle's cookie, as a cookie-less
    # client asks it again and again, gets the session that token already holds: each one more cost a login per
    # reconnect, 20,043 for 20,000.
    jar = "/tmp/again.jar"
    first = mailu_session(jar)
    machine.succeed(f"sed -i '/roundcube_sessid/d' {jar} && {FRONT.format(jar)} -L http://localhost/login/sso.php")
    again = machine.succeed(f"awk '$6 == \"roundcube_sessid\" {{print $7}}' {jar}").strip()
    assert first and again == first, (first, again)


def archive_lists_once() -> None:
    # vm-full only: Archive by sender reads the folder list once at the end, as the plugin's `subfolder_worker`
    # lists once: a refresh per new sender folder cost a LIST each, and a STATUS of every folder each.
    folder, senders = "Senders", 24
    machine.succeed(f"{tier.doveadm} mailbox create -u {tier.user} {folder}")
    for n in range(senders):
        raw = f"From: s{n}@example.net\r\nTo: {tier.user}\r\nSubject: sender {n}\r\n\r\nx\r\n"
        machine.succeed(f"printf %s {shlex.quote(raw)} | docker exec -i mailu-imap doveadm save -u {tier.user} -m {folder}")
    before = request("GET", "api/settings")[1]
    lists = "docker logs kubicle 2>&1 | grep -c 'command=\"LIST\"' || true"
    page = f"api/list?folder={folder}&page=0&size=50&sort=Arrival&order=Descending&threads=false"
    try:
        settings_change(archive_type="Sender")
        deadline = time.monotonic() + 60  # until kubicle's folder round lists the folder
        while request("GET", page)[1].get("total") != senders:
            assert time.monotonic() < deadline, "kubicle never listed the senders"
            time.sleep(2)
        listed = int(machine.succeed(lists))
        assert request("POST", "api/change", {"Archive": {"folder": folder, "messages": "All", "zone": "UTC"}})[0] == 204
        archived = int(machine.succeed(lists)) - listed
        assert archived <= 3, f"{senders} sender folders made {archived} LISTs"  # one, and the 15 s round's
    finally:
        settings_change(archive_type=before.get("archive_type", "None"))
        for name in [folder, *(f"Archive.s{n}@example-net" for n in range(senders))]:
            request("POST", "api/change", {"DeleteFolder": {"name": name}})


def boot_hash_encoded() -> None:
    # vm-full only: boot.js's session names the hash's folder encoded again, so a link in a mail cannot add
    # parameters of its own to it.
    wd("go", "about:blank")  # a new hash on the open app would load nothing anew
    wd("go", "http://localhost/login/#folder=INBOX&search=x&page=1")
    wait("!!document.querySelector('.folder')", 60)
    logged = "docker logs kubicle 2>&1 | grep 'api/session?folder=INBOX' | tail -50"
    machine.wait_until_succeeds(f"{logged} | grep -q 'folder=INBOX%26search%3Dx'", 30)
    machine.fail(f"{logged} | grep -q 'folder=INBOX&search=x'")
    reload()


def own_tokens() -> None:
    # vm-full only: each kubicle session keeps the Mailu token its sso.php proved. Once a Mailu sign-out ends one and
    # the IMAP connection drops, that session ends and the user's others go on: the older one first (another token
    # kept it alive), then the newest (refused, it ended them all, this tab's too).
    address, _, _ = kubicle_direct()

    def status(session: str) -> str:
        return machine.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}}' -b roundcube_sessid={session} http://{address}/api/session"
        )

    def sign_out_and_reconnect(jar: str, session: str) -> None:
        machine.succeed(f"{FRONT.format(jar)} http://localhost/sso/logout")
        machine.succeed(f"{tier.doveadm} kick {tier.user}")
        deadline = time.monotonic() + 30
        while (code := status(session)) != "401":
            assert time.monotonic() < deadline, f"30 s after its Mailu sign-out the session answered {code}"
            time.sleep(1)

    older, newer = mailu_session("/tmp/older.jar"), mailu_session("/tmp/newer.jar")
    assert older and newer and status(older) == status(newer) == "200", (older, newer)
    sign_out_and_reconnect("/tmp/older.jar", older)
    assert status(newer) == "200", "the newer session ended with the older one"
    sign_out_and_reconnect("/tmp/newer.jar", newer)
    assert request("GET", "api/session")[0] == 200, "this tab's session ended with the newest one"


def search_stops_with_its_request() -> None:
    # vm-full only: a search over every folder stops before its next folder once its request ended, as the tab's abort
    # of a query a newer one replaced ends it, through front. IMAP is paused, so the search waits for the engine until
    # the abort. No record of the latest query decides it, as requests can arrive out of order.
    view = {"folder": "INBOX", "page": 0, "size": 50, "sort": "Unsorted", "order": "Descending", "threads": "false"}
    search = {"text": "stopped", "fields": {}, "filter": "All", "interval": "Any", "scope": "All", "full": True}
    url = "api/list?" + urllib.parse.urlencode({**view, "search": json.dumps(search)})
    machine.succeed("docker pause mailu-imap")
    try:
        abortable = "window.kubicleAbort = new AbortController(); fetch(URL, {signal: kubicleAbort.signal}).catch(e => e)"
        js(abortable.replace("URL", json.dumps(url)))
        time.sleep(1)
        js("kubicleAbort.abort()")
        time.sleep(1)
    finally:
        machine.succeed("docker unpause mailu-imap")
    stopped = "docker logs kubicle 2>&1 | grep -q 'a search stopped: its request ended'"
    machine.wait_until_succeeds(stopped, timeout=30)


def render_marker() -> None:
    # vm-full only: a render that killed kubicle leaves its live marker (http.rs, Renders), which the next start makes
    # a crash record: that message answers 422, where another one renders and another build's markers go. A render
    # whose request went, or that runs beside another of the same message, leaves no record.
    message = lambda uid: request("GET", f"api/message?folder=INBOX&uid={uid}")[0]  # noqa: E731
    left, other = (deliver_raw(s, f"From: a@example.org\nTo: {tier.user}\nSubject: {s}\n\nx") for s in ("Killed it", "Did not"))
    validity = machine.succeed(f"{tier.doveadm} -f flow mailbox status -u {tier.user} uidvalidity INBOX").split("=")[1].strip()
    name = hashlib.sha256(f"{tier.user}\nINBOX\n{validity}\n{left}".encode()).hexdigest()[:16]
    rendering = "/var/lib/kubicle/rendering"
    (build,) = machine.succeed(f"ls {rendering}").split()
    machine.succeed(
        f"mkdir {rendering}/stale && touch {rendering}/{build}/live/{name}.7 && chown -R --reference={rendering} {rendering}"
    )
    restart_kubicle()
    assert message(left) == 422, "the message whose render left a marker rendered"
    assert message(other) == 200, "another message did not render"
    machine.fail(f"test -e {rendering}/stale")
    machine.succeed(f"rm {rendering}/{build}/{name}")
    assert message(left) == 200, "the message did not render once its marker went"
    # About a second's render: its request ends long before, then two at once.
    with tempfile.NamedTemporaryFile("w", suffix=".eml", delete=False) as f:
        f.write(
            f"From: a@example.org\r\nTo: {tier.user}\r\nSubject: Slow to render\r\nContent-Type: text/html\r\n\r\n"
            + "<p>x</p>" * 200_000
        )
    machine.copy_from_host(f.name, "/tmp/slow.eml")
    machine.succeed(f"docker exec -i mailu-imap doveadm save -u {tier.user} -m INBOX </tmp/slow.eml")
    slow = arrived("INBOX", "Slow to render")
    address, session, _ = kubicle_direct()
    machine.execute(f"curl -s -m 0.1 -b roundcube_sessid={session} 'http://{address}/api/message?folder=INBOX&uid={slow}'")
    time.sleep(5)
    url = json.dumps(f"api/message?folder=INBOX&uid={slow}")
    both = json.loads(wd("async", f"Promise.all([fetch({url}), fetch({url})]).then(rs => done(rs.map(r => r.status)))", "60"))
    assert both == [200, 200], f"after a render whose request went, two at once answered {both}"


def address_made_anew() -> None:
    # vm-full only, last, as it leaves INBOX another UIDVALIDITY: Mailu keeps no account id, and a deleted address's
    # new holder has a new INBOX once the old maildir went or moved (Mailu's FAQ, deleting users). Signed in on one
    # with another UIDVALIDITY, the address finds none of the last account's settings, uploads or browser copies.
    subject = "Left by the account before"
    settings_change(remote_images="Always")
    status, uploaded = upload("new Blob(['x'])", "before.txt")
    assert status == 200, uploaded
    upload_id = json.loads(uploaded)["id"]
    status, prefill = request("POST", "api/compose", {"mode": "New", "messages": [], "zone": ""})
    copy = {"draft": {**prefill["draft"], "subject": subject}, "edited": True, "alive": 0, "forwards_bcc": False, "pending": True}
    # The key Compose keeps a copy under: `kubicle.draft.`, the address and the account (`Session::account`).
    wd(
        "async",
        f"""fetch('api/session').then(r => r.json()).then(s => {{
        localStorage.setItem(`kubicle.draft.${{s.user + '.' + s.account}}.12345`,
            {json.dumps(json.dumps(copy))}); done(); }})""",
    )
    validity = int(machine.succeed(f"{tier.doveadm} -f flow mailbox status -u {tier.user} uidvalidity INBOX").split("=")[1])
    machine.succeed(f"{tier.doveadm} mailbox update -u {tier.user} --uid-validity {validity + 1} INBOX")
    # Signed in again while the engine still runs, its connection holding the old INBOX open: a STATUS on it once
    # answered the old UIDVALIDITY, and the last account's state stayed.
    wd("go", "http://localhost/login/sso.php")
    wait("location.pathname === '/login/' && !!document.querySelector('.folder')", 60)
    assert request("GET", "api/settings")[1]["remote_images"] == "Trusted", "the last account's settings stayed"
    assert request("GET", f"api/upload?id={upload_id}&name=x")[0] == 404, "the last account's upload stayed"
    time.sleep(5)  # past the restore that saves pending copies to Drafts
    assert drafts_with(subject) == 0, "the last account's browser copy went to Drafts"


def mailu_sign_in_again() -> None:
    """Mailu's sign-in form, reached by kubicle's own way back, filled in."""
    wait("!!document.querySelector('input[name=pw]')", 60)
    wd("type", "input[name=email]", tier.user)
    wd("type", "input[name=pw]", "fixture-password")
    wd("click", "form [type=submit]")


def signed_in_again() -> None:
    """Back in the app after a scenario that ended Mailu's session, however it ended."""
    if not js("return location.pathname === '/login/' && !!document.querySelector('.folder')"):
        sign_in()


NOTICE = "document.querySelector('.signed-out')?.textContent.includes('Your session is invalid or expired.')"


def sign_out_saves_in_turn() -> None:
    # Sign out while a save is on its way: the sign-out's save must wait for it and replace the copy it makes. The
    # page holds the first save's request, and the sign-out until that is done, so without the wait the sign-out's
    # save reaches the server first and the held one, sent later, replaces it: Drafts keeps the older text.
    subject = "A draft signed out while it saves"
    draft_in_drafts(subject)
    try:
        js(r"""const plain = window.fetch; let first = true;
            const hold = new Promise(r => window.kubicleRelease = r), out = new Promise(r => window.kubicleLeave = r);
            window.fetch = (url, ...rest) => url === 'api/draft' && first
                ? (first = false, hold.then(() => plain(url, ...rest)))
                : url === 'api/logout' ? out.then(() => plain(url, ...rest)) : plain(url, ...rest);""")
        wd("type", ".compose input[aria-label=Subject]", " once")
        wd("click", ".compose .tool[title='Save as draft now']")
        wd("type", ".compose input[aria-label=Subject]", " twice")
        wd("click", "button.tool.logout")
        time.sleep(1)
        js("window.kubicleRelease()")
        time.sleep(2)
        js("window.kubicleLeave()")
        wait("location.pathname !== '/login/'", 60)
        copies = (drafts_with(subject), drafts_with(f"{subject} once twice"))
        assert copies == (1, 1), f"Drafts holds {copies[0]} copies, {copies[1]} with the last text"
    finally:
        signed_in_again()
        reload()


def session_end_in_compose() -> None:
    # Roundcube's session_error in Compose: the page stays with its text and the notice; Sign in again goes through
    # Mailu's sign-in and comes back to the same Compose (its #draft= address).
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('textarea.compose-body')")
        wd("type", ".compose input[aria-label=Subject]", "Written while signed out")
        wd("type", "textarea.compose-body", "Kept through the sign-in.")
        address = wait("location.hash.startsWith('#draft=') && location.hash")
        drop_mailu_session()
        wd("click", ".compose .tool[title='Save as draft now']")
        wait(NOTICE)
        time.sleep(3)  # past the 2 s after which any other view signs in
        assert js("return location.pathname") == "/login/" and js("return location.hash") == address
        assert "Kept through the sign-in." in js("return document.querySelector('textarea.compose-body').value")
        wd("click", ".signed-out button")
        mailu_sign_in_again()
        wait(
            f"location.pathname === '/login/' && location.hash === {json.dumps(address)}"
            " && document.querySelector('textarea.compose-body')?.value.includes('Kept through the sign-in.')",
            60,
        )
        discard()
    finally:
        signed_in_again()


def session_end_leaves_compose() -> None:
    # Compose left while signed out keeps its text in the browser, and once signed in again the page saves it to
    # Drafts, unasked: the restore question would offer it only 3 minutes later.
    subject = "Left while signed out"
    wd("click", ".compose-button")
    try:
        wait("!!document.querySelector('textarea.compose-body')")
        wd("type", ".compose input[aria-label=Subject]", subject)
        drop_mailu_session()
        wd("click", ".compose .tool[title='Save as draft now']")
        wait(NOTICE)
        wd("click", ".compose .tool[title='Close; the draft stays in Drafts']")
        mailu_sign_in_again()
        wait("location.pathname === '/login/' && !!document.querySelector('.folder')", 60)
        arrived("Drafts", subject)
        wait(f"!{KEPT}.includes({json.dumps(subject)})")
        assert drafts_with(subject) == 1, f"Drafts holds {drafts_with(subject)} copies"
    finally:
        signed_in_again()


def session_end_keeps_page() -> None:
    # Elsewhere the notice, then the sign-in after 2 s, back on the same folder and page.
    wd("go", "http://localhost/login/#folder=INBOX.github&page=2")
    js("location.reload()")
    wait("document.querySelector('.list .row') && location.hash === '#folder=INBOX.github&page=2'", 60)
    try:
        drop_mailu_session()
        js("window.dispatchEvent(new Event('online'))")  # the event stream opens again, and meets the sign-in page
        wait(NOTICE)
        mailu_sign_in_again()
        wait(
            "location.pathname === '/login/' && location.hash === '#folder=INBOX.github&page=2'"
            " && !!document.querySelector('.list .row')",
            60,
        )
    finally:
        signed_in_again()
        reload()


# Notes in local storage, which outlives each navigation, the page that showed a toast (the session notice or an
# error) and its text: an observer from the start of every document the tab loads while this script is added, Mailu's
# sign-in page included.
NOTICE_WATCH = """new MutationObserver(() => {
        const t = document.querySelector('.toast')
        if (t) localStorage.setItem('kubicle-test.notice', `${location.pathname}: ${t.textContent}`)
    }).observe(document, {childList: true, subtree: true})"""


def first_load_signs_in_at_once() -> None:
    # Refused before its first session, a page goes to sso.php at once, as Mailu's Roundcube does, with no notice or
    # its 2 s: here the service worker's stored app, opened after Mailu's session ended, as on a phone (a kubicle
    # restart's 401 takes the same path, `api::answer`). Refused again before a session came, the notice and its 2 s
    # sign-in instead: going at once could loop with no pause, as where kubicle's cookie does not stick.
    def jumps_at_once() -> None:
        js("localStorage.removeItem('kubicle-test.notice'); return true")
        wd("go", "about:blank")
        drop_mailu_session()
        wd("go", "http://localhost/login/")
        wait("!!document.querySelector('input[name=pw]')", 60)
        shown = js("return localStorage.getItem('kubicle-test.notice')")
        assert not shown, f"a toast showed before a first session, on {shown}"

    watch = json.loads(wd("cdp", "Page.addScriptToEvaluateOnNewDocument", json.dumps({"source": NOTICE_WATCH})))
    try:
        reload()  # the service worker controls the page from its second load
        jumps_at_once()
        mailu_sign_in_again()
        wait("location.pathname === '/login/' && !!document.querySelector('.folder')", 60)
        jumps_at_once()  # again, as that session cleared the tab's mark (`api::session_answered`)
        wd("go", "http://localhost/login/")  # back from Mailu's sign-in page unsigned: refused again before a session
        wait(NOTICE)
        mailu_sign_in_again()
        wait("location.pathname === '/login/' && !!document.querySelector('.folder')", 60)
    finally:
        wd("cdp", "Page.removeScriptToEvaluateOnNewDocument", json.dumps({"identifier": watch["identifier"]}))
        signed_in_again()


def run_scenarios() -> None:
    scenario("sign-in through sso.php", sign_in)
    scenario("a page of INBOX", inbox_page)
    scenario("folder list and counts", folders)
    scenario("open a message", open_message)
    scenario("state changes need the CSRF token", csrf)
    scenario("a forged sso.php is refused", forged_sso)
    scenario("the image's healthcheck", healthcheck)
    scenario("send through submission", send)
    scenario("resend sends the message on unchanged", resend)
    scenario("the mail server's size limit", size_limit)
    scenario("replies and forwards prefill as Roundcube's", reply_prefill)
    scenario("sent text goes out flowed", flowed_send)
    scenario("Reply all, mark read and Discard in Compose", compose_actions)
    scenario("the remote resources setting", remote_images)
    scenario("a blocked image without a size takes no space", blocked_image_boxes)
    scenario("the server's own Authentication-Results, read by its grammar", authentication_results)
    scenario("an imported mail brings no verdict of the server's", imported_verdict)
    scenario("uploads take turns", uploads_take_turns)
    scenario("forward block, quote shape and (was: ...)", forward_and_quote)
    scenario("To headers and addresses beyond ASCII", addresses_out)
    scenario("a deleted Sent choice falls back to the server's", stale_sent_choice)
    scenario("a quick run of arrow keys marks only where it stops", arrow_burst)
    scenario("a page asked before a delete was answered", no_stale_page)
    scenario("a list asked during a move waits for it", list_waits_for_move)
    scenario("Forward, then Delete before Compose opened", forward_then_delete)
    scenario("Delete from the toolbar keeps the keyboard focus", delete_keeps_focus)
    scenario("Last page keeps the keyboard focus", last_page_keeps_focus)
    scenario("a search's deleted row stays off", search_delete_stays_off)
    scenario("Always for a sender keeps the keyboard focus", trust_keeps_focus)
    scenario("the remote images notice fits a phone", images_notice_fits_a_phone)
    scenario("a phone's bottom bars keep clear of its gesture bar", bottom_bars_clear_the_gesture_bar)
    scenario("arrow keys with the folder column in the list's place", folder_column_arrows)
    scenario("Discard's answer gives the list the focus at once", discard_keeps_focus)
    scenario("the print page's lines and attachments", print_page)
    scenario("Print marks the message read", print_marks_read)
    scenario("the print page runs no script", print_no_script)
    scenario("Junk and Not junk store their keywords", junk_keywords)
    scenario("Archive skips Archive's subtree and marks read", archive_rules)
    scenario("search results follow the list order", search_order)
    scenario("a sort change keeps the page", sort_keeps_page)
    scenario("a sorted view stays right after a read", sorted_view_after_read)
    scenario("Mark all as read with subfolders", mark_all_scope)
    scenario("new mail marks its folder recent", recent_accent)
    scenario("the quota bar", quota_bar)
    scenario("search prefixes", search_prefixes)
    scenario("the search reset button", search_reset)
    scenario("search shows the server's answer alone", search_server_answer)
    scenario("a new search closes the open message", search_closes_message)
    scenario("drag-hover opens a folder, Shift+Left folds a subtree", folder_tree)
    scenario("a double-click opens the message view", full_view)
    scenario("the message window", message_window)
    scenario("Compose in a message window closes it", message_window_compose)
    scenario("download of several as Mbox and Maildir", zip_download)
    scenario("folder names and sizes", folder_names)
    scenario("Clear Trash on logout, and the empty Trash icon", logout_purge)
    scenario("forward several as attachments, and their draft", forward_several)
    scenario("forwards past the size limit are left out", forward_left_out)
    scenario("Send's checks: subject, text, public recipients", send_checks)
    scenario("Compose is locked while it sends", send_locks_compose)
    scenario("a question goes with the Compose it was asked in", question_goes_with_compose)
    scenario("recipient chips and spelling", compose_fields)
    scenario("a renamed attachment goes out under its name", attachment_rename)
    scenario("upload progress and Cancel", upload_cancel)
    scenario("uploads go on Remove, Discard and close, not during a send", uploads_freed)
    scenario("Open in new window and Compose in a new window", compose_windows)
    scenario("identity Reply-To and Organization", identity_fields)
    scenario("composing settings in a reply", composing_settings)
    scenario("mailto: Reply-To and In-Reply-To", mailto_fields)
    scenario("the About dialog", about)
    scenario("HTML mail as plain text", html_as_text)
    scenario("plain text as Roundcube shows it", plain_text_view)
    scenario("attachments open as sandboxed pages", open_attachments)
    scenario("a forwarded message shows inline", forwarded_inline)
    scenario("inline images by Roundcube's rule", inline_images_listed)
    scenario("no mail is ever cached", cache_policy)
    scenario("Chrome can install the app", installable)
    scenario("a signed URL serves only as signed", signed_links_refused)
    scenario("a recreated folder's mail does not inherit Show remote images", shown_after_recreate)
    scenario("the flag before the subject", flag_in_reader)
    scenario("a message that cannot load says so", failed_open)
    scenario("an empty attachment says so", empty_attachment)
    scenario("a draft's notice opens it in Compose", draft_notice)
    scenario("a closed Compose shows no draft its saves replaced", draft_edit_close)
    scenario("a save on its way when Compose closes", draft_saves_in_turn)
    scenario("a Drafts page asked before a save", draft_page_during_save)
    scenario("a draft below Drafts stays there", draft_in_subfolder)
    scenario("Drafts set to None: no save, and leaving asks", drafts_none)
    scenario("Details rows and N more", details_rows)
    scenario("a name holding an address, and show-email", address_names)
    scenario("look-alike domains get a notice", look_alike_domains)
    scenario("Displaying Messages labels", displaying_labels)
    scenario("Settings shows a zone by its new name", old_zone_name_shown)
    scenario("two settings changed at once both stay", settings_saved_in_turn)
    scenario("a refused save shows the stored settings", refused_save_shows_stored)
    scenario("encrypted mail notice", encrypted_notice)
    scenario("a bounce's report parts show inline", report_parts)
    scenario("text/enriched shows formatted", enriched_text)
    scenario("uuencoded files are listed", uuencoded_files)
    scenario("the summary falls back to Cc, Bcc and the date", summary_fallback)
    scenario("Ctrl+arrows, Space and ranges in the list", list_cursor)
    scenario("Left and Right in a thread", thread_arrows)
    scenario("the unroot underline only on a read thread root", unroot_underline)
    scenario("collapsing the open folder's parent opens it", collapse_opens_parent)
    scenario("a collapsed folder is bold over unread subfolders", collapsed_parent_bold)
    scenario("only Shift asks Move or Copy on a drop", drop_modifiers)
    scenario("the arrival time under Arrival sort", arrival_column)
    scenario("Sent's subfolders list and sort by recipients", sent_subfolder_recipients)
    scenario("the list's sender cell", list_sender_names)
    scenario("the tab says Search result", search_title)
    scenario("an unsubscribed special folder can come back", special_resubscribe)
    scenario("an unsubscribed parent shows above its subfolders", virtual_parents)
    scenario("the hidden list takes no keys", hidden_list_keys)
    scenario("arrows scroll the message view", message_view_arrows)
    scenario("the Message headers dialog", headers_dialog)
    scenario("mailto: links keep valid addresses only", mailto_links)
    scenario(".eml names, calendar.ics and the source charset", eml_names_and_source)
    scenario("a forward carries what Roundcube's does", forward_carries)
    scenario("receipt request, X-Sender, file names and Date zone", sent_headers)
    scenario("rows per page clamp to 2..200", rows_per_page)
    scenario("special folders show under their role's name", role_names)
    scenario("special folder choices: no INBOX, None", special_folder_choices)
    scenario("one folder holds two roles", two_roles)
    scenario("Archive divides by month and by sender", archive_division)
    scenario("autocomplete from Bcc and trusted senders, Tab moves on", autocomplete_sources)
    # Last: they end Mailu's session, and sign in again however they end.
    scenario("sign-out saves a draft after the save on its way", sign_out_saves_in_turn)
    scenario("sign-out keeps the files of a Compose open in another tab", sign_out_keeps_open_files)
    scenario("session end in Compose keeps it, and comes back to it", session_end_in_compose)
    scenario("Compose left while signed out goes to Drafts after the sign-in", session_end_leaves_compose)
    scenario("session end elsewhere comes back to the folder and page", session_end_keeps_page)
    scenario("a page that never had a session signs in at once", first_load_signs_in_at_once)
    scenario("a deploy reaches the open page", deploy_reaches_open_page)
    scenario("the size limit is learned once submission answers", size_limit_retry)
    scenario("refused sign-ins leave nothing behind", forged_sign_ins_leave_nothing)
    scenario("Select disables an entry that would change nothing", select_menu_no_dead_entry)


def deploy_reaches_open_page() -> None:
    # A deploy restarts kubicle on a new client build, which ends every session: the open page signs in again, and
    # its service worker must hand it the new build, not the stored old one. mailu.nix's `nextWeb` is the new build.
    reload()  # the service worker controls the page from its second load
    built = machine.succeed("readlink /var/lib/kubicle/web").strip()
    machine.succeed(f"ln -sfn {tier.next_web} /var/lib/kubicle/web && systemctl restart docker-kubicle")
    try:
        wait("window.kubicleBuild === 'kubicle_web_bg.0123456789abcdef.wasm' && !!document.querySelector('.folder')", 120)
    finally:
        machine.succeed(f"ln -sfn {built} /var/lib/kubicle/web && systemctl restart docker-kubicle")
        # Until the page signed in again and reloaded onto the build put back: the old page's folders show at once,
        # and a request meanwhile meets front's 502 while kubicle starts, then a 401.
        wait("window.kubicleBuild !== 'kubicle_web_bg.0123456789abcdef.wasm' && !!document.querySelector('.folder')", 120)


def restart_kubicle() -> None:
    """kubicle anew, a process with no session and no memory of earlier scenarios, and this tab signed in to it."""
    machine.succeed("systemctl restart docker-kubicle")
    machine.wait_until_succeeds("docker exec mailu-front curl -sf http://webmail/healthz", timeout=60)
    reload()
    assert request("GET", "api/session")[0] == 200, "the tab did not sign in again"


def kubicle_direct() -> tuple[str, str, str]:
    """kubicle's address on the webmail network, which the VM's gateway address reaches as a trusted peer, with
    this tab's session cookie and CSRF token, for requests that go around front."""
    address = machine.succeed("docker inspect -f '{{(index .NetworkSettings.Networks \"webmail\").IPAddress}}' kubicle")
    cookies = json.loads(wd("cdp", "Network.getCookies", json.dumps({"urls": ["http://localhost/login/"]})))["cookies"]
    session = next(c["value"] for c in cookies if c["name"] == "roundcube_sessid")
    return address.strip(), session, request("GET", "api/session")[1]["csrf"]


def kubicle_kb(field: str) -> int:
    """VmRSS or VmHWM of kubicle's process, in KB."""
    pid = machine.succeed("docker inspect -f '{{.State.Pid}}' kubicle").strip()
    return int(machine.succeed(f"awk '/^{field}:/ {{print $2}}' /proc/{pid}/status"))


def size_limit_retry() -> None:
    # The size limit is asked from start until front's submission port answers, which it may not do yet when kubicle
    # starts: started while the port refuses, kubicle learns the limit once it answers, with no sign-in in between.
    pid = machine.succeed("docker inspect -f '{{.State.Pid}}' mailu-front").strip()
    refuse = f"nsenter -t {pid} -n iptables -{{}} INPUT -p tcp --dport 10025 -j REJECT --reject-with tcp-reset"
    machine.succeed(refuse.format("I"))
    try:
        restart_kubicle()
        known = request("GET", "api/session")[1]["max_message_size"]
        assert known is None, f"kubicle knew the limit {known} while submission refused"
    finally:
        machine.succeed(refuse.format("D"))
    deadline = time.monotonic() + 90
    while (known := request("GET", "api/session")[1]["max_message_size"]) != tier.size_limit:
        assert time.monotonic() < deadline, f"90 s after submission answered, the session says {known}"
        time.sleep(1)


def forged_sign_ins_leave_nothing() -> None:
    # sso.php holds a lock per user name while an IMAP login proves the name, before anything vouched for it. After 64
    # refused sign-ins under names of their own from a trusted peer, the next sign-in must find no more than its own
    # held, by the count each sign-in logs ("a sign-in started", session.rs). The page's own may be one more.
    address, _, _ = kubicle_direct()
    forge = shlex.quote(
        "curl -s -o /dev/null -w '%{http_code}\\n' -H \"X-Remote-User: forged-$0@example.org\""
        f" -H 'X-Remote-User-Token: token-forged' http://{address}/sso.php"
    )
    codes = machine.succeed(f"seq 64 | xargs -P 32 -n 1 sh -c {forge}").split()
    assert codes == ["403"] * 64, codes
    machine.succeed(f"sh -c {forge} last")
    started = machine.succeed("docker logs kubicle 2>&1 | sed 's/\\x1b\\[[0-9;]*m//g' | grep 'a sign-in started' | tail -1")
    found = re.search(r"users=(\d+)", started)
    assert found, f"no sign-in count in the log: {started!r}"
    held = int(found.group(1))
    assert held <= 2, f"the sign-in after 64 refused ones found {held} names held"


def uploads_take_turns() -> None:
    # A user's uploads and imports are read into memory two at a time: four 16 MB uploads sent straight to kubicle
    # at 4 MB/s, all at once, finish in two rounds, the last two waiting for a turn.
    address, session, csrf = kubicle_direct()
    machine.succeed("head -c 16000000 /dev/zero >/tmp/upload.bin")
    upload = (
        f"curl -s -o /tmp/upload-$i.json -w '%{{time_total}}\\n' --limit-rate 4M -b roundcube_sessid={session}"
        f" -H 'x-kubicle-csrf: {csrf}' -F 'file=@/tmp/upload.bin;filename=turn-'$i.bin http://{address}/api/upload"
    )
    times = sorted(float(t) for t in machine.succeed(f"for i in 1 2 3 4; do {upload} & done; wait").split())
    ids = [json.loads(machine.succeed(f"cat /tmp/upload-{i}.json"))["id"] for i in range(1, 5)]
    assert request("POST", "api/upload/remove", ids)[0] == 204
    assert times[2] > 1.4 * times[1], f"four uploads at once took {times} s: no two waited for a turn"


def run_tier(*more: tuple, shared: bool = True) -> None:
    """The tier's setup, then the shared scenarios unless not `shared` and the (name, check) pairs in `more`, then
    the evidence and the verdict."""
    guarded("setup", tier_setup)
    if not VERDICT:
        if shared:
            guarded("scenarios", run_scenarios)
        for name, check in more:
            guarded(name, lambda: scenario(name, check))
    for name in failed:
        fail(f"scenario: {name}")
    collect(machine, "/tmp/failed-*.png", "/tmp/netlog.json")
    record()
