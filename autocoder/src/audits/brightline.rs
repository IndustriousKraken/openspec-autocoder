//! Architecture-brightline audit. Pure-code metrics; no LLM invocation,
//! no network. `requires_head_change = true`, `WritePolicy::None`.
//!
//! Surfaces structural metrics that frequently signal drift in a code
//! base: oversize source files and identical function signatures across
//! files. The set is intentionally small in the foundation change;
//! future audits can plug in more checks via additional `Audit`
//! implementations or by extending this module's metric list.

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::{Audit, AuditContext, AuditOutcome, Finding, Severity, WritePolicy};
use crate::config::AuditSettings;

const DEFAULT_FILE_LINES_THRESHOLD: u64 = 800;
const SETTINGS_KEY_FILE_LINES: &str = "file_lines_threshold";

/// Directories to skip entirely. Vendored / generated trees would
/// dominate the findings otherwise.
const EXCLUDED_DIR_COMPONENTS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
    ".git",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
];

/// Extensions the scanner examines. Anything else is ignored; binary
/// formats and asset blobs would just clutter the report.
const SCANNED_EXTENSIONS: &[&str] = &[
    "rs", "py", "cs", "ts", "tsx", "js", "jsx", "go", "java", "kt", "swift",
];

#[derive(Clone)]
pub struct ArchitectureBrightlineAudit {
    file_lines_threshold: u64,
}

impl ArchitectureBrightlineAudit {
    /// Build the audit, pulling thresholds out of `audit_settings`
    /// (under the audit's slug key in `settings.extra`). Falls back to
    /// the compile-time defaults when a knob is unset.
    pub fn new(audit_settings: &HashMap<String, AuditSettings>) -> Self {
        let file_lines_threshold = audit_settings
            .get(Self::TYPE)
            .and_then(|s| s.extra.get(SETTINGS_KEY_FILE_LINES))
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_FILE_LINES_THRESHOLD);
        Self {
            file_lines_threshold,
        }
    }

    pub const TYPE: &'static str = "architecture_brightline";

    /// Run both metrics against `workspace`. Returned findings sort by
    /// severity (high → medium → low) then by subject for stability
    /// across invocations.
    pub fn analyze(&self, workspace: &Path) -> Result<Vec<Finding>> {
        let scanned = collect_source_files(workspace)?;
        let mut findings = Vec::new();
        for path in &scanned {
            if let Some(f) = check_file_size(path, workspace, self.file_lines_threshold) {
                findings.push(f);
            }
        }
        findings.extend(check_signature_duplicates(&scanned, workspace));
        // Deterministic ordering: severity (high first), then subject.
        findings.sort_by(|a, b| {
            severity_rank(b.severity)
                .cmp(&severity_rank(a.severity))
                .then(a.subject.cmp(&b.subject))
        });
        Ok(findings)
    }
}

#[async_trait]
impl Audit for ArchitectureBrightlineAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let findings = self.analyze(ctx.workspace)?;
        let _ = ctx.log_writer.write_section(
            "brightline_summary",
            &format!(
                "file_lines_threshold: {}\nfindings_count: {}",
                self.file_lines_threshold,
                findings.len()
            ),
        );
        Ok(AuditOutcome::Reported(findings))
    }
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::High => 3,
        Severity::Medium => 2,
        Severity::Low => 1,
    }
}

fn collect_source_files(workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(workspace, workspace, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if EXCLUDED_DIR_COMPONENTS.iter().any(|d| *d == name) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(root, &path, out)?;
        } else if ft.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if SCANNED_EXTENSIONS.contains(&ext) {
                    out.push(path);
                }
            }
        }
    }
    Ok(())
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

fn check_file_size(path: &Path, root: &Path, threshold: u64) -> Option<Finding> {
    let contents = std::fs::read_to_string(path).ok()?;
    let n = contents.lines().count() as u64;
    if n <= threshold {
        return None;
    }
    let rel = relative_path(path, root);
    Some(Finding {
        severity: Severity::Medium,
        subject: format!("file {rel} is {n} lines (threshold: {threshold})"),
        body: format!("path: {rel}\nlines: {n}\nthreshold: {threshold}"),
        anchor: Some(format!("{rel}:1")),
    })
}

/// Detect identical function/method signatures across files. We use a
/// simple regex per language and stay deliberately approximate — the
/// audit's value is fast smoke-testing, not full parsing.
fn check_signature_duplicates(files: &[PathBuf], root: &Path) -> Vec<Finding> {
    // signature_key → list of (rel_path, line_number)
    let mut occurrences: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for path in files {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Strip Rust `mod tests { ... }` blocks (brace-counted) so test
        // helpers don't pollute the duplicate set.
        let stripped = if ext == "rs" {
            strip_rust_tests_modules(&contents)
        } else {
            contents.clone()
        };
        for (lineno, sig_key) in extract_signatures(&stripped, ext) {
            occurrences
                .entry(sig_key)
                .or_default()
                .push((relative_path(path, root), lineno));
        }
    }
    let mut findings = Vec::new();
    for (sig_key, places) in occurrences {
        if places.len() < 2 {
            continue;
        }
        // Group by file: a signature appearing twice in the SAME file is
        // not a cross-file collision and isn't what this metric is for.
        let mut files_seen: BTreeMap<&String, Vec<usize>> = BTreeMap::new();
        for (p, l) in &places {
            files_seen.entry(p).or_default().push(*l);
        }
        if files_seen.len() < 2 {
            continue;
        }
        let mut subject_locations: Vec<String> = files_seen
            .iter()
            .map(|(p, lines)| {
                let first = lines.first().copied().unwrap_or(1);
                format!("{p}:{first}")
            })
            .collect();
        subject_locations.sort();
        let body = subject_locations.join("\n");
        let subject = format!(
            "duplicate signature `{sig_key}` across {n} files",
            n = files_seen.len()
        );
        findings.push(Finding {
            severity: Severity::Low,
            subject,
            body,
            anchor: subject_locations.first().cloned(),
        });
    }
    findings
}

fn extract_signatures(contents: &str, ext: &str) -> Vec<(usize, String)> {
    let re = match signature_regex(ext) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (idx, line) in contents.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            // Normalize whitespace in the parameter list so trivial
            // formatting differences don't dodge the duplicate check.
            let normalized_params: String = params.split_whitespace().collect::<Vec<_>>().join(" ");
            let key = format!("{name}({normalized_params})");
            out.push((idx + 1, key));
        }
    }
    out
}

fn signature_regex(ext: &str) -> Option<Regex> {
    let pattern = match ext {
        "rs" => Some(r"^\s*(?:pub\s+(?:\([^)]*\)\s+)?)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(([^)]*)\)"),
        "py" => Some(r"^\s*(?:async\s+)?def\s+(\w+)\s*\(([^)]*)\)\s*(?:->[^:]+)?\s*:"),
        "cs" => Some(r"^\s*(?:public|private|protected|internal)?\s*(?:static\s+)?(?:async\s+)?\w[\w<>?]*\s+(\w+)\s*\(([^)]*)\)"),
        "ts" | "tsx" | "js" | "jsx" => Some(
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)\s*\(([^)]*)\)",
        ),
        "go" => Some(r"^\s*func\s+(?:\([^)]+\)\s+)?(\w+)\s*\(([^)]*)\)"),
        "java" | "kt" => Some(
            r"^\s*(?:public|private|protected)?\s*(?:static\s+)?(?:async\s+)?[\w<>?\[\]]+\s+(\w+)\s*\(([^)]*)\)",
        ),
        "swift" => Some(r"^\s*(?:public|private|internal|fileprivate)?\s*func\s+(\w+)\s*\(([^)]*)\)"),
        _ => None,
    };
    pattern.and_then(|p| Regex::new(p).ok())
}

/// Strip every `mod tests { ... }` block from Rust source, brace-matched
/// so nested braces don't trip the scanner. The block can be preceded by
/// `#[cfg(test)]` or any other attribute. Returns the source with the
/// matched ranges replaced by empty lines (preserving line numbers for
/// the duplicate-signature anchors).
fn strip_rust_tests_modules(src: &str) -> String {
    let re = match Regex::new(r"(?m)^\s*(?:#\[[^\]]+\]\s*)?mod\s+tests\s*\{") {
        Ok(r) => r,
        Err(_) => return src.to_string(),
    };
    let mut out = String::with_capacity(src.len());
    let mut last = 0;
    while let Some(m) = re.find_at(src, last) {
        out.push_str(&src[last..m.start()]);
        // Walk forward from the opening brace position to find the
        // matching closing brace.
        let body_start = m.end(); // position right after `{`
        let mut depth: i64 = 1;
        let mut idx = body_start;
        let bytes = src.as_bytes();
        let mut in_line_comment = false;
        let mut in_block_comment = false;
        while idx < bytes.len() {
            let b = bytes[idx];
            if in_line_comment {
                if b == b'\n' {
                    in_line_comment = false;
                }
                idx += 1;
                continue;
            }
            if in_block_comment {
                if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
                    in_block_comment = false;
                    idx += 2;
                    continue;
                }
                idx += 1;
                continue;
            }
            if b == b'/' && idx + 1 < bytes.len() {
                let next = bytes[idx + 1];
                if next == b'/' {
                    in_line_comment = true;
                    idx += 2;
                    continue;
                } else if next == b'*' {
                    in_block_comment = true;
                    idx += 2;
                    continue;
                }
            }
            if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth == 0 {
                    idx += 1;
                    break;
                }
            }
            idx += 1;
        }
        // Replace the entire `mod tests { ... }` span with blank lines
        // matching the original newline count, preserving line numbers.
        let span = &src[m.start()..idx];
        let newlines = span.bytes().filter(|b| *b == b'\n').count();
        for _ in 0..newlines {
            out.push('\n');
        }
        last = idx;
    }
    out.push_str(&src[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, contents: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }

    fn settings_with_threshold(t: u64) -> HashMap<String, AuditSettings> {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_FILE_LINES.to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(t)),
        );
        let mut s = HashMap::new();
        s.insert(
            ArchitectureBrightlineAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        s
    }

    #[test]
    fn file_size_metric_flags_long_files() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // 1200 lines under src/ — exceeds default 800.
        let big: String = (0..1200).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/big.rs"), &big);
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f.subject.contains("src/big.rs") && f.subject.contains("1200")),
            "expected a finding for src/big.rs; got: {findings:?}"
        );
    }

    #[test]
    fn file_size_metric_respects_threshold_override() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let medium: String = (0..600).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/medium.rs"), &medium);
        // Default (800) → no finding.
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            !findings.iter().any(|f| f.subject.contains("medium.rs")),
            "default threshold should not flag 600-line file: {findings:?}"
        );
        // Override (400) → finding.
        let audit2 = ArchitectureBrightlineAudit::new(&settings_with_threshold(400));
        let findings2 = audit2.analyze(ws).unwrap();
        assert!(
            findings2.iter().any(|f| f.subject.contains("medium.rs")),
            "override threshold should flag 600-line file: {findings2:?}"
        );
    }

    #[test]
    fn file_size_metric_ignores_excluded_dirs() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let big: String = (0..2000).map(|i| format!("// l {i}\n")).collect();
        write(&ws.join("node_modules/lib/big.js"), &big);
        write(&ws.join("target/debug/big.rs"), &big);
        write(&ws.join("vendor/dep/big.go"), &big);
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings.is_empty(),
            "excluded dirs must not contribute findings: {findings:?}"
        );
    }

    #[test]
    fn signature_duplicate_metric_flags_cross_file_collisions_rust() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings.iter().any(|f| f.subject.contains("helper")),
            "expected a duplicate-signature finding for `helper`: {findings:?}"
        );
    }

    #[test]
    fn signature_duplicate_metric_ignores_tests_module() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn alpha() {}\n#[cfg(test)]\nmod tests { fn alpha() {} }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "#[cfg(test)]\nmod tests { fn alpha() {} }\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        // `alpha` appears as a real fn only in src/a.rs; the others are
        // inside `mod tests { ... }` and must be stripped → no cross-file
        // collision.
        assert!(
            !findings
                .iter()
                .any(|f| f.subject.contains("duplicate signature") && f.subject.contains("alpha")),
            "tests module signatures must be ignored: {findings:?}"
        );
    }

    #[test]
    fn audit_returns_no_findings_on_clean_codebase() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // A small file, no duplicates.
        write(&ws.join("src/lib.rs"), "pub fn one() {}\n");
        write(&ws.join("src/main.rs"), "fn two() {}\nfn main() { one(); two(); }\n");
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(findings.is_empty(), "clean codebase: {findings:?}");
    }

    #[test]
    fn audit_returns_findings_for_known_violations() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Both: file-size violation AND signature duplicate.
        let big: String = (0..1500)
            .map(|i| format!("fn shared_name() {{ /* {i} */ }}\n"))
            .collect();
        write(&ws.join("src/giant.rs"), &big);
        write(&ws.join("src/other.rs"), "fn shared_name() {}\n");
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(findings.len() >= 2);
        let mut subjects: Vec<&String> = findings.iter().map(|f| &f.subject).collect();
        subjects.sort();
        let joined = subjects
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("giant.rs is 1500 lines"),
            "expected size finding for giant.rs: {joined}"
        );
        assert!(
            joined.contains("duplicate signature `shared_name"),
            "expected duplicate-signature finding: {joined}"
        );
    }

    #[test]
    fn excluded_dirs_skipped_during_walk() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(&ws.join("src/a.rs"), "fn x() {}\n");
        write(&ws.join("node_modules/a.js"), "function x() {}\n");
        write(&ws.join(".git/HEAD"), "ref: refs/heads/main\n");
        let collected = collect_source_files(ws).unwrap();
        let rels: Vec<String> = collected
            .iter()
            .map(|p| relative_path(p, ws))
            .collect();
        assert_eq!(rels, vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn signature_regex_parses_async_pub_rust() {
        let re = signature_regex("rs").unwrap();
        let captures = re
            .captures("    pub async fn do_thing(a: u32) -> Result<()> {")
            .unwrap();
        assert_eq!(&captures[1], "do_thing");
        assert_eq!(captures[2].trim(), "a: u32");
    }
}
