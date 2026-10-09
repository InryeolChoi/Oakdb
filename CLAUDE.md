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

Oakdb must build, pass tests and keep the same durability guarantees on both macOS and Linux (see `docs/adr/0003-support-macos-and-linux.md`). GitHub Actions runs the tests on both OSes on every push; set this up together with the Rust project skeleton.
