# Rules structure

All `.md` files under `.claude/rules/` load into every Claude Code session in this repo.

- Name files `NNN-name.md` with a unique 3-digit number, and start each with an H1 title.
- Rules are directives. Rationale lives in `docs/design.md`; point at it rather than repeating it.
- Keep each file at 100 lines or fewer, and split one that grows past that.
- Point at files, never at line numbers, which go stale silently.
- Wrap prose at about 110 columns and fill each line. ASCII only: `-` and `->`, no em-dash or arrows.
