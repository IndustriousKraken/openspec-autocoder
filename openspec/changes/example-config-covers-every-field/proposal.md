## Why

`config.example.yaml` was written when autocoder's schema was much smaller and has fallen behind. A field-by-field audit against `autocoder/src/config.rs` shows these configurable fields are entirely absent from the example operators consult when setting up the daemon:

- **Top-level `audits:`** — the periodic-audit framework (5 registered audits: `architecture_brightline`, `dependency_update_triage`, `drift_audit`, `missing_tests_audit`, `security_bug_audit`) is unmentioned. Operators discover it only by reading `docs/` or the spec, then guess at cadence syntax and per-audit `extra` keys.
- **Per-repo overrides** under `repositories[]`: `chatops_channel_id`, `max_changes_per_pr`, `audits:` (per-repo cadence overrides).
- **Executor knobs**: `implementer_prompt_path`, `perma_stuck_after_failures`, `max_changes_per_pr`, `startup_jitter_max_secs`, `inter_iteration_jitter_pct`.
- **`github.recreate_fork_on_reinit`** — the destructive recovery flag from change `optional-recreate-fork-on-reinit`.
- **`chatops.notifications.pr_opened`** — the third notification toggle (sibling of `start_work` and `failure_alerts`, both already in the example).

Closing the gap is straightforward — write the YAML. The more durable problem is the *contract*: each of these fields shipped in a change whose tasks.md called for documentation but did not call for updating `config.example.yaml`. Without a forcing function, the next configurable field added will repeat the omission.

This change does both:
1. Establishes a policy under `project-documentation` that every YAML-deserializable field in `Config` MUST have a corresponding mention in `config.example.yaml` — either as an active default value or as a commented annotation. Implementing agents shipping a new configurable field MUST update the example in the same commit as the schema change.
2. Closes the existing gap by adding each missing field above to `config.example.yaml`, commented out with a short description and (where applicable) a typical value.
3. Adds a `config::tests` unit test that asserts every documented top-level field name appears as a substring in `config.example.yaml`. The test maintains a hand-written list of field names; agents adding a new field must update the test list alongside the example. The test fails loudly when the two drift, catching omissions at CI time rather than at operator-onboarding time.

## What Changes

- **Policy:** ADDED requirement under `project-documentation` — `config.example.yaml` is the canonical operator-facing reference for the YAML schema. Every YAML field deserialized by `Config` and its nested types SHALL appear in the example, either active or commented. When a change adds a new configurable field, its commit MUST also update `config.example.yaml`.
- **Code:** `config.example.yaml` is expanded to cover every currently-missing field. Existing sections are preserved; missing fields are added in the most natural spot (audits as a new top-level block; executor knobs near existing executor entries; per-repo overrides under `repositories[]`).
- **Tests:** a new test in `config::tests` (call it `example_yaml_mentions_every_top_level_field`) reads `config.example.yaml` from disk via `CARGO_MANIFEST_DIR` (or repo-relative resolution) and asserts each known field name appears as a substring. The field-name list is maintained in the test.

## Impact

- Affected specs: `project-documentation` (one ADDED requirement establishing the example-coverage contract).
- Affected code: `config.example.yaml` (gap-filling additions), `autocoder/src/config.rs` (new test).
- Operator-visible behavior: none at runtime. The example file is documentation only; operators copying it to `config.yaml` and uncommenting fields they want will continue to get the same daemon behavior. The improvement is purely in what an operator can see when reading the example.
- Breaking: no. The example file is operator-facing reference material, not parsed by autocoder at runtime. Adding commented annotations cannot break any existing setup.
- Acceptance: every field name from the inventory above appears in `config.example.yaml`; the new test passes; `openspec validate example-config-covers-every-field --strict` passes; `cargo test` passes.
