# Python monorepo fixture

Two-package uv workspace with a cross-package import, used by
`tests/lang_fixtures.rs` to exercise pyright end-to-end. The venv is
never committed. From this directory, prepare both the pinned pyright and venv:

```bash
npm ci --ignore-scripts
uv sync --frozen
```

From the repository root, run the real-server suites as the Validation section of [`CLAUDE.md`](../../../../CLAUDE.md) shows.
