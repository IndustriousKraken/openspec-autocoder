---
changelog: skip
---

## Why

`autocoder/src/control_socket.rs::dispatch_request` (lines 207-224) has
three early-return error shapes:

- Line 211: `{"ok": false, "error": "malformed JSON: ..."}` — when the
  incoming line cannot be parsed as JSON.
- Line 217: `{"ok": false, "error": "malformed request: missing
  \`action\` field"}` — when the JSON parses but has no `action` field.
- Line 222: `{"ok": false, "error": "unknown action: ..."}` — when the
  action string is not recognized.

Existing tests cover only the third path
(`unknown_action_returns_error`). The first two are reachable in
production by any operator typo (e.g. piping `{ "actionn": "reload"
` instead of `{"action":"reload"}` into `nc -U`) but have no test.
A regression that, say, conflated the missing-action and
malformed-JSON branches into a single panic would slip through CI.

## What Changes

Add two tests under `autocoder/src/control_socket.rs`'s existing
`tests` module that connect to the same fixture listener already used
by the other reload tests and send:

- A line whose body is not valid JSON.
- A line whose JSON parses but lacks the `action` key (e.g. `{}` or
  `{"unrelated":"x"}`).

Each test asserts the response is `{"ok": false, "error": <expected
substring>}`.

Direct unit tests on `dispatch_request` are also acceptable (the
function is `pub async` and takes a `&ControlState`); preferring the
end-to-end socket form keeps the new tests stylistically consistent
with the existing reload tests and exercises the JSON-line framing
path on the listener side too.

No production code changes.

## Impact

- Affected code: `autocoder/src/control_socket.rs`
  (`#[cfg(test)] mod tests`).
- No spec changes — the malformed-request envelope is an internal
  protocol shape; no capability requirement currently spells it out
  literally.
- Breaking: no.
