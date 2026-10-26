# Prepended to every VM test script (tests/judge.nix): the run records and never fails on what it found, so its
# evidence stays in $out; the check over it judges. `fail` notes a failure, `guarded` runs a step and notes what it
# raised, `collect` copies a machine's journal and files out, and `record` writes the verdict last.
import os
import pathlib
import traceback

VERDICT: list[str] = []


def fail(what: str) -> None:
    VERDICT.append(what)
    print(f"FAILED: {what}")


def guarded(name: str, step) -> None:
    try:
        step()
    except Exception:
        fail(f"{name}: {traceback.format_exc(limit=4).strip()}")


def collect(machine, *paths: str) -> None:
    """The machine's whole journal and `paths` (globs allowed) into $out/<machine>/. A machine that crashed keeps
    only what its console printed, which the run's build log holds (`nix log` on the run)."""
    names = " ".join(paths)
    script = (
        "rm -rf /tmp/record && mkdir -p /tmp/record && journalctl -o short-monotonic --no-pager > /tmp/record/journal.txt;"
        + f' for p in {names}; do [ -e "$p" ] && cp -r --parents "$p" /tmp/record/; done; true'
    )
    try:
        machine.execute(script, timeout=120)
        machine.copy_from_machine("/tmp/record", machine.name)
    except Exception:
        fail(f"{machine.name}: its evidence could not be copied: {traceback.format_exc(limit=2).strip()}")


def record() -> None:
    (pathlib.Path(os.environ["out"]) / "failed").write_text("".join(f"{v}\n" for v in VERDICT))
