#!/usr/bin/env bash
# Every formatter, linter and type checker, the same run locally and in CI (.github/workflows/quality.yml):
#
#   nix develop .#lint -c tests/lint.sh   # the lint shell pins each tool; the default shell has them too
#
# Each gate runs even after one failed, and the exit code is 1 when any did. `taplo fmt`, `ruff format`,
# `prettier --write` and `cargo fmt --all` rewrite what their gate reports.
set -uo pipefail
cd "$(dirname "$0")/.." || exit
failed=0
gate() { # gate <name> <command...>
  if "${@:2}"; then echo "ok   $1"; else echo "FAIL $1" && failed=1; fi
}
# shellcheck disable=SC2329 # called through gate
tracked() { # tracked <glob> <command...>: the command on every tracked file the glob matches
  git ls-files -z "$1" | xargs -0 -r "${@:2}" --
}
gate rustfmt cargo fmt --all --check
gate alejandra alejandra -q -c .
gate deadnix deadnix --fail .
gate taplo taplo fmt --check # TOML, as .taplo.toml sets it
gate "ruff format" tracked "*.py" ruff format --check
gate "ruff check" tracked "*.py" ruff check
# The tests' own Python environments are not in the shell, so their imports go unchecked.
gate mypy tracked "*.py" mypy --explicit-package-bases --ignore-missing-imports
gate prettier prettier --check "**/*.{js,mjs,ts,yaml,yml}"
gate oxlint oxlint --deny-warnings
gate shellcheck tracked "*.sh" shellcheck
gate zizmor zizmor .github/workflows # online audits too when GH_TOKEN is set, as in CI
exit "$failed"
