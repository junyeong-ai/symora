# Python monorepo fixture

Two-package uv workspace with a cross-package import, used by
`tests/lang_fixtures.rs` to exercise pyright end-to-end. The venv is
never committed — build it with `uv sync --frozen` in this directory.
