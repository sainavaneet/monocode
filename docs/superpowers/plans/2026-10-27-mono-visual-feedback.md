# Mono visual feedback — Dev preview

User request: improve Mono's visualization and animations, fix visible bugs, and install into the Dev app for review.

## This iteration

1. Give Mono a persistent activity strip above the composer with plain-language status, worker title/model, and an action to open the worker.
2. Derive waiting from pending completion subscriptions. Remove completed/dismissed subscriptions, scope to the owning chat, and distinguish receiving results from running work.
3. Surface permissions, questions, authentication problems, blocked tasks, and paused tasks instead of presenting them as idle. Keep the same status vocabulary in title bars and floating views.
4. Reduce working mascot motion, give waiting a gentle breathing movement, honor static avatars and system reduced motion, and keep the attention indicator inside the avatar bounds.
5. Test status priority and completion/dismissal behavior; run GUI tests, formatting and clippy; build a release, back up Dev, install and verify Dev only.

## Review checklist

- Send work to a worker: the strip names the work and model, with an Open worker action.
- Worker finishes: receiving-results state lasts until Mono receives the report; old completed/cancelled work does not leave Mono waiting.
- Permission or question: show Needs input rather than Idle.
- Authentication failure: show Account needs attention, without exposing credentials.
- Native tasks: show blocked/paused/supervising state alongside existing task controls.
- Reduced motion: status remains readable without animation; static sidebar avatars stay still.

## Scope

This is a focused first iteration, not a claim that every Mono bug is fixed. Preserve the main app and database. No changes to provider routing or authentication policy. Retain the prior tested chat/task wiring already in the working tree.

## Delivered

Installed and relaunched `/Applications/Monocode Native Dev.app`. Verified the activity strip visually at narrow and wide conversation widths; verified the final accessibility tree exposes “Mono status: Ready. Send a message to start or continue work.” Main app binary checksum remained unchanged. Dev bundle and both Dev databases were backed up before installation.

Validation: full GUI suite 732 passed, 11 ignored; clippy with warnings denied passed; formatting and diff checks passed; optimized release built and its installed signature verified. The final accessibility label was verified in the running app after the test run. Waiting/completion and dismissed/other-chat handoff behavior are covered by regression tests; no new live provider task was launched for this review.

Dev bundle backup: `/Applications/Monocode Native Dev (before visual update 20261027-151901).app`.
Dev data backup: `~/Library/Application Support/Monocode Backups/dev-visual-20261027-152120`.
