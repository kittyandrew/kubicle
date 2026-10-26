# Vocabulary

One file for every project term. Use the exact word; do not coin synonyms.

## Who performs a step: `manual` / `agentic` / `automated`

Every step is exactly one of these three, chosen by who must do it, not by where it runs. A step that feels
like a blend is more than one step: split it into steps that each take a single label ("the agent deploys,
then a human signs in to Mailu", never one fuzzy step).

- **`manual`**: a human does it; an agent cannot. Signing in to Mailu in a browser, judging how a theme
  looks, `git push`, a go/no-go on anything destructive.
- **`agentic`**: an agent does it. Editing and building, running the suites, deploying, reading logs and
  metrics. Agentic even when a human could also do it.
- **`automated`**: the system does it unattended, triggered by neither: CI on push, IMAP IDLE delivering new
  mail, the container restart after a crash.

**Closed vocabulary.** Use only these terms, exactly: no `semi-agentic`, `mostly automated` or other blends.
Adding or changing a term needs explicit human approval; propose it, never just use it. Label only steps,
not prose or file names.
