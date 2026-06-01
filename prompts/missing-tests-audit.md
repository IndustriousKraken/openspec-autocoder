MAX_PROPOSALS: {{MAX_PROPOSALS}}

You are auditing test coverage for this repository. Your output is
zero or more new OpenSpec change directories under `openspec/changes/`,
each describing a meaningful coverage gap AND proposing tests to fill it.

OpenSpec format reference: https://github.com/Fission-AI/OpenSpec/tree/main/docs
(`concepts.md` for scenario syntax `GIVEN`/`WHEN`/`THEN`, delta blocks
`ADDED`/`MODIFIED`/`REMOVED`/`RENAMED`, AND requirement-header rules).
Consult on `openspec validate --strict` failures.

## What to do

1. Survey the source tree. Identify source files via extensions:
   `.rs`, `.py`, `.cs`, `.go`, `.js`, `.ts`, `.rb`, `.java`, `.kt`,
   `.swift`, `.cpp`, `.cc`, `.c`, `.h`. Use `Glob` to enumerate; use
   `Grep` AND `Read` to inspect.
2. For each meaningful function, identify whether it has tests AND
   whether those tests exercise its error/edge paths.
3. Focus on gaps with behavioral consequences:
   - `Error`/`Result` paths with no test (happy path covered, failure
     branches not).
   - Branches without assertions (test runs the code but never
     verifies output).
   - Obvious edge cases from the signature: boundary values,
     `None`/`null`/empty inputs, off-by-one conditions, zero-length
     collections, integer overflow.

## What NOT to flag

Suppress trivial gaps:
- Getters and setters with no logic.
- Single-line constructors.
- `Default` impls.
- `From`/`Into` conversions with no behavior beyond field copying.
- Code in clearly experimental modules (`// EXPERIMENTAL`, files
  under `experimental/`).

Do NOT propose changes to test code that already works:
- Do NOT propose deleting existing tests.
- Do NOT propose modifying existing tests unless factually broken
  (does not compile, or runs but never asserts).
- When in doubt, leave the existing test alone AND propose a NEW test.

## Cap on proposals per run

`MAX_PROPOSALS` is the maximum number of change directories per
invocation. Order by priority:

1. Missing tests on error paths (highest).
2. Untested branches.
3. Obvious edge cases (lowest).

## OpenSpec format

Each change is `openspec/changes/<change_name>/`. Required files:

- `proposal.md` — `## Why` (names the coverage gap concretely:
  functions, paths), `## What Changes` (names the new tests),
  `## Impact` (names the files the tests will land in).
- `tasks.md` — numbered, bracketed-checkbox checklist. Each item is
  a specific test function to add. Example:
  ```
  ## 1. Add error-path tests for parse_config
  - [ ] 1.1 `parse_config_errors_on_missing_required_field` —
    asserts `parse_config(input_with_missing_field)` returns
    `Err(ConfigError::MissingField("name"))`.
  - [ ] 1.2 `parse_config_errors_on_negative_port` —
    asserts `parse_config(toml_with_port_eq_minus_one)` returns
    `Err(ConfigError::InvalidPort)`.
  ```
- When the gap implies a capability invariant (maps to an existing
  requirement under `openspec/specs/<capability>/spec.md`),
  additionally include `specs/<capability>/spec.md` with a
  `## MODIFIED Requirements` block adding new `#### Scenario:`
  entries. Otherwise this file is optional.

### `tasks.md` items must be agent-actionable

Every task goes to the implementer agent on a subsequent iteration.
Tasks the implementer's sandbox cannot perform belong in `docs/`, NOT
in tasks.md. Forbidden task shapes:

- Manual operator runbook steps (real-server smoke tests, SSH-based
  verification, dashboard inspection).
- `sudo` against live hosts; hardware or OS-version smoke tests.
- "A human operator runs X" — the implementer cannot perform these.

If a coverage gap can only be filled by a manual procedure (e.g.,
"verify the load balancer rotates correctly under live traffic"), it
is OUT OF SCOPE for this audit. Skip it. The implementer pre-flight
rejects specs containing forbidden tasks AND throws the spec back for
revision.

## Naming convention

Prefix every change directory with `tests-`. Names are kebab-case AND
descriptive — name the SUBJECT of the missing tests, not their
location.

- `tests-error-paths-in-queue-engine`
- `tests-edge-cases-in-busy-marker-recovery`
- `tests-boundary-values-in-rate-limiter`

## Hard constraints

- Do NOT modify any file outside `openspec/changes/`. Sandbox
  WritePolicy is `OpenSpecOnly`; writes elsewhere fail the run.
- Do NOT propose deleting tests.
- Do NOT propose modifying existing tests unless factually broken.
- Do NOT exceed `MAX_PROPOSALS` change directories.
- Do NOT post chatops messages, run git commits, OR push branches.
  The audit framework commits validated changes after your run
  finishes.

Zero meaningful gaps after a good-faith inspection is a valid
outcome. Create zero change directories AND exit cleanly.
