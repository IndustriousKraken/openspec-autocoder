//! Classification of mid-iteration recovery failures into transient (retry
//! on next polling tick) vs. permanent (operator inspection required).
//!
//! See `openspec/changes/a14-no-permanent-skip-on-transient-unreachable/`
//! for the design. The classifier walks the `anyhow::Error` source chain
//! and matches strings / `std::io::ErrorKind` against the documented
//! patterns. Unrecognized errors default to `Transient` — operators have
//! the chatops `🛑 perma-stuck` plus manual-skip escape hatches if a
//! genuinely-permanent failure mis-classifies.
//!
//! Startup-time recovery is unchanged (still skip-for-lifetime regardless
//! of classification); a future spec MAY extend classification there too.

use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryFailureClass {
    Transient,
    Permanent,
}

impl RecoveryFailureClass {
    /// Suffix appended to the `WorkspaceInitFailure` /
    /// `WorkspaceDirtyMidIteration` alert text so the operator can tell at
    /// a glance whether to wait (transient) or SSH and investigate
    /// (permanent). Whitespace is included in the returned string so the
    /// caller concatenates without an extra space.
    pub fn alert_suffix(self) -> &'static str {
        match self {
            Self::Transient => " (transient; retrying)",
            Self::Permanent => {
                " (permanent; skipped until daemon restart) — operator inspection required"
            }
        }
    }

    /// Short tag for structured log fields (`class=transient` /
    /// `class=permanent`).
    pub fn log_tag(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }
}

const TRANSIENT_SUBSTRINGS: &[&str] = &[
    "Could not resolve host",
    "Connection timed out",
    "Connection refused",
    "Connection reset",
    "TLS handshake",
    "the remote end hung up",
    "Network is unreachable",
    "Temporary failure in name resolution",
    "Operation timed out",
];

const PERMANENT_SUBSTRINGS: &[&str] = &[
    "invalid configuration",
    "malformed YAML",
    "no matching token route",
    "still dirty after recovery",
    "remains dirty after recovery",
];

/// Classify a recovery failure. See module docs for the rule set.
pub fn classify_recovery_failure(err: &anyhow::Error) -> RecoveryFailureClass {
    // Walk the source chain once, collecting each link's display string
    // and looking at downcast targets. The error's outer `Display` from
    // `format!("{err:#}")` collapses the chain via `:#`, so a single
    // composite string covers most pattern matches; structured types
    // (`io::Error`, status-code-bearing errors) we inspect separately.
    let composite = format!("{err:#}");

    if let Some(class) = classify_io_chain(err) {
        return class;
    }

    if let Some(class) = classify_http_status_chain(&composite) {
        return class;
    }

    if let Some(class) = classify_git_exit_chain(err, &composite) {
        return class;
    }

    if has_any_substring(&composite, PERMANENT_SUBSTRINGS) {
        return RecoveryFailureClass::Permanent;
    }
    if is_missing_required_binary(&composite) {
        return RecoveryFailureClass::Permanent;
    }
    if has_any_substring(&composite, TRANSIENT_SUBSTRINGS) {
        return RecoveryFailureClass::Transient;
    }

    // Default: transient (conservative — retry rather than skip).
    RecoveryFailureClass::Transient
}

fn has_any_substring(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// `git` reports network/transport failures with exit code 128 and
/// stderr containing one of the strings in `TRANSIENT_SUBSTRINGS`. The
/// error message produced by `crate::git` typically includes the exit
/// status and the stderr text, so a substring match on the composite
/// string is sufficient. We pattern-match the literal "exit status: 128"
/// fragment commonly produced by `Command::status()` formatting plus the
/// `git -c ...` / `git: '...'` prefixes used by our wrappers.
fn classify_git_exit_chain(_err: &anyhow::Error, composite: &str) -> Option<RecoveryFailureClass> {
    let looks_like_git_failure = composite.contains("git ")
        || composite.contains("git:")
        || composite.contains("`git`")
        || composite.contains("fetch")
        || composite.contains("clone")
        || composite.contains("reset");
    if !looks_like_git_failure {
        return None;
    }
    if has_any_substring(composite, TRANSIENT_SUBSTRINGS) {
        return Some(RecoveryFailureClass::Transient);
    }
    None
}

fn classify_http_status_chain(composite: &str) -> Option<RecoveryFailureClass> {
    const TRANSIENT_STATUSES: &[&str] = &[
        "502", "503", "504", "522", "524", // gateway / upstream
        "401", "403", // auth blip (recoverable on token rotation)
        "429", // rate limit
    ];
    let mentions_http =
        composite.contains("HTTP") || composite.contains("status") || composite.contains("GitHub");
    if !mentions_http {
        return None;
    }
    for code in TRANSIENT_STATUSES {
        if composite.contains(code) {
            return Some(RecoveryFailureClass::Transient);
        }
    }
    None
}

fn classify_io_chain(err: &anyhow::Error) -> Option<RecoveryFailureClass> {
    for link in err.chain() {
        if let Some(io_err) = link.downcast_ref::<io::Error>() {
            match io_err.kind() {
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe => {
                    return Some(RecoveryFailureClass::Transient);
                }
                _ => {}
            }
        }
    }
    None
}

fn is_missing_required_binary(composite: &str) -> bool {
    // "No such file or directory" + one of the required binary names. The
    // call sites surface these via `spawn` errors, where the OS message is
    // included in the chain.
    if !composite.contains("No such file or directory")
        && !composite.contains("not found on PATH")
        && !composite.contains("not found")
    {
        return false;
    }
    composite.contains("openspec")
        || composite.contains("claude")
        || (composite.contains("git") && composite.contains("binary"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn transient(err: &anyhow::Error) {
        assert_eq!(
            classify_recovery_failure(err),
            RecoveryFailureClass::Transient,
            "expected Transient for: {err:#}"
        );
    }

    fn permanent(err: &anyhow::Error) {
        assert_eq!(
            classify_recovery_failure(err),
            RecoveryFailureClass::Permanent,
            "expected Permanent for: {err:#}"
        );
    }

    // ---- Transient: string patterns from §1.2 ----

    #[test]
    fn dns_resolution_failure_is_transient() {
        let err = anyhow!("fatal: Could not resolve host: github.com");
        transient(&err);
    }

    #[test]
    fn connection_timed_out_is_transient() {
        let err = anyhow!("git fetch failed: Connection timed out after 30s");
        transient(&err);
    }

    #[test]
    fn connection_refused_is_transient() {
        let err = anyhow!("error: Connection refused while contacting api.github.com");
        transient(&err);
    }

    #[test]
    fn tls_handshake_failure_is_transient() {
        let err = anyhow!("clone failed: TLS handshake failure during fetch");
        transient(&err);
    }

    #[test]
    fn remote_hung_up_is_transient() {
        let err = anyhow!("fatal: the remote end hung up unexpectedly");
        transient(&err);
    }

    #[test]
    fn network_unreachable_is_transient() {
        let err = anyhow!("fatal: unable to access github.com: Network is unreachable");
        transient(&err);
    }

    #[test]
    fn temporary_dns_failure_is_transient() {
        let err =
            anyhow!("clone failed: Temporary failure in name resolution while contacting github");
        transient(&err);
    }

    #[test]
    fn operation_timed_out_is_transient() {
        let err = anyhow!("git fetch: Operation timed out");
        transient(&err);
    }

    // ---- Transient: HTTP statuses ----

    #[test]
    fn http_502_is_transient() {
        let err = anyhow!("GitHub returned HTTP 502 Bad Gateway");
        transient(&err);
    }

    #[test]
    fn http_503_is_transient() {
        let err = anyhow!("GitHub status: 503 Service Unavailable");
        transient(&err);
    }

    #[test]
    fn http_504_is_transient() {
        let err = anyhow!("HTTP 504 Gateway Timeout from GitHub");
        transient(&err);
    }

    #[test]
    fn http_522_is_transient() {
        let err = anyhow!("HTTP 522 from GitHub edge");
        transient(&err);
    }

    #[test]
    fn http_524_is_transient() {
        let err = anyhow!("HTTP 524 from GitHub origin");
        transient(&err);
    }

    #[test]
    fn http_401_is_transient_for_auth_blip() {
        let err = anyhow!("GitHub status: 401 Unauthorized");
        transient(&err);
    }

    #[test]
    fn http_403_is_transient_for_auth_blip() {
        let err = anyhow!("GitHub HTTP 403 — token may have rotated");
        transient(&err);
    }

    #[test]
    fn http_429_rate_limit_is_transient() {
        let err = anyhow!("GitHub status: 429 Too Many Requests");
        transient(&err);
    }

    // ---- Transient: git exit 128 with network stderr ----

    #[test]
    fn git_exit_128_with_dns_error_is_transient() {
        let err = anyhow!(
            "git fetch failed (exit status: 128): fatal: Could not resolve host: github.com"
        );
        transient(&err);
    }

    // ---- Transient: io::Error kinds ----

    #[test]
    fn io_timed_out_kind_is_transient() {
        let io_err: anyhow::Error =
            anyhow::Error::from(io::Error::new(io::ErrorKind::TimedOut, "request timed out"))
                .context("while pulling from origin");
        transient(&io_err);
    }

    #[test]
    fn io_connection_reset_kind_is_transient() {
        let io_err: anyhow::Error =
            anyhow::Error::from(io::Error::new(io::ErrorKind::ConnectionReset, "RST"))
                .context("during fetch");
        transient(&io_err);
    }

    #[test]
    fn io_connection_aborted_kind_is_transient() {
        let io_err: anyhow::Error =
            anyhow::Error::from(io::Error::new(io::ErrorKind::ConnectionAborted, "abort"))
                .context("during fetch");
        transient(&io_err);
    }

    #[test]
    fn io_broken_pipe_kind_is_transient() {
        let io_err: anyhow::Error =
            anyhow::Error::from(io::Error::new(io::ErrorKind::BrokenPipe, "EPIPE"))
                .context("during fetch");
        transient(&io_err);
    }

    #[test]
    fn io_would_block_kind_is_transient() {
        let io_err: anyhow::Error =
            anyhow::Error::from(io::Error::new(io::ErrorKind::WouldBlock, "EAGAIN"))
                .context("during fetch");
        transient(&io_err);
    }

    // ---- Permanent ----

    #[test]
    fn remains_dirty_after_recovery_is_permanent() {
        let err = anyhow!(
            "workspace /tmp/x still dirty after recovery; refusing to proceed:\n M foo.rs"
        );
        permanent(&err);
    }

    #[test]
    fn invalid_configuration_is_permanent() {
        let err = anyhow!("invalid configuration: required field `agent_branch` missing");
        permanent(&err);
    }

    #[test]
    fn malformed_yaml_is_permanent() {
        let err = anyhow!("malformed YAML at line 12: mapping expected");
        permanent(&err);
    }

    #[test]
    fn no_matching_token_route_is_permanent() {
        let err = anyhow!("no matching token route for owner `foo`");
        permanent(&err);
    }

    #[test]
    fn missing_openspec_binary_is_permanent() {
        let err = anyhow!("openspec preflight failed: `openspec` binary not found on PATH.");
        permanent(&err);
    }

    #[test]
    fn missing_claude_binary_is_permanent() {
        let err = anyhow!("failed to spawn `claude`: No such file or directory");
        permanent(&err);
    }

    // ---- Default-to-transient for unclassified errors ----

    #[test]
    fn unknown_error_defaults_to_transient() {
        let err = anyhow!("something weird happened that we have never seen before");
        transient(&err);
    }

    #[test]
    fn alert_suffix_strings_are_pinned() {
        // Pin the exact suffix strings — they are operator-visible AND
        // referenced in docs/CHATOPS.md examples.
        assert_eq!(
            RecoveryFailureClass::Transient.alert_suffix(),
            " (transient; retrying)"
        );
        assert_eq!(
            RecoveryFailureClass::Permanent.alert_suffix(),
            " (permanent; skipped until daemon restart) — operator inspection required"
        );
    }

    #[test]
    fn log_tag_strings_are_pinned() {
        assert_eq!(RecoveryFailureClass::Transient.log_tag(), "transient");
        assert_eq!(RecoveryFailureClass::Permanent.log_tag(), "permanent");
    }
}
