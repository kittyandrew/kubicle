#!/usr/bin/env bash
# Every flake check at once, in about the time of the longest one. The checks' own derivations build in the Nix
# daemon; the VM tests' drivers then run side by side in a slice of the user's systemd, where one memory cap bounds
# them all and CPU weights keep the timing-sensitive VMs ahead of hostile's rendering. Same tests, same verdicts
# as `nix flake check` (tests/judge.nix). tests/lint.sh, all that CI runs, runs beside the builds.
#
#   tests/suite.sh                  # every check, and lint
#   tests/suite.sh vm-full hostile  # only these
#
# Each VM check's evidence stays in $KUBICLE_SUITE_DIR/<check> (default /tmp/kubicle-suite): out/ holds what the
# flake check's run would, log is its console. Exits 1 when any check failed.
set -euo pipefail
# One run at a time on the machine, from any checkout: each run sets the shared slice's memory cap to its own
# budget, so two at once OOM-killed a VM (Oct 25, 2026). A second run waits here for the first.
exec 9>/tmp/kubicle-suite.lock
flock -n 9 || { echo "waiting for another tests/suite.sh to finish"; flock 9; }
K=$(cd "$(dirname "$0")/.." && pwd)
DIR=${KUBICLE_SUITE_DIR:-${TMPDIR:-/tmp}/kubicle-suite}
SYS=x86_64-linux
SLICE=kubicle-suite.slice
ALL_VMS=(vm-full hostile explore vm-live-mail vm-live-offline vm-live-imap-cut)
ALL_BUILDS=(fmt toml clippy-server clippy-web server web)
VMS=() BUILDS=()
for c in "${ALL_VMS[@]}"; do [ $# -eq 0 ] || [[ " $* " == *" $c "* ]] && VMS+=("$c"); done
[[ " $* " == *" vm-settings "* ]] && VMS+=(vm-settings) # 21 minutes against Roundcube: only when named
for c in "${ALL_BUILDS[@]}"; do [ $# -eq 0 ] || [[ " $* " == *" $c "* ]] && BUILDS+=("$c"); done
LINT= && { [ $# -eq 0 ] || [[ " $* " == *" lint "* ]]; } && LINT=1
now() { date +%s%N; }
secs() { echo "$(( ($(now) - $1) / 1000000000 ))s"; }
start=$(now)

# The drivers first: the VMs start as soon as they exist, while lint and the release builds run beside them.
installables=()
for c in "${VMS[@]}"; do installables+=("$K#checks.$SYS.$c.run.driver"); done
json=$(nix build --no-link --json "${installables[@]}") # stops here on a failed build, which <(...) would hide
mapfile -t built < <(jq -r '.[].outputs.out' <<<"$json")
declare -A driver
for i in "${!VMS[@]}"; do driver[${VMS[$i]}]=${built[$i]}; done
echo "drivers built in $(secs "$start")"

builds_log=$DIR/builds.log
mkdir -p "$DIR"
if [ ${#BUILDS[@]} -gt 0 ]; then
  nix build --no-link --keep-going "${BUILDS[@]/#/$K#checks.$SYS.}" >"$builds_log" 2>&1 &
  builds_pid=$!
fi
if [ -n "$LINT" ]; then
  nix develop "$K#lint" -c "$K/tests/lint.sh" >"$DIR/lint.log" 2>&1 &
  lint_pid=$!
fi

# One cap for every VM: what is free now, less 2 GB for the desktop. Past it the kernel kills inside the slice.
avail=$(awk '/MemAvailable/ {print int($2 / 1024)}' /proc/meminfo)
budget=$((avail - 2048))
systemctl --user set-property --runtime "$SLICE" MemoryMax="${budget}M" MemorySwapMax=0 2>/dev/null || true
# Each check's memory peak in MB, its scope's as systemd logged it (Oct 25, 2026): the VMs' memory plus the store
# pages they read, which count against the slice. Checks start longest first while their peaks fit the budget.
declare -A peak=([explore]=2800 [hostile]=10800 [vm-live-offline]=3800 [vm-live-imap-cut]=2800 [vm-live-mail]=3000
  [vm-full]=4200 [vm-settings]=4500)
# Latency budgets and deadlines in every VM check but hostile, which counts leaks and takes what CPU is left.
weight() { [ "$1" = hostile ] && echo 50 || echo 1000; }
order=()
for c in vm-settings explore hostile vm-live-offline vm-live-imap-cut vm-live-mail vm-full; do
  [[ " ${VMS[*]} " == *" $c "* ]] && order+=("$c")
done
declare -A pid began
used=0
launch() {
  local c=$1
  rm -rf "${DIR:?}/$c" && mkdir -p "$DIR/$c/out" "$DIR/$c/tmp"
  began[$c]=$(now)
  used=$((used + peak[$c]))
  # The driver keeps VM state and the switch's socket in XDG_RUNTIME_DIR, under the node's name: every test calls
  # its VM `machine`, so runs sharing it deleted each other's state and hung at "start all VLans".
  systemd-run --user --scope -q --unit="kubicle-suite-$c-$$" --slice="$SLICE" -p CPUWeight="$(weight "$c")" \
    env out="$DIR/$c/out" TMPDIR="$DIR/$c/tmp" XDG_RUNTIME_DIR="$DIR/$c/tmp" \
    "${driver[$c]}/bin/nixos-test-driver" -o "$DIR/$c/out" </dev/null >"$DIR/$c/log" 2>&1 &
  pid[$c]=$!
}
verdict() {
  local c=$1 took
  took=$(secs "${began[$c]}")
  if [ ! -e "$DIR/$c/out/failed" ]; then
    echo "FAIL $c ($took): the run ended before recording a verdict; its console: $DIR/$c/log" && failed=1
  elif [ -s "$DIR/$c/out/failed" ]; then
    echo "FAIL $c ($took):" && sed 's/^/  /' "$DIR/$c/out/failed" && failed=1
  else
    echo "ok   $c ($took)"
  fi
}
failed=0
queue=("${order[@]}")
running=()
while [ ${#queue[@]} -gt 0 ] || [ ${#running[@]} -gt 0 ]; do
  while [ ${#queue[@]} -gt 0 ] && { [ ${#running[@]} -eq 0 ] || [ $((used + peak[${queue[0]}])) -le "$budget" ]; }; do
    launch "${queue[0]}" && running+=("${queue[0]}") && queue=("${queue[@]:1}")
  done
  pids=() && for c in "${running[@]}"; do pids+=("${pid[$c]}"); done
  ended= && wait -n -p ended "${pids[@]}" || true
  still=()
  for c in "${running[@]}"; do
    if [ "${pid[$c]}" = "$ended" ]; then
      used=$((used - peak[$c]))
      verdict "$c"
    else
      still+=("$c")
    fi
  done
  running=("${still[@]}")
done
if [ ${#BUILDS[@]} -gt 0 ]; then
  if wait "$builds_pid"; then echo "ok   ${BUILDS[*]}"; else echo "FAIL a build or lint check: $builds_log" && failed=1; fi
fi
if [ -n "$LINT" ]; then
  if wait "$lint_pid"; then echo "ok   lint"; else echo "FAIL tests/lint.sh: $DIR/lint.log" && failed=1; fi
fi
echo "all in $(secs "$start")"
exit "$failed"
