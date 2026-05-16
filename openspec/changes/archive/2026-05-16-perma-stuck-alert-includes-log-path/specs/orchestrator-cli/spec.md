## ADDED Requirements

### Requirement: Perma-stuck chatops alert content
When autocoder writes a `.perma-stuck.json` marker for a change AND chatops is configured AND `failure_alerts_enabled` is true, autocoder SHALL post exactly one chatops notification (subject to the existing per-change 24h throttle) whose body names the repository URL, the change name, the consecutive failure count, the last reason excerpt, the marker file path, AND the per-change run log path.

#### Scenario: Alert body includes the run log path
- **WHEN** autocoder writes the perma-stuck marker for change
  `<change>` in workspace `<workspace>` AND the alert is not
  throttled
- **THEN** the posted chatops message body contains a line of
  the form `run_log: <log_path>` where `<log_path>` is the
  per-change run log written by the executor (for the Claude
  CLI executor, this is `/tmp/autocoder/logs/<workspace_basename>/<change>.log`)
- **AND** the line appears BEFORE the operator-action sentence
  describing how to retry (so the operator reads the diagnostic
  pointer before the action they would take to re-engage)

#### Scenario: Alert body retains pre-existing fields
- **WHEN** the alert is posted
- **THEN** the body still contains: `repo:`, `change:`,
  `consecutive_failures:`, `last_reason:`, AND a sentence
  naming the marker path that the operator must remove to
  retry
- **AND** the existing 24h-per-change throttle still applies
  (a second perma-stuck mark within the throttle window does
  not re-post)

#### Scenario: Log path is omitted when not derivable
- **WHEN** the executor backend does not expose a per-change
  run log path (e.g. a future executor with no run-log
  convention)
- **THEN** the `run_log:` line is omitted from the message body
  rather than rendering an empty path
- **AND** the rest of the body is unchanged
