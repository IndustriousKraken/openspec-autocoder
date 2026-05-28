//! CI-enforced rule: no hard-coded production path literals in
//! `autocoder/src/**/*.rs` or `autocoder/tests/**/*.rs` outside a
//! narrow allowlist.
//!
//! The audited substring is the legacy daemon path prefix
//! (`/tmp/autocoder`). Pre-`a09`, the codebase had multiple sites that
//! constructed daemon state-file paths by hand instead of routing
//! through the `DaemonPaths` resolver. When some sites used the
//! resolver and others hard-coded the legacy prefix, the read and
//! write paths drifted apart after the legacy-to-standard migration
//! AND operators saw the symptoms documented in the `a09` proposal
//! (`send it` returning `?` for real audit threads; `@<bot> status`
//! reporting `idle` while the busy marker existed).
//!
//! Extended in `a10` to also scan test code. On hosts where autocoder
//! works on itself, the wrapped agent's `cargo test` would otherwise
//! drop test-fixture state files into the production prefix alongside
//! the live daemon's real state. The discipline: tests use
//! `crate::testing::test_daemon_paths()` (which hands out a tempdir-
//! scoped `DaemonPaths`) or `temp_env::with_var(...)` for env-driven
//! cases — never the production path literal.
//!
//! Allowlists: the `src/` allowlist contains only the migration scan,
//! which legitimately references legacy paths to identify state that
//! needs to be moved. The `tests/` allowlist is intentionally empty —
//! the scanner file's own self-reference (it has to mention the
//! substring at least in error-message text) is excluded by a
//! filename-based self-check, not by an allowlist entry, so the rule
//! "no test allowlist entries" stays true on inspection.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Files under `autocoder/src/` allowed to contain the audited
/// substring. Paths are relative to the crate root
/// (`CARGO_MANIFEST_DIR`). Keep this list intentionally narrow.
const SRC_ALLOWLIST: &[&str] = &[
    // The migration scan IS the legacy-path consumer. Its constants
    // name the legacy paths by definition — that's the data it's
    // moving out of.
    "src/migration.rs",
];

/// Files under `autocoder/tests/` allowed to contain the audited
/// substring. **Empty by spec** (`a10`): no test should reference the
/// production path literal. The scanner's own source file is exempted
/// by filename self-check below, not by an allowlist entry.
const TESTS_ALLOWLIST: &[&str] = &[];

/// File name of THIS test file. The scanner self-skips this entry when
/// walking `tests/` so it does not flag its own runtime-needle string
/// or its docs as violations. Matched by basename (Cargo invokes the
/// file under a stable name).
const SCANNER_FILENAME: &str = "path_literals_audit.rs";

#[test]
fn no_hardcoded_tmp_autocoder_literals_outside_allowlist() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Build the audited needle at runtime from two fragments so that
    // this very line does not contain the full substring. Combined
    // with the filename self-skip, that means the scanner file does
    // not match itself on this line either.
    let needle: String = ["/tmp/", "autocoder"].concat();

    let src_root = crate_root.join("src");
    let tests_root = crate_root.join("tests");
    assert!(
        src_root.is_dir(),
        "src/ must exist under {}",
        crate_root.display()
    );
    assert!(
        tests_root.is_dir(),
        "tests/ must exist under {}",
        crate_root.display()
    );

    let mut rs_files = Vec::new();
    collect_rs_files(&src_root, &mut rs_files);
    collect_rs_files(&tests_root, &mut rs_files);

    let mut violations: Vec<String> = Vec::new();
    for path in &rs_files {
        // Self-skip: the scanner's own source file is not a consumer of
        // the literal path; it's the file that names the substring for
        // grep purposes. Matching by basename keeps this robust against
        // Cargo's various `file!()` representations.
        if path.file_name() == Some(OsStr::new(SCANNER_FILENAME)) {
            continue;
        }
        let rel = path
            .strip_prefix(&crate_root)
            .expect("walker returns paths under crate root")
            .to_string_lossy()
            .replace('\\', "/");
        let allow = if rel.starts_with("src/") {
            SRC_ALLOWLIST.contains(&rel.as_str())
        } else if rel.starts_with("tests/") {
            TESTS_ALLOWLIST.contains(&rel.as_str())
        } else {
            false
        };
        if allow {
            continue;
        }
        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => panic!("could not read {}: {e}", path.display()),
        };
        for (lineno, line) in contents.lines().enumerate() {
            if line.contains(needle.as_str()) {
                violations.push(format!("{}:{}: {}", rel, lineno + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Hard-coded production path literals found outside the allowlist:\n\n{}\n\n\
         Fix in production code (src/): use the `DaemonPaths` resolver. \
         Add a per-shape helper to `autocoder/src/paths.rs` if no \
         matching helper exists yet, then call it from the consumer.\n\n\
         Fix in test code (tests/, src/ test modules): use \
         `crate::testing::test_daemon_paths()` to obtain a tempdir-scoped \
         `DaemonPaths`, or `temp_env::with_var(...)` if the test must \
         exercise an `AUTOCODER_*_DIR` env-var path. Never hard-code the \
         production path literal in tests.",
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
