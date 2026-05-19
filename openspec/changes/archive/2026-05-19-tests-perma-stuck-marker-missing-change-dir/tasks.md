## 1. Write-marker guard against missing change directory

- [x] 1.1 `write_marker_errors_when_change_directory_absent` (in
  `autocoder/src/perma_stuck.rs` tests module) — create a fresh
  `TempDir`, do NOT create `openspec/changes/foo/`, call
  `write_marker(ws, "foo", &fixture_entry())`, assert the result is
  `Err`, assert `format!("{err:#}")` contains the substring
  `change directory does not exist` AND `foo`.
- [x] 1.2 Same test — additionally assert the marker file at
  `openspec/changes/foo/.perma-stuck.json` does NOT exist after the
  failed call (guard fires before any filesystem write, so no
  partial state is left behind).
