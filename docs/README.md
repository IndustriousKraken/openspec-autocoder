# Documentation

This directory holds the long-form documentation for autocoder. The main [README](../README.md) covers what it is, how to install it, and [what you can do with it](../README.md#what-you-can-do-with-it). The files below are the reference material an operator consults occasionally.

## Getting started

- [INSTALL.md](INSTALL.md) — Manual install from source. The `autocoder install` wizard handles most cases; use this for contributor setups, air-gapped builds, or installs that need to inspect the build itself.
- [CONFIG.md](CONFIG.md) — Full `config.yaml` schema. Every field, every default. Includes the multi-token routing for operators running across more than one GitHub owner.
- [DEPLOYMENT.md](DEPLOYMENT.md) — Recommended binary deploy with systemd: user setup, SSH keys, the unit file, env-var layout, manual upgrades, unattended `update.sh` via cron with atomic rollback, applying config changes without a restart, the `--reconfigure <section>` flag for editing one config section post-install.

## Feature surfaces

- [CHATOPS.md](CHATOPS.md) — Chat-driven workflows (`propose`, `send it`, `audit`, `revise`), operator recovery verbs, `AskUser` escalation, progress notifications, Slack Socket Mode inbound listener, experimental non-Slack backends.
- [CODE-REVIEW.md](CODE-REVIEW.md) — The optional AI code-reviewer: scope, verdict semantics, prompt template, reviewer-initiated revisions, PR composition.

## Operating the daemon

- [OPERATIONS.md](OPERATIONS.md) — Operating notes: workspace paths, polling cadence + firewall considerations, queue order, busy markers, perma-stuck recovery, spec-needs-revision recovery, self-heal, periodic audits + on-demand triggers, on-demand audit triggers, rebuilding canonical specs, runtime config reload, dirty-workspace auto-recovery, revising an open PR via comment.
- [CLI.md](CLI.md) — `run`, `reload`, `rewind`, `audit run`, `sync-specs` subcommand reference.
- [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — Diagnosing common failure modes: rebuild failures, revision-loop misfires, audit timeouts, partial-clone artifacts, post-reboot audit storms, `send it` and `propose` polite refusals.

## Reference

- [SECURITY.md](SECURITY.md) — AI security & guardrails: credential scoping, branch protection, the self-modifying-AI risk, workspace isolation, secrets in config (inline vs env-var), dedicated user, fork-and-PR workflow, executor tool sandbox.
- [STATE-LAYOUT.md](STATE-LAYOUT.md) — The four daemon data directories (`state`, `cache`, `logs`, `runtime`), resolution precedence, and the legacy-`/tmp` migration on first startup.

## Internals

- [foundation-smoke-test.md](foundation-smoke-test.md) — Optional manual end-to-end procedure against throwaway GitHub repos. The in-tree `cargo test` suite is the primary coverage; this is operator confidence-building before pointing the daemon at a repo you care about.
- [test-reliability.md](test-reliability.md) — Living reference of known test-suite flakes, root causes, and dispositions. Updated by implementing agents under the `project-documentation` spec.

## Contributing

- [OpenSpec conventions](https://github.com/Fission-AI/OpenSpec/tree/main/docs) — Upstream spec-format reference. This project follows OpenSpec for change management. The `concepts.md` and `getting-started.md` pages cover scenario syntax (`GIVEN`/`WHEN`/`THEN`), delta blocks (`ADDED`/`MODIFIED`/`REMOVED`/`RENAMED`), and requirement-header rules. Consult these when drafting a new `openspec/changes/<slug>/` proposal.
