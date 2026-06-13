use super::*;

// persist-on-demand-audit-queue: the durable storage primitive for the
// on-demand audit-run queue (`pending_audit_runs`). These exercise the
// save/load round-trip, the corrupt-file degradation, and the startup
// orphan reconciliation in isolation; the restart-simulation that drives a
// loaded queue through `run_due_audits` + prune lives in the scheduler's
// tests (it needs the workspace + registry fixtures there).

/// §5.1 Round-trip: `save_pending_audit_runs` then `load_pending_audit_runs`
/// returns the same entries — including the chat `origin` on an entry that
/// carries one.
#[test]
fn save_then_load_round_trips_entries_including_origin() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    let ws = paths.cache.join("workspaces").join("github_com_owner_repo");
    let queue = vec![
        QueuedAudit {
            audit_type: "drift_audit".to_string(),
            origin: None,
        },
        QueuedAudit {
            audit_type: "security_bug_audit".to_string(),
            origin: Some(ChatOrigin {
                channel: "C0123".to_string(),
                thread_ts: Some("1700000000.000100".to_string()),
            }),
        },
    ];
    save_pending_audit_runs(&paths, &ws, &queue).expect("save round-trip");

    let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
    let loaded = load_pending_audit_runs(&paths, &basename);
    assert_eq!(
        loaded, queue,
        "loaded queue must equal saved queue, origin included"
    );
}

/// §5.1 (boundary): an empty queue persists as `[]` and loads back empty,
/// so the durable copy can faithfully represent "nothing remains" after a
/// prune.
#[test]
fn save_empty_queue_loads_back_empty() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    let ws = paths.cache.join("workspaces").join("github_com_owner_repo");
    save_pending_audit_runs(&paths, &ws, &[]).expect("save empty");
    let basename = ws.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        load_pending_audit_runs(&paths, &basename).is_empty(),
        "empty queue must round-trip to empty"
    );
}

/// §5.3 Corrupt-file load returns an empty queue and does NOT panic.
#[test]
fn corrupt_file_loads_empty_without_panicking() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    let basename = "github_com_owner_repo";
    let path = paths.pending_audit_runs_path(basename);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ this is not valid json ]]").unwrap();

    let loaded = load_pending_audit_runs(&paths, basename);
    assert!(
        loaded.is_empty(),
        "a corrupt queue file must degrade to an empty queue"
    );
}

/// §5.3 (boundary): a missing file is the common case (no audit ever
/// queued) and loads empty without a WARN-worthy error.
#[test]
fn missing_file_loads_empty() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    assert!(
        load_pending_audit_runs(&paths, "never_written").is_empty(),
        "missing queue file must load as an empty queue"
    );
}

/// §5.4 Orphan reconciliation: a persisted entry for a repo that is no
/// longer in the configured set is dropped at load (startup sweep), while a
/// still-configured repo's file is preserved with its entries intact.
#[test]
fn orphan_reconciliation_drops_unconfigured_repo_file() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    let keep_ws = paths.cache.join("workspaces").join("github_com_owner_keep");
    let gone_ws = paths.cache.join("workspaces").join("github_com_owner_gone");
    save_pending_audit_runs(
        &paths,
        &keep_ws,
        &[QueuedAudit {
            audit_type: "drift_audit".to_string(),
            origin: None,
        }],
    )
    .unwrap();
    save_pending_audit_runs(
        &paths,
        &gone_ws,
        &[QueuedAudit {
            audit_type: "security_bug_audit".to_string(),
            origin: None,
        }],
    )
    .unwrap();

    let mut configured = std::collections::HashSet::new();
    configured.insert("github_com_owner_keep".to_string());
    reconcile_pending_audit_runs(&paths, &configured);

    assert!(
        paths.pending_audit_runs_path("github_com_owner_keep").exists(),
        "a still-configured repo's queue file must survive reconciliation"
    );
    assert!(
        !paths.pending_audit_runs_path("github_com_owner_gone").exists(),
        "an unconfigured repo's queue file must be dropped at load"
    );
    // The survivor still loads its entry — reconciliation touched only the
    // orphan.
    let kept = load_pending_audit_runs(&paths, "github_com_owner_keep");
    assert_eq!(kept.len(), 1, "configured repo's entries must be intact");
    assert_eq!(kept[0].audit_type, "drift_audit");
}

/// §5.4 (boundary): reconciliation over a never-created directory is a
/// no-op, not an error.
#[test]
fn reconciliation_with_no_directory_is_a_noop() {
    let (_t, paths) = crate::testing::test_daemon_paths();
    let mut configured = std::collections::HashSet::new();
    configured.insert("github_com_owner_keep".to_string());
    // No pending-audit-runs dir exists yet; must not panic or error.
    reconcile_pending_audit_runs(&paths, &configured);
}
