MAX_PROPOSALS: {{MAX_PROPOSALS}}

You are auditing test coverage for this repository. Your output is zero
or more new OpenSpec change directories under `openspec/changes/`, each
describing a meaningful coverage gap and proposing tests to fill it.

## What to do

1. Survey the source tree. Identify source files via their extensions:
   `.rs`, `.py`, `.cs`, `.go`, `.js`, `.ts`, `.rb`, `.java`, `.kt`,
   `.swift`, `.cpp`, `.cc`, `.c`, `.h`. Use `Glob` to enumerate; use
   `Grep` and `Read` to inspect.
2. For each meaningful function/method in those files, identify whether
   it has tests AND whether those tests exercise its error/edge paths.
3. Focus on gaps with behavioral consequences:
   - `Error`/`Result` paths with no test (the happy path is covered but
     the failure branches are not).
   - Branches without assertions (the test runs the code but never
     verifies the output).
   - Obvious edge cases from the function signature: boundary values,
     `None`/`null`/empty inputs, off-by-one conditions, zero-length
     collections, integer overflow.

## What NOT to flag (anti-noise)

Suppress trivial gaps:
- Getters and setters with no logic.
- Single-line constructors.
- `Default` impls.
- `From`/`Into` (or equivalent) conversions with no behavior beyond
  field copying.
- Code in clearly experimental modules (e.g. files annotated
  `// EXPERIMENTAL` or under an `experimental/` directory).

Do NOT propose changes to test code that already works. Specifically:
- Do NOT propose deleting existing tests.
- Do NOT propose modifying existing tests unless they are factually
  broken (the test does not compile, or runs but never asserts).
- When in doubt, leave the existing test alone and propose a NEW test.

## Cap on proposals per run

`MAX_PROPOSALS` at the top of this prompt is the maximum number of
change directories you may create in this invocation. Pick the highest-
priority gaps:

1. Missing tests on error paths (highest).
2. Untested branches.
3. Obvious edge cases (lowest).

Emit at most `MAX_PROPOSALS` change directories. Remaining gaps will
be re-surfaced on subsequent runs.

## OpenSpec format

Each change is a directory under `openspec/changes/<change_name>/`.

Required files per change:

- `proposal.md` — three sections: `## Why`, `## What Changes`,
  `## Impact`. The `## Why` section names the coverage gap concretely
  (which functions, which paths). The `## What Changes` section names
  the new tests to be added. The `## Impact` section names the files
  the tests will land in.
- `tasks.md` — a numbered, bracketed-checkbox checklist where each
  item is a specific test function to add. Example:
  ```
  ## 1. Add error-path tests for parse_config
  - [ ] 1.1 `parse_config_errors_on_missing_required_field` —
    asserts `parse_config(input_with_missing_field)` returns
    `Err(ConfigError::MissingField("name"))`.
  - [ ] 1.2 `parse_config_errors_on_negative_port` —
    asserts `parse_config(toml_with_port_eq_minus_one)` returns
    `Err(ConfigError::InvalidPort)`.
  ```
- When the gap implies a capability invariant (i.e. the missing
  behavior maps to an existing requirement in
  `openspec/specs/<capability>/spec.md`), additionally include
  `specs/<capability>/spec.md` with a `## MODIFIED Requirements`
  block that adds new `#### Scenario:` entries describing the
  invariants the new tests will lock in. Otherwise this file is
  optional and may be omitted entirely.

## Naming convention

Prefix every change directory name with `tests-` so operators
recognize audit-produced changes at a glance. Examples:

- `tests-error-paths-in-queue-engine`
- `tests-edge-cases-in-busy-marker-recovery`
- `tests-boundary-values-in-rate-limiter`

Names are kebab-case and descriptive: name the SUBJECT of the missing
tests, not their location. (Good: `tests-empty-input-in-csv-parser`.
Bad: `tests-csv_parser_rs`.)

## Hard constraints

- Do NOT modify any file outside `openspec/changes/`. Your sandbox's
  WritePolicy is `OpenSpecOnly`; the framework reverts the entire
  diff and treats the run as failed if you write elsewhere.
- Do NOT propose deleting tests.
- Do NOT propose modifying existing tests unless they are factually
  broken (the test does not compile, or runs but never asserts).
- Do NOT exceed `MAX_PROPOSALS` change directories.
- Do NOT post chatops messages, run git commits, or push branches.
  The audit framework commits validated changes for you after your
  run finishes.

If you find zero meaningful gaps after a good-faith inspection,
create zero change directories and exit cleanly. The framework
treats an empty result as success (no chatops post, no commit).
