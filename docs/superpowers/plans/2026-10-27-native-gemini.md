# Native Gemini support

The Tauri app in `/Users/navaneet/Developer/monocode` implements Gemini via the installed Gemini CLI's ACP interface. Native previously had neither a Gemini provider identity nor a Gemini API-key setting.

## Implementation

- Register Gemini in native provider discovery, pickers, and session routing. Launch `gemini --acp` in the conversation project directory.
- Use the existing ACP streaming, tools, approval, cancellation, and session loading transport. Map read-only/planning to `plan`, full access to `yolo`, supervised to `default`, and automatic edits to `autoEdit`. Bound initialization and configuration waits.
- Discover models from the CLI's live catalog, including account-specific and preview identifiers. No invented model list or model inference is involved in discovery.
- Add Gemini's icon and Settings → Providers API-key controls. Keys retain the existing private file storage and environment-only child-process delivery.
- API billing uses an app-owned Gemini profile under `provider-accounts/gemini/api-key`. The existing Google-login profile is untouched. The profile is deliberately separate: personal Gemini global extensions, trust, and global registrations are not imported. Project settings still follow Gemini's native configuration and trust rules. Existing system settings remain authoritative.
- Supply the stored key through both Gemini/Google key variable names so an inherited or project-loaded alternate key cannot win. Do not switch to Vertex or Google subscription credentials implicitly.
- Catalog discovery follows the API toggle and invalidates its cache when billing changes or a key is saved.

## Validation

- Core suite: 861 passed, 2 ignored.
- Gemini transport-mode and API-profile tests passed.
- Native settings layout tests: 6 passed.
- Real installed Gemini CLI, isolated test profile, no inference: returned Auto, Gemini 3.1 Pro Preview (customtools native ID), Gemini 3 Flash Preview, Gemini 2.5 Pro, Gemini 3.8 Flash, and Gemini 3.5 Flash Lite.
- Repeated API catalog probe with an existing `oauth-personal` profile: succeeded, original settings byte-for-byte unchanged. Fixture: `/var/folders/18/q1bxsjls7qg8c7w274fthfbr0000gn/T/mono-gemini-final-7kppa44l`.
- This is catalog/transport verification with a dummy key, not proof of successful paid inference. A valid user-supplied Gemini API key is required for chat.

Install scope: Monocode Native Dev only. Preserve main application, user credentials, project MCP registrations, and unrelated working changes.

## Installed result

Final release build and all-target native Clippy passed; formatting and diff checks passed. The final API-key suite passed all 5 tests. Installed in `/Applications/Monocode Native Dev.app`, preserving its Dev identity/data mode. Verified the signed executable's text section against the release build and checked the bundle signature. Main executable and protected knowledge-graph MCP configuration hashes remained unchanged.

On-screen verification confirmed a Gemini provider tab in Models, Gemini CLI details/default-provider controls, and the Gemini API-key field, Save button, and Use Gemini API key switch in Settings → Providers. Left Dev on that settings page for the user to enter their key. No real API key was entered and no paid Gemini chat was submitted.

## Saved-key and Argon follow-up

At the user's explicit request, loaded the existing Gemini CLI key through its credential-storage API and transferred it directly to Dev's private Gemini key file without displaying it. Enabled Gemini API billing in Dev. The original credential remains unchanged.

Explicitly include `gemini-4-argon` in the Gemini catalog when CLI presets omit it, retaining an existing CLI entry and the default selection. Live Gemini ACP test with the saved key accepted that exact ID and returned `GEMINI_ARGON_OK` without tool execution.

The first Dev test revealed that Full access is rejected for a folder untrusted in the isolated API profile. Native now reports this specific cause instead of only “Internal error.” A failed startup also leaves a provider session identifier that Gemini has not saved; native now creates a new session only for Gemini's explicit invalid-session error, preserving failures for other causes. Regression tests cover model insertion/deduplication and that recovery behavior. The test chat uses Supervised access; folder trust was not bypassed.

Final live Dev verification: session `56c8b227-ba2e-4b1c-949d-a6e015fcc47a`, persisted model `gemini:gemini-4-argon`, Supervised access, API billing on. The app returned `GEMINI_ARGON_OK` in about five seconds; verified both the persisted transcript and the visible Dev window. The explicit Argon picker entry is installed and selected. Final Gemini regressions: 4 passed; all-target native Clippy, release build, and signature verification passed.
