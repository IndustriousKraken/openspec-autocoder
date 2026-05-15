## ADDED Requirements

### Requirement: Control socket for runtime daemon interaction
autocoder SHALL listen for control requests on a Unix domain socket at `<system-temp>/autocoder/control/control.sock` during the lifetime of the daemon process. The socket SHALL be created with permissions `0600` and owned by the user running the daemon, restricting access to that user. Control requests use a line-delimited JSON protocol; each connection accepts one request, responds with one JSON object, and closes.

#### Scenario: Socket is created and listening at startup
- **WHEN** the daemon starts
- **THEN** a Unix domain socket is created at
  `<system-temp>/autocoder/control/control.sock` with mode `0600`
- **AND** any pre-existing file at that path is removed before the
  new socket is created (stale socket from a previous run is not a
  startup failure)
- **AND** a tokio task accepts connections on the socket
  concurrently with the polling tasks

#### Scenario: Socket is removed at shutdown
- **WHEN** the daemon receives a shutdown signal AND the
  cancellation token fires
- **THEN** the socket file is removed before the process exits
- **AND** failure to remove the socket file is logged at WARN but
  does NOT block shutdown

#### Scenario: Request protocol
- **WHEN** a client connects to the control socket and sends a line
  of JSON terminated by `\n`
- **THEN** the daemon parses the line as a JSON object with at
  least an `action` field
- **AND** the daemon responds with a single line of JSON terminated
  by `\n` whose shape is `{"ok": true, ...}` on success or
  `{"ok": false, "error": "<message>"}` on failure
- **AND** the daemon closes the connection after sending the
  response

#### Scenario: Unknown action
- **WHEN** the request's `action` field is not one this daemon
  version recognizes
- **THEN** the response is `{"ok": false, "error": "unknown action: <action>"}`

#### Scenario: Malformed request
- **WHEN** the request is not valid JSON OR lacks an `action` field
- **THEN** the response is `{"ok": false, "error": "<parse error description>"}`
- **AND** the connection is closed

### Requirement: `autocoder reload` subcommand
autocoder SHALL provide a `reload` CLI subcommand that connects to the running daemon's control socket, sends `{"action":"reload"}`, prints the response, and exits 0 on success or non-zero on failure. The subcommand SHALL NOT require the daemon's `--config` path as an argument; the daemon already knows its config path and re-reads it from there.

#### Scenario: Successful reload
- **WHEN** the operator runs `autocoder reload`
- **THEN** the CLI connects to
  `<system-temp>/autocoder/control/control.sock`, sends the request,
  reads the response, prints it (pretty-printed JSON) to stdout,
  and exits 0 IF the response's `ok` field is `true`

#### Scenario: Reload rejected
- **WHEN** the daemon's reload handler returns `{"ok": false, ...}`
  (validation failure, IO error reading config, etc.)
- **THEN** the CLI prints the response to stderr and exits with
  a non-zero status

#### Scenario: Daemon not running
- **WHEN** `autocoder reload` is invoked and the control socket
  does not exist OR the connection is refused
- **THEN** the CLI prints an error message naming the expected
  socket path and exits non-zero
- **AND** the message hints at the likely cause: the daemon is
  not running, or is running under a different user

### Requirement: Reload handler hot-applies the safe config subset
The control socket's `reload` handler SHALL re-read the YAML config path the daemon was launched with, validate the new content fully (parse + semantic checks), and either reject or hot-apply changes to `github`, `reviewer`, and `chatops` sections. Changes to `repositories` and `executor` sections SHALL NOT be hot-applied in this version; the handler SHALL report those as `requires-restart` so the operator knows exactly which fields still need a full restart.

#### Scenario: Reload with no changes
- **WHEN** the YAML file is unchanged since startup AND the reload
  is triggered
- **THEN** the response is
  `{"ok": true, "applied": [], "requires_restart": [], "unchanged": ["github", "reviewer", "chatops", "repositories", "executor"]}`
- **AND** no in-memory state is modified

#### Scenario: Reload with hot-applicable changes
- **WHEN** the new YAML differs from the in-memory config ONLY in
  `github`, `reviewer`, or `chatops` sections
- **THEN** each changed section is swapped into its `ArcSwap` holder
- **AND** the response is
  `{"ok": true, "applied": ["<sections>"], "requires_restart": [], "unchanged": ["<other sections>"]}`
- **AND** the next iteration of each polling task reads the new
  values; in-flight iterations finish with the previous values

#### Scenario: Reload with restart-required changes
- **WHEN** the new YAML differs in `repositories` or `executor`
- **THEN** those sections are NOT hot-applied
- **AND** the response includes the changed-but-not-applied sections
  under `requires_restart`
- **AND** if the new YAML also changes hot-applicable sections,
  those ARE applied — the partial-apply result is reflected in the
  `applied` array

#### Scenario: Reload rejected by validation
- **WHEN** the new YAML fails to parse (`serde_yaml` error) OR
  fails semantic validation (workspace collision, missing token
  route, etc.)
- **THEN** the response is `{"ok": false, "error": "<message>"}`
  naming the validation failure
- **AND** no in-memory state is modified
- **AND** the daemon continues running with the previous config

#### Scenario: Reload rejected by IO failure
- **WHEN** the YAML file cannot be read (permission denied, file
  missing)
- **THEN** the response is `{"ok": false, "error": "config file <path>: <error>"}`
- **AND** no in-memory state is modified
