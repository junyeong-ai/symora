# Python monorepo fixture

Two-package uv workspace with a cross-package import, used by
`tests/lang_fixtures.rs` to exercise pyright end-to-end. The venv is
never committed. From this directory, prepare both the pinned pyright and venv:

```bash
npm ci --ignore-scripts
uv sync --frozen
```

From the repository root, run the fixtures with the local pyright on `PATH`:

```bash
PATH="$PWD/tests/fixtures/python/monorepo/node_modules/.bin:$PATH" \
  cargo test --test lang_fixtures -- --include-ignored
```
