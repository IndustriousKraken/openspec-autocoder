## ADDED Requirements

### Requirement: STATE-LAYOUT.md documents the resolver-only rule and the CI check
`docs/STATE-LAYOUT.md` SHALL include a section titled "Path resolution rule" documenting that every daemon state-file read AND write routes through the `DaemonPaths` resolver, the rationale (preventing read/write drift bugs), AND the CI-enforced check that fails on hard-coded `/tmp/autocoder/` literals outside an allowlist.

#### Scenario: Section exists with full coverage
- **WHEN** a future contributor reads `docs/STATE-LAYOUT.md`
- **THEN** a section titled "Path resolution rule" appears alongside the existing migration AND defaults sections
- **AND** the section names the `DaemonPaths` resolver AND its helper methods
- **AND** the section explains the CI-test enforcement (the `path_literals_audit` test in `cargo test`)
- **AND** the section names what to do when adding a new state-file shape: add a helper to `DaemonPaths`, use it from the consumer, the CI check passes automatically
