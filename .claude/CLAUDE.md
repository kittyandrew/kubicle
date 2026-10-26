# kubicle

A webmail for Mailu in Rust: an axum server that lets Mailu's Dovecot sort, thread and search, and a Leptos
client compiled to WebAssembly. It takes Roundcube's place in Mailu's webmail slot and signs in through Mailu.
AGPL-3.0, local repository until its owner publishes it.

- `.github/README.md` is the user-facing setup.
- `docs/design.md` holds every design decision, the Mailu contracts, the threat model and what kubicle does not
  build. Read it before changing behaviour, and update it in the same change.
- A plan not built yet gets its own file in `docs/ideas/`, created with the first plan; it moves into
  `docs/design.md` once built.
- `.claude/rules/` is the operating manual, loaded every session: `001-architecture.md` for the parts and the
  contracts between them, `002-dev.md` for the gates and test suites, `003-vocabulary.md` for labelling who does
  a step.
