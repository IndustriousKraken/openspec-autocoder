## Why

A `models:` registry entry always drives an **agentic** CLI (opencode for `openai_compatible`/`ollama`), which talks to the provider via the `@ai-sdk/openai-compatible` SDK and POSTs to `<base>/chat/completions`. So a **path-less** `api_base_url` (e.g. `http://host:11434`) hits the wrong path and the endpoint returns `404 page not found` — the gate then holds the change with an opaque cause. Ollama (and most OpenAI-compatible endpoints) serve under `/v1`. The convention is already specified ("operator owns the API root; the client does NOT auto-append `/v1`"), but nothing surfaced the mistake at config time, and the shipped example used a path-less Ollama base — so the footgun cost a long live debugging chase.

## What Changes

- `validate_config` gains an **advisory check**: for each `models:` registry entry with provider `openai_compatible`/`ollama` whose `api_base_url` is path-less (host[:port] with no path, or a bare trailing slash), emit a **WARN** (category `schema`) naming the model + pointer `models/<name>/api_base_url`, suggesting `/v1`. Never a hard error — a few endpoints serve chat-completions at the root. Surfaces at daemon startup AND in `check-config` (same `validate_config` surface).
- Scoped to the **registry** (unambiguously agentic). The one-shot HTTP Ollama path uses the native `/api/chat` endpoint with a bare base, so it is deliberately NOT flagged.
- `config.example.yaml` Ollama registry example corrected to `http://localhost:11434/v1` with an explanatory comment; `docs/CONFIG.md` `api_base_url` row spells out "full OpenAI-compatible base including its path."

## Impact

- **Affected specs:** `orchestrator-cli` — MODIFY `check-config subcommand validates a config file without side effects` (the schema check adds the advisory path-less-base WARN + a scenario).
- **Affected code:** `config.rs` — `is_pathless_base`, `check_model_base_urls`, wired into `validate_config`; two tests.
- **Affected docs:** `config.example.yaml`, `docs/CONFIG.md`.
- **Operator-visible:** a new advisory `WARN: schema:` line for a path-less registry base; exit code 1 (not 2) for a config whose only finding is this WARN. No behavior change to a correctly-configured daemon.
- **Acceptance:** `cargo test` (detection + warn-not-error tests) + `openspec validate warn-pathless-openai-base --strict`.
