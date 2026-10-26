# The list stays live with no Refresh button (live.nix): after each disturbance, with no click, the browser's
# first INBOX page must match IMAP's and every folder badge IMAP's unseen count, within a deadline. Changes to the
# open folder come through IDLE; other folders through the 15 s STATUS poll.
# mypy: disable-error-code="name-defined"
# `machine` comes from the test driver; `tier`, `wd`, `js`, the imports and the other helpers from scenarios.py,
# which runs first: the driver type-checks (ty) and lints (ruff F) the joined script, where every name is defined.
ROWS = """return [...document.querySelectorAll('.row')].map(r =>
    [Number(r.id.slice(4)), r.classList.contains('unread'), r.classList.contains('flagged')])"""
BADGES = """return Object.fromEntries([...document.querySelectorAll('.folder')].map(b =>
    [b.querySelector('.folder-name').textContent, b.querySelector('.badge')?.textContent ?? '']))"""
LABELS = {"INBOX": "Inbox", "INBOX.github": "github", "INBOX.finance": "finance"}  # other folders show their name


def imap_page() -> list:
    # Arrival order rises with the UID in this mailbox (seed.py writes in date order, new mail is dated now), so
    # the newest 300 UIDs hold the first page even after a few moves and expunges.
    user, doveadm = tier.user, tier.doveadm
    out = machine.succeed(
        f"n=$({doveadm} -f tab mailbox status -u {user} uidnext INBOX | tail -1 | awk '{{print $NF}}')"
        + f" && {doveadm} -f tab fetch -u {user} 'uid flags date.received' mailbox INBOX uid $((n > 300 ? n - 300 : 1)):*"
    )
    rows = [line.split("\t") for line in out.splitlines()[1:] if line.strip()]
    rows.sort(key=lambda r: (r[2], int(r[0])), reverse=True)  # kubicle's default: arrival, then UID, newest first
    return [[int(uid), "\\Seen" not in flags, "\\Flagged" in flags] for uid, flags, _ in rows[:50]]


def imap_badges() -> dict:
    out = machine.succeed(
        f"for m in $({tier.doveadm} mailbox list -u {tier.user}); do"
        + f' echo "$m $({tier.doveadm} -f tab mailbox status -u {tier.user} unseen "$m" | tail -1 | awk \'{{print $NF}}\')"; done'
    )
    badges = {}
    for line in out.splitlines():
        name, n = line.rsplit(" ", 1)
        badges[LABELS.get(name, name)] = f"{int(n):,}" if int(n) else ""
    return badges


def matches(what: str, seconds: float, since: float) -> None:
    deadline = since + seconds
    while True:
        page, want_page = js(ROWS), imap_page()
        badges, want_badges = js(BADGES), imap_badges()
        if page == want_page and badges == want_badges:
            print(f"live: {what}: the list matched IMAP {time.monotonic() - since:.1f} s after it")
            return
        if time.monotonic() > deadline:
            stale = [r for r in page if r not in want_page][:5], [r for r in want_page if r not in page][:5]
            wrong = {k: (badges.get(k), v) for k, v in want_badges.items() if badges.get(k) != v}
            raise AssertionError(
                f"{what}: after {seconds} s the list shows {stale[0]} instead of {stale[1]}, " + f"badges (shown, IMAP) {wrong}"
            )
        time.sleep(0.5)


def second_client() -> None:
    page = imap_page()
    unread = [uid for uid, is_unread, _ in page[1:] if is_unread]  # page[0] gets the flag
    machine.succeed(f"imap-client INBOX flag {page[0][0]} '\\Flagged'")
    machine.succeed(f"imap-client INBOX flag {unread[0]} '\\Seen'")
    machine.succeed(f"imap-client INBOX expunge {unread[1]}")
    matches("a second client flagged, read and expunged in the open folder", 5, time.monotonic())
    junk = machine.succeed(f"{tier.doveadm} search -u {tier.user} mailbox Junk unseen").split()[1::2]
    machine.succeed(f"imap-client INBOX move {unread[2]} Archive")
    machine.succeed(f"imap-client Junk flag {junk[0]} '\\Seen' && imap-client Junk expunge {junk[1]}")
    matches("a second client moved out of the open folder and changed Junk", 20, time.monotonic())


def offline() -> None:
    subject = "Delivered while the browser was offline"
    conditions = {"offline": True, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1}
    wd("cdp", "Network.enable", "{}")
    wd("cdp", "Network.emulateNetworkConditions", json.dumps(conditions))
    # Emulation only stalls an open stream, which then delivers late; killing it loses what the server sends.
    assert machine.succeed("ss -K -H state established '( dport = :80 )'").strip(), "no stream to front to kill"
    time.sleep(5)
    machine.succeed(tier.deliver(subject))
    time.sleep(55)
    assert subject not in js(f"return {SUBJECTS}"), "the offline emulation did not cut the event stream"
    wd("cdp", "Network.emulateNetworkConditions", json.dumps(conditions | {"offline": False}))
    matches("mail arrived while the browser was offline for 60 s", 15, time.monotonic())


def dovecot_restart() -> None:
    machine.succeed("docker restart mailu-imap")
    machine.wait_until_succeeds(f"{tier.doveadm} mailbox status -u {tier.user} messages INBOX", timeout=60)
    machine.succeed(tier.deliver("Delivered after Dovecot restarted"))
    matches("Dovecot restarted, then mail arrived", 60, time.monotonic())


def imap_cut() -> None:
    # Packets dropped, never refused: no RST tells kubicle. After the restore the old connections stay
    # black-holed, as when a NAT lost their state, so only a reply deadline and a reconnect bring the list back.
    pid = machine.succeed("docker inspect -f '{{.State.Pid}}' kubicle").strip()
    ns = f"nsenter -t {pid} -n"
    flows = machine.succeed(f"{ns} ss -Htn state established '( dport = :10143 )'").split()[2::4]
    ports = [flow.rsplit(":", 1)[1] for flow in flows]  # kubicle's local ports to front's IMAP
    assert ports, "kubicle holds no IMAP connection to cut"
    path = ["OUTPUT -p tcp --dport 10143", "INPUT -p tcp --sport 10143"]
    machine.succeed(" && ".join(f"{ns} iptables -I {rule} -j DROP" for rule in path))
    machine.succeed(tier.deliver("Delivered while the IMAP path was cut"))
    time.sleep(30)
    for port in ports:
        old = [f"OUTPUT -p tcp --sport {port}", f"INPUT -p tcp --dport {port}"]
        machine.succeed(" && ".join(f"{ns} iptables -I {rule} -j DROP" for rule in old))
    machine.succeed(" && ".join(f"{ns} iptables -D {rule} -j DROP" for rule in path))
    matches("the IMAP path was cut for 30 s mid-IDLE, then restored", 60, time.monotonic())


LIVE = {  # live.nix groups these into checks
    "delivery": ("live: new mail while the open folder is idle", lambda: live_update(1000)),
    "second-client": ("live: a second client changes mail", second_client),
    "offline": ("live: the browser offline for 60 s", offline),
    "dovecot-restart": ("live: Dovecot restarted", dovecot_restart),
    "imap-cut": ("live: the IMAP path cut without a reset", imap_cut),
}
