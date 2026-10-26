"""What the exploration observed, pinned with inline-snapshot, so a change in behaviour shows as a diff.

Bombadil walks the UI at random, so no two traces match step for step. The pins are what stays put across walks:
the folder pane, which comes before any action can change it, compared exactly; and for the rest, sets of
everything seen anywhere in the walk, compared with `<=`, so a walk that saw less still passes and anything new
fails. A property violation or a refused packet fails outright and is never pinned.

KUBICLE_EXPLORE is the explore VM test's output (default.nix). To accept a change: build
`.#checks.x86_64-linux.explore.run`, then run this file with that path and `--inline-snapshot=fix`.
"""

import json
import os
import re
from pathlib import Path

from inline_snapshot import snapshot

OUT = Path(os.environ["KUBICLE_EXPLORE"])
LINES = (OUT / "explore/trace.jsonl").read_text().strip()
assert LINES, "empty trace: Bombadil stopped before its first step, as on a spec that does not compile (`nix log` on the run)"
TRACE = [
    json.loads(line) for line in LINES.split("\n")
]  # not splitlines: typed text in the trace holds U+2028, which it would split on


def value(entry, name):
    return next(item["value"] for item in entry["snapshots"] if item["name"] == name)


def values(name):
    return (value(entry, name) for entry in TRACE)


def test_the_walk_was_long_enough():
    # Bombadil stops early when it fails itself (a CDP answer it cannot decode, a spec that throws), with a
    # trace that violates nothing: a whole walk spans 267 to 298 s of its 300 (Oct 25, 2026), one cut short 35
    # to 102. One that stalls (a backgrounded tab) runs its time out with few steps: 131 in 300 s, where a whole
    # one takes 9 to 12 a second.
    span = (TRACE[-1]["timestamp"] - TRACE[0]["timestamp"]) / 1e6
    assert len(TRACE) >= 100 and span >= 0.8 * int(os.environ["KUBICLE_EXPLORE_SECONDS"]), f"{len(TRACE)} steps in {span:.0f} s"
    assert len(TRACE) >= 2 * span, f"{len(TRACE)} steps in {span:.0f} s: the walk stalled"


def test_no_property_was_violated():
    violations = sorted({v["name"] for entry in TRACE for v in entry["violations"]})
    assert violations == [], json.dumps([v for entry in TRACE for v in entry["violations"]][:3], indent=1)


def test_nothing_left_the_browser():
    # Not the DNS log: it also holds the image proxy's lookups, made by the server when a walk shows images.
    # Chromium's own update and sign-in hosts resolve to the second sink address (../stack.nix), never a page's.
    sink = [json.loads(line) for line in (OUT / "sink.log").read_text().splitlines()]
    assert [event for event in sink if not event["dst"].startswith("198.51.100.2:")] == []
    leaks = (OUT / "leaks.log").read_text().splitlines()
    assert [line for line in leaks if "DST=198.51.100.2 " not in line] == []


def test_folder_pane():
    # The names only: unread counts can arrive after the first action, which may already have read a message.
    first = next(entry for entry in TRACE if value(entry, "folders"))
    assert [folder["name"] for folder in value(first, "folders")] == snapshot(
        ["Inbox", "finance", "github", "Drafts", "Sent", "Junk", "Trash", "Archive", "Проєкти"]
    )


def test_frames_seen():
    seen = {json.dumps(frame, sort_keys=True) for frames in values("frames") for frame in frames}
    assert seen <= snapshot(
        {'{"cspInHead": true, "referrerpolicy": "no-referrer", "sandbox": "allow-popups allow-popups-to-escape-sandbox"}'}
    )


def test_api_routes_called():
    paths = {re.sub(r"\?.*", "", url.split("/login/", 1)[-1]) for seen in values("requests") for url in seen["urls"]}
    assert {p for p in paths if p.startswith("api/")} <= snapshot(
        {
            "api/addresses",
            "api/autocomplete",
            "api/change",
            "api/compose",
            "api/draft",
            "api/events",
            "api/list",
            "api/logout",
            "api/message",
            "api/search",
            "api/send",
            "api/session",
            "api/settings",
            "api/sizes",
            "api/source",
            "api/upload/remove",
            "api/view",
        }
    )


def test_toasts_shown():
    # The walk types random text, which error toasts quote after "address: ", and copies any address it meets.
    toasts = {re.sub(r"^(Copied |.*address: ).*", r"\1...", re.sub(r"\d+", "N", t)) for t in values("toast") if t}
    assert toasts <= snapshot(
        {
            "Copied ...",
            "Copying message(s)...",
            "Deleting message(s)...",
            "Draft kept in this browser only: invalid From address",
            "Draft saved.",
            "Folder successfully emptied.",
            "Loading...",
            "Marking message(s)...",
            "Message(s) copied successfully.",
            "Message(s) deleted successfully.",
            "Message(s) marked successfully.",
            "Message(s) moved successfully.",
            "Message(s) moved to Trash successfully.",
            "Moving message(s)...",
            # the VM has no submission server, so the connection is refused
            "Not sent: the connection to the mail server failed: Connection refused (os error N)",
            "Not sent: invalid address: ...",
            "Not sent: letters beyond ASCII before the @ are not supported in the address: ...",
            "Not sent: no recipients",
            "Not sent: the From address is not one of yours",  # the walk types into From
            "Please enter at least one recipient.",
            "Please select a file to upload.",
            "Settings not saved: theme ids are N-N characters of a-z, N-N and -",
            "Successfully archived",
            "Successfully reported as junk",
            "Successfully reported as not junk",
        }
    )
