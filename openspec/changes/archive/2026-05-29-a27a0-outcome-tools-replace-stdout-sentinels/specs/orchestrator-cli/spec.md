## ADDED Requirements

### Requirement: Control socket exposes `record_outcome` AND `consume_outcome` actions for execution-scoped outcome storage

The control socket SHALL accept two new actions that mediate outcome-signaling between the per-execution MCP child AND the executor's classifier:

- **`record_outcome`** — writes a recorded outcome to the daemon's execution-scoped outcome store, keyed by `(workspace_basename, change)`. Request shape:

  ```json
  {
    "action": "record_outcome",
    "workspace_basename": "<sanitized basename of the workspace>",
    "change": "<openspec change name>",
    "outcome": { ... variant-tagged payload ... }
  }
  ```

  The `outcome` field is variant-tagged with `"type"`:

  - `"type": "success"` with optional `"final_answer": string` (defaults to empty string on absence).
  - `"type": "spec_needs_revision"` with `"unimplementable_tasks": Array<{ task_id: string, task_text: string, reason: string }>` (non-empty) AND `"revision_suggestion": string` (non-empty).

  Response shape on success: `{ "ok": true }`. Response shape on a malformed payload (missing fields, unknown variant tag, wrong types): `{ "ok": false, "error": "<message>" }`. The handler trusts the relayed payload (the MCP layer validated it before sending); the failure case exists to surface programmer error during development of new clients, NOT to enforce business rules.

  Storage semantics: last-writer-wins. A second `record_outcome` for the same `(workspace_basename, change)` key replaces the prior entry. This handles the corner case where the agent calls an outcome tool twice in the same session (e.g. retry after an error). The classifier consumes whichever entry was last written.

- **`consume_outcome`** — atomically reads AND removes the entry for a given key. Request shape:

  ```json
  {
    "action": "consume_outcome",
    "workspace_basename": "<sanitized basename>",
    "change": "<openspec change name>"
  }
  ```

  Response shape: `{ "ok": true, "outcome": <recorded outcome variant-tagged object OR null> }`. The `outcome` field is `null` when no entry exists for the key. A subsequent `consume_outcome` for the same key returns `null` (the read drained the store).

The daemon-side outcome store SHALL be in-memory (not file-backed). The store's lifecycle matches the daemon's lifecycle; a daemon restart loses any in-flight outcomes. This is acceptable: outcome reporting is synchronous (the MCP tool call happens milliseconds before the wrapped CLI exits AND the classifier's `consume_outcome` runs microseconds after); restart-survives durability is not required AND would create cleanup AND staleness concerns the file marker for `ask_user` deliberately accepts BECAUSE `ask_user` IS asynchronous.

The outcome store MAY periodically evict entries older than a coarse threshold (60 minutes is sufficient) to bound memory growth in the corner case where `consume_outcome` is never called for a recorded key (autocoder crashes between subprocess exit AND classifier drain). Implementation is OPTIONAL for this requirement; the implementer MAY defer it to a follow-on change if the immediate memory pressure is bounded.

Authorization: the same authn / authz that the existing control-socket actions use (per the canonical "Control socket for runtime daemon interaction" requirement) applies unchanged. The MCP child runs in the daemon's trust domain by virtue of being launched by the daemon's executor; no additional auth surface is introduced.

#### Scenario: `record_outcome` followed by `consume_outcome` round-trips a success outcome
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"my-repo","change":"a30-foo","outcome":{"type":"success","final_answer":"done"}}` to the control socket
- **AND** receives `{"ok":true}` in response
- **AND** subsequently sends `{"action":"consume_outcome","workspace_basename":"my-repo","change":"a30-foo"}`
- **THEN** the response is `{"ok":true,"outcome":{"type":"success","final_answer":"done"}}`
- **AND** a second `consume_outcome` for the same key returns `{"ok":true,"outcome":null}`

#### Scenario: `record_outcome` round-trips a spec_needs_revision outcome with all fields
- **WHEN** a client sends a `record_outcome` with `{"type":"spec_needs_revision","unimplementable_tasks":[{"task_id":"6.4","task_text":"Manual: SSH...","reason":"no SSH access"}],"revision_suggestion":"Replace 6.4 with a mocked unit test"}` as the outcome
- **AND** subsequently sends `consume_outcome` for the same key
- **THEN** the consumed outcome's `unimplementable_tasks` array AND `revision_suggestion` match the recorded values byte-for-byte
- **AND** the store entry is cleared

#### Scenario: `record_outcome` for an already-occupied key replaces the prior entry
- **WHEN** a client sends a `record_outcome` for key `("my-repo", "a30-foo")` with a `success` variant
- **AND** subsequently sends a second `record_outcome` for the same key with a `spec_needs_revision` variant
- **THEN** a following `consume_outcome` for the key returns the `spec_needs_revision` variant
- **AND** the prior `success` variant is NOT returned

#### Scenario: `consume_outcome` for an unknown key returns null
- **WHEN** a client sends `{"action":"consume_outcome","workspace_basename":"my-repo","change":"never-recorded"}` to a control socket whose outcome store has no matching entry
- **THEN** the response is `{"ok":true,"outcome":null}`

#### Scenario: `record_outcome` with an unknown variant tag returns a structured error
- **WHEN** a client sends `{"action":"record_outcome","workspace_basename":"x","change":"y","outcome":{"type":"unknown_variant","data":{}}}` to the control socket
- **THEN** the response is `{"ok":false,"error":"<message naming the unknown variant tag>"}`
- **AND** the outcome store remains unchanged

#### Scenario: Outcome-store keys are per `(workspace_basename, change)` AND do not collide across repos
- **WHEN** a client sends `record_outcome` for `("repo-a", "a30-foo")` AND another `record_outcome` for `("repo-b", "a30-foo")`
- **THEN** a `consume_outcome` for `("repo-a", "a30-foo")` returns the first entry
- **AND** a `consume_outcome` for `("repo-b", "a30-foo")` returns the second entry
- **AND** neither read drains the other key's entry
