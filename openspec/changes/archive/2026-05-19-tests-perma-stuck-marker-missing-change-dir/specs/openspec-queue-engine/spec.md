## ADDED Requirements

### Requirement: Perma-stuck marker write guards against missing change directory
The `perma_stuck::write_marker` helper SHALL refuse to write the marker file when the change's directory under `<workspace>/openspec/changes/` does not exist. The error SHALL name the missing directory so a caller looking at logs can tell the change was deleted out-of-band rather than blaming the marker-write step itself. The guard SHALL fire BEFORE any filesystem write so a failed call leaves no partial state behind.

#### Scenario: write_marker is called for a change directory that does not exist
- **WHEN** `perma_stuck::write_marker(workspace, "foo", &entry)` is called AND `<workspace>/openspec/changes/foo/` does not exist
- **THEN** the call returns `Err(_)` whose message contains the substring `change directory does not exist` AND the change name `foo`
- **AND** `<workspace>/openspec/changes/foo/.perma-stuck.json` does NOT exist on the filesystem after the failed call (the guard runs before any tempfile is created)
