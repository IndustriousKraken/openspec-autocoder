## ADDED Requirements

### Requirement: Timeout-bounded remote fetch drains output concurrently
The git workflow manager SHALL drain a timeout-bounded `git fetch` child's stdout AND stderr concurrently with waiting for the process to exit, so that the amount of output the fetch may produce is bounded only by available memory, NOT by the operating system's pipe buffer. A fetch whose combined output exceeds the pipe buffer SHALL complete normally and surface its real outcome; it SHALL NOT be misreported as a timeout caused by an unread pipe. The genuine-timeout behavior is unchanged: when the child does not exit within the configured window, the manager SHALL kill AND reap the child AND return a timeout error.

#### Scenario: Fetch producing more than the pipe buffer of output completes
- **WHEN** `fetch_remote_with_timeout` runs a `git fetch` whose combined stdout + stderr exceeds the OS pipe buffer (e.g. an upstream with thousands of new refs or tags on a first fetch)
- **THEN** the child process writes all of its output without blocking because the manager drains both pipes while the child runs
- **AND** the function returns the fetch's real outcome — `Ok` on a zero exit, or `Err` carrying the captured stderr on a non-zero exit
- **AND** the function does NOT return a timeout error so long as the child exits within the configured window

#### Scenario: Genuine timeout still kills the child and reports timeout
- **WHEN** the child `git fetch` does not exit within the configured timeout window (e.g. an unreachable network host)
- **THEN** the manager kills the child process AND reaps it with a follow-up wait
- **AND** the function returns an `Err` whose message names the timeout (`git fetch <remote> timed out after <timeout_secs>s`)
- **AND** no pipe-reader thread is left running after the function returns
