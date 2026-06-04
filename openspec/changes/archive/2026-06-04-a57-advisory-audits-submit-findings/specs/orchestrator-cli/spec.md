# orchestrator-cli — delta for a57-advisory-audits-submit-findings

## MODIFIED Requirements

### Requirement: Drift audit
autocoder SHALL register a `drift_audit` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a drift-detection prompt, then surfaces findings via chatops. The agent SHALL return its findings by calling the `submit_findings` MCP tool — validated against the drift finding schema and consumed by the daemon as the audit result — rather than by emitting JSON on stdout. The audit is `requires_head_change = true` and `WritePolicy::None`.

#### Scenario: Invokes the CLI with a read-only sandbox
- **WHEN** the audit runs
- **THEN** autocoder spawns the configured `executor.command`
  (typically `claude`) with `--settings` pointing at a generated
  sandbox file whose `permissions.deny` excludes `Write` and
  `Edit` and whose CLI tool permissions contain only
  `["Read", "Glob", "Grep", "Bash"]`
- **AND** a generated `.mcp.json` exposes the `submit_findings`
  MCP tool with `ORCH_MCP_ROLE` set to `drift_audit`, so the agent
  can return findings but still cannot `Write` or `Edit`
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

#### Scenario: Returns findings via the submit_findings tool
- **WHEN** the agent has finished its analysis
- **THEN** it calls the `submit_findings` MCP tool with a payload
  of shape:
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
- **AND** the daemon validates the payload against the drift
  finding schema (via a56's `record_submission`), surfacing a
  schema violation to the agent as a correctable tool error
- **AND** after the audit subprocess exits the daemon
  `consume_submission`s the stored payload to produce `Finding`
  values for the `AuditOutcome::Reported(...)` return

#### Scenario: Filters out low-severity wording-only differences
- **WHEN** the prompt instructs the agent on severity classification
- **THEN** the prompt explicitly states: "Do NOT report findings
  whose only divergence is wording, formatting, or phrasing.
  Only report divergences with behavioral consequences."
- **AND** the agent SHOULD self-filter such findings before
  submitting

#### Scenario: Empty findings list produces silent outcome
- **WHEN** the agent calls `submit_findings` with an empty
  `findings` array
- **THEN** the audit returns `AuditOutcome::Reported(vec![])`
- **AND** per the framework-level "Reported with no findings"
  scenario, no chatops post is made unless
  `notify_on_clean: true`

#### Scenario: No valid submission fails the audit
- **WHEN** the agent never calls `submit_findings`, OR every
  `submit_findings` call is rejected by the schema (malformed
  shape, missing top-level `findings`, non-array value) and the
  session ends with no stored submission
- **THEN** the audit returns `Err` with a diagnostic AND a
  truncated stdout/stderr excerpt
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

### Requirement: Architecture consultative audit
autocoder SHALL register an `architecture_consultative` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a consultative architecture prompt; it returns 0-5 anchored architecture questions as findings via chatops. The agent SHALL return those findings by calling the `submit_findings` MCP tool — validated against the architecture finding schema, which caps the array at 5 entries, and consumed by the daemon as the audit result — rather than by emitting JSON on stdout. The audit is `requires_head_change = true` and `WritePolicy::None`.

#### Scenario: Prompt forbids "rewrite at scale" suggestions
- **WHEN** the prompt is loaded
- **THEN** the prompt explicitly forbids the agent from suggesting:
  - splitting the codebase into microservices, separate processes,
    or separate binaries
  - rewrites in a different programming language
  - new infrastructure dependencies (message queues, databases,
    caches, RPC frameworks) unless the project already uses one
    of equivalent shape
  - patterns implying team-of-50 scale (event sourcing for a
    single-operator daemon, CQRS where a simple function would
    do, etc.)
- **AND** the prompt explicitly directs the agent to:
  - frame observations as questions, not directives
  - anchor each observation to a specific `file:line` range
  - drop suggestions whose implementation adds more code than
    it removes

#### Scenario: Prompt is language-agnostic
- **WHEN** the prompt is loaded
- **THEN** the prompt makes NO assumptions about programming
  language, framework, or runtime
- **AND** the prompt operates from observable structure (file
  organization, function boundaries, module interfaces) without
  language-specific idioms
- **AND** the prompt explicitly allows polyglot codebases
  (front-end + back-end, multi-language tools, language
  bridges) as a normal configuration to be observed, not
  flagged

#### Scenario: Returns 0-5 findings per run
- **WHEN** the audit runs
- **THEN** the agent calls the `submit_findings` MCP tool with a
  payload of shape:
  ```json
  {
    "findings": [
      {
        "subject": "Should X be its own module?",
        "body": "<one paragraph of context>",
        "anchor": "path/to/file.ext:120-180",
        "severity": "low" | "medium"
      }
    ]
  }
  ```
- **AND** the `findings` array contains AT MOST 5 entries — the
  registered schema rejects a submission with more than 5,
  surfacing it to the agent as a correctable tool error
- **AND** if the audit produces 0 findings (no observations rise
  above the prompt's quality bar), the agent calls
  `submit_findings` with an empty array, the result is
  `AuditOutcome::Reported(vec![])`, and per framework behavior no
  chatops post is sent unless `notify_on_clean: true`

#### Scenario: Findings render as questions in chatops
- **WHEN** the audit produces N findings AND posts to chatops
- **THEN** each bullet in the message is the finding's `subject`,
  which by prompt construction is phrased as a question
- **AND** the `anchor` is included so the operator can navigate
  directly to the cited code
- **AND** the full body text is preserved in the audit-run log
  (chatops only shows the subject + anchor for compactness)

#### Scenario: No valid submission fails the audit
- **WHEN** the agent never calls `submit_findings`, OR every
  `submit_findings` call is rejected by the schema (malformed
  shape, or more than 5 findings) and the session ends with no
  stored submission
- **THEN** the audit returns `Err` with a diagnostic AND a
  truncated stdout/stderr excerpt
- **AND** the framework treats this as audit failure: state is
  NOT updated, chatops alert posts under the existing
  audit-failure category, the next iteration retries

#### Scenario: Audit-run log captures the full agent output
- **WHEN** the audit runs (success or failure)
- **THEN** the audit-run log contains the prompt sent to the CLI,
  the full raw stdout, the full raw stderr, and the final
  outcome variant
- **AND** operators reviewing a confusing chatops finding can
  consult this log to see exactly what the agent produced

### Requirement: Documentation audit reports coverage, stale-reference, and organization findings
autocoder SHALL register a `documentation_audit` audit type in the periodic-audit framework. The audit is LLM-driven, declares `WritePolicy::None`, `requires_head_change = true`, AND a sandbox profile allowing `Read`, `Glob`, `Grep`, AND `Bash` (read-only) plus the `submit_findings` MCP tool through which the agent returns its findings — validated against the documentation finding schema (`category`, `severity`, `anchor`, `body`) and consumed by the daemon as the audit result, rather than emitted as JSON on stdout. It produces `AuditOutcome::Reported(findings)` covering three categories of documentation defect:

1. **Coverage** — code or canonical-spec features that user-facing docs (`README.md`, `docs/*.md`) don't mention. Heuristic: any canonical-spec requirement whose body mentions operator-visible artifacts (`@<bot>` verbs, config keys, CLI flags, file paths the operator interacts with) is in scope. Pure-internal capabilities are NOT flagged.
2. **Stale references** — docs references to code symbols (function names in code blocks, CLI verbs, config fields, file paths under `src/`) that don't exist in the current code or canonical specs. Catches dead references from removed features.
3. **Organization** — qualitative structural findings: README exceeding `extra.readme_max_lines` lines (default `200`), docs pages exceeding `extra.page_max_lines_without_toc` (default `500`) without a TOC, important user-visible features buried below setup/admin material on their page, two docs pages covering the same topic without cross-linking, capabilities surfaced only in CHANGELOG but never in operator docs.

The audit's findings SHALL be tagged with `severity` of `low` OR `medium` ONLY — the audit deliberately does NOT emit `high` (documentation drift is rarely emergency-grade; promotion would crowd out genuinely-urgent audit signals from other types). An `anchor` field names `<file>:<line>` for stale-reference findings AND `<file>` (no line) for coverage AND organization findings.

The audit's prompt template `prompts/documentation-audit.md` ships embedded via `include_str!` AND is overridable via `audits.settings.documentation_audit.prompt_path`. Two `extra` knobs apply: `readme_max_lines` (default `200`) AND `page_max_lines_without_toc` (default `500`). The prompt receives these knobs as part of its input AND respects them when emitting organization findings.

The audit does NOT produce LLM-generated documentation proposals (unlike `missing_tests_audit` / `security_bug_audit`). Findings ship as `Reported` outcomes; operators run `@<bot> send it` in the audit's threaded notification to trigger a triage executor run that produces a docs-fix PR (NOT a spec PR). The PR participates in the standard `@<bot> revise <text>` revision loop.

When `a21`'s canonical-spec RAG is enabled in the same workspace, the audit's prompt MAY use the `query_canonical_specs` MCP tool to fetch focused canonical context. The audit functions correctly without RAG too; the RAG integration is an opportunistic enhancement, not a requirement.

#### Scenario: Audit detects implementation-without-documentation
- **WHEN** the canonical spec contains a requirement whose body mentions an operator-visible feature (e.g. `@<bot> propose` verb)
- **AND** none of `README.md` or `docs/*.md` mentions `propose`
- **THEN** the audit emits a finding with `category: coverage`, `severity: medium`, `anchor: <docs-or-spec-file-where-the-feature-is-defined>`, AND a body explaining the missing documentation

#### Scenario: Audit detects documentation-without-implementation
- **WHEN** `docs/CONFIG.md` references a config field `executor.foo_bar_quux` in a code block
- **AND** no Rust source file under `<workspace>/<source-tree>/` defines a field named `foo_bar_quux` in any struct
- **THEN** the audit emits a finding with `category: stale_reference`, `severity: medium`, `anchor: docs/CONFIG.md:<line>`, AND a body naming the missing referent

#### Scenario: Audit detects organization issues
- **WHEN** `docs/CHATOPS.md` is 600 lines long AND has no top-of-file TOC
- **AND** the page documents user-driving workflows (`propose`, `send it`) AND administrative recovery verbs (`clear-perma-stuck`)
- **AND** the user-driving content appears below the admin material
- **THEN** the audit MAY emit findings with `category: organization`, `severity: low` or `medium`, naming each separately (missing TOC; burial of user-driving content)

#### Scenario: Audit deliberately does not emit `high` severity
- **WHEN** the LLM's response contains a finding marked `"severity": "high"`
- **THEN** the audit demotes it to `"medium"` AND logs a WARN naming the demotion
- **AND** the operator-visible finding lists severity `medium`

#### Scenario: Audit honors `requires_head_change = true`
- **WHEN** the audit's `last_run_sha` equals the current base-branch HEAD AND the cadence has elapsed
- **THEN** the framework skips the audit (per the existing framework requirement)
- **AND** the next iteration after a HEAD change re-evaluates

#### Scenario: Pure-internal capability is NOT flagged for coverage
- **WHEN** a capability's canonical spec exists BUT every requirement body covers pure-internal mechanics (no operator-visible artifacts)
- **THEN** the audit does NOT emit a coverage finding for that capability
- **AND** the heuristic recognizes "internal" via the absence of `@<bot>` verbs, config keys, CLI flags, AND operator-facing file paths in the requirement bodies

#### Scenario: `extra` knobs apply to organization thresholds
- **WHEN** `audits.settings.documentation_audit.extra.readme_max_lines: 400`
- **AND** `README.md` is 300 lines
- **THEN** the audit does NOT emit a "README too long" finding (the threshold is operator-raised)
- **WHEN** the same config AND `README.md` grows to 500 lines
- **THEN** the audit emits the organization finding

#### Scenario: Audit works without `a21`'s RAG
- **WHEN** `canonical_rag` is disabled (no block OR `enabled: false`)
- **AND** `documentation_audit` runs
- **THEN** the audit completes successfully without invoking `query_canonical_specs`
- **AND** findings are emitted based on the prompt's direct access to canonical specs (read via the sandbox's `Read` tool)

#### Scenario: Audit uses RAG when available
- **WHEN** `canonical_rag` is enabled AND a documentation_audit run starts
- **THEN** the audit's executor invocation has access to `query_canonical_specs` via MCP
- **AND** the prompt MAY direct the LLM to use the tool for canonical-context retrieval
- **AND** the implementation detail (whether the LLM uses the tool) is left to the prompt's design — both with-RAG AND without-RAG produce valid output

#### Scenario: Findings can be acted on via `send it`
- **WHEN** the audit posts a threaded notification with findings AND the operator replies `@<bot> send it` in that thread
- **THEN** the existing `audit-reply-acts` mechanism triggers a triage executor run
- **AND** the triage produces a doc-fix PR (changes to `README.md` / `docs/*.md` files)
- **AND** the triage does NOT produce a spec PR (documentation is not OpenSpec material)
- **AND** the doc-fix PR participates in the standard `@<bot> revise <text>` revision loop

#### Scenario: Returns findings via the submit_findings tool
- **WHEN** the agent has finished its analysis
- **THEN** it calls the `submit_findings` MCP tool with the documentation findings (`category`, `severity` of `low` | `medium`, `anchor`, `body`)
- **AND** the daemon validates the payload (via a56's `record_submission`) and, after the subprocess exits, `consume_submission`s it to produce the `Reported` findings — a `high` severity in the submission is demoted to `medium` per the existing demotion scenario
- **AND** a schema-invalid submission is surfaced to the agent as a correctable tool error

#### Scenario: No valid submission fails the audit
- **WHEN** the agent never calls `submit_findings` AND the session ends with no stored submission
- **THEN** the audit returns `Err`
- **AND** the framework treats this as audit failure: state is NOT updated, the chatops audit-failure alert posts, the next iteration retries
