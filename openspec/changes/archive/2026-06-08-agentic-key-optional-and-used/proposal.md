## Why

The intended rule for an agentic CLI role is simple AND uniform across all three CLIs (`claude`, `opencode`, `agy`): **no key → the CLI uses its own authenticated session; a supplied key → the CLI uses that key; the key is always optional.** Today's behavior drifted from that on two axes, and the drift is a footgun:

- **Config-load REQUIRES `api_key`** for `anthropic` AND `openai_compatible` (the provider-driven rule), so a CLI role that intends to rely on the CLI's own login still cannot start without configuring a key.
- **a003 then makes the `claude` AND `opencode` strategies IGNORE any supplied key** (the credential is never passed to the subprocess) — so the same key it forced you to set is then unused, and a supplied key can never override the CLI's ambient auth.

The result: required-but-unused. An operator who wants to point a gate/reviewer at a specific model via a supplied key cannot, and an operator who wants pure CLI-self-auth is forced to configure a phantom key. `agy` (antigravity) already does the intended thing (optional key → `AV_API_KEY`); this change makes `claude` AND `opencode` match it.

## What Changes

**Key is optional for CLI/agentic roles.** Config-load stops requiring `api_key` when the resolved model is driven by a CLI strategy (the CLI self-authenticates). The `api_key` requirement stays for **in-process HTTP** consumers (the non-agentic `oneshot` reviewer and the RAG/embedding HTTP calls), which genuinely need the key in the daemon's process.

**A supplied key is used.** When a CLI role has a configured `api_key`, the strategy passes it to the CLI so the CLI uses that key: `claude` via `ANTHROPIC_API_KEY`, `opencode` via its provider config, `agy` via `AV_API_KEY` (unchanged). No key → no credential reaches the subprocess; the CLI uses its own login/store (the prior a003 default, preserved as the safe default).

**Best-effort hiding + documented residual.** When a key IS supplied, it is placed where the existing config-store protection covers it — the CLI's own config store, reached by `engine_deny` (the per-invocation tool denylist) — and **never** a workspace file. The hard constraint, stated plainly: the model and the wrapped CLI are the **same process and uid**, so a key the CLI can use is one the model can ultimately reach. `engine_deny` is deterrence, not a bound (per the existing os-hide/engine-deny requirement), and a CLI that only accepts a key via the subprocess env (`claude` → `ANTHROPIC_API_KEY`) leaves the key model-readable. Supplying a key is therefore an explicit operator opt-in to that exposure; the no-key default keeps a003's no-credential posture. We do what the CLI tooling allows AND document the residual.

## Impact

- **Affected specs:**
  - `executor` — MODIFY `CLI strategies pass no LLM credential to the wrapped subprocess` (no-key default vs supplied-key opt-in; best-effort hiding; documented residual).
  - `orchestrator-cli` — MODIFY `Canonical LlmProvider enum AND per-provider auth semantics` (`api_key` mandatory-ness becomes consumer-aware: required for in-process HTTP consumers, optional for CLI/agentic roles).
- **Affected code:** `validate_llm_provider_config` + the per-subsystem config-load callers (skip the `api_key`-required check when the resolved model is CLI-driven); `ClaudeStrategy::apply_model_selection` (set `ANTHROPIC_API_KEY` when a key is present) and the `opencode` strategy's provider config (write the key to opencode's config store, never the workspace `opencode.json`) — `agy` already passes `AV_API_KEY`; the `engine_deny` set covers the supplied-key location.
- **Operator-visible behavior:** a CLI role with no key starts AND runs on the CLI's own auth; a CLI role with a key uses that key; no phantom key is required. Existing in-process HTTP reviewer/RAG configs are unchanged (key still required there).
- **Security:** documented trade-off — no-key is the no-exposure default; a supplied key is an opt-in that the same-uid model can ultimately read (deterred, not bounded). `docs/SECURITY.md` states this.
- **Non-goals:** NOT changing `api_base_url` mandatory-ness; NOT changing the OS sandbox mask-list/policies; NOT changing the in-process HTTP key handling. Full isolation of a supplied key from the model is explicitly out of reach (same process/principal) and is documented rather than attempted.
- **Dependencies:** builds on `CLI config stores are protected by OS-hide and engine-deny` AND the model registry. No unmerged dependencies.
- **Acceptance:** `cargo test` + `openspec validate agentic-key-optional-and-used --strict` pass. Tests: a CLI role with no key loads AND spawns with no credential in the subprocess; a CLI role with a key passes it to the CLI (claude → `ANTHROPIC_API_KEY`; opencode → config store, never the workspace file); config-load no longer requires a key for a CLI-driven `anthropic`/`openai_compatible` role but still requires it for the in-process HTTP reviewer/RAG; the supplied-key location is in the `engine_deny` set.
