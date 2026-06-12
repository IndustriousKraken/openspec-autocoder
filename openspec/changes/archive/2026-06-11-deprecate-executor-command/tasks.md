# Tasks

## 1. Spec

- [x] 1.1 MODIFY `config.example.yaml is the canonical operator reference` (project-documentation): add the deprecated-field carve-out (deprecated fields are removed from the example + CONFIG.md, still honored, exempt from the coverage check).

## 2. Code

- [x] 2.1 `config.rs`: mark `ExecutorConfig::command` with a `DEPRECATED:` doc comment — still parsed AND honored; no functional change.

## 3. Docs

- [x] 3.1 `config.example.yaml`: drop the commented `command:` line from the `executor:` block.
- [x] 3.2 `docs/CONFIG.md`: drop the `command` row from the executor table; ensure the binary-resolution guidance (put the CLI on the daemon login PATH; symlink the canonical name for a fork) is present.

## 4. Acceptance

- [x] 4.1 `cargo test` passes (config round-trip + `example_yaml_mentions_every_top_level_field` still green — `command` stays live via the reviewer's field).
- [x] 4.2 `openspec validate deprecate-executor-command --strict` passes.
