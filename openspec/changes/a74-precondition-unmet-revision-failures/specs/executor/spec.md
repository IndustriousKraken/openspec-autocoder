# executor — delta for a74-precondition-unmet-revision-failures

## ADDED Requirements

### Requirement: Agentic run surfaces a precondition-unmet failure distinct from a run failure
When an agentic run cannot start because a required precondition is unmet — the agent subprocess never spawns (e.g. no usable OS-level sandbox mechanism is available, per the sandbox-mechanism gate) — the executor SHALL surface a classifiable **precondition-unmet** failure, distinct from a substantive `Failed` outcome where the subprocess ran and then the task failed. The distinction SHALL be carried by the outcome/error **kind**, NOT by matching a substring of the message, so callers can branch on it reliably.

#### Scenario: The sandbox-mechanism gate yields a precondition-unmet failure
- **WHEN** an agentic run is attempted on a host with no usable sandbox mechanism AND the operator has not opted into unsandboxed operation
- **THEN** the executor surfaces a precondition-unmet failure (the subprocess never started)
- **AND** it is distinguishable by kind from a substantive run failure

#### Scenario: A substantive run failure is not precondition-unmet
- **WHEN** the agent subprocess starts and then fails (e.g. a non-zero exit after running)
- **THEN** the executor surfaces a substantive `Failed` outcome
- **AND** it is NOT classified as precondition-unmet
