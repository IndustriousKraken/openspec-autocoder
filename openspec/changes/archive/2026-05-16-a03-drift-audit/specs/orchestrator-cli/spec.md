## ADDED Requirements

### Requirement: Drift audit
autocoder SHALL register a `drift_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a drift-detection prompt, then surfaces findings via chatops. The audit is `requires_head_change = true` and `WritePolicy::None`.

#### Scenario: Invokes the CLI with a read-only sandbox
- **WHEN** the audit runs
- **THEN** autocoder spawns the configured `executor.command`
  (typically `claude`) with `--settings` pointing at a generated
  sandbox file whose `permissions.deny` excludes `Write` and
  `Edit` and whose `allowed_tools` contains only
  `["Read", "Glob", "Grep", "Bash"]`
- **AND** the prompt is the embedded `prompts/drift-audit.md`
  template OR the operator-supplied override at
  `audits.drift_audit.prompt_path`
- **AND** the agent's working directory is the repository's
  workspace root

#### Scenario: Reads canonical specs from openspec/specs
- **WHEN** the drift-audit prompt instructs the agent to examine
  canonical specs
- **THEN** the prompt directs the agent to glob
  `openspec/specs/*/spec.md` AND read each capability's
  requirements
- **AND** the prompt directs the agent to ignore
  `openspec/changes/` (in-flight changes) and
  `openspec/changes/archive/` (historical changes)

#### Scenario: Outputs findings in a parseable format
- **WHEN** the agent completes
- **THEN** the agent's stdout SHALL be a single JSON object of
  shape:
  ```json
  {
    "findings": [
      {
        "capability": "orchestrator-cli",
        "requirement": "Per-repository asynchronous polling loop",
        "severity": "high",
        "code_anchors": ["autocoder/src/polling_loop.rs:45-95"],
        "divergence": "Spec requires <X>; code does <Y>."
      }
    ]
  }
  ```
- **AND** autocoder parses this JSON to produce `Finding`
  values for the `AuditOutcome::Reported(...)` return

#### Scenario: Filters out low-severity wording-only differences
- **WHEN** the prompt instructs the agent on severity classification
- **THEN** the prompt explicitly states: "Do NOT report findings
  whose only divergence is wording, formatting, or phrasing.
  Only report divergences with behavioral consequences."
- **AND** the agent SHOULD self-filter such findings before
  emitting the JSON

#### Scenario: Empty findings list produces silent outcome
- **WHEN** the agent returns an empty `findings` array
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** per the framework-level "Reported with no findings"
  scenario, no chatops post is made unless
  `notify_on_clean: true`

#### Scenario: Malformed agent output fails the audit
- **WHEN** the agent's stdout is not parseable as the expected
  JSON shape (missing top-level `findings`, non-array value,
  malformed JSON, etc.)
- **THEN** the audit returns `Err` with the parse error AND a
  truncated stdout excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert posts under the existing
  audit-failure category, the next iteration retries

#### Scenario: Write attempt is blocked and treated as failure
- **WHEN** the agent attempts to call `Write` or `Edit` despite
  the sandbox
- **THEN** the CLI's permission system denies the call (the agent
  observes a tool error) AND on audit return the post-hoc
  `git status --porcelain` is empty
- **AND** if for any reason the post-hoc diff IS non-empty (e.g.
  the agent shelled out through Bash to a writeable command),
  the foundation's `WritePolicy::None` enforcement reverts via
  `git reset --hard HEAD` AND fails the audit

#### Scenario: Audit-run log captures the full agent output
- **WHEN** the audit runs (success or failure)
- **THEN** the audit-run log at
  `/tmp/autocoder/logs/<basename>/audits/drift_audit-<timestamp>.log`
  contains the prompt sent to the CLI AND the full raw stdout
  AND the full raw stderr AND the final outcome variant
- **AND** operators reviewing a confusing chatops finding can
  consult this log to see exactly what the agent produced
