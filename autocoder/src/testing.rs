//! Test-only helpers that keep filesystem-touching tests isolated from
//! the live daemon's state paths.
//!
//! The motivating bug (see `a10-tests-use-tempdirs-not-production-paths`):
//! when the wrapped agent runs `cargo test` on a production host, any
//! test that writes under the legacy production prefix lands alongside
//! the daemon's real state. Even if the synthetic fixture filenames
//! never collide with real Slack `thread_ts` shapes today, the
//! colocation is a latent failure mode (a future test using a
//! realistic filename could trample real state).
//!
//! Discipline: every test that needs a daemon `state` / `cache` /
//! `logs` / `runtime` root acquires it from `test_daemon_paths()`. The
//! returned `TempDir` MUST be kept alive in the test's local binding
//! (typically `let (_temp, paths) = test_daemon_paths();`) so the
//! tempdir is dropped — and auto-cleaned — at the end of the test.
//!
//! The CI-enforced `path_literals_audit` integration test scans both
//! `src/` and `tests/` for hard-coded production-prefix references
//! and fails the build if any appear in test code; this helper is the
//! single supported onramp for tests that previously would have
//! reached for that literal.

use crate::paths::DaemonPaths;
use tempfile::TempDir;

/// Construct a fresh `DaemonPaths` whose four roots live under a new
/// per-call `TempDir`. The four directories are created on disk so
/// callers can immediately read/write through `paths.state_dir()` etc.
/// without first running `mkdir -p`.
///
/// Return contract: the `TempDir` is returned to the caller (never
/// dropped inside this function) so the test's binding controls its
/// lifetime. When the binding is dropped at the end of the test, the
/// tempdir — and every file the test wrote — is removed.
#[allow(dead_code)]
pub fn test_daemon_paths() -> (TempDir, DaemonPaths) {
    let tempdir = TempDir::new().expect("create tempdir for test_daemon_paths");
    let paths = DaemonPaths::under_root(tempdir.path());
    for dir in [&paths.state, &paths.cache, &paths.logs, &paths.runtime] {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| {
            panic!(
                "test_daemon_paths: failed to create `{}`: {e}",
                dir.display()
            )
        });
    }
    (tempdir, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_creates_four_directories_under_tempdir() {
        let (temp, paths) = test_daemon_paths();
        let root = temp.path();
        for (label, dir) in [
            ("state", &paths.state),
            ("cache", &paths.cache),
            ("logs", &paths.logs),
            ("runtime", &paths.runtime),
        ] {
            assert!(
                dir.is_dir(),
                "{label} dir `{}` was not created",
                dir.display()
            );
            assert!(
                dir.starts_with(root),
                "{label} dir `{}` is not under the tempdir root `{}`",
                dir.display(),
                root.display()
            );
        }
    }

    #[test]
    fn each_call_returns_a_distinct_tempdir() {
        let (t1, p1) = test_daemon_paths();
        let (t2, p2) = test_daemon_paths();
        assert_ne!(
            t1.path(),
            t2.path(),
            "two calls must hand out independent tempdirs"
        );
        assert_ne!(p1.state, p2.state);
    }

    #[test]
    fn dropping_the_tempdir_removes_every_subdirectory() {
        let saved_root;
        {
            let (temp, paths) = test_daemon_paths();
            saved_root = temp.path().to_path_buf();
            // Drop a sentinel file so we can confirm cleanup wipes
            // user-written state alongside the four roots.
            std::fs::write(paths.state.join("sentinel"), b"x").unwrap();
            assert!(saved_root.is_dir());
        }
        assert!(
            !saved_root.exists(),
            "tempdir `{}` should be removed once the TempDir binding drops",
            saved_root.display()
        );
    }
}
