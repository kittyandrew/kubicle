#!/usr/bin/env bash
# DEVELOPMENT ONLY: the speed budgets of docs/design.md, measured on this machine with the staged build. Not a
# flake check and never run in a VM, where every step ran two to five times slower and the budgets only ever
# failed (Oct 25, 2026). Run it alone on a quiet machine; it exits 1 when a budget is missed.
#
#   tests/budgets/run.sh
#
# Starts the Dovecot fixture if it is not running (tests/fixture/fixture.sh), then budgets.py runs its own server.
set -euo pipefail
K=$(cd "$(dirname "$0")/../.." && pwd)
read -r SERVER WEB < <(nix build "$K#server" "$K#web" --no-link --print-out-paths | tr '\n' ' '; echo)
docker inspect -f '{{.State.Running}}' kubicle-fixture 2>/dev/null | grep -qx true || "$K/tests/fixture/fixture.sh" start
# The flake's own nixpkgs, read from git's index as every build here is, for the browsers and their driver.
PKGS="(builtins.getFlake \"git+file://$K\").inputs.nixpkgs.legacyPackages.\${builtins.currentSystem}"
BROWSERS=$(nix build --impure --no-link --print-out-paths --expr "$PKGS.playwright-driver.browsers")
nix shell --impure --expr "$PKGS.python3.withPackages (p: [p.playwright])" -c \
  env PLAYWRIGHT_BROWSERS_PATH="$BROWSERS" PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS=true \
  python3 -I "$K/tests/budgets/budgets.py" "$SERVER/bin/kubicle-server" "$WEB" "${KUBICLE_FIXTURE_CA:-${TMPDIR:-/tmp}/kubicle-fixture-ca.pem}"
