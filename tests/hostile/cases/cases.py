"""Every hostile case as a mail in a Maildir++ tree, one folder per corpus split at 200 (the largest list page),
plus manifest.json: the case ids of each folder in UID order. Dovecot numbers new files in file-name order.

usage: python3 -I cases.py <out> <js.jsonl> <httpleaks> <portswigger> <wpt> <roundcube> <mail-parser> <mimekit>
       <mime-torture> <espoofer testcases.py> <blns.json>

Payload calls to alert, confirm and prompt also post a message to the app page, which the driver counts: a sandbox
that lets script run shows even where the browser blocks the dialog. Own cases carry that tripwire in every
vector they try.
"""

import ast
import base64
import email
import email.policy
import json
import re
import sys
import warnings
from email.header import Header
from html.parser import HTMLParser
from pathlib import Path

OUT = Path(sys.argv[1])
JS, HTTPLEAKS, PORTSWIGGER, WPT, ROUNDCUBE, MAIL_PARSER, MIMEKIT, TORTURE, ESPOOFER, BLNS = map(Path, sys.argv[2:12])
FOLDER_SIZE = 200
TRIP = "top.postMessage(1,String.fromCharCode(42))"  # no quotes, so it fits in any attribute

cases: dict[str, list[tuple[str, bytes]]] = {}  # folder -> [(id, raw bytes)]


def add(corpus, case_id, raw):
    cases.setdefault(corpus, []).append((case_id, raw))


def tripwire(html):
    return re.sub(r"\b(alert|confirm|prompt)\b(?=\s*[(`])", rf"({TRIP},\1)", html)


def mail(case_id, html=None, headers="", body=None, content_type="text/html; charset=utf-8"):
    """A mail around one case: its HTML base64-encoded (no line of it can break the MIME), or a ready body."""
    if body is None:
        body = base64.encodebytes(tripwire(html).encode()).decode().replace("\n", "\r\n")
        headers += "Content-Transfer-Encoding: base64\r\n"
    return (
        f"From: Hostile <hostile@evil.test>\r\nTo: andrew@example.test\r\nSubject: {case_id}\r\n"
        f"Message-ID: <{case_id}@evil.test>\r\nDate: Tue, 21 Oct 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\n"
        f"{headers}Content-Type: {content_type}\r\n\r\n{body}\r\n"
    ).encode()


def multipart(case_id, *parts, headers=""):
    body = "".join(f"--b\r\n{part}\r\n" for part in parts) + "--b--"
    return mail(case_id, headers=headers, body=body, content_type='multipart/mixed; boundary="b"')


HTML_PART = "Content-Type: text/html; charset=utf-8\r\n\r\n<p>see the attached part</p>"

for line in JS.read_text().strip().split("\n"):  # not splitlines: payloads hold U+2028
    case = json.loads(line)
    corpus, case_id = case["corpus"], f"{case['corpus']}-{case['id']}"
    if "html" in case:
        add(corpus, case_id, mail(case_id, case["html"]))
        continue
    # ept3: the tracking host named after the test, so a leak names its case.
    host = f"{case['id'].lower()}.ept3.canary.test"
    block = re.sub(r"TRACKING_(URL|HOST)", host, case["block"])
    block = block.replace("TRACKING_ADDRESS", case["id"]).replace("TRACKING_PATH", "p").replace("TRACKING_CODE", "c")
    block = block.replace("ANY_OTHER_URL", f"other.{host}")
    if block.startswith("BEGIN:VCALENDAR"):
        raw = multipart(case_id, HTML_PART, f"Content-Type: text/calendar; charset=utf-8\r\n\r\n{block}")
    elif block.startswith("BEGIN:VCARD"):
        raw = multipart(case_id, HTML_PART, f"Content-Type: text/vcard; charset=utf-8\r\n\r\n{block}")
    elif re.match(r"[A-Za-z-]+: ", block):
        raw = mail(case_id, "<p>the hostile part is a header</p>", headers=block + "\r\n")
    elif not block.startswith("<"):
        raw = mail(case_id, f"<style>body{{{block}}}</style><p>CSS from a part</p>")
    elif case["id"] in ("rfc822Img", "messageGlobalImg"):
        kind = "global" if "Global" in case["id"] else "rfc822"
        inner = f"Subject: inner\r\nContent-Type: text/html\r\n\r\n{block}"
        raw = multipart(case_id, HTML_PART, f"Content-Type: message/{kind}\r\n\r\n{inner}")
    else:
        raw = mail(case_id, block)
    add("ept3", case_id, raw)

# HTTPLeaks: whole, then one case per %Section, since a later declaration of a property hides an earlier one.
leak = (HTTPLEAKS / "leak.html").read_text()
add("httpleaks", "httpleaks-all", mail("httpleaks-all", leak))
for n, section in enumerate(re.split(r"<!--\s*\n%", leak)[1:]):
    title = re.sub(r"[^a-z0-9]+", "-", section.split("\n", 1)[0].lower()).strip("-")
    case_id = f"httpleaks-{n:02d}-{title}"
    add("httpleaks", case_id, mail(case_id, "<!--\n%" + section))


def codes(value):
    if isinstance(value, dict):
        if isinstance(value.get("code"), str):
            yield value["code"]
        for v in value.values():
            yield from codes(v)
    elif isinstance(value, list):
        for v in value:
            yield from codes(v)


n = 0
for path in sorted((PORTSWIGGER / "json").glob("*.json")):
    for code in codes(json.loads(path.read_text())):
        case_id = f"portswigger-{path.stem}-{n:04d}"
        add("portswigger", case_id, mail(case_id, code))
        n += 1


def dat_cases(path):
    """`#data` sections of html5lib's test format; a section ends at the next `#name` line."""
    data, inside = [], False
    for line in path.read_text().split("\n"):
        if re.fullmatch(r"#[a-z-]+", line):
            if inside:
                yield "\n".join(data)
            inside, data = line == "#data", []
        elif inside:
            data.append(line)
    if inside:
        yield "\n".join(data)


for path in sorted((WPT / "html/syntax/parsing/resources").glob("*.dat")) + sorted((WPT / "sanitizer-api").glob("*.dat")):
    for i, html in enumerate(dat_cases(path)):
        case_id = f"wpt-{path.name.removesuffix('.dat').removesuffix('.sub')}-{i:03d}"
        add("wpt", case_id, mail(case_id, html))


def php_strings(source):
    """String literals of PHP source, adjacent ones joined where the code concatenates them with `.`."""
    out, i, current, pending_dot = [], 0, None, False
    escapes = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"', "$": "$", "0": "\0"}
    while i < len(source):
        c = source[i]
        if source.startswith("//", i) or c == "#":
            i = source.find("\n", i) % (len(source) + 1)
        elif source.startswith("/*", i):
            i = source.find("*/", i) + 2
        elif c in "'\"":
            j, text = i + 1, []
            while source[j] != c:
                if source[j] == "\\" and (c == '"' and source[j + 1] in escapes or source[j + 1] in "'\\"):
                    text.append(escapes.get(source[j + 1], source[j + 1]))
                    j += 2
                else:
                    text.append(source[j])
                    j += 1
            literal = "".join(text)
            current = current + literal if pending_dot and current is not None else literal
            pending_dot, i = False, j + 1
        elif c == ".":
            pending_dot, i = True, i + 1
        elif c.isspace():
            i += 1
        else:
            if current is not None and not pending_dot:
                out.append(current)
                current = None
            pending_dot, i = False, i + 1
    return out


seen = set()
for name in [
    "Washtml",
    "Text2Html",
    "Html2text",
    "Enriched",
    "StringReplacer",
    "Utils",
]:  # Roundcube's HTML and text renderers
    path = ROUNDCUBE / f"tests/Framework/{name}.php"
    for html in php_strings(path.read_text(errors="replace")):
        if "<" in html and re.search(r"<[a-zA-Z!/]", html) and html not in seen:
            seen.add(html)
            case_id = f"roundcube-{path.stem.lower()}-{len(seen):03d}"
            add("roundcube", case_id, mail(case_id, html))
for name in ["htmlxss.txt", "htmlbody.txt", "htmlbase.txt", "htmlcom.txt", "invalidchars.html"]:
    case_id = f"roundcube-{name}"
    add("roundcube", case_id, mail(case_id, (ROUNDCUBE / "tests/src" / name).read_text(errors="replace")))
for name in ["html.msg", "mail0.eml"]:
    add("roundcube", f"roundcube-{name}", (ROUNDCUBE / "tests/src" / name).read_bytes())

for path in sorted((MAIL_PARSER / "resources/eml").rglob("*.eml")):
    add("mail-parser", f"mail-parser-{path.parent.name}-{path.stem}", path.read_bytes())
for path in sorted((MIMEKIT / "UnitTests/TestData/messages").iterdir()):
    if path.suffix in (".txt", ".eml"):
        add("mimekit", f"mimekit-{path.stem}", path.read_bytes())
add("mime-torture", "mime-torture", re.sub(rb"\AFrom [^\n]*\n", b"", TORTURE.read_bytes()))


def literal(node):
    """espoofer's byte expressions, with its helpers replaced by fixed values."""
    if isinstance(node, ast.Constant):
        return node.value
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return literal(node.left) + literal(node.right)
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
        fixed = {"get_date": lambda: b"Tue, 21 Oct 2026 10:00:00 +0000", "id_generator": lambda: b"fixture"}
        fixed["bs64encode"] = base64.b64encode
        return fixed[node.func.id](*map(literal, node.args))
    raise ValueError(f"espoofer: cannot evaluate {ast.unparse(node)}")


def entries(node: ast.expr) -> dict[str, ast.expr]:
    """One of espoofer's dict literals, by its string keys."""
    assert isinstance(node, ast.Dict), f"espoofer: no dict literal: {ast.unparse(node)[:80]}"
    found = {}
    for k, v in zip(node.keys, node.values):
        assert isinstance(k, ast.Constant) and isinstance(k.value, str), f"espoofer: a key that is no string: {k}"
        found[k.value] = v
    return found


warnings.simplefilter("ignore", SyntaxWarning)  # espoofer's own invalid escapes
tree = ast.parse(ESPOOFER.read_text())
table = next(
    n.value
    for n in tree.body
    if isinstance(n, ast.Assign) and isinstance(n.targets[0], ast.Name) and n.targets[0].id == "test_cases"
)
for key, value in entries(table).items():
    parts = {k: literal(v) for k, v in entries(entries(value)["data"]).items()}
    raw = b"".join(parts[k] for k in ("from_header", "to_header", "subject_header", "other_headers", "body"))
    add("espoofer", f"espoofer-{key}", raw)

for i, text in enumerate(s for s in json.loads(BLNS.read_text()) if s):
    case_id = f"blns-{i:03d}"
    name = Header(text, "utf-8").encode()
    filename = base64.b64encode(text.encode()).decode()
    raw = multipart(
        case_id,
        HTML_PART.replace("see the attached part", text),
        f"Content-Type: text/plain; charset=utf-8\r\n\r\n{text}",
        f'Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename="=?utf-8?B?{filename}?="\r\n\r\nx',
        headers=f"Reply-To: {name} <naughty@evil.test>\r\nX-Subject: {name}\r\n",
    ).replace(b"From: Hostile <", f"From: {name} <".encode(), 1)
    add("blns", case_id, raw.replace(f"Subject: {case_id}".encode(), f"Subject: {name}".encode(), 1))

# Own cases: what the corpora leave out, and app-origin loads that only the mail frame's own CSP refuses.
OWN = {
    "relative-app": '<img src="api/session"><img src="./"><link rel=stylesheet href="base.css"><img src="sso.php">'
    '<video src="api/events"></video><img srcset="api/session 1x">',
    "meta-refresh": '<meta http-equiv="refresh" content="0;url=https://refresh.own.canary.test/">refresh',
    "meta-refresh-js": f'<meta http-equiv="refresh" content="0;url=javascript:{TRIP}">x',
    "meta-refresh-data": f'<meta http-equiv="refresh" content="0;url=data:text/html,<script>{TRIP}</script>">x',
    "base": '<base href="https://base.own.canary.test/"><img src="x.png"><a href="y">l</a>',
    "style-urls": "<style>@import url(https://import.own.canary.test/a.css);"
    "@font-face{font-family:x;src:url(https://font.own.canary.test/f.woff)}"
    "body{background:url(https://bg.own.canary.test/b.png);font-family:x}</style>"
    '<div style="background-image:url(https://styleattr.own.canary.test/a.png);width:9px;height:9px">x</div>'
    '<div style="background:image-set(url(https://imageset.own.canary.test/a.png) 1x);width:9px;height:9px">y</div>'
    '<div style="background:URL(https://upper.own.canary.test/a.png);width:9px;height:9px">z</div>',
    "images": '<img src="https://img.own.canary.test/a.png">'
    '<img srcset="https://srcset.own.canary.test/a.png 1x, https://srcset2.own.canary.test/b.png 2x">'
    '<picture><source srcset="https://picture.own.canary.test/a.webp"><img src="https://pi.own.canary.test/a.png">'
    '</picture><table background="https://table.own.canary.test/a.png"><tr><td>t</td></tr></table>'
    '<video poster="https://poster.own.canary.test/a.png"></video><input type=image src="https://in.own.canary.test/a">'
    '<img src="//protorel.own.canary.test/a.png">',
    "prefetch": '<link rel=dns-prefetch href="//dnsprefetch.own.canary.test"><link rel=preconnect '
    'href="https://preconnect.own.canary.test"><link rel=prefetch href="https://prefetch.own.canary.test/a">'
    '<link rel=preload as=image href="https://preload.own.canary.test/a"><a href="https://anchor.own.canary.test/">a</a>',
    "svg": '<svg><image href="https://svgimage.own.canary.test/a.png"/><filter><feImage '
    'href="https://feimage.own.canary.test/a"/></filter><use href="https://use.own.canary.test/a.svg#x"/></svg>',
    "form": '<form action="https://form.own.canary.test/"><input name=q autofocus value=x><button>go</button></form>',
    "frames": '<iframe src="https://iframe.own.canary.test/"></iframe><object data="https://object.own.canary.test/">'
    '</object><embed src="https://embed.own.canary.test/a"><iframe src="./"></iframe>',
    "csp-override": '<meta http-equiv="Content-Security-Policy" content="img-src *; default-src *">'
    '<img src="https://override.own.canary.test/a.png">',
    "script": f"<script>{TRIP}</script><img src=x onerror={TRIP}><svg onload={TRIP}></svg>"
    f"<body onload={TRIP}><a id=j href=javascript:{TRIP}>j</a>"
    f'<iframe srcdoc="<script>{TRIP}</script>"></iframe><iframe src="data:text/html,<script>{TRIP}</script>">'
    f"</iframe><details open ontoggle={TRIP}></details><style onload={TRIP}></style>",
    "script-src-app": '<script src="boot.js" type=module></script><script src="kubicle_web.js" type=module></script>',
    "redress": '<div style="position:fixed;top:0;left:0;width:100vw;height:100vh;background:red;z-index:9">cover</div>',
    "mathml": f"<math><mtext><table><mglyph><style><img src=x onerror={TRIP}>",
    "noscript": f'<noscript><p title="</noscript><img src=x onerror={TRIP}>">',
    "popup": '<a href="https://popup.own.canary.test/" target=_blank id=p>p</a>'
    '<form target=_blank action="https://popupform.own.canary.test/"><button autofocus>x</button></form>',
    "speculation": '<script type=speculationrules>{"prefetch":[{"source":"list",'
    '"urls":["https://speculation.own.canary.test/"]}]}</script>',
}
for name, html in OWN.items():
    add("own", f"own-{name}", mail(f"own-{name}", html))
add(
    "own",
    "own-cid-and-svg-parts",
    multipart(
        "own-cid-and-svg-parts",
        'Content-Type: text/html\r\n\r\n<img src="cid:logo@x"><img src="cid:svg@x"><img src="cid:missing@x">',
        "Content-Type: image/png\r\nContent-ID: <logo@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n"
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
        f"Content-Type: image/svg+xml\r\nContent-ID: <svg@x>\r\n\r\n<svg xmlns='http://www.w3.org/2000/svg'>"
        f"<script>{TRIP}</script><image href='https://cidsvg.own.canary.test/'/></svg>",
        "Content-Type: text/html; name=page.html\r\nContent-Disposition: attachment; filename=page.html\r\n\r\n"
        f"<script>{TRIP}</script>",
    ),
)


# Attributes that load, navigate, style or animate: the ones a fingerprint tells apart. The rest only lay out.
LOADING_ATTRIBUTES = set(
    "href src srcset action formaction background poster data codebase xlink:href style http-equiv content ping rel "
    "target srcdoc lowsrc dynsrc longdesc usemap manifest icon archive classid itemtype values from to by "
    "attributename begin filter mask fill clip-path marker-end is type sandbox allow loading".split()
)


class Features(HTMLParser):
    """What a case's HTML uses: tags, loading attributes (event handlers as one), URL schemes, CSS functions and at-rules.
    Two cases with the same set exercise the same browser paths, as far as this can tell."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.found, self.in_style = set(), False

    def css(self, text):
        self.found |= {f"cssfn:{f.lower()}" for f in re.findall(r"([a-zA-Z-]+)\(", text)}
        self.found |= {f"css@{a.lower()}" for a in re.findall(r"@([a-zA-Z-]+)", text)}

    def handle_starttag(self, tag, attrs):
        self.found.add(f"tag:{tag}")
        self.in_style = tag == "style"
        for name, value in attrs:
            if name.startswith("on") or name in LOADING_ATTRIBUTES:
                self.found.add("attr:on*" if name.startswith("on") else f"attr:{name}")
            if scheme := re.match(r"\s*([a-zA-Z][a-zA-Z0-9+.-]*):", value or ""):
                self.found.add(f"scheme:{scheme[1].lower()}")
            if name == "style":
                self.css(value or "")

    def handle_data(self, data):
        if self.in_style:
            self.css(data)


def features(raw):
    found = set()
    for part in email.message_from_bytes(raw, policy=email.policy.compat32).walk():
        found.add(f"mime:{part.get_content_type()}")
        if part.get_content_type() == "text/html":
            parser = Features()
            try:
                parser.feed((part.get_payload(decode=True) or b"").decode(errors="replace"))
            except Exception:  # malformed markup is the point of these corpora
                pass
            found |= parser.found
    return found


manifest, everything = {}, {}
quick: dict[str, list[str | None]] = {}  # folder -> its cases' ids, None where the case is left out
for corpus, items in cases.items():
    for start in range(0, len(items), FOLDER_SIZE):
        chunk = items[start : start + FOLDER_SIZE]
        folder = corpus if len(items) <= FOLDER_SIZE else f"{corpus}-{start // FOLDER_SIZE + 1}"
        manifest[folder] = [case_id for case_id, _ in chunk]
        for k, (case_id, raw) in enumerate(chunk):
            everything[case_id] = (folder, k, features(raw))
        cur = OUT / "mail" / f".{folder}" / "cur"
        cur.mkdir(parents=True)
        for k, (_, raw) in enumerate(chunk):
            (cur / f"{1600000000 + k}.M{k}P1.hostile:2,").write_bytes(raw)
for sub in ("cur", "new", "tmp"):
    (OUT / "mail" / sub).mkdir(parents=True, exist_ok=True)
(OUT / "mail" / "subscriptions").write_text("".join(f"{f}\n" for f in manifest))
(OUT / "manifest.json").write_text(json.dumps(manifest, indent=1))

# The quick set: the own cases, then greedily the case adding the most features not yet covered, until every
# feature of the corpus is. Its manifest keeps each folder's positions, with null where a case is left out.
chosen = [case_id for case_id in everything if everything[case_id][0] == "own"]
covered = set().union(*(everything[c][2] for c in chosen))
while missing := set().union(*(f for _, _, f in everything.values())) - covered:
    best = max(everything, key=lambda c: len(everything[c][2] & missing))
    chosen.append(best)
    covered |= everything[best][2]
for case_id in chosen:
    folder, k, _ = everything[case_id]
    quick.setdefault(folder, [None] * len(manifest[folder]))[k] = case_id
(OUT / "quick.json").write_text(json.dumps(quick, indent=1))

# The reader set, opened one by one through the real client: the own cases, which prove the path end to end, and
# every case with a srcset, since Firefox's srcset race shows only in the reader's frame, never on the batch page.
reader: dict[str, list[str | None]] = {}
for case_id, (folder, k, found) in everything.items():
    if folder == "own" or "attr:srcset" in found:
        reader.setdefault(folder, [None] * len(manifest[folder]))[k] = case_id
(OUT / "reader.json").write_text(json.dumps(reader, indent=1))
print(f"quick set: {len(chosen)} cases cover {len(covered)} features")
print({folder: len(ids) for folder, ids in manifest.items()}, sum(map(len, manifest.values())))
