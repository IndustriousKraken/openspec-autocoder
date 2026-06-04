# Changelog

All notable changes to autocoder are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [v1.1.1] - 2026-05-24

First tagged release. autocoder is an autonomous, multi-repository daemon that
works through an OpenSpec change queue: it drives a swappable AI executor to
implement each queued change, opens a reviewed pull request per polling pass,
and escalates to a human over chat when it needs a decision.

### Highlights

- **Autonomous multi-repo orchestrator** — a Rust daemon polls every configured repository, drives an AI executor through the OpenSpec change queue, and opens a pull request per pass, with no human in the loop.
- **Fork-based PRs with automated code review** — autocoder pushes to a fork and opens the PR upstream (no upstream write access required), and every PR gets an AI code-quality review before a human merges.
- **ChatOps across Slack, Discord, Teams, Mattermost, and Matrix** — the daemon escalates agent questions to chat, survives restarts mid-conversation, announces opened PRs, and accepts operator recovery commands from the channel.
- **Periodic repository audits** — scheduled drift, missing-tests, security/bug, architecture, and spec-sync audits surface findings and can file new OpenSpec changes.
- **One-command install and pre-built binaries** — `autocoder install` runs a setup wizard, and a GitHub Actions pipeline publishes tagged release binaries.

### Added

**Core orchestrator**

- Add the orchestrator daemon and its `orchestrator-cli`, `openspec-queue-engine`, `executor`, and `git-workflow-manager` capabilities: a backend-agnostic executor implements each queued change while the daemon owns queue, git, ChatOps, and recovery.
- Run multiple repositories concurrently from a single config and one daemon instance.
- Add the `rewind` subcommand (with a `--repo` selector) to roll back an agent branch and recover a repository.
- Add a `spec-needs-revision` executor outcome so the agent can flag tasks it cannot perform from its sandbox instead of failing or faking completion.

**Pull requests and code review**

- Add fork-and-PR mode so autocoder operates without push access to upstream repositories.
- Add an automated AI code-quality review step on each agent branch before human merge.
- Include the implementer agent's own summary in the PR body.
- Cap how many changes are bundled into a single PR (`max_changes_per_pr`) so reviews stay manageable.

**ChatOps**

- Add asynchronous ChatOps escalation: agent questions are routed to a human, conversation state is persisted to disk and resumed when an answer arrives, and other changes keep processing in the meantime.
- Add experimental Discord, Microsoft Teams, Mattermost, and Matrix providers alongside Slack.
- Add a ChatOps notification when a PR is opened.
- Add operator recovery commands that run directly from the chat channel.

**Periodic audits**

- Add a periodic-audit framework that runs repository-wide audits on a cadence, reports findings to ChatOps, and can author new OpenSpec changes.
- Add drift, missing-tests, security/bug, architecture-consultative, and archived-spec-sync audits.

**Configuration and operations**

- Add per-owner GitHub token routing so one daemon can manage repos across multiple personal and org accounts.
- Allow secrets to be written inline in `config.yaml` instead of only through environment variables.
- Add a daemon control socket that hot-reloads tokens, reviewer credentials, and ChatOps config without interrupting in-flight runs.
- Extend hot-reload to the `repositories` list, so repos can be added, removed, or retuned without a restart.

**Install and releases**

- Add the `autocoder install` subcommand with a first-run setup wizard.
- Add a GitHub Actions release pipeline that tags releases and publishes pre-built binaries.
- Extend the install wizard to configure audits.

**Recovery and self-healing**

- Add perma-stuck detection that stops re-running a change which repeatedly fails the same way and alerts the operator, naming both the marker file to clear and the run-log path to inspect.
- Self-heal changes whose implementation is already in `HEAD` by archiving them instead of re-running them forever.
- Add an option to recreate the fork from scratch on workspace re-initialization.
- Add a path to rebuild canonical specs from the change archive, repairing pre-existing spec drift.

### Changed

- Rename the project to **autocoder** and refresh operator-facing naming and CLI ergonomics.
- Archive each change with `openspec archive` so canonical specs in `openspec/specs/` stay in sync (replacing the prior in-process file rename).
- Halt the queue walk on the first non-archived outcome (failed or escalated) instead of continuing to later changes.
- Stagger and jitter per-repository polling so simultaneous fetches don't trip intrusion-detection systems.
- Write more informative PR titles and bodies.

### Fixed

- Treat a "completed" outcome that left the workspace unmodified as a failure, instead of archiving an unimplemented change.
- Skip re-implementing changes that already have an open PR, instead of thrashing the PR branch and erroring on duplicate-PR creation.
- Commit the final change's archive in each pass; previously the last archive of a pass was never committed or pushed.
- Detect archive-destination collisions and broaden perma-stuck handling so a colliding change no longer loops through repeated executor runs.
- Recover automatically from a workspace left dirty by a failed or timed-out run, instead of stalling until manual cleanup.
- Raise a ChatOps alert when the workspace is dirty mid-iteration instead of looping silently.
- Fetch the fork's agent branch at workspace init so `git push --force-with-lease` stops misfiring with "stale info".
- Track the spawned agent's own process group so orphan cleanup can terminate stuck agent subprocess trees.

### Also included

- Expand `config.example.yaml` to document every configurable field.
