# Tasks

## 1. Spec

- [x] 1.1 MODIFY `check-config subcommand validates a config file without side effects` (orchestrator-cli): schema validation adds an advisory WARN for a path-less `openai_compatible`/`ollama` registry `api_base_url`; new scenario.

## 2. Code (`config.rs`)

- [x] 2.1 `is_pathless_base(url)` — host[:port] with no path, or a bare trailing slash.
- [x] 2.2 `check_model_base_urls(config, report)` — WARN (category `schema`, pointer `models/<name>/api_base_url`) per path-less `openai_compatible`/`ollama` registry entry; wired into `validate_config`. Scoped to the registry (agentic); not a hard error.

## 3. Tests

- [x] 3.1 `pathless_base_detection` — the host/path heuristic.
- [x] 3.2 `registry_pathless_openai_base_warns_not_errors` — path-less entry warns by pointer, a `/v1` entry does not, no hard error.

## 4. Docs

- [x] 4.1 `config.example.yaml` — Ollama registry example corrected to `http://localhost:11434/v1` with an explanatory comment.
- [x] 4.2 `docs/CONFIG.md` — `api_base_url` row spells out "full OpenAI-compatible base including its path; check-config WARNs on a path-less base."

## 5. Acceptance

- [x] 5.1 `cargo test` passes (new tests + full suite green).
- [x] 5.2 `openspec validate warn-pathless-openai-base --strict` passes.
