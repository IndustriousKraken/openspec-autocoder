# OpenSpec CI/CD Orchestrator

The OpenSpec CI/CD Orchestrator is an autonomous server daemon designed to read OpenSpec implementation proposals, execute an AI implementation agent (like Gemini or Claude) via a CLI, and create Pull Requests. This effectively creates an "Autonomous AI Software Factory" using OpenSpec as the standardized work order system.

## Architecture

The Orchestrator operates on a "Serial Queue" model per repository to manage dependent features safely and avoid Git merge conflicts.

1. **Queue Engine**: Polling mechanism that monitors the `openspec/changes/` directory for ready changes and reads a YAML configuration for multiple repositories.
2. **Workspace Manager**: Clones or pulls target repositories into `/tmp/workspaces/<repo-name>` to isolate agent execution.
3. **Agent Subprocess Runner**: Executes the AI agent CLI (e.g. `gemini-cli /opsx:apply <change-name>`) as a blocking subprocess.
4. **Git Workflow**: Automates `git checkout`, creating a new agent branch (`agent-q` or custom), committing the AI's changes, and opening a monolithic PR at the end of the polling cycle.
5. **Reviewer Integration**: An automated post-commit review step invoking a second agent (like Grok or MiMo) to assess code quality before human review.
6. **Recovery System**: A robust `rewind` command that cleans up corrupted branches and unarchives changes back into the active queue when a failure occurs or a PR is rejected.

---

## Configuration

The Orchestrator accepts a `config.yaml` file specifying the repositories to watch, their polling intervals, and custom branch names.

Copy the example file to get started:
```bash
cp config.example.yaml config.yaml
```

**Example `config.yaml`:**
```yaml
repositories:
  - url: "git@github.com:my-org/project-alpha.git"
    poll_interval_sec: 3600       # Poll every hour
    base_branch: "dev"            # Branch to branch off of
    agent_branch: "agent-q"       # Branch the agent will commit to
```

---

## CLI Usage

The orchestrator provides a simple CLI with two main commands:

### `run`
Starts the asynchronous polling daemon. It will infinitely poll all configured repositories.

```bash
# Run with default config.yaml
cargo run -- run

# Run with a custom config file
cargo run -- run --config /path/to/my-config.yaml
```

### `rewind`
A recovery command to use when an agent fails or a PR is rejected. It resets the workspace and unarchives changes so the agent can try again.

```bash
# Safely rewind a specific change (with a y/N prompt for branch deletion)
cargo run -- rewind my-broken-change --config config.yaml

# Hard rewind multiple changes (bypasses branch deletion prompt)
cargo run -- rewind change-A change-B --hard
```

---

## Deployment Guide

For long-term production use, it is highly recommended to run the Orchestrator as a persistent systemd service on a dedicated Linux server rather than running it locally or via Cron (since the daemon handles its own polling loop internally).

### 1. Build the Release Binary
```bash
cargo build --release
sudo cp target/release/orchestrator /usr/local/bin/openspec-orchestrator
```

### 2. Set up Systemd Service
Create a new file at `/etc/systemd/system/openspec-orchestrator.service`:

```ini
[Unit]
Description=OpenSpec Autonomous CI/CD Orchestrator
After=network.target

[Service]
Type=simple
User=orchestrator-user
# Ensure this directory contains your config.yaml
WorkingDirectory=/opt/openspec-orchestrator
# Pass environment variables for your AI Agents
Environment="GEMINI_API_KEY=your_key_here"
Environment="ANTHROPIC_API_KEY=your_key_here"
# The git client will use the user's ~/.ssh keys for repo access
ExecStart=/usr/local/bin/openspec-orchestrator run --config config.yaml
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
```

### 3. Start and Enable
```bash
sudo systemctl daemon-reload
sudo systemctl enable openspec-orchestrator
sudo systemctl start openspec-orchestrator
```

### Updating the Daemon
To update the orchestrator, pull the latest code, rebuild the release binary, copy it to `/usr/local/bin`, and restart the service: `sudo systemctl restart openspec-orchestrator`.

---

## AI Security & Guardrails

Running autonomous agents with push access to your repositories introduces unique security challenges. Please adhere to the following best practices:

### 1. Credential Scoping
Never give the orchestrator a Personal Access Token (PAT) or SSH key with admin access to your organization. Provide it with **scoped, write-only access** strictly limited to the repositories defined in `config.yaml`.

### 2. Branch Protection
Protect your `main` and `dev` branches. The orchestrator should **never** be allowed to push directly to protected branches. It must be constrained to pushing to its designated `agent_branch` and opening Pull Requests for human review.

### 3. The "Self-Modifying AI" Risk
If you configure the Orchestrator to watch its own repository (e.g., `cicd-impl-agents`), you introduce the risk of the AI modifying its own source code. 
*   **The Danger:** If an agent gets stuck in a loop of failing tests, a "lazy" LLM might attempt to solve the problem by deleting the tests, modifying the OpenSpec schema, or altering its own system prompts within the orchestrator codebase.
*   **The Mitigation:** Always require Human + Reviewer Agent approval for PRs merged into the orchestrator's own repository. Never configure the orchestrator to automatically merge its own PRs without human intervention.

### 4. Workspace Isolation
The orchestrator clones repositories into `/tmp/workspaces/`. Ensure this partition has sufficient disk space and is regularly cleared of orphaned locks (`.in-progress`) upon system restarts. Do not run the orchestrator with root privileges.

---
*Note: This documentation is maintained per the `project-documentation` OpenSpec rule. Any new capabilities or operational shifts must be updated here.*