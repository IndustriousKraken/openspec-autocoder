## ADDED Requirements

### Requirement: Post a one-way notification
The chatops-manager SHALL expose a `post_notification(channel, text)`
method distinct from `post_question`. The method SHALL post the given
text to the channel without returning a thread/reply handle. The
method's contract is one-way: there is no expectation that callers
will track or read replies to a notification.

#### Scenario: Notification posts to Slack with no return handle
- **WHEN** `post_notification(channel, text)` is called against
  `SlackBackend`
- **THEN** the backend issues an HTTP POST to
  `https://slack.com/api/chat.postMessage` with the text in the
  `text` JSON field
- **AND** the method returns `Ok(())` on success (no thread handle
  is exposed to the caller)
- **AND** on a 2xx response with `ok: false`, the method returns an
  error whose text contains the Slack `error` field verbatim
- **AND** on a non-2xx response, the method returns an error whose
  text contains the HTTP status

#### Scenario: Notification posting is independent of question state
- **WHEN** the manager has an in-flight `post_question` thread for a
  given channel AND `post_notification(channel, text)` is called
- **THEN** the notification posts as a new top-level message in the
  same channel, NOT as a threaded reply to the in-flight question
- **AND** the notification's emission does NOT affect any
  `poll_thread_for_human_reply` poll in progress
