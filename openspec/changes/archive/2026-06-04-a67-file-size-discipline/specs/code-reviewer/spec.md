# code-reviewer — delta for a67-file-size-discipline

## ADDED Requirements

### Requirement: Reviewer flags files and functions that breach the size brightline
The code-reviewer SHALL add an advisory, non-blocking size observation to its `ReviewReport.markdown` when the pass under review pushes a changed file or function past a size threshold, OR grows one that is already past it. For each file in the `ReviewContext`'s changed-files set, the reviewer SHALL determine, from the file's contents AND the unified diff, (a) whether the file — or a function within it — exceeds the file-size OR function-size threshold, AND (b) whether this pass added net lines to that file or function. When BOTH hold, the report SHALL note the path, the resulting line count, AND — for whole-file findings where test-only regions are identifiable — the production/test split. The thresholds are the project's configured file-size AND function-size thresholds (the same values the `architecture-brightline` audit applies; defaults file `800`, function `200`).

A changed file or function that exceeds a threshold but to which the pass adds NO net lines (it is left the same size or made smaller) SHALL NOT be flagged, so a pass that reduces oversized code is not penalized AND pre-existing bloat the pass does not enlarge is not re-litigated on every unrelated PR.

Size is a maintainability signal, NOT a correctness defect: a size observation SHALL NOT, on its own, set the verdict to `Block`. The verdict continues to reflect the code-quality criteria of the `AI-driven code-quality review` requirement; the size observation is appended to the markdown regardless of verdict.

#### Scenario: A pass that pushes a file over the threshold is flagged
- **WHEN** the pass adds net lines to a changed file such that its resulting line count exceeds the file-size threshold
- **THEN** `ReviewReport.markdown` includes an advisory observation naming the file AND its resulting line count
- **AND** the observation does not, on its own, force the verdict to `Block`

#### Scenario: A pass that shrinks an oversized file is not flagged for size
- **WHEN** a changed file already exceeds the file-size threshold AND the pass leaves it the same size or smaller
- **THEN** the reviewer adds no size observation for that file

#### Scenario: A pass that grows a function past the function threshold is flagged
- **WHEN** the pass adds net lines to a function such that its resulting span exceeds the function-size threshold
- **THEN** the report includes an advisory observation naming the function AND its resulting line count
- **AND** the verdict is not forced to `Block` by the size observation alone

#### Scenario: A whole-file size observation reports the production/test split
- **WHEN** the pass pushes a file past the file-size threshold AND the file contains identifiable test-only regions
- **THEN** the advisory observation reports the production-line / test-line breakdown alongside the total resulting line count
