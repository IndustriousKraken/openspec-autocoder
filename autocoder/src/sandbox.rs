//! OS-level sandbox around every `agentic_run` subprocess (a006).
//!
//! a003 closed the credential *key-flow* (no key reaches a subprocess); this
//! module is the *reach* half: a kernel-enforced jail around every CLI the
//! shared [`crate::agentic_run::agentic_run`] primitive spawns, so a model
//! cannot go *get* a credential that exists on the host (another CLI's config
//! store, `~/.ssh`, autocoder's own config) even though the wrapped CLI's own
//! sandbox would have permitted it. Enforcement is external to the CLI — the
//! kernel applies it around the subprocess regardless of the CLI's settings.
//!
//! ## Role-dependent filesystem policy (a013)
//!
//! The filesystem view depends on the role, because the executor must run the
//! project's build toolchain while read-only roles only read:
//!
//! - **Executor — exposed home, default-deny mask-list (denylist).** `$HOME`
//!   is present AND writable (so `~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.pyenv`,
//!   caches, … work without enumeration), EXCEPT a bounded **mask-list** of
//!   sensitive paths which is masked (empty/inaccessible) even inside an
//!   otherwise-exposed tool tree (deny-overrides-allow — `~/.cargo` is exposed
//!   but `~/.cargo/credentials.toml` is masked). See [`DEFAULT_MASK_RELATIVE`].
//! - **Read-only roles (audits, agentic reviewer) — masked-home allowlist.**
//!   `$HOME` is masked; only the read-only workspace, the role's own CLI store,
//!   the **resolved CLI binary + its home-resident dependency closure** (the
//!   folded a012 binding, [`cli_binary_binds`]), and the minimal runtime are
//!   bound back. **Strict mode** offers this same allowlist as an opt-in for
//!   the executor on high-compliance hosts; it is NOT the default.
//!
//! ## Mechanisms (probed on the daemon host)
//!
//! [`detect_mechanism`] picks a **platform-appropriate** mechanism at startup:
//!
//! - **Linux `systemd-run` (transient *service* mode — NOT `--scope`).** PID 1
//!   applies the filesystem + namespace properties; stdout/stderr are captured
//!   with `--pipe --wait --collect`. The properties used
//!   (`man systemd.exec`): `ProtectSystem=strict` (whole fs read-only); for the
//!   executor `ReadWritePaths=$HOME` + `InaccessiblePaths=<mask entry>`; for
//!   read-only roles + strict mode `ProtectHome=tmpfs` + `BindReadOnlyPaths=`
//!   (the allowlist); `PrivateTmp`, `PrivateDevices`, `ProtectProc=invisible` +
//!   `ProcSubset=pid` (no other process's `environ`/`mem`), `NoNewPrivileges`,
//!   `CapabilityBoundingSet=~…` (drop `CAP_NET_RAW`/`CAP_NET_ADMIN`/
//!   `CAP_SYS_PTRACE`), `RestrictAddressFamilies=~AF_PACKET`.
//! - **Linux `bwrap` (bubblewrap) fallback** for unprivileged / non-systemd /
//!   in-container hosts: `--ro-bind / /` then, for the executor, `--bind
//!   <home>` rw + `--tmpfs`/`--ro-bind /dev/null` over each mask entry; for
//!   read-only roles + strict mode `--tmpfs <home>` + `--ro-bind-try` the
//!   allowlist; `--proc /proc`, `--dev /dev`, `--tmpfs /tmp`, `--unshare-*`,
//!   `--cap-drop`, `--die-with-parent`.
//! - **macOS `sandbox-exec` (Seatbelt)** with a generated profile realizing the
//!   same policy ([`seatbelt_profile`]): `(allow default)` minus the mask
//!   subpaths for the executor; `(deny default)` plus the allowlist for
//!   read-only roles. Ships with the OS, so the gate is normally satisfied.
//!
//! Outbound network egress is **deliberately not restricted** here (no
//! `--unshare-net`, no `RestrictAddressFamilies` beyond `AF_PACKET`): egress
//! control belongs to the host firewall, and there is no maintainable in-app
//! allowlist for CDN'd API/forge hosts. The sandbox does filesystem and host
//! isolation, not a network allowlist.
//!
//! ## Credential-store layers
//!
//! Two complementary, independently-toggleable layers protect CLI config
//! stores (both ON by default; see [`crate::config`]):
//!
//! - **`os_hide` (mask-list membership).** The store of every CLI *other than
//!   the running role's own* is in the executor's mask-list (so it is masked
//!   from the namespace) and is absent from a read-only role's allowlist. It
//!   cannot protect the running role's own store, which must stay readable for
//!   the CLI to authenticate.
//! - **`engine_deny`** — the per-invocation tool-use denylist the executor
//!   already supplies to the CLI (see [`crate::audits::write_sandbox_settings`])
//!   extended to deny the agent's `Read`/`Bash` tools on *every* registered
//!   CLI store, the self-store included. A string-pattern speed bump that
//!   covers the one store `os_hide` cannot; it deters, it does not bound.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::config::CliKind;

/// The kernel mechanism that applies the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMechanism {
    /// `systemd-run` in transient service mode (PID 1 applies the namespace).
    SystemdRun,
    /// `bwrap` (bubblewrap) — the unprivileged / non-systemd Linux fallback.
    Bwrap,
    /// `sandbox-exec` (the macOS Seatbelt sandbox) with a generated profile
    /// (a013, folding in a73). Ships with the OS, so the gate is normally
    /// satisfied without any install.
    SandboxExec,
}

impl SandboxMechanism {
    /// The binary this mechanism invokes.
    pub fn program(self) -> &'static str {
        match self {
            Self::SystemdRun => "systemd-run",
            Self::Bwrap => "bwrap",
            Self::SandboxExec => "sandbox-exec",
        }
    }

    /// Operator-facing label for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemdRun => "systemd-run",
            Self::Bwrap => "bwrap",
            Self::SandboxExec => "sandbox-exec",
        }
    }
}

/// Capabilities dropped from the subprocess's bounding set: no raw-socket
/// sniffing (`CAP_NET_RAW`), no route/iptables hijack (`CAP_NET_ADMIN`), no
/// reading another process's memory (`CAP_SYS_PTRACE`).
pub const DROPPED_CAPS: [&str; 3] = ["CAP_NET_RAW", "CAP_NET_ADMIN", "CAP_SYS_PTRACE"];

/// The home directory the allowlist is built relative to. `$HOME`, falling
/// back to `/root` only if unset (the daemon always runs with `HOME` set).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// The on-disk config/credential store directories for one CLI kind, under
/// `home`. These are the paths the OS allowlist admits (self) or hides
/// (others), AND that `engine_deny` denies the agent's read tools.
///
/// `claude` keeps its login + settings under `~/.claude`. `opencode` keeps
/// its credential store under `~/.local/share/opencode` and its config under
/// `~/.config/opencode`; both are protected. `agy` (Antigravity, a69) keeps
/// its OAuth login + settings + per-conversation state under `~/.gemini`
/// (including `~/.gemini/antigravity-cli` and `oauth_creds.json`) and a cache
/// under `~/.cache/antigravity`; both are protected.
pub fn config_stores_for(cli: CliKind, home: &Path) -> Vec<PathBuf> {
    match cli {
        CliKind::Claude => vec![home.join(".claude")],
        CliKind::Opencode => vec![
            home.join(".local/share/opencode"),
            home.join(".config/opencode"),
        ],
        CliKind::Antigravity => vec![
            home.join(".gemini"),
            home.join(".cache/antigravity"),
        ],
    }
}

/// Every registered CLI strategy's config store, driven by [`CliKind::ALL`]
/// so the set grows automatically as strategies are added (task 5.2) — never
/// a hardcoded literal list.
pub fn all_config_stores(home: &Path) -> Vec<PathBuf> {
    CliKind::ALL
        .iter()
        .flat_map(|cli| config_stores_for(*cli, home))
        .collect()
}

/// The `engine_deny` read-deny glob patterns covering every registered CLI
/// store (the self-store included). Supplied per-invocation through the CLI's
/// own settings mechanism — never by mutating the operator's global config.
pub fn engine_deny_read_paths(home: &Path) -> Vec<String> {
    all_config_stores(home)
        .into_iter()
        .map(|p| format!("{}/**", p.display()))
        .collect()
}

// ---------------------------------------------------------------------------
// a013: the executor's default-deny mask-list.
//
// Under the executor's exposed-home denylist policy, `$HOME` is present and
// writable so build toolchains (`~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.pyenv`,
// …) work without enumeration, EXCEPT this bounded set of sensitive paths
// which is masked. Two categories: credential paths (read-protection) AND
// shell-init/persistence paths (write-protection). The other-CLI-store subset
// is NOT here — it is added dynamically and governed by `os_hide`.
// ---------------------------------------------------------------------------

/// Default mask-list entries, relative to `$HOME`. Masks apply even inside
/// otherwise-exposed tool trees (deny-overrides-allow): `~/.cargo` is exposed
/// but `~/.cargo/credentials.toml` is masked.
pub const DEFAULT_MASK_RELATIVE: &[&str] = &[
    // --- Credential paths (read-protection) ---
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".config/gcloud", // Google Cloud SDK tokens
    ".azure",         // Azure CLI tokens
    ".kube",          // Kubernetes credentials
    ".docker/config.json", // Docker registry auth
    ".config/gh",     // GitHub CLI token
    ".git-credentials", // git `store` helper plaintext tokens
    ".config/git/credentials", // git `store` helper under XDG config
    ".gitcookies",    // git http cookie file (`cookie` helper / curl)
    ".cargo/credentials.toml",
    ".cargo/credentials", // older cargo credentials filename
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    // --- Shell-init / persistence paths (write-protection) ---
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".ssh/authorized_keys",
    ".config/autostart", // XDG autostart (persistence)
];

/// The default mask-list resolved against `home`.
pub fn default_mask_list(home: &Path) -> Vec<PathBuf> {
    DEFAULT_MASK_RELATIVE.iter().map(|r| home.join(r)).collect()
}

/// Of the operator's `mask_remove` entries, those that name a DEFAULT mask
/// entry (so exposing them is a relaxed posture worth a startup WARN). The
/// returned strings are the operator's `~`-style spellings, deduped, in
/// default-list order — so the WARN names them as the operator wrote them.
pub fn removed_default_mask_entries(mask_remove: &[String]) -> Vec<String> {
    // Compare by home-relative normalization so `~/.ssh`, `$HOME/.ssh`, and an
    // absolute spelling all match the default `.ssh`. We normalize against a
    // sentinel home so the comparison is independent of the real `$HOME`.
    let sentinel = Path::new("/__home__");
    let defaults: std::collections::HashSet<PathBuf> =
        default_mask_list(sentinel).into_iter().collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in mask_remove {
        let normalized = expand_home(entry, sentinel);
        if defaults.contains(&normalized) && seen.insert(entry.clone()) {
            out.push(entry.clone());
        }
    }
    out
}

/// Expand a configured path string against `home`: a leading `~/` or `$HOME/`
/// (and bare `~` / `$HOME`) resolves to `home`; anything else is taken as-is
/// (absolute, or relative to the daemon's cwd).
pub(crate) fn expand_home(s: &str, home: &Path) -> PathBuf {
    if s == "~" || s == "$HOME" {
        return home.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home.join(rest);
    }
    if let Some(rest) = s.strip_prefix("$HOME/") {
        return home.join(rest);
    }
    PathBuf::from(s)
}

/// Sort + dedup a path list, dropping any path that is a descendant of another
/// included path (the ancestor's bind/mask already covers it). Keeps the list
/// minimal so the argv builders never bind/mask a path twice or nest mounts.
pub(crate) fn dedup_paths(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if out.last().is_some_and(|prev| p.starts_with(prev)) {
            continue;
        }
        out.push(p);
    }
    out
}

/// One resolved mask-list entry. `is_dir` selects the masking primitive: an
/// empty tmpfs / `InaccessiblePaths` for a directory, an inaccessible bind for
/// a single file (so masking `~/.cargo/credentials.toml` does not hide the
/// surrounding exposed `~/.cargo` toolchain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Resolve a program name to its on-disk path, following `$PATH` when the name
/// has no path separator. Does NOT canonicalize symlinks (the caller does).
pub fn resolve_program_path(program: &OsStr) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() || p.components().count() > 1 {
        return Some(p.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(p))
            .find(|c| c.is_file())
    })
}

/// The read-only/executable binds the running role's CLI binary needs to exec
/// under an allowlist policy where `$HOME` is masked (the folded a012 binding).
/// Resolves the binary (`$PATH` search + symlink follow) and returns its
/// home-resident dependency closure: the PATH location, and — when it
/// redirects through a symlink into an install tree — the real target's
/// package directory. Paths OUTSIDE `$HOME` are omitted (the read-only root
/// keeps them visible); empty when the binary cannot be resolved or lives
/// entirely outside `$HOME`.
pub fn cli_binary_binds(program: &OsStr, home: &Path) -> Vec<PathBuf> {
    let Some(found) = resolve_program_path(program) else {
        return Vec::new();
    };
    let mut binds: Vec<PathBuf> = vec![found.clone()];
    // Follow symlinks to the real target. When the PATH entry redirects into a
    // separate install tree (the typical `~/.local/bin/<cli>` → package case),
    // bind the target's package directory as the dependency closure. A
    // self-contained binary (real == found) needs only the file itself.
    if let Some(real) = std::fs::canonicalize(&found).ok().filter(|r| *r != found) {
        if let Some(parent) = real.parent() {
            binds.push(parent.to_path_buf());
        }
        binds.push(real);
    }
    // Canonicalize home to handle platform symlinks (e.g., /var -> /private/var on macOS)
    let canonical_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    binds.retain(|p| {
        let canonical_p = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        canonical_p.starts_with(&canonical_home)
    });
    dedup_paths(binds)
}

/// The role-dependent filesystem policy for one run, consumed by the argv
/// builders (a013).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsPolicy {
    /// **Executor default — exposed home, default-deny mask-list.** `$HOME` is
    /// present and writable; each mask entry is masked (empty/inaccessible)
    /// even inside an otherwise-exposed tool tree (deny-overrides-allow).
    Denylist { mask: Vec<MaskEntry> },
    /// **Read-only roles (always) AND the executor under strict mode.** `$HOME`
    /// is masked; only these paths are bound back read-only.
    Allowlist {
        /// The running role's own CLI config store(s), admitted read-only so
        /// the wrapped CLI can authenticate.
        self_stores: Vec<PathBuf>,
        /// Other CLI stores admitted read-only — populated ONLY when `os_hide`
        /// is off (so a nested CLI of that kind could authenticate live).
        extra_ro_stores: Vec<PathBuf>,
        /// The resolved CLI binary + its home-resident dependency closure,
        /// bound read-only/executable so the wrapped CLI execs under a masked
        /// home (the folded a012 binding).
        cli_binary_binds: Vec<PathBuf>,
    },
}

/// The filesystem policy + role for one run, consumed by the argv builders.
#[derive(Debug, Clone)]
pub struct SandboxPlan {
    /// The run's workspace (always present in the namespace).
    pub workspace: PathBuf,
    /// `true` mounts the workspace read-write (the executor, incl. strict
    /// mode); `false` mounts it read-only (audits, agentic reviewer,
    /// contradiction checks).
    pub workspace_writable: bool,
    /// The home directory: exposed read-write under the denylist, masked under
    /// the allowlist (then selectively re-bound).
    pub home: PathBuf,
    /// The role-dependent filesystem policy (a013).
    pub policy: FsPolicy,
    /// Additional host paths bound read-only into the child namespace,
    /// applied AFTER the policy's masking steps (the private `/tmp`, the
    /// masked home) so a path under a masked location is re-exposed. The
    /// daemon control socket goes here so the per-execution MCP child can
    /// `connect()` to relay outcomes/submissions even when the socket lives
    /// under `/tmp` or a masked home.
    pub extra_ro_paths: Vec<PathBuf>,
}

/// The program + args + explicit env of the strategy-built inner command,
/// extracted so it can be re-wrapped under a mechanism. The working directory
/// is applied by the wrapper (`--working-directory` / `--chdir <workspace>`),
/// so it is not carried here.
#[derive(Debug, Clone)]
pub struct InnerCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// Env vars the strategy set explicitly (e.g. `ANTHROPIC_BASE_URL`).
    pub env: Vec<(OsString, OsString)>,
}

impl InnerCommand {
    /// Extract the inner invocation from a strategy-built [`tokio::process::Command`]
    /// before stdio/process-group are applied.
    pub fn from_command(cmd: &tokio::process::Command) -> Self {
        let std = cmd.as_std();
        let program = std.get_program().to_os_string();
        let args = std.get_args().map(|a| a.to_os_string()).collect();
        let env = std
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
            .collect();
        Self {
            program,
            args,
            env,
        }
    }
}

/// Env-var names/prefixes forwarded into the `systemd-run` service unit (which
/// does NOT inherit the caller's full environment). The wrapped CLI needs
/// `HOME`/`PATH`/`USER` to locate its store + binaries; the MCP child needs
/// the `ORCH_*` control-socket vars; the strategy's explicit `ANTHROPIC_*` /
/// model-selection env is forwarded separately from [`InnerCommand::env`].
const SYSTEMD_ENV_PASSTHROUGH: &[&str] = &["HOME", "PATH", "USER", "LOGNAME", "LANG", "TERM"];
const SYSTEMD_ENV_PASSTHROUGH_PREFIXES: &[&str] = &["ORCH_", "XDG_", "ANTHROPIC_"];

fn should_passthrough(name: &str) -> bool {
    SYSTEMD_ENV_PASSTHROUGH.contains(&name)
        || SYSTEMD_ENV_PASSTHROUGH_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
}

/// Build the full `systemd-run` argv (program included) for `plan` wrapping
/// `inner`. Transient *service* mode with `--pipe --wait --collect` so the
/// existing streaming-JSON and capture output modes are preserved; the
/// filesystem allowlist, capability drops, and `/proc` restriction are applied
/// as `--property=` settings by PID 1.
pub fn systemd_run_argv(plan: &SandboxPlan, inner: &InnerCommand) -> Vec<OsString> {
    fn prop(argv: &mut Vec<OsString>, key: &str, val: &OsStr) {
        let mut s = OsString::from(format!("--property={key}="));
        s.push(val);
        argv.push(s);
    }

    let mut argv: Vec<OsString> = Vec::new();
    argv.push(OsString::from(SandboxMechanism::SystemdRun.program()));
    for flag in ["--quiet", "--pipe", "--wait", "--collect"] {
        argv.push(OsString::from(flag));
    }

    prop(&mut argv, "WorkingDirectory", plan.workspace.as_os_str());
    // Host isolation + capability drops + /proc restriction. NOTE: `ProtectHome`
    // is NOT set unconditionally — it is set to `tmpfs` only under the allowlist
    // policy (below); the denylist exposes home read-write instead.
    prop(&mut argv, "NoNewPrivileges", OsStr::new("yes"));
    prop(&mut argv, "ProtectSystem", OsStr::new("strict"));
    prop(&mut argv, "PrivateTmp", OsStr::new("yes"));
    prop(&mut argv, "PrivateDevices", OsStr::new("yes"));
    prop(&mut argv, "ProtectProc", OsStr::new("invisible"));
    prop(&mut argv, "ProcSubset", OsStr::new("pid"));
    prop(
        &mut argv,
        "CapabilityBoundingSet",
        OsStr::new(&format!("~{}", DROPPED_CAPS.join(" "))),
    );
    prop(&mut argv, "RestrictAddressFamilies", OsStr::new("~AF_PACKET"));

    // Workspace posture (rw for the executor incl. strict mode; ro otherwise).
    if plan.workspace_writable {
        prop(&mut argv, "ReadWritePaths", plan.workspace.as_os_str());
    } else {
        prop(&mut argv, "BindReadOnlyPaths", plan.workspace.as_os_str());
    }

    // Role-dependent filesystem policy (a013).
    match &plan.policy {
        FsPolicy::Denylist { mask } => {
            // Exposed home: bind it read-write (so toolchains + caches work),
            // then mask each entry inaccessible. `InaccessiblePaths` overrides
            // the `ReadWritePaths=$HOME` for that subpath (deny-overrides-allow)
            // and works for both directories and single files.
            prop(&mut argv, "ReadWritePaths", plan.home.as_os_str());
            for e in mask {
                prop(&mut argv, "InaccessiblePaths", e.path.as_os_str());
            }
        }
        FsPolicy::Allowlist {
            self_stores,
            extra_ro_stores,
            cli_binary_binds,
        } => {
            // Masked home: empty tmpfs, then bind the allowlist read-only.
            prop(&mut argv, "ProtectHome", OsStr::new("tmpfs"));
            for p in self_stores
                .iter()
                .chain(extra_ro_stores.iter())
                .chain(cli_binary_binds.iter())
            {
                prop(&mut argv, "BindReadOnlyPaths", p.as_os_str());
            }
        }
    }

    // Extra read-only binds (the daemon control socket). Applied after the
    // policy match — systemd applies bind mounts after `PrivateTmp` /
    // `ProtectHome`, so a socket under `/tmp` or the masked home is re-exposed
    // and remains connectable.
    for p in &plan.extra_ro_paths {
        prop(&mut argv, "BindReadOnlyPaths", p.as_os_str());
    }

    // Forward the strategy's explicit env + the curated passthrough set.
    for (k, v) in &inner.env {
        let mut s = OsString::from("--setenv=");
        s.push(k);
        s.push("=");
        s.push(v);
        argv.push(s);
    }
    let explicit: std::collections::HashSet<&OsStr> =
        inner.env.iter().map(|(k, _)| k.as_os_str()).collect();
    for (k, v) in std::env::vars_os() {
        if explicit.contains(k.as_os_str()) {
            continue;
        }
        if k.to_str().is_some_and(should_passthrough) {
            let mut s = OsString::from("--setenv=");
            s.push(&k);
            s.push("=");
            s.push(&v);
            argv.push(s);
        }
    }

    argv.push(OsString::from("--"));
    argv.push(inner.program.clone());
    argv.extend(inner.args.iter().cloned());
    argv
}

/// Build the full `bwrap` argv (program included) for `plan` wrapping `inner`.
/// `bwrap` inherits the caller's environment, so the strategy's explicit env
/// is applied onto the wrapper [`tokio::process::Command`] in
/// [`wrap_command`] rather than encoded here. Network namespaces are NOT
/// unshared (egress stays open by design).
pub fn bwrap_argv(plan: &SandboxPlan, inner: &InnerCommand) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::new();
    let push = |argv: &mut Vec<OsString>, s: &str| argv.push(OsString::from(s));

    push(&mut argv, SandboxMechanism::Bwrap.program());
    push(&mut argv, "--die-with-parent");
    push(&mut argv, "--new-session");
    // Isolate namespaces EXCEPT the network (egress stays open by design).
    push(&mut argv, "--unshare-user");
    push(&mut argv, "--unshare-ipc");
    push(&mut argv, "--unshare-pid");
    push(&mut argv, "--unshare-uts");
    push(&mut argv, "--unshare-cgroup");

    // Whole root read-only first; the policy branch then shapes home.
    push(&mut argv, "--ro-bind");
    push(&mut argv, "/");
    push(&mut argv, "/");

    // Role-dependent filesystem policy (a013).
    match &plan.policy {
        FsPolicy::Denylist { mask } => {
            // Exposed home: bind it read-write over the read-only root, then
            // mask each entry. A directory becomes an empty tmpfs; a single
            // file is shadowed by an inaccessible read-only bind of /dev/null
            // (so masking `~/.cargo/credentials.toml` leaves the rest of the
            // exposed `~/.cargo` toolchain intact). Masks come AFTER the home
            // bind so they override it (deny-overrides-allow).
            push(&mut argv, "--bind");
            argv.push(plan.home.as_os_str().to_os_string());
            argv.push(plan.home.as_os_str().to_os_string());
            for e in mask {
                if e.is_dir {
                    push(&mut argv, "--tmpfs");
                    argv.push(e.path.as_os_str().to_os_string());
                } else {
                    push(&mut argv, "--ro-bind");
                    push(&mut argv, "/dev/null");
                    argv.push(e.path.as_os_str().to_os_string());
                }
            }
        }
        FsPolicy::Allowlist {
            self_stores,
            extra_ro_stores,
            cli_binary_binds,
        } => {
            // Masked home: empty tmpfs over it, then re-bind the allowlist
            // read-only (the stores for auth + the CLI binary closure so the
            // wrapped CLI execs under the mask — the folded a012 binding).
            push(&mut argv, "--tmpfs");
            argv.push(plan.home.as_os_str().to_os_string());
            for p in self_stores
                .iter()
                .chain(extra_ro_stores.iter())
                .chain(cli_binary_binds.iter())
            {
                push(&mut argv, "--ro-bind-try");
                argv.push(p.as_os_str().to_os_string());
                argv.push(p.as_os_str().to_os_string());
            }
        }
    }

    // Workspace posture (rw for the executor incl. strict mode; ro otherwise).
    push(&mut argv, if plan.workspace_writable { "--bind" } else { "--ro-bind" });
    argv.push(plan.workspace.as_os_str().to_os_string());
    argv.push(plan.workspace.as_os_str().to_os_string());

    push(&mut argv, "--proc");
    push(&mut argv, "/proc");
    push(&mut argv, "--dev");
    push(&mut argv, "/dev");
    push(&mut argv, "--tmpfs");
    push(&mut argv, "/tmp");

    // Extra read-only binds (the daemon control socket). Placed AFTER
    // `--tmpfs /tmp` (and the policy's home `--tmpfs`) so a socket under
    // `/tmp` or the masked home is re-exposed over the masking tmpfs.
    // `--ro-bind-try` tolerates an absent path (the socket may not exist
    // in a degraded run) the same way the store binds do.
    for p in &plan.extra_ro_paths {
        push(&mut argv, "--ro-bind-try");
        argv.push(p.as_os_str().to_os_string());
        argv.push(p.as_os_str().to_os_string());
    }

    for cap in DROPPED_CAPS {
        push(&mut argv, "--cap-drop");
        push(&mut argv, cap);
    }

    push(&mut argv, "--chdir");
    argv.push(plan.workspace.as_os_str().to_os_string());

    push(&mut argv, "--");
    argv.push(inner.program.clone());
    argv.extend(inner.args.iter().cloned());
    argv
}

/// Quote a path as a Seatbelt string literal (the profile is an S-expression;
/// backslashes and double-quotes are escaped).
fn sb_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Generate the macOS Seatbelt profile realizing `plan`'s policy (a013,
/// folding in a73). The executor's denylist becomes `(allow default)` minus
/// `(deny … (subpath <mask>))`; a read-only role's allowlist becomes
/// `(deny default)` plus the specific allows (read-only root, masked home,
/// re-exposed stores + CLI binary, workspace). Both deny the macOS analogs of
/// the Linux capability drops: cross-process inspection (`process-info*`),
/// inbound/raw networking (`network-inbound`) — egress stays open — and
/// privilege elevation (`setuid` via `(deny default)` for the allowlist; for
/// the denylist there is no `setuid` operation, so `NoNewPrivileges` has no
/// exact Seatbelt analog and is omitted).
pub fn seatbelt_profile(plan: &SandboxPlan) -> String {
    let mut out = String::from("(version 1)\n");
    match &plan.policy {
        FsPolicy::Denylist { mask } => {
            out.push_str("(allow default)\n");
            // Mask the sensitive set even inside exposed tool trees.
            for e in mask {
                out.push_str(&format!(
                    "(deny file-read* file-write* (subpath {}))\n",
                    sb_quote(&e.path)
                ));
            }
            // Capability-drop analogs.
            out.push_str("(deny process-info*)\n");
            out.push_str("(deny network-inbound)\n");
        }
        FsPolicy::Allowlist {
            self_stores,
            extra_ro_stores,
            cli_binary_binds,
        } => {
            out.push_str("(deny default)\n");
            // Minimal runtime: exec/fork, sysctl reads, mach lookups, egress.
            out.push_str("(allow process-exec*)\n");
            out.push_str("(allow process-fork)\n");
            out.push_str("(allow sysctl-read)\n");
            out.push_str("(allow mach-lookup)\n");
            out.push_str("(allow network-outbound)\n");
            // System paths readable; home masked (the allowlist's masked home).
            out.push_str("(allow file-read* (subpath \"/\"))\n");
            out.push_str(&format!(
                "(deny file-read* file-write* (subpath {}))\n",
                sb_quote(&plan.home)
            ));
            // Re-expose the allowlist read-only under the masked home.
            for p in self_stores
                .iter()
                .chain(extra_ro_stores.iter())
                .chain(cli_binary_binds.iter())
            {
                out.push_str(&format!("(allow file-read* (subpath {}))\n", sb_quote(p)));
            }
            // The workspace: read-write for the executor (strict mode),
            // read-only for read-only roles.
            out.push_str(&format!(
                "(allow file-read* (subpath {}))\n",
                sb_quote(&plan.workspace)
            ));
            if plan.workspace_writable {
                out.push_str(&format!(
                    "(allow file-write* (subpath {}))\n",
                    sb_quote(&plan.workspace)
                ));
            }
            // Capability-drop analogs (redundant under deny-default, explicit
            // for parity with the denylist + clarity of intent).
            out.push_str("(deny process-info*)\n");
            out.push_str("(deny network-inbound)\n");
        }
    }
    // Extra read-only paths (the daemon control socket): allow reading the
    // exact path so a Unix-socket `connect()` reaches it even when it sits
    // under the masked home (the allowlist denies the home subtree above;
    // these allows come last and win — Seatbelt is last-match-wins). Outbound
    // is already permitted, and the denylist's `(allow default)` already covers
    // these, so the allows are a harmless no-op there.
    for p in &plan.extra_ro_paths {
        out.push_str(&format!("(allow file-read* (literal {}))\n", sb_quote(p)));
    }
    out
}

/// Build the full `sandbox-exec` argv (program included) for `plan` wrapping
/// `inner` (a013, folding in a73). The generated Seatbelt profile is passed
/// inline via `-p` (no temp-file lifetime to manage); `sandbox-exec` then execs
/// the inner CLI under the policy. Like `bwrap`, it inherits the caller's env,
/// so the strategy's explicit env is applied onto the wrapper command in
/// [`wrap_command`].
pub fn sandbox_exec_argv(plan: &SandboxPlan, inner: &InnerCommand) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        OsString::from(SandboxMechanism::SandboxExec.program()),
        OsString::from("-p"),
        OsString::from(seatbelt_profile(plan)),
        OsString::from("--"),
        inner.program.clone(),
    ];
    argv.extend(inner.args.iter().cloned());
    argv
}

/// Build the wrapper [`tokio::process::Command`] for `mechanism`. The caller
/// applies stdio + `process_group(0)` + `current_dir` to the returned command
/// exactly as it would to an unwrapped spawn, so the timeout/kill/streaming
/// behavior is unchanged.
pub fn wrap_command(
    mechanism: SandboxMechanism,
    plan: &SandboxPlan,
    inner: &InnerCommand,
) -> tokio::process::Command {
    let argv = match mechanism {
        SandboxMechanism::SystemdRun => systemd_run_argv(plan, inner),
        SandboxMechanism::Bwrap => bwrap_argv(plan, inner),
        SandboxMechanism::SandboxExec => sandbox_exec_argv(plan, inner),
    };
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // bwrap inherits the env; systemd-run encodes it as --setenv (above) but
    // setting it on the wrapper too is harmless. Either way the strategy's
    // explicit env reaches the inner command.
    for (k, v) in &inner.env {
        cmd.env(k, v);
    }
    cmd
}

/// The spawn decision after the mechanism gate (tasks 4.1 / 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPlan {
    /// Wrap the subprocess in the OS-level sandbox via this mechanism.
    Wrap(SandboxMechanism),
    /// No mechanism is available, but the operator opted into unsandboxed
    /// operation — spawn the bare subprocess (the loud WARN was emitted at
    /// startup).
    Unsandboxed,
}

/// The operator-facing message carried by [`SandboxMechanismUnavailable`] —
/// the fail-closed gate's refusal text (mechanisms missing + the remedy). A
/// const so the typed error AND any diagnostic share one source of truth.
pub const SANDBOX_GATE_REFUSAL_MESSAGE: &str =
    "no platform-appropriate OS sandbox mechanism is available on this \
     host: on Linux neither `systemd-run` (transient service mode) nor \
     `bwrap` can apply the sandbox; on macOS `sandbox-exec` is \
     unavailable. Refusing to spawn an unsandboxed agentic subprocess. \
     Install/enable one of them, or set \
     `executor.sandbox.allow_unsandboxed: true` to override (NOT \
     recommended — the model could then reach host credentials).";

/// The pre-spawn refusal raised by [`decide_spawn`] when no OS sandbox
/// mechanism is available AND the operator has NOT opted into unsandboxed
/// operation (a74). A *typed* error — NOT a bare `anyhow!` — so callers branch
/// on its KIND via [`precondition_unmet_message`] (`downcast_ref` through the
/// `anyhow` chain) rather than matching the message text. This is a
/// *precondition-unmet* failure: the agent subprocess never started, so the
/// revise path treats it distinctly from a substantive `Failed` (no revision
/// slot charged; manual re-trigger).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMechanismUnavailable {
    /// The operator-facing guidance (mechanisms missing + the remedy).
    pub message: String,
}

impl std::fmt::Display for SandboxMechanismUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SandboxMechanismUnavailable {}

/// The guidance message from the sandbox-mechanism gate's pre-spawn refusal,
/// if `err` — or anything in its `anyhow` cause chain — carries one (a74);
/// `None` for any other error (so `is_some()` doubles as the precondition-unmet
/// predicate). When `Some`, the agentic run could not START because a required
/// precondition was unmet (the subprocess never spawned), as opposed to a
/// substantive failure where the subprocess ran and then failed. Driven by the
/// error's KIND ([`SandboxMechanismUnavailable`] via `downcast`), NOT by
/// matching message text, so callers branch reliably — and it survives the
/// `.context(...)` wrapper `agentic_run` adds. Used by the executor to surface
/// the precondition-unmet reason on its outcome.
pub fn precondition_unmet_message(err: &anyhow::Error) -> Option<String> {
    err.chain()
        .find_map(|c| c.downcast_ref::<SandboxMechanismUnavailable>())
        .map(|e| e.message.clone())
}

/// Decide how to spawn given the detected mechanism + the unsandboxed opt-in.
/// Fail-closed: with no mechanism AND no opt-in, return a typed
/// [`SandboxMechanismUnavailable`] error (classifiable as precondition-unmet)
/// naming the missing mechanisms so NO unsandboxed subprocess is spawned
/// (task 4.1).
pub fn decide_spawn(
    mechanism: Option<SandboxMechanism>,
    allow_unsandboxed: bool,
) -> anyhow::Result<SpawnPlan> {
    match (mechanism, allow_unsandboxed) {
        (Some(m), _) => Ok(SpawnPlan::Wrap(m)),
        (None, true) => Ok(SpawnPlan::Unsandboxed),
        (None, false) => Err(anyhow::Error::new(SandboxMechanismUnavailable {
            message: SANDBOX_GATE_REFUSAL_MESSAGE.to_string(),
        })),
    }
}

/// The loud startup WARN emitted once when the daemon will run agentic
/// subprocesses unsandboxed (no mechanism available AND the operator opted
/// in). `None` when a mechanism exists or no opt-in was given. Separated from
/// the logging site so it can be asserted without a daemon (task 8.7).
pub fn startup_unsandboxed_warning(
    mechanism: Option<SandboxMechanism>,
    allow_unsandboxed: bool,
) -> Option<String> {
    (mechanism.is_none() && allow_unsandboxed).then(|| {
        "no platform-appropriate OS sandbox mechanism (Linux: systemd-run / \
         bwrap; macOS: sandbox-exec) is available AND \
         `executor.sandbox.allow_unsandboxed` is set: agentic subprocesses \
         are running UNSANDBOXED. A wrapped CLI's model can reach host \
         credentials (other CLIs' stores, ~/.ssh, autocoder config). Install \
         a sandbox mechanism, or unset the opt-in, to restore the sandbox."
            .to_string()
    })
}

/// Everything one `agentic_run` call needs to apply (or skip) the OS-level
/// sandbox. Constructed per-run by the daemon from the detected mechanism, the
/// resolved per-repo toggles, and the role's read/write posture + CLI kind.
///
/// `enforce == false` (the [`Default`]) skips the OS layer entirely — used by
/// test fixtures AND any not-yet-wired path so existing behavior is unchanged.
/// Production sets `enforce == true` via [`RunSandbox::for_role`], which makes
/// the mechanism gate fail-closed when no mechanism is available.
#[derive(Debug, Clone)]
pub struct RunSandbox {
    pub enforce: bool,
    pub mechanism: Option<SandboxMechanism>,
    pub allow_unsandboxed: bool,
    pub workspace_writable: bool,
    /// The CLI the running role drives — selects its own store (admitted
    /// read-only for auth) vs the other stores (hidden under `os_hide`).
    pub cli: CliKind,
    pub os_hide: bool,
    pub engine_deny: bool,
    /// a013: run the executor under the allowlist (home masked). Read-only
    /// roles (`workspace_writable == false`) always use the allowlist
    /// regardless; this only matters for the executor.
    pub strict_mode: bool,
    /// a013: operator additions to the executor's filesystem mask-list.
    pub mask_add: Vec<String>,
    /// a013: default mask-list entries the operator removed (exposed).
    pub mask_remove: Vec<String>,
}

impl Default for RunSandbox {
    fn default() -> Self {
        Self {
            enforce: false,
            mechanism: None,
            allow_unsandboxed: false,
            workspace_writable: false,
            cli: CliKind::Claude,
            os_hide: true,
            engine_deny: true,
            strict_mode: false,
            mask_add: Vec::new(),
            mask_remove: Vec::new(),
        }
    }
}

impl RunSandbox {
    /// Build the enforced sandbox for one role. `workspace_writable` is `true`
    /// for the executor and `false` for read-only roles (audits, agentic
    /// reviewer, contradiction checks). `cli` is the role's resolved
    /// [`CliKind`] (the self-store is derived from it).
    pub fn for_role(
        mechanism: Option<SandboxMechanism>,
        allow_unsandboxed: bool,
        cli: CliKind,
        workspace_writable: bool,
        toggles: crate::config::SandboxToggles,
    ) -> Self {
        Self {
            enforce: true,
            mechanism,
            allow_unsandboxed,
            workspace_writable,
            cli,
            os_hide: toggles.os_hide,
            engine_deny: toggles.engine_deny,
            strict_mode: toggles.strict_mode,
            mask_add: toggles.mask_add,
            mask_remove: toggles.mask_remove,
        }
    }

    /// Whether this run uses the exposed-home **denylist** (a013): the executor
    /// under its default policy. Read-only roles (`!workspace_writable`) AND the
    /// executor under strict mode use the masked-home allowlist instead.
    pub fn uses_denylist(&self) -> bool {
        self.workspace_writable && !self.strict_mode
    }

    /// The effective filesystem mask-list for the executor's denylist: the
    /// defaults + operator additions − operator removals, plus the other-CLI
    /// stores when `os_hide` is on (deny-overrides-allow). Each entry is
    /// resolved against the host so the argv builders know dir vs file; only
    /// host-existing entries are masked (a mount-based mechanism can only
    /// shadow an existing inode).
    pub fn resolve_mask_list(&self, home: &Path) -> Vec<MaskEntry> {
        let removals: std::collections::HashSet<PathBuf> = self
            .mask_remove
            .iter()
            .map(|s| expand_home(s, home))
            .collect();
        let mut paths: Vec<PathBuf> = default_mask_list(home)
            .into_iter()
            .filter(|p| !removals.contains(p))
            .collect();
        for s in &self.mask_add {
            let p = expand_home(s, home);
            if !removals.contains(&p) {
                paths.push(p);
            }
        }
        // `os_hide` governs the other-CLI-store subset of the mask-list.
        if self.os_hide {
            for c in CliKind::ALL.iter().filter(|c| **c != self.cli) {
                for store in config_stores_for(*c, home) {
                    if !removals.contains(&store) {
                        paths.push(store);
                    }
                }
            }
        }
        dedup_paths(paths)
            .into_iter()
            .filter_map(|p| {
                let md = std::fs::metadata(&p).ok()?;
                Some(MaskEntry {
                    is_dir: md.is_dir(),
                    path: p,
                })
            })
            .collect()
    }

    /// The role-dependent filesystem plan for this run (a013). `program` is the
    /// running role's CLI command (e.g. `claude`), resolved + bound under the
    /// allowlist so the wrapped CLI execs under a masked home (folded a012).
    pub fn build_plan(&self, workspace: &Path, program: &OsStr) -> SandboxPlan {
        self.build_plan_with_home(workspace, &home_dir(), program)
    }

    /// [`build_plan`](Self::build_plan) against an explicit `home` (so the
    /// policy construction is testable without mutating `$HOME`).
    pub fn build_plan_with_home(
        &self,
        workspace: &Path,
        home: &Path,
        program: &OsStr,
    ) -> SandboxPlan {
        let policy = if self.uses_denylist() {
            FsPolicy::Denylist {
                mask: self.resolve_mask_list(home),
            }
        } else {
            let self_stores = config_stores_for(self.cli, home)
                .into_iter()
                .filter(|p| p.exists())
                .collect();
            let extra_ro_stores = if self.os_hide {
                Vec::new()
            } else {
                CliKind::ALL
                    .iter()
                    .filter(|c| **c != self.cli)
                    .flat_map(|c| config_stores_for(*c, home))
                    .filter(|p| p.exists())
                    .collect()
            };
            let cli_binary_binds = cli_binary_binds(program, home);
            FsPolicy::Allowlist {
                self_stores,
                extra_ro_stores,
                cli_binary_binds,
            }
        };
        SandboxPlan {
            workspace: workspace.to_path_buf(),
            workspace_writable: self.workspace_writable,
            home: home.to_path_buf(),
            policy,
            // Populated by the caller (agentic_run) with the control socket
            // when the relay is configured; the plan builder itself adds none.
            extra_ro_paths: Vec::new(),
        }
    }

    /// The `engine_deny` read-deny patterns to fold into the per-invocation
    /// tool-use denylist (empty when the toggle is off). Covers EVERY
    /// registered CLI store, the self-store included.
    pub fn engine_deny_paths(&self) -> Vec<String> {
        if self.engine_deny {
            engine_deny_read_paths(&home_dir())
        } else {
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon-global sandbox context.
//
// The detected mechanism + the unsandboxed opt-in are genuinely daemon-wide,
// so they live in a process-global set once at startup. The *active* per-repo
// toggle override is set for the duration of one change's pipeline via
// [`enter_repo`] (the daemon processes one iteration at a time under the
// busy-marker model), so the executor AND the in-iteration pre-flight /
// review roles all see that repository's resolved toggles.
// ---------------------------------------------------------------------------

use std::sync::{Mutex, OnceLock};

use crate::config::SandboxToggles;

struct GlobalSandbox {
    mechanism: Option<SandboxMechanism>,
    allow_unsandboxed: bool,
    /// The global (`executor.sandbox`) resolved toggles — the fallback when
    /// no per-repo override is active.
    global_toggles: SandboxToggles,
}

static GLOBAL: OnceLock<GlobalSandbox> = OnceLock::new();
static ACTIVE_TOGGLES: Mutex<Option<SandboxToggles>> = Mutex::new(None);

/// Initialize the daemon-global sandbox context once at startup (idempotent —
/// a second call is ignored). After this, [`current_run_sandbox`] returns an
/// *enforced* [`RunSandbox`] so every `agentic_run` spawn is gated + wrapped.
/// Before it (unit tests, non-daemon binaries), `current_run_sandbox` returns
/// the unenforced default so existing behavior is unchanged.
pub fn init_global(
    mechanism: Option<SandboxMechanism>,
    allow_unsandboxed: bool,
    global_toggles: SandboxToggles,
) {
    let _ = GLOBAL.set(GlobalSandbox {
        mechanism,
        allow_unsandboxed,
        global_toggles,
    });
}

/// RAII guard returned by [`enter_repo`]; clears the active per-repo toggle
/// override when dropped so the next iteration starts from the global default.
#[must_use]
pub struct RepoToggleGuard(());

impl Drop for RepoToggleGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_TOGGLES.lock() {
            *active = None;
        }
    }
}

/// Set the active per-repository toggle override for the duration of one
/// change's pipeline. Resolves the repo's `sandbox` block over the global
/// toggles (per-repo overrides global, per field), so the executor AND the
/// in-iteration pre-flight/review roles wrap with this repository's effective
/// posture. A no-op (returns a guard anyway) before [`init_global`].
pub fn enter_repo(repo: Option<&crate::config::RepoSandboxConfig>) -> RepoToggleGuard {
    if let Some(g) = GLOBAL.get() {
        let toggles = g.global_toggles.with_repo_override(repo);
        if let Ok(mut active) = ACTIVE_TOGGLES.lock() {
            *active = Some(toggles);
        }
    }
    RepoToggleGuard(())
}

/// Build the [`RunSandbox`] for one spawn from the daemon-global context.
/// `cli` is the running role's resolved CLI (selects its own store); `writable`
/// is `true` for the executor and `false` for read-only roles. Before
/// [`init_global`] (tests / non-daemon paths) the unenforced default is
/// returned so the OS layer is skipped.
pub fn current_run_sandbox(cli: CliKind, workspace_writable: bool) -> RunSandbox {
    match GLOBAL.get() {
        None => RunSandbox::default(),
        Some(g) => {
            let toggles = ACTIVE_TOGGLES
                .lock()
                .ok()
                .and_then(|a| a.clone())
                .unwrap_or_else(|| g.global_toggles.clone());
            RunSandbox::for_role(g.mechanism, g.allow_unsandboxed, cli, workspace_writable, toggles)
        }
    }
}

/// Probe whether `systemd-run` can apply the sandbox in transient service
/// mode on this host. Runs a trivial `true` unit; success means PID 1 will
/// accept our property set. Unprivileged hosts without polkit/session-bus
/// access fail this probe (→ fall back to `bwrap`).
fn systemd_run_usable() -> bool {
    which("systemd-run")
        && std::process::Command::new("systemd-run")
            .args(["--quiet", "--pipe", "--wait", "--collect"])
            .arg("--")
            .arg("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Probe whether `bwrap` can apply the sandbox (it needs unprivileged user
/// namespaces; some hosts disable them).
fn bwrap_usable() -> bool {
    which("bwrap")
        && std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--proc", "/proc", "--", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Whether `bin` is resolvable on `$PATH`.
pub(crate) fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
        })
        .unwrap_or(false)
}

/// Detect the usable **platform-appropriate** sandbox mechanism at daemon
/// startup (a013): on macOS `sandbox-exec` (the Seatbelt sandbox, which ships
/// with the OS); on Linux, preferring `systemd-run` service mode, else `bwrap`;
/// else `None`. The `None` case drives the fail-closed gate ([`decide_spawn`]).
pub fn detect_mechanism() -> Option<SandboxMechanism> {
    if cfg!(target_os = "macos") {
        return which("sandbox-exec").then_some(SandboxMechanism::SandboxExec);
    }
    if systemd_run_usable() {
        Some(SandboxMechanism::SystemdRun)
    } else if bwrap_usable() {
        Some(SandboxMechanism::Bwrap)
    } else {
        None
    }
}

/// Richer than [`detect_mechanism`]: the dependency preflight (a011) needs to
/// tell "a mechanism binary is present but cannot apply the sandbox" (e.g.
/// `bwrap` installed but the host disables unprivileged user namespaces) apart
/// from "no mechanism present at all", so it can report *unusable* vs *missing*.
/// This complements — it does not replace — the spawn-time fail-closed gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxAvailability {
    /// A mechanism is usable; the field is its operator-facing name.
    Usable { mechanism: &'static str },
    /// One or more mechanism binaries are present but none can apply the
    /// sandbox on this host (the classic case: `bwrap` present, unprivileged
    /// user namespaces disabled).
    PresentButUnusable { present: Vec<&'static str> },
    /// No platform sandbox mechanism is present at all.
    Absent,
}

/// Probe the platform sandbox mechanism for the dependency preflight. On
/// Linux this prefers a usable `systemd-run` service mode, then a usable
/// `bwrap`; if neither *applies* the sandbox but a binary is *present*, it is
/// reported `PresentButUnusable`. On macOS the seatbelt compiler
/// `sandbox-exec` need only be present (per a011 task 1.2).
pub fn sandbox_availability() -> SandboxAvailability {
    if cfg!(target_os = "macos") {
        return if which("sandbox-exec") {
            SandboxAvailability::Usable { mechanism: "sandbox-exec" }
        } else {
            SandboxAvailability::Absent
        };
    }
    if systemd_run_usable() {
        return SandboxAvailability::Usable { mechanism: "systemd-run" };
    }
    if bwrap_usable() {
        return SandboxAvailability::Usable { mechanism: "bwrap" };
    }
    let mut present = Vec::new();
    if which("systemd-run") {
        present.push("systemd-run");
    }
    if which("bwrap") {
        present.push("bwrap");
    }
    if present.is_empty() {
        SandboxAvailability::Absent
    } else {
        SandboxAvailability::PresentButUnusable { present }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osv(items: &[OsString]) -> Vec<String> {
        items.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// An executor-style **denylist** plan (a013): exposed home, the given mask.
    fn deny_plan(writable: bool, mask: Vec<MaskEntry>) -> SandboxPlan {
        SandboxPlan {
            workspace: PathBuf::from("/home/u/.cache/ws"),
            workspace_writable: writable,
            home: PathBuf::from("/home/u"),
            policy: FsPolicy::Denylist { mask },
            extra_ro_paths: Vec::new(),
        }
    }

    /// A read-only-role / strict-mode **allowlist** plan (a013): masked home,
    /// the self store + (os_hide off) another store + the CLI binary bound.
    fn allow_plan(writable: bool, os_hide: bool) -> SandboxPlan {
        let home = PathBuf::from("/home/u");
        SandboxPlan {
            workspace: home.join(".cache/ws"),
            workspace_writable: writable,
            policy: FsPolicy::Allowlist {
                self_stores: vec![home.join(".claude")],
                extra_ro_stores: if os_hide {
                    Vec::new()
                } else {
                    vec![home.join(".local/share/opencode")]
                },
                cli_binary_binds: vec![home.join(".local/bin/claude")],
            },
            home,
            extra_ro_paths: Vec::new(),
        }
    }

    /// A sample mask: a directory (`~/.ssh`) AND a file inside an exposed tree
    /// (`~/.cargo/credentials.toml`) — exercising both masking primitives.
    fn sample_mask() -> Vec<MaskEntry> {
        vec![
            MaskEntry {
                path: PathBuf::from("/home/u/.ssh"),
                is_dir: true,
            },
            MaskEntry {
                path: PathBuf::from("/home/u/.cargo/credentials.toml"),
                is_dir: false,
            },
        ]
    }

    fn inner() -> InnerCommand {
        InnerCommand {
            program: OsString::from("claude"),
            args: vec![OsString::from("--settings"), OsString::from("/tmp/s.json")],
            env: vec![(
                OsString::from("ANTHROPIC_BASE_URL"),
                OsString::from("https://example.invalid"),
            )],
        }
    }

    #[test]
    fn config_stores_cover_each_cli_kind() {
        let home = Path::new("/home/u");
        assert_eq!(config_stores_for(CliKind::Claude, home), vec![home.join(".claude")]);
        let oc = config_stores_for(CliKind::Opencode, home);
        assert!(oc.iter().any(|p| p.ends_with("opencode")));
        // all_config_stores spans every registered CLI kind (driven by ALL).
        let all = all_config_stores(home);
        for cli in CliKind::ALL {
            for store in config_stores_for(cli, home) {
                assert!(all.contains(&store), "all_config_stores must include {store:?}");
            }
        }
    }

    #[test]
    fn engine_deny_paths_cover_every_store_recursively() {
        let home = Path::new("/home/u");
        let pats = engine_deny_read_paths(home);
        // The self (claude) AND another CLI's (opencode) store are both denied.
        assert!(pats.iter().any(|p| p.contains("/.claude/**")));
        assert!(pats.iter().any(|p| p.contains("opencode") && p.ends_with("/**")));
    }

    // a013: the executor's denylist exposes home read-write and masks the
    // mask-list entries (dir → InaccessiblePaths; file → InaccessiblePaths).
    #[test]
    fn systemd_denylist_exposes_home_and_masks_entries() {
        let a = osv(&systemd_run_argv(&deny_plan(true, sample_mask()), &inner()));
        // Transient service mode, NOT --scope; --pipe --wait --collect.
        assert_eq!(a[0], "systemd-run");
        assert!(a.contains(&"--pipe".to_string()));
        assert!(!a.iter().any(|x| x == "--scope"), "must NOT be scope mode");
        // Host isolation + capability drops are unchanged.
        assert!(a.iter().any(|x| x
            == "--property=CapabilityBoundingSet=~CAP_NET_RAW CAP_NET_ADMIN CAP_SYS_PTRACE"));
        assert!(a.iter().any(|x| x == "--property=NoNewPrivileges=yes"));
        assert!(a.iter().any(|x| x == "--property=ProtectSystem=strict"));
        assert!(a.iter().any(|x| x == "--property=ProtectProc=invisible"));
        // Home is EXPOSED read-write, NOT masked by ProtectHome.
        assert!(a.iter().any(|x| x == "--property=ReadWritePaths=/home/u"));
        assert!(
            !a.iter().any(|x| x == "--property=ProtectHome=tmpfs"),
            "the denylist must NOT mask home"
        );
        // The executor's workspace is read-write.
        assert!(a.iter().any(|x| x == "--property=ReadWritePaths=/home/u/.cache/ws"));
        // Mask entries are inaccessible — a directory AND a file inside an
        // otherwise-exposed tool tree (deny-overrides-allow).
        assert!(a.iter().any(|x| x == "--property=InaccessiblePaths=/home/u/.ssh"));
        assert!(a
            .iter()
            .any(|x| x == "--property=InaccessiblePaths=/home/u/.cargo/credentials.toml"));
        // Strategy env is forwarded; the inner command is after `--`.
        assert!(a
            .iter()
            .any(|x| x == "--setenv=ANTHROPIC_BASE_URL=https://example.invalid"));
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dd + 1], "claude");
    }

    // a013: a read-only role's allowlist masks home, binds the self store + the
    // resolved CLI binary (folded a012), and the workspace read-only.
    #[test]
    fn systemd_allowlist_masks_home_binds_stores_and_cli_binary() {
        let a = osv(&systemd_run_argv(&allow_plan(false, true), &inner()));
        assert!(a.iter().any(|x| x == "--property=ProtectHome=tmpfs"));
        assert!(a.iter().any(|x| x == "--property=BindReadOnlyPaths=/home/u/.cache/ws"));
        assert!(
            !a.iter().any(|x| x == "--property=ReadWritePaths=/home/u/.cache/ws"),
            "a read-only role must NOT get the workspace read-write"
        );
        assert!(a.iter().any(|x| x == "--property=BindReadOnlyPaths=/home/u/.claude"));
        // Folded a012: the CLI binary is bound so the wrapped CLI execs.
        assert!(a
            .iter()
            .any(|x| x == "--property=BindReadOnlyPaths=/home/u/.local/bin/claude"));
        // The allowlist does NOT expose home read-write.
        assert!(!a.iter().any(|x| x == "--property=ReadWritePaths=/home/u"));
    }

    #[test]
    fn systemd_allowlist_os_hide_off_admits_other_store() {
        // os_hide off → the opencode store is admitted read-only.
        let a = osv(&systemd_run_argv(&allow_plan(false, false), &inner()));
        assert!(a
            .iter()
            .any(|x| x.starts_with("--property=BindReadOnlyPaths=") && x.contains("opencode")));
        // os_hide on → it is absent.
        let b = osv(&systemd_run_argv(&allow_plan(false, true), &inner()));
        assert!(!b.iter().any(|x| x.contains("opencode")));
    }

    // sandbox-binds-control-socket: extra_ro_paths (the daemon control socket)
    // are bound read-only in every mechanism, after the masking steps, so the
    // per-execution MCP relay can connect() even to a /tmp- or home-resident
    // socket.
    const SOCK: &str = "/tmp/1000-runtime/autocoder/control.sock";

    fn deny_plan_with_socket() -> SandboxPlan {
        let mut p = deny_plan(true, sample_mask());
        p.extra_ro_paths.push(PathBuf::from(SOCK));
        p
    }
    fn allow_plan_with_socket() -> SandboxPlan {
        let mut p = allow_plan(false, true);
        p.extra_ro_paths.push(PathBuf::from(SOCK));
        p
    }

    #[test]
    fn systemd_binds_control_socket_under_both_policies() {
        for plan in [deny_plan_with_socket(), allow_plan_with_socket()] {
            let a = osv(&systemd_run_argv(&plan, &inner()));
            assert!(
                a.iter().any(|x| x == &format!("--property=BindReadOnlyPaths={SOCK}")),
                "systemd argv must bind the control socket: {a:?}"
            );
        }
    }

    #[test]
    fn bwrap_binds_control_socket_after_tmpfs_tmp() {
        for plan in [deny_plan_with_socket(), allow_plan_with_socket()] {
            let a = osv(&bwrap_argv(&plan, &inner()));
            let bind_at = a.windows(3).position(|w| w == ["--ro-bind-try", SOCK, SOCK]);
            assert!(bind_at.is_some(), "bwrap argv must ro-bind the control socket: {a:?}");
            // The bind must follow `--tmpfs /tmp` so a /tmp socket is re-exposed.
            let tmpfs_at = a
                .windows(2)
                .position(|w| w == ["--tmpfs", "/tmp"])
                .expect("tmpfs /tmp present");
            assert!(
                bind_at.unwrap() > tmpfs_at,
                "socket bind must come AFTER --tmpfs /tmp: {a:?}"
            );
        }
    }

    #[test]
    fn seatbelt_allowlist_allows_control_socket_after_home_deny() {
        let profile = seatbelt_profile(&allow_plan_with_socket());
        let allow = format!("(allow file-read* (literal \"{SOCK}\"))");
        assert!(
            profile.contains(&allow),
            "seatbelt must allow reading the socket: {profile}"
        );
        let deny_home = profile
            .find("(deny file-read* file-write* (subpath \"/home/u\"))")
            .expect("home deny present");
        let allow_sock = profile.find(&allow).unwrap();
        assert!(allow_sock > deny_home, "socket allow must follow the home deny");
    }

    #[test]
    fn no_extra_ro_paths_adds_no_control_socket_bind() {
        let s = osv(&systemd_run_argv(&deny_plan(true, sample_mask()), &inner()));
        assert!(!s.iter().any(|x| x.contains("control.sock")));
        let b = osv(&bwrap_argv(&allow_plan(false, true), &inner()));
        assert!(!b.iter().any(|x| x.contains("control.sock")));
        let p = seatbelt_profile(&deny_plan(true, sample_mask()));
        assert!(!p.contains("control.sock"));
    }

    // Generality regression: the control socket frequently resolves to a path
    // UNDER `$HOME` (the non-server, no-`$XDG_RUNTIME_DIR` deployment shape —
    // e.g. `~/.local/state/autocoder/runtime/control.sock`). Under the allowlist
    // policy `$HOME` is masked, so a masked-home agentic role (reviewer, audits)
    // can only reach the socket if the bind RE-EXPOSES it AFTER the home mask.
    // These assert that for each mechanism — i.e. the fix works for more than
    // the one setup whose socket happens to sit outside the masked tree.
    const HOME_SOCK: &str = "/home/u/.local/state/autocoder/runtime/control.sock";

    fn allow_plan_with_home_socket() -> SandboxPlan {
        let mut p = allow_plan(false, true);
        p.extra_ro_paths.push(PathBuf::from(HOME_SOCK));
        p
    }

    #[test]
    fn bwrap_allowlist_rebinds_home_resident_socket_after_home_tmpfs() {
        let a = osv(&bwrap_argv(&allow_plan_with_home_socket(), &inner()));
        let bind_at = a
            .windows(3)
            .position(|w| w == ["--ro-bind-try", HOME_SOCK, HOME_SOCK]);
        assert!(
            bind_at.is_some(),
            "bwrap must ro-bind a home-resident control socket: {a:?}"
        );
        let home_tmpfs_at = a
            .windows(2)
            .position(|w| w == ["--tmpfs", "/home/u"])
            .expect("the allowlist masks home with a tmpfs");
        assert!(
            bind_at.unwrap() > home_tmpfs_at,
            "socket bind must come AFTER the home tmpfs so it re-exposes the masked path: {a:?}"
        );
    }

    #[test]
    fn systemd_allowlist_binds_home_resident_socket_under_masked_home() {
        let a = osv(&systemd_run_argv(&allow_plan_with_home_socket(), &inner()));
        assert!(
            a.iter().any(|x| x == "--property=ProtectHome=tmpfs"),
            "the allowlist masks home: {a:?}"
        );
        assert!(
            a.iter()
                .any(|x| x == &format!("--property=BindReadOnlyPaths={HOME_SOCK}")),
            "systemd must bind the home-resident control socket even under the masked home: {a:?}"
        );
    }

    #[test]
    fn seatbelt_allowlist_allows_home_resident_socket_after_home_deny() {
        let profile = seatbelt_profile(&allow_plan_with_home_socket());
        let allow = format!("(allow file-read* (literal \"{HOME_SOCK}\"))");
        let deny_home = profile
            .find("(deny file-read* file-write* (subpath \"/home/u\"))")
            .expect("the allowlist denies the home subtree");
        let allow_sock = profile
            .find(&allow)
            .expect("the home-resident socket is re-allowed");
        assert!(
            allow_sock > deny_home,
            "socket allow must follow the home deny so it wins (last-match): {profile}"
        );
    }

    // a013: the bwrap executor denylist binds home read-write then masks each
    // entry (dir → tmpfs; file → ro-bind /dev/null).
    #[test]
    fn bwrap_denylist_binds_home_rw_and_masks_entries() {
        let a = osv(&bwrap_argv(&deny_plan(true, sample_mask()), &inner()));
        assert_eq!(a[0], "bwrap");
        assert!(
            a.windows(3).any(|w| w == ["--ro-bind", "/", "/"]),
            "whole root is bound read-only"
        );
        // Home EXPOSED read-write, NOT replaced by tmpfs.
        assert!(a
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/home/u" && w[2] == "/home/u"));
        assert!(
            !a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/home/u"),
            "the denylist must NOT tmpfs home"
        );
        // Directory mask → tmpfs; file mask → inaccessible /dev/null bind.
        assert!(a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/home/u/.ssh"));
        assert!(a
            .windows(3)
            .any(|w| w == ["--ro-bind", "/dev/null", "/home/u/.cargo/credentials.toml"]));
        // Workspace read-write for the executor; caps dropped; egress open.
        assert!(a
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/home/u/.cache/ws" && w[2] == "/home/u/.cache/ws"));
        for cap in DROPPED_CAPS {
            assert!(a.windows(2).any(|w| w[0] == "--cap-drop" && w[1] == cap));
        }
        assert!(!a.iter().any(|x| x == "--unshare-net"), "egress must stay open");
        assert!(a.iter().any(|x| x == "--die-with-parent"));
    }

    #[test]
    fn bwrap_allowlist_masks_home_binds_stores_and_cli_binary() {
        let a = osv(&bwrap_argv(&allow_plan(false, true), &inner()));
        // Home masked by tmpfs, then the allowlist re-bound read-only.
        assert!(a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == "/home/u"));
        assert!(a.windows(2).any(|w| w[0] == "--ro-bind-try" && w[1] == "/home/u/.claude"));
        // Folded a012: the CLI binary is bound.
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--ro-bind-try" && w[1] == "/home/u/.local/bin/claude"));
        // Workspace read-only.
        assert!(a
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/home/u/.cache/ws" && w[2] == "/home/u/.cache/ws"));
        assert!(
            !a.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/home/u/.cache/ws"),
            "a read-only role must NOT bind the workspace read-write"
        );
    }

    // task 4.1 / 8.7: no mechanism + no opt-in fails closed; opt-in proceeds.
    #[test]
    fn gate_fails_closed_without_mechanism_or_opt_in() {
        let err = decide_spawn(None, false).unwrap_err().to_string();
        assert!(err.contains("systemd-run") && err.contains("bwrap"));
        assert!(err.to_lowercase().contains("refus"));
    }

    // a74 task 1.1: the gate's pre-spawn refusal is classifiable as
    // precondition-unmet by its KIND (the typed `SandboxMechanismUnavailable`),
    // surviving an `anyhow` `.context(...)` wrapper — NOT by message substring.
    #[test]
    fn gate_refusal_is_classifiable_precondition_unmet_by_kind() {
        use anyhow::Context as _;
        let err = decide_spawn(None, false).unwrap_err();
        assert_eq!(
            precondition_unmet_message(&err).as_deref(),
            Some(SANDBOX_GATE_REFUSAL_MESSAGE)
        );
        // Through a context layer (as `agentic_run` adds) the kind still
        // classifies — the helper walks the whole chain.
        let wrapped =
            Err::<(), _>(err).context("OS-level sandbox mechanism gate").unwrap_err();
        assert_eq!(
            precondition_unmet_message(&wrapped).as_deref(),
            Some(SANDBOX_GATE_REFUSAL_MESSAGE)
        );
    }

    // a74 task 1.2: a substantive error (subprocess ran, then failed) is NOT
    // classified as precondition-unmet — even when its message happens to
    // mention the same words, classification keys off the error KIND.
    #[test]
    fn substantive_error_is_not_precondition_unmet() {
        let substantive = anyhow::anyhow!(
            "executor exited with status 1: systemd-run bwrap refusing nonsense"
        );
        assert!(precondition_unmet_message(&substantive).is_none());
    }

    #[test]
    fn gate_opt_in_proceeds_unsandboxed() {
        assert_eq!(decide_spawn(None, true).unwrap(), SpawnPlan::Unsandboxed);
    }

    #[test]
    fn gate_wraps_when_mechanism_available() {
        assert_eq!(
            decide_spawn(Some(SandboxMechanism::SystemdRun), false).unwrap(),
            SpawnPlan::Wrap(SandboxMechanism::SystemdRun)
        );
        assert_eq!(
            decide_spawn(Some(SandboxMechanism::Bwrap), true).unwrap(),
            SpawnPlan::Wrap(SandboxMechanism::Bwrap)
        );
    }

    #[test]
    fn unsandboxed_warning_only_when_no_mechanism_and_opt_in() {
        assert!(startup_unsandboxed_warning(None, true).is_some());
        assert!(startup_unsandboxed_warning(None, false).is_none());
        assert!(startup_unsandboxed_warning(Some(SandboxMechanism::Bwrap), true).is_none());
    }

    #[test]
    fn inner_command_extracts_program_args_env() {
        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("--settings").arg("/tmp/s.json");
        cmd.env("ANTHROPIC_MODEL", "claude-opus-4-8");
        let inner = InnerCommand::from_command(&cmd);
        assert_eq!(inner.program, OsString::from("claude"));
        assert_eq!(inner.args, vec![OsString::from("--settings"), OsString::from("/tmp/s.json")]);
        assert!(inner
            .env
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "claude-opus-4-8"));
    }

    // a006/a013 / task 8.4: under the default (os_hide on) the other CLI's
    // store is absent from a read-only role's allowlist AND present in the
    // executor's denylist mask; with os_hide off it is admitted read-only and
    // dropped from the mask, while engine_deny still denies it at the CLI layer.
    #[test]
    fn os_hide_controls_other_store_presence_in_allowlist() {
        // A temp HOME with BOTH a claude and an opencode store present.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/share/opencode")).unwrap();
        std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        let ws = home.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        // Read-only role (allowlist), os_hide ON.
        let run_on = RunSandbox::for_role(
            Some(SandboxMechanism::Bwrap),
            false,
            CliKind::Claude,
            false,
            crate::config::SandboxToggles::default(),
        );
        let plan_on = run_on.build_plan_with_home(&ws, home.path(), OsStr::new("claude"));
        match &plan_on.policy {
            FsPolicy::Allowlist {
                self_stores,
                extra_ro_stores,
                ..
            } => {
                assert!(self_stores.iter().any(|p| p.ends_with(".claude")));
                assert!(
                    extra_ro_stores.is_empty(),
                    "os_hide on: no other CLI store is admitted: {extra_ro_stores:?}"
                );
            }
            other => panic!("a read-only role must use the allowlist, got {other:?}"),
        }

        let off = crate::config::SandboxToggles {
            os_hide: false,
            ..Default::default()
        };
        // Read-only role with os_hide OFF → the other store is admitted.
        let run_off = RunSandbox::for_role(
            Some(SandboxMechanism::Bwrap),
            false,
            CliKind::Claude,
            false,
            off.clone(),
        );
        let plan_off = run_off.build_plan_with_home(&ws, home.path(), OsStr::new("claude"));
        match &plan_off.policy {
            FsPolicy::Allowlist { extra_ro_stores, .. } => assert!(
                extra_ro_stores
                    .iter()
                    .any(|p| p.to_string_lossy().contains("opencode")),
                "os_hide off: the other CLI store is admitted read-only: {extra_ro_stores:?}"
            ),
            other => panic!("expected allowlist, got {other:?}"),
        }
        // Executor denylist with os_hide OFF → the other store is NOT masked.
        let exec_off =
            RunSandbox::for_role(Some(SandboxMechanism::Bwrap), false, CliKind::Claude, true, off);
        let mask = exec_off.resolve_mask_list(home.path());
        assert!(
            !mask.iter().any(|e| e.path.to_string_lossy().contains("opencode")),
            "os_hide off removes the other CLI store from the executor mask: {mask:?}"
        );
        // engine_deny still covers every store (self + others) at the CLI layer.
        let deny = run_off.engine_deny_paths();
        assert!(deny.iter().any(|p| p.contains("/.claude/**")));
        assert!(deny.iter().any(|p| p.contains("opencode") && p.ends_with("/**")));
    }

    // a006: engine_deny off contributes no read-deny patterns.
    #[test]
    fn engine_deny_off_yields_no_patterns() {
        let toggles = crate::config::SandboxToggles {
            engine_deny: false,
            ..Default::default()
        };
        let run = RunSandbox::for_role(None, true, CliKind::Claude, true, toggles);
        assert!(run.engine_deny_paths().is_empty());
    }

    // a006: the unenforced default skips the OS layer (existing behavior).
    #[test]
    fn default_run_sandbox_is_unenforced() {
        let run = RunSandbox::default();
        assert!(!run.enforce);
        assert!(run.engine_deny_paths().is_empty() || !run.enforce);
    }

    // ----- a013 mask-list, CLI-binary, and macOS-profile unit tests -----

    #[test]
    fn default_mask_list_covers_credentials_and_persistence() {
        let home = Path::new("/home/u");
        let m = default_mask_list(home);
        for rel in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".netrc",
            ".cargo/credentials.toml",
            ".npmrc",
            ".pypirc",
            ".gem/credentials",
            // git credential-helper stores (`store` / `cookie`).
            ".git-credentials",
            ".config/git/credentials",
            ".gitcookies",
            ".bashrc",
            ".profile",
            ".ssh/authorized_keys",
        ] {
            assert!(m.contains(&home.join(rel)), "default mask must include {rel}");
        }
    }

    #[test]
    fn expand_home_resolves_tilde_and_dollar_home() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home("~/.ssh", home), home.join(".ssh"));
        assert_eq!(expand_home("$HOME/.aws", home), home.join(".aws"));
        assert_eq!(expand_home("~", home), home.to_path_buf());
        assert_eq!(expand_home("/etc/secret", home), PathBuf::from("/etc/secret"));
    }

    #[test]
    fn dedup_paths_drops_descendants_keeps_siblings() {
        let out = dedup_paths(vec![
            PathBuf::from("/a/b"),
            PathBuf::from("/a"),
            PathBuf::from("/a/b/c"),
            PathBuf::from("/ab"),
        ]);
        assert_eq!(out, vec![PathBuf::from("/a"), PathBuf::from("/ab")]);
    }

    #[test]
    fn removed_default_mask_entries_names_only_defaults() {
        let removed = vec![
            "~/.ssh".to_string(),
            "~/myproject".to_string(),
            "$HOME/.aws".to_string(),
        ];
        let named = removed_default_mask_entries(&removed);
        assert!(named.contains(&"~/.ssh".to_string()));
        assert!(named.contains(&"$HOME/.aws".to_string()));
        assert!(
            !named.iter().any(|s| s.contains("myproject")),
            "removing a NON-default path is not a relaxed posture: {named:?}"
        );
    }

    #[test]
    fn resolve_mask_list_honors_add_remove_and_os_hide() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".ssh")).unwrap();
        std::fs::create_dir_all(h.join(".cargo")).unwrap();
        std::fs::write(h.join(".cargo/credentials.toml"), "x").unwrap();
        // The OTHER CLI's store (opencode) — masked when os_hide is on.
        std::fs::create_dir_all(h.join(".local/share/opencode")).unwrap();
        std::fs::create_dir_all(h.join(".config/opencode")).unwrap();
        // An operator addition.
        std::fs::create_dir_all(h.join("mydir")).unwrap();

        let toggles = crate::config::SandboxToggles {
            mask_add: vec!["~/mydir".to_string()],
            mask_remove: vec!["~/.ssh".to_string()],
            ..Default::default()
        };
        let run =
            RunSandbox::for_role(Some(SandboxMechanism::Bwrap), false, CliKind::Claude, true, toggles);
        let mask = run.resolve_mask_list(h);

        // The removed default (.ssh) is exposed; the credentials FILE is still
        // masked inside the otherwise-exposed .cargo tree.
        assert!(
            !mask.iter().any(|e| e.path.ends_with(".ssh")),
            "a removed default mask entry must be exposed: {mask:?}"
        );
        assert!(mask
            .iter()
            .any(|e| e.path.ends_with(".cargo/credentials.toml") && !e.is_dir));
        // The operator addition is masked (a directory).
        assert!(mask.iter().any(|e| e.path.ends_with("mydir") && e.is_dir));
        // os_hide on → the other CLI store is in the mask.
        assert!(mask
            .iter()
            .any(|e| e.path.to_string_lossy().contains("opencode")));
    }

    #[test]
    fn cli_binary_binds_follows_symlink_into_install_tree() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".local/bin")).unwrap();
        std::fs::create_dir_all(h.join(".local/share/cli/bin")).unwrap();
        let target = h.join(".local/share/cli/bin/realcli");
        std::fs::write(&target, "#!/bin/sh\ntrue\n").unwrap();
        let link = h.join(".local/bin/cli");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let binds = cli_binary_binds(link.as_os_str(), h);
        // Canonicalize expected paths to handle platform symlinks (e.g., /var -> /private/var on macOS)
        let canonical_link = std::fs::canonicalize(&link).unwrap_or(link.clone());
        let canonical_package_dir = std::fs::canonicalize(h.join(".local/share/cli/bin"))
            .unwrap_or_else(|_| h.join(".local/share/cli/bin"));
        
        // The PATH location (the symlink under ~/.local/bin) is bound...
        assert!(
            binds.iter().any(|p| {
                let canonical_p = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                canonical_p == canonical_link
            }),
            "binds the PATH location: {binds:?}"
        );
        // ...and the real target's package directory (dependency closure).
        assert!(
            binds.iter().any(|p| {
                let canonical_p = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                canonical_p == canonical_package_dir
            }),
            "binds the install/package dir: {binds:?}"
        );
    }

    #[test]
    fn cli_binary_binds_skips_binary_outside_home() {
        // /bin/sh exists on the host, outside this test home → no home bind.
        let binds = cli_binary_binds(OsStr::new("/bin/sh"), Path::new("/home/nonexistent-u"));
        assert!(binds.is_empty(), "a binary outside home needs no bind: {binds:?}");
    }

    #[test]
    fn resolve_program_path_absolute_and_relative_used_as_is() {
        assert_eq!(
            resolve_program_path(OsStr::new("/usr/bin/claude")),
            Some(PathBuf::from("/usr/bin/claude"))
        );
        assert_eq!(
            resolve_program_path(OsStr::new("./claude")),
            Some(PathBuf::from("./claude"))
        );
    }

    #[test]
    fn seatbelt_profile_executor_is_allow_default_minus_mask() {
        let p = seatbelt_profile(&deny_plan(true, sample_mask()));
        assert!(p.contains("(allow default)"));
        assert!(p.contains("(deny file-read* file-write* (subpath \"/home/u/.ssh\"))"));
        assert!(p.contains("/home/u/.cargo/credentials.toml"));
        // Capability-drop analogs.
        assert!(p.contains("(deny process-info*)"));
        assert!(p.contains("(deny network-inbound)"));
    }

    #[test]
    fn seatbelt_profile_readonly_role_is_deny_default_allowlist() {
        let p = seatbelt_profile(&allow_plan(false, true));
        assert!(p.contains("(deny default)"));
        // Home masked; the self store + CLI binary re-exposed.
        assert!(p.contains("(deny file-read* file-write* (subpath \"/home/u\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/home/u/.claude\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/home/u/.local/bin/claude\"))"));
        // A read-only workspace → readable, NOT writable.
        assert!(p.contains("(allow file-read* (subpath \"/home/u/.cache/ws\"))"));
        assert!(!p.contains("(allow file-write* (subpath \"/home/u/.cache/ws\"))"));
    }

    #[test]
    fn seatbelt_profile_strict_executor_allows_workspace_write() {
        // Strict-mode executor = allowlist + a writable workspace.
        let plan = SandboxPlan {
            workspace: PathBuf::from("/home/u/.cache/ws"),
            workspace_writable: true,
            home: PathBuf::from("/home/u"),
            policy: FsPolicy::Allowlist {
                self_stores: vec![],
                extra_ro_stores: vec![],
                cli_binary_binds: vec![],
            },
            extra_ro_paths: Vec::new(),
        };
        let p = seatbelt_profile(&plan);
        assert!(p.contains("(deny default)"));
        assert!(p.contains("(allow file-write* (subpath \"/home/u/.cache/ws\"))"));
    }

    #[test]
    fn sandbox_exec_argv_uses_inline_profile_then_inner() {
        let a = osv(&sandbox_exec_argv(&deny_plan(true, sample_mask()), &inner()));
        assert_eq!(a[0], "sandbox-exec");
        assert_eq!(a[1], "-p");
        assert!(a[2].contains("(allow default)"));
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dd + 1], "claude");
    }

    #[test]
    fn build_plan_selects_policy_by_role_and_strict_mode() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join(".claude")).unwrap();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // Executor default → denylist.
        let exec = RunSandbox::for_role(
            Some(SandboxMechanism::Bwrap),
            false,
            CliKind::Claude,
            true,
            crate::config::SandboxToggles::default(),
        );
        assert!(exec.uses_denylist());
        assert!(matches!(
            exec.build_plan_with_home(&ws, h, OsStr::new("claude")).policy,
            FsPolicy::Denylist { .. }
        ));

        // Read-only role → allowlist.
        let ro = RunSandbox::for_role(
            Some(SandboxMechanism::Bwrap),
            false,
            CliKind::Claude,
            false,
            crate::config::SandboxToggles::default(),
        );
        assert!(!ro.uses_denylist());
        assert!(matches!(
            ro.build_plan_with_home(&ws, h, OsStr::new("claude")).policy,
            FsPolicy::Allowlist { .. }
        ));

        // Executor strict → allowlist (home masked), workspace still writable.
        let strict_toggles = crate::config::SandboxToggles {
            strict_mode: true,
            ..Default::default()
        };
        let strict = RunSandbox::for_role(
            Some(SandboxMechanism::Bwrap),
            false,
            CliKind::Claude,
            true,
            strict_toggles,
        );
        assert!(!strict.uses_denylist());
        let plan = strict.build_plan_with_home(&ws, h, OsStr::new("claude"));
        assert!(matches!(plan.policy, FsPolicy::Allowlist { .. }));
        assert!(
            plan.workspace_writable,
            "a strict-mode executor keeps a writable workspace"
        );
    }

    // ----- Gated enforcement integration tests (tasks 5.1–5.3, 5.5) -----
    // These exercise REAL kernel enforcement and so run only where a mechanism
    // is usable; elsewhere (e.g. unprivileged CI) they skip so `cargo test`
    // stays green. The test "home" is created UNDER the real `$HOME` (never
    // `/tmp`, which `bwrap` replaces with a tmpfs) so the policy binds apply.

    fn run_wrapped(plan: &SandboxPlan, program: &str, args: &[&str]) -> std::process::Output {
        let mech = detect_mechanism().expect("caller checked a mechanism is available");
        let inner = InnerCommand {
            program: OsString::from(program),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
        };
        let argv = match mech {
            SandboxMechanism::SystemdRun => systemd_run_argv(plan, &inner),
            SandboxMechanism::Bwrap => bwrap_argv(plan, &inner),
            SandboxMechanism::SandboxExec => sandbox_exec_argv(plan, &inner),
        };
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .expect("wrapped command spawns")
    }

    /// A throwaway sandbox "home" under the crate directory — NOT `/tmp` (which
    /// `bwrap` replaces with a tmpfs) and NOT the live `$HOME` (which a
    /// concurrent test may mutate, breaking the policy binds). The base is the
    /// compile-time `CARGO_MANIFEST_DIR`, so it is immune to runtime env
    /// mutation. Cleaned on drop.
    fn test_home() -> tempfile::TempDir {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).ok();
        tempfile::Builder::new()
            .prefix(".autocoder-sbx-")
            .tempdir_in(&base)
            .expect("create a test home under the crate target dir")
    }

    fn out_text(o: &std::process::Output) -> String {
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    // task 5.1: under the executor denylist, the toolchain (`~/.cargo`) is
    // readable AND a tool cache is writable, while `~/.ssh` AND
    // `~/.cargo/credentials.toml` are masked (read fails) — the credential file
    // even though it sits inside the otherwise-exposed `~/.cargo`.
    #[test]
    fn enforced_executor_reads_toolchain_masks_credentials() {
        if detect_mechanism().is_none() {
            eprintln!("skipping 5.1: no sandbox mechanism");
            return;
        }
        let home = test_home();
        let h = home.path();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(h.join(".cargo")).unwrap();
        std::fs::write(h.join(".cargo/config.toml"), "toolchain").unwrap();
        std::fs::write(h.join(".cargo/credentials.toml"), "CARGO_TOKEN").unwrap();
        std::fs::create_dir_all(h.join(".ssh")).unwrap();
        std::fs::write(h.join(".ssh/id_ed25519"), "SSH_SECRET").unwrap();

        let run = RunSandbox::for_role(
            detect_mechanism(),
            false,
            CliKind::Claude,
            true,
            crate::config::SandboxToggles::default(),
        );
        let plan = run.build_plan_with_home(&ws, h, OsStr::new("true"));

        let cfg = run_wrapped(&plan, "cat", &[h.join(".cargo/config.toml").to_str().unwrap()]);
        assert!(
            cfg.status.success() && out_text(&cfg).contains("toolchain"),
            "the exposed toolchain must be readable: {:?}",
            String::from_utf8_lossy(&cfg.stderr)
        );
        let cache = run_wrapped(
            &plan,
            "sh",
            &["-c", &format!("echo X > {}", h.join(".cargo/cache").display())],
        );
        assert!(cache.status.success(), "a tool cache write under exposed home must succeed");

        let ssh = run_wrapped(&plan, "cat", &[h.join(".ssh/id_ed25519").to_str().unwrap()]);
        assert!(
            !ssh.status.success() && !out_text(&ssh).contains("SSH_SECRET"),
            "~/.ssh must be masked"
        );
        let creds = run_wrapped(&plan, "cat", &[h.join(".cargo/credentials.toml").to_str().unwrap()]);
        assert!(
            !creds.status.success() && !out_text(&creds).contains("CARGO_TOKEN"),
            "~/.cargo/credentials.toml must be masked even inside the exposed ~/.cargo"
        );
    }

    // task 5.2: a write to a masked persistence file (`~/.bashrc`) does not
    // persist to the real file.
    #[test]
    fn enforced_masked_persistence_write_does_not_persist() {
        if detect_mechanism().is_none() {
            eprintln!("skipping 5.2: no sandbox mechanism");
            return;
        }
        let home = test_home();
        let h = home.path();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(h.join(".bashrc"), "ORIGINAL\n").unwrap();

        let run = RunSandbox::for_role(
            detect_mechanism(),
            false,
            CliKind::Claude,
            true,
            crate::config::SandboxToggles::default(),
        );
        let plan = run.build_plan_with_home(&ws, h, OsStr::new("true"));
        let _ = run_wrapped(
            &plan,
            "sh",
            &["-c", &format!("echo HACKED >> {}", h.join(".bashrc").display())],
        );
        let after = std::fs::read_to_string(h.join(".bashrc")).unwrap();
        assert!(
            !after.contains("HACKED"),
            "a write to masked ~/.bashrc must not persist to the real file: {after:?}"
        );
    }

    // task 5.3: a read-only role runs under the home-masked allowlist with its
    // CLI binary bound (resolved from ~/.local/bin, symlinks followed); a
    // workspace write fails AND the masked home is unreadable.
    #[test]
    fn enforced_readonly_role_binds_cli_binary_blocks_workspace_write() {
        if detect_mechanism().is_none() {
            eprintln!("skipping 5.3: no sandbox mechanism");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let home = test_home();
        let h = home.path();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(h.join(".local/bin")).unwrap();
        std::fs::create_dir_all(h.join(".local/share/cli/bin")).unwrap();
        let target = h.join(".local/share/cli/bin/realcli");
        std::fs::write(&target, "#!/bin/sh\necho CLI_RAN\n").unwrap();
        let mut perm = std::fs::metadata(&target).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&target, perm).unwrap();
        let link = h.join(".local/bin/cli");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::write(h.join("secret"), "SECRET").unwrap();

        let run = RunSandbox::for_role(
            detect_mechanism(),
            false,
            CliKind::Claude,
            false,
            crate::config::SandboxToggles::default(),
        );
        let plan = run.build_plan_with_home(&ws, h, link.as_os_str());

        // The bound CLI binary (symlink under ~/.local/bin, target followed)
        // execs under the masked-home allowlist.
        let exec = run_wrapped(&plan, link.to_str().unwrap(), &[]);
        assert!(
            exec.status.success() && out_text(&exec).contains("CLI_RAN"),
            "the bound CLI binary must exec under the masked-home allowlist: {:?}",
            String::from_utf8_lossy(&exec.stderr)
        );
        // Home is masked: the secret is unreadable.
        let sec = run_wrapped(&plan, "cat", &[h.join("secret").to_str().unwrap()]);
        assert!(!sec.status.success(), "a masked-home secret must be unreadable");
        // The workspace is read-only.
        let w = run_wrapped(
            &plan,
            "sh",
            &["-c", &format!("echo Y > {}", ws.join("out").display())],
        );
        assert!(!w.status.success(), "a read-only role must not write the workspace");
    }

    // task 5.5: strict mode masks ALL of home for the executor (even the
    // toolchain), while keeping the workspace writable.
    #[test]
    fn enforced_strict_mode_masks_all_home_for_executor() {
        if detect_mechanism().is_none() {
            eprintln!("skipping 5.5: no sandbox mechanism");
            return;
        }
        let home = test_home();
        let h = home.path();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(h.join(".cargo")).unwrap();
        std::fs::write(h.join(".cargo/config.toml"), "toolchain").unwrap();

        let toggles = crate::config::SandboxToggles {
            strict_mode: true,
            ..Default::default()
        };
        let run = RunSandbox::for_role(detect_mechanism(), false, CliKind::Claude, true, toggles);
        let plan = run.build_plan_with_home(&ws, h, OsStr::new("true"));

        // Strict = allowlist: even the toolchain under home is masked.
        let cargo = run_wrapped(&plan, "cat", &[h.join(".cargo/config.toml").to_str().unwrap()]);
        assert!(
            !cargo.status.success(),
            "strict mode masks all of home (toolchain included)"
        );
        // But the executor's workspace stays writable.
        let w = run_wrapped(
            &plan,
            "sh",
            &["-c", &format!("echo Z > {}", ws.join("out").display())],
        );
        assert!(w.status.success(), "a strict-mode executor keeps a writable workspace");
    }

    // task 8.3 (carried): a capability-gated operation (raw/packet socket open)
    // fails inside the sandbox because CAP_NET_RAW is not in the bounding set.
    #[test]
    fn enforced_raw_socket_open_fails() {
        if detect_mechanism().is_none() {
            eprintln!("skipping enforced_raw_socket_open_fails: no sandbox mechanism");
            return;
        }
        if !which("python3") {
            eprintln!("skipping enforced_raw_socket_open_fails: python3 absent");
            return;
        }
        let home = test_home();
        let h = home.path();
        let ws = h.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let run = RunSandbox::for_role(
            detect_mechanism(),
            false,
            CliKind::Claude,
            true,
            crate::config::SandboxToggles::default(),
        );
        let plan = run.build_plan_with_home(&ws, h, OsStr::new("python3"));
        // Exit 0 only if a raw packet socket opened (which the dropped
        // CAP_NET_RAW must prevent).
        let prog = "import socket,sys\n\
                    try:\n  s=socket.socket(socket.AF_PACKET, socket.SOCK_RAW)\n  s.close()\n  sys.exit(0)\n\
                    except Exception:\n  sys.exit(3)\n";
        let out = run_wrapped(&plan, "python3", &["-c", prog]);
        assert!(
            !out.status.success(),
            "opening a raw packet socket must fail with CAP_NET_RAW dropped"
        );
    }
}
