# Repository Guidelines

## Project Structure & Module Organization

This Rust 2021 binary crate uses `src/main.rs` to wire the Axum server. The `src/*.rs` modules implement APIs, configuration, conversations, routing, browser sessions, and persistence. Keep HTTP handlers in `*_api.rs` and stateful/runtime behavior in the matching domain module (for example, `browser_session_api.rs` and `browser_session_runtime.rs`). Routing lives in `src/routing/`; protocol compatibility code is in `src/compat/`.

The local web interface is in `ui/`, browser-provider adapters are in `adapters/`, and smoke tests/test doubles are in `scripts/`. Use `config/llmgateway.example.toml` as the configuration baseline; place design notes in `docs/`.

## Build, Test, and Development Commands

- `cargo run --release` starts the gateway locally after copying the example config and `.env.example`.
- `cargo check --all-targets` performs a fast compile check.
- `cargo fmt --check` verifies Rust formatting; run `cargo fmt` to apply it.
- `cargo clippy --all-targets` checks Rust code for common mistakes.
- `cargo test --all-targets` runs the in-module Rust test suite.
- `node --check ui/app.js` validates UI JavaScript; `node scripts/test-browser-adapter-fixtures.mjs` tests adapter fixtures.
- `bash scripts/smoke-local.sh` runs the primary end-to-end smoke test. Run a focused `smoke-*.sh` script for the area changed.
- `docker build -t llmgateway:ci .` verifies the container build.

## Coding Style & Naming Conventions

Use standard `rustfmt` output (four-space indentation). Follow Rust conventions: `snake_case` for modules, files, functions, and variables; `PascalCase` for types; `SCREAMING_SNAKE_CASE` for constants. Keep modules narrowly scoped, favor error propagation over panics, and use descriptive names such as `provider_conversation_affinity`. Match the existing plain JavaScript style in `ui/` and `adapters/`; do not add a frontend build step.

## Testing Guidelines

Place unit tests in a `#[cfg(test)] mod tests` block beside covered code. Name tests by behavior, e.g. `preserves_target_provider_identity`; use `#[tokio::test]` for async cases. Add or extend a focused smoke script for changes across HTTP, browser, persistence, or routing boundaries. CI has no stated coverage threshold; all relevant checks must pass.

## Commit & Pull Request Guidelines

Use concise Conventional Commit-style subjects consistent with history: `test: preserve target provider identity across stream control` or `ci: run native conversation affinity gate earlier`. Prefer a scoped prefix when useful, such as `fix:`, `feat:`, `docs:`, `test:`, or `ci:`.

Pull requests should explain the behavioral change, configuration or migration impact, and tests run. Link the related issue when available. Include screenshots for visible UI changes and redact API keys, cookies, browser profiles, and local `.env` values. Do not commit generated `data/` state or personal configuration.


## Agent Skill Guidelines

The portable llmgateway Agent Skill lives in `skills/llmgateway/`. Keep `SKILL.md` concise and use `references/` for progressive disclosure. Runtime behavior documented by the skill must match the current API/config contracts; do not embed a second model-ranking or routing engine in agent instructions.

The helper CLI in `skills/llmgateway/scripts/llmgateway_agent.py` is intentionally READ + EXECUTE only. Do not add destructive or state-mutating commands there without an explicit design/security review. Test helper changes with:

```bash
python3 -m unittest skills/llmgateway/tests/test_llmgateway_agent.py
```

When adding future multimodal, MCP, or capability-routing guidance, feature-detect against shipped code and keep credentials, browser auth material, CAPTCHA/2FA, and provider anti-abuse boundaries out of agent control.
