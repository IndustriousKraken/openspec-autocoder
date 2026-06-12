# Tasks

## 1. Spec

- [x] 1.1 MODIFY `OpencodeStrategy implements the opencode CLI for agentic roles` (executor): `--model` + provider-block by case (autocoder-defined vs login-deferred); agentic roles pass their resolved model.

## 2. Code (implemented)

- [x] 2.1 `agentic_run.rs`: `OpencodeStrategy::writes_provider_block(m)` (ollama OR keyed authenticating); `provider_block` returns `None` for a key-less authenticating provider (no shadowing block); `apply_model_selection` selects `--model <provider-id>/<model>` when a block is written, else the model VERBATIM.
- [x] 2.2 `code_reviewer.rs`: `CodeReviewer.resolved_model` (+ `with_resolved_model`), set in `from_config` via `llm::resolve_reviewer_model`, threaded into `CliReviewSessionRunner` and passed to `agentic_run` (was `model: None`).
- [x] 2.3 `llm.rs`: `resolve_reviewer_model` (mirrors the gate model resolver; key optional → empty for a self-auth reviewer).

## 3. Tests (implemented)

- [x] 3.1 `agentic_run.rs`: a key-less `openai_compatible` model → no provider block + `--model` verbatim; keyed → block + `<provider>/<model>`; ollama → block + `ollama/<model>`.
- [x] 3.2 `llm.rs`: `resolve_reviewer_model` empties the key when keyless, resolves it when configured.
- [x] 3.3 `code_reviewer.rs`: `from_config` threads the resolved model onto the reviewer (keyless + keyed).

## 4. Docs (implemented)

- [x] 4.1 `docs/CONFIG.md`: the per-case `model:` convention for opencode-driven entries (login-authed keyless → full opencode id; keyed → bare id + base + key; ollama → bare id + base; `api_base_url`/path-less check only when autocoder writes the block).
- [x] 4.2 `config.example.yaml`: three worked examples (`hosted_login`, `beefy_security`, `fast_local`).

## 5. Acceptance

- [x] 5.1 `cargo test` passes (full suite green; strategy/reviewer/resolver tests assert the per-case selection).
- [x] 5.2 `openspec validate opencode-model-selection --strict` passes.
