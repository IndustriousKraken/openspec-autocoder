## ADDED Requirements

### Requirement: Architecture consultative audit
autocoder SHALL register an `architecture_consultative` audit in the periodic-audit framework. The audit invokes the wrapped agent CLI with a read-only sandbox and a consultative architecture prompt; it returns 0-5 anchored architecture questions as findings via chatops. The audit is `requires_head_change = true` and `WritePolicy::None`.

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
- **THEN** the agent's output contains a JSON object of shape:
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
- **AND** the `findings` array contains AT MOST 5 entries
- **AND** if the audit produces 0 findings (no observations rise
  above the prompt's quality bar), the result is
  `AuditOutcome::Reported(vec![])` and per framework behavior no
  chatops post is sent unless `notify_on_clean: true`

#### Scenario: Findings render as questions in chatops
- **WHEN** the audit produces N findings AND posts to chatops
- **THEN** each bullet in the message is the finding's `subject`,
  which by prompt construction is phrased as a question
- **AND** the `anchor` is included so the operator can navigate
  directly to the cited code
- **AND** the full body text is preserved in the audit-run log
  (chatops only shows the subject + anchor for compactness)

#### Scenario: Malformed agent output fails the audit
- **WHEN** the agent's stdout cannot be parsed as the expected
  JSON shape OR includes more than 5 findings
- **THEN** the audit returns `Err` with the parse error AND a
  truncated stdout excerpt
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
