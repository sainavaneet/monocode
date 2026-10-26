# Account switch keeps the conversation

Switching a conversation's provider account in the native app must keep the
same conversation, the way the React/Tauri app does.

## Root cause

`Shell::select_provider_account` (`shell/accounts.rs`) has three branches:

1. Usage limit active and idle: `Conversation::switch_usage_account` carries
   the provider thread to the other account and continues in place.
2. No messages: `set_blank_account` repoints the blank conversation.
3. Anything else: a **new conversation** is created on the other account with a
   copy of the settings, and the workspace switches to it.

Branch 3 is the bug. A conversation with history and no usage limit always lands
there. The composer's account menu has the same gap: it calls
`switch_usage_account`, which returns early when no limit is active.

The transfer itself already exists (`monocode_core::accounts::Profile::transfer`
copies Claude transcripts and Codex rollouts between account homes), so the fix
is routing, not new persistence.

## Reference behaviour (React app, `prepareProviderAccountSwitch`)

- Turn running: refuse with "Wait for the current turn to finish before
  switching accounts."
- No messages: switch immediately.
- History present: stop the warm CLI, copy the provider transcript to the other
  account, rebind the same session to that account, and post "Switched to
  <account>. This conversation will resume with its history on your next
  message." No message is sent automatically.
- Copy failed: the React app opens a new session. The native app already has a
  better fallback for the limit flow, a handoff summary inside the same
  conversation, so the native fix uses that instead. The same conversation is
  kept in every case.

## Design

- Add `Conversation::switch_account(profile)`: the carry-over flow from
  `switch_usage_account` without the limit requirement and without
  `resume_usage`. It transfers the thread in the background, forgets the warm
  CLI for the old account, sets the account, refreshes the catalog, saves, and
  sets the status line to the "Switched to…" notice. When the transfer fails it
  resets the provider id and installs the pending provider switch plus handoff
  brief, with a status saying the next message continues from a summary.
- `switch_usage_account` becomes a thin wrapper: it records the limit, runs the
  shared carry-over, then calls `resume_usage` as today.
- `Shell::select_provider_account`: branch 3 becomes `switch_account` when the
  conversation is idle, and a status notice without switching when it is busy.
  Blank and limited branches are unchanged. No new conversation is ever created.
- Composer account menu (`usage-account:` choice): call `switch_usage_account`
  when a limit is active, otherwise `switch_account`.
- Monos keep their existing rule: Codex Mono threads are not transferable, so
  they take the handoff path.

## Tests

- Shell: switching an idle conversation with history and no limit keeps the
  session, account, tabs and provider id, copies the transcript, leaves no usage
  limit and launches no prompt. Missing native history keeps the session with a
  handoff. A busy conversation refuses with the status message. The existing
  "threads stay pinned" test flips to expect the same session.
- Conversation: the composer account menu switches without a limit.
- Existing limit tests stay green.
