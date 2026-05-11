## MODIFIED Requirements

### Requirement: Rewind subcommand
The orchestrator SHALL provide a `rewind` subcommand that recovers from a failed PR or bad implementation by unarchiving specified changes and resetting the relevant agent branch. **The subcommand SHALL accept a `--repo <selector>` argument; the argument is required when the config contains multiple repositories AND optional (defaulting to the only configured repo) when the config contains exactly one.**

#### Scenario: Multi-repo rewind requires --repo
- **WHEN** the loaded config contains 2 or more repositories AND the user invokes `orchestrator rewind <change> --config <path>` without `--repo`
- **THEN** the process exits non-zero within 5 seconds
- **AND** stderr names the missing argument AND lists the configured repositories' short names as candidate selectors

#### Scenario: Single-repo rewind defaults to the only repo
- **WHEN** the loaded config contains exactly one repository AND `--repo` is omitted
- **THEN** the process operates against that repository's workspace without prompting for the selector
- **AND** a log line at start of execution names the repository being rewound

#### Scenario: Selector resolution by URL or short-name
- **WHEN** `--repo <selector>` is provided
- **THEN** the orchestrator attempts to match the selector against each configured repository's full URL (exact string equality) AND against a derived short-name (the URL's basename with any `.git` suffix removed)
- **AND** if exactly one repository matches, the rewind proceeds against that repo
- **AND** if zero repositories match, the process exits non-zero with stderr naming the unmatched selector and listing the available short-names
- **AND** if two or more repositories match (ambiguous selector), the process exits non-zero with stderr naming all matching repository URLs

#### Scenario: Soft rewind requires confirmation
- **WHEN** the user invokes rewind WITHOUT `--hard` (after selector resolution)
- **THEN** the process prints to stderr the line `This will delete branch '<agent_branch>' (local) and unarchive <N> change(s) (<comma-separated names>). Proceed? [y/N]`
- **AND** reads one line from stdin
- **AND** if the trimmed input is not exactly `y` or `Y`, the process logs `rewind cancelled` and exits with status 0 without modifying any branch or archive state

#### Scenario: Hard rewind deletes the agent branch locally and remotely
- **WHEN** the user invokes rewind WITH `--hard`
- **THEN** the process skips the confirmation prompt
- **AND** runs `git branch -D <agent_branch>` against the resolved repository's workspace
- **AND** runs `git push origin --delete <agent_branch>` against the resolved repository's workspace
- **AND** if remote deletion fails because the remote branch does not exist, the failure is logged at debug level and rewind proceeds; other remote-deletion failures (auth, network) are logged at error level but do NOT halt unarchive

#### Scenario: Unarchive of multiple changes
- **WHEN** the user passes two or more change names to rewind
- **THEN** the process attempts unarchive for each in command-line order
- **AND** if any individual unarchive fails (no matching archive entry, destination collision), the process continues with the remaining changes
- **AND** at the end, if any unarchive failed, the process exits non-zero with stderr listing the failed changes and their reasons; otherwise it exits 0 with a summary log line naming all rewound changes
