## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues through the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Workflow

Each core feature goes through four steps, and each step leaves a Korean, blog-ready record under `docs/`:

1. **Design**: explain the options and trade-offs in plain language; the user decides. Record hard-to-reverse decisions in `docs/adr/`.
2. **Build**: AI implements it test-first.
3. **Verify**: code review (including Rust-specific design points), fault experiments in `docs/experiments/`, benchmarks in `docs/benchmarks/`.
4. **Retrospective**: what was learned, in `docs/retros/`. Don't add interview Q&A sections.

Each ticket is built on its own branch (`ticket/<NN>-<slug>`) and lands through a pull request whose description says `Closes #<issue>`. CI must pass on both OSes before the user merges it; never commit to `main` directly.

Before the user merges, run the pre-merge verification against a fixed point (the base of the PR): the two-axis code review (Standards / Spec), a blind tester that writes tests from the issue, spec and ADRs without reading the implementation, and an A/B hypothesis test (with an A/A control) when the ticket makes a performance claim. Post each result as a PR comment. Fix nothing and close nothing on the strength of these results until the user decides.

Write every PR description in Korean with `.github/pull_request_template.md`: goal first, then tables for how many completion criteria are met, verification results and hypothesis tests, then the decisions the user has to make. Tables before prose; keep each explanation to a line or two. Update the description when the PR changes, so it always shows the current state.

Oakdb must build, pass tests and keep the same durability guarantees on both macOS and Linux (see `docs/adr/0003-support-macos-and-linux.md`). GitHub Actions runs the tests on both OSes on every push; set this up together with the Rust project skeleton.
