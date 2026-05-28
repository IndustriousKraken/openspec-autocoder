//! CI-enforced rule: no hard-coded `/tmp/autocoder/...` literals in
//! `autocoder/src/**/*.rs` outside a narrow allowlist.
//!
//! Why: pre-`a09`, the codebase had multiple sites that constructed
//! daemon state-file paths by hand instead of routing through the
//! `DaemonPaths` resolver. When some sites used the resolver and
//! others hard-coded `/tmp/autocoder/...`, the read and write paths
//! drifted apart after the legacy-to-standard migration AND operators
//! saw the symptoms documented in the proposal (`send it` returning
//! `?` for real audit threads; `@<bot> status` reporting `idle` while
//! the busy marker existed).
//!
//! Allowlist: the migration scan legitimately references the legacy
//! `/tmp/autocoder/...` paths to identify state that needs to be
//! moved. Every other source file MUST construct paths through the
//! `DaemonPaths` helpers (`state`, `cache`, `logs`, `runtime`, or one
//! of the per-shape `audit_threads_dir()` / `busy_markers_dir()` /
//! etc.).
//!
//! Adding a new state-file shape: add a helper to `DaemonPaths`, use
//! it from the consumer side. The test then passes automatically.

use std::fs;
use std::path::{Path, PathBuf};

/// Files in `autocoder/src/` allowed to contain the literal substring
/// `/tmp/autocoder`. Paths are relative to the crate root
/// (`CARGO_MANIFEST_DIR`). Keep this list intentionally narrow.
const ALLOWLIST: &[&str] = &[
    // The migration scan IS the legacy-path consumer. Its constants
    // name `/tmp/autocoder/...` by definition — that's the data it's
    // moving out of.
    "src/migration.rs",
];

#[test]
fn no_hardcoded_tmp_autocoder_literals_outside_allowlist() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_root = crate_root.join("src");
    assert!(
        src_root.is_dir(),
        "src/ must exist under {}",
        crate_root.display()
    );

    let mut rs_files = Vec::new();
    collect_rs_files(&src_root, &mut rs_files);

    let mut violations: Vec<String> = Vec::new();
    for path in &rs_files {
        let rel = path
            .strip_prefix(&crate_root)
            .expect("walker returns paths under crate root")
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWLIST.contains(&rel.as_str()) {
            continue;
        }
        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => panic!("could not read {}: {e}", path.display()),
        };
        for (lineno, line) in contents.lines().enumerate() {
            if line.contains("/tmp/autocoder") {
                violations.push(format!("{}:{}: {}", rel, lineno + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Hard-coded `/tmp/autocoder` literals found outside the allowlist:\n\n{}\n\n\
         Fix: use the `DaemonPaths` resolver. Add a per-shape helper to \
         `autocoder/src/paths.rs` if no matching helper exists yet, then \
         call it from the consumer.",
        violations.join("\n"),
    );
}

/// Recursively collect every `*.rs` file under `dir` into `out`. Symlinks
/// and non-existent paths are skipped. Errors reading a directory abort
/// the test loudly because they indicate a broken sandbox.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("read_dir {}: {e}", dir.display()),
    };
    for entry in entries {
        let entry = entry.expect("read_dir entry must succeed");
        let path = entry.path();
        let file_type = entry.file_type().expect("file_type must succeed");
        if file_type.is_dir() {
            collect_rs_files(&path, out);
        } else if file_type.is_file()
            && path.extension().and_then(|s| s.to_str()) == Some("rs")
        {
            out.push(path);
        }
    }
}
