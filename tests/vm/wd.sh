# shellcheck shell=bash
# The VM tests' browser driver: a W3C WebDriver client for one headless Chromium session, through chromedriver
# on 127.0.0.1:9515 (browser.nix). The session id lives in /tmp/wd-session, or in $WD_SESSION for a second
# browser, whose network log then goes to $WD_SESSION.netlog.json.
#
#   wd start                   open Chromium at 1440x900
#   wd go URL
#   wd js 'return ...'         run a script in the page, print its JSON value
#   wd async 'done(...)' [s]   the same for a script that answers through the callback `done` (default 30 s)
#   wd cdp METHOD 'json'       a Chrome DevTools Protocol command, e.g. Network.emulateNetworkConditions
#   wd wait 'expression' [s]   poll the page until the expression is truthy (default 30 s), print its value
#   wd click CSS
#   wd type CSS TEXT
#   wd shot FILE               PNG screenshot
#   wd windows                 the session's window handles, as a JSON list
#   wd window HANDLE           switch to that window
#   wd close                   close the window commands go to
#
# Scripts run as the WebDriver spec defines, so the page's CSP does not apply to them, but it does to eval():
# `wait` splices its expression into the script text instead.

url=http://127.0.0.1:9515

call() { # METHOD PATH [JSON]: the answer's value, or its error on stderr and exit 1
  local out
  out=$(curl -sS -X "$1" "$url$2" -H 'content-type: application/json' ${3:+--data-binary "$3"})
  if jq -e '.value | objects | has("error")' <<<"$out" >/dev/null; then
    jq -r '.value | "webdriver: \(.error): \(.message)"' <<<"$out" | head -c 2000 >&2
    exit 1
  fi
  jq -c .value <<<"$out"
}

file=${WD_SESSION:-/tmp/wd-session}
netlog=${WD_SESSION:+$WD_SESSION.netlog.json}
netlog=${netlog:-/tmp/netlog.json}

session() { cat "$file"; }

element() { # CSS -> element id
  call POST "/session/$(session)/element" "$(jq -n --arg css "$1" '{using: "css selector", value: $css}')" | jq -r '.[]'
}

async() { # SCRIPT TIMEOUT_MS
  call POST "/session/$(session)/timeouts" "{\"script\": $(($2 + 5000))}" >/dev/null
  call POST "/session/$(session)/execute/async" \
    "$(jq -n --arg s "const done = arguments[arguments.length - 1]; $1" '{script: $s, args: []}')"
}

case $1 in
  start)
    caps=$(jq -n --arg bin "$CHROMIUM" '{capabilities: {alwaysMatch: {browserName: "chrome", "goog:chromeOptions": {
      binary: $bin, args: ["--headless=new", "--no-sandbox", "--window-size=1440,900", "--no-first-run",
      "--log-net-log=\($netlog)"]}}}}' --arg netlog "$netlog")
    call POST /session "$caps" | jq -r .sessionId >"$file"
    ;;
  go) call POST "/session/$(session)/url" "$(jq -n --arg u "$2" '{url: $u}')" >/dev/null ;;
  js) call POST "/session/$(session)/execute/sync" "$(jq -n --arg s "$2" '{script: $s, args: []}')" ;;
  async) async "$2" $((${3:-30} * 1000)) ;;
  cdp) call POST "/session/$(session)/goog/cdp/execute" "$(jq -n --arg c "$2" --argjson p "$3" '{cmd: $c, params: $p}')" ;;
  wait) # one short call per poll: a navigation meanwhile (sso.php reloads the page) fails that poll, not the wait
    end=$(($(date +%s) + ${3:-30}))
    script=$(jq -n --arg s "try { return ($2) || null; } catch (e) { return null; }" '{script: $s, args: []}')
    while [ "$(date +%s)" -lt "$end" ]; do
      value=$(call POST "/session/$(session)/execute/sync" "$script" 2>/dev/null) || value=null
      if [ "$value" != null ]; then
        echo "$value"
        exit 0
      fi
      sleep 0.05
    done
    echo "wd: timed out after ${3:-30} s waiting for: $2" >&2
    exit 1
    ;;
  click) call POST "/session/$(session)/element/$(element "$2")/click" '{}' >/dev/null ;;
  type) call POST "/session/$(session)/element/$(element "$2")/value" "$(jq -n --arg t "$3" '{text: $t}')" >/dev/null ;;
  shot) call GET "/session/$(session)/screenshot" | jq -r . | base64 -d >"$2" ;;
  windows) call GET "/session/$(session)/window/handles" ;;
  window) call POST "/session/$(session)/window" "$(jq -n --arg h "$2" '{handle: $h}')" >/dev/null ;;
  close) call DELETE "/session/$(session)/window" >/dev/null ;;
  *)
    echo "wd: unknown command $1" >&2
    exit 2
    ;;
esac
