# Mono live activity and motion

The live transcript previously rendered its mascot with `still: true`, while the composer status used generic text and a large persistent idle card.

Changes:
- Live transcript mascot animates only while active; completed signatures remain still.
- Shared-clock waveforms make running tools and waiting workers visible, with different cadences.
- Status card displays the current tool action, tinted border/background, and individual clickable worker rows (up to four).
- Completed tool calls transition to “Reviewing results” rather than continuing to say “Running command”.
- Ready state is a compact line. Project configuration details expand on demand; multiple-project errors remain visible.
- Reduced motion renders static indicators. Loops use the existing visibility-aware shared pulse, with no separate timers or idle waveform leases.

Validation:
- Release build, clippy with warnings denied, formatting and diff checks passed.
- Full GUI suite: 733 passed, 11 ignored; one outdated expectation for a completed tool label failed. Updated that expectation to Reviewing results; targeted rerun passed.
- Latest activity tests: 3 passed.
- Installed and signed Dev only; backed up its bundle and databases. Main binary hash unchanged.
- Live MonoGhost sleep-20 check: inspected two animation frames, observed Running command → Reviewing results → Ready. Compact idle layout confirmed visually.
- Worker-row rendering and reduced-motion live settings were not manually exercised in this pass; reduced-motion uses the existing shared clock contract.


## Follow-up: simpler motion and MCP footer

At the user's request, removed all waveform/equalizer bars. Kept mascot animation, live action labels and worker rows. Removed the project configuration line from above the composer.

Inspected the installed Tauri app's MCP chip: it displays server connection states in a small panel. Added MCP beside API in the native footer with a scrollable status list and Manage MCP servers action. It reads the active conversation's latest provider report and explicitly labels it as a startup snapshot; it does not perform the Tauri app's on-demand connection probe. No credentials or raw configuration are displayed. Existing startup tool details retain configuration-source evidence.

Validation: clippy clean; three existing usage-footer tests passed. Verified actual MonoGhost server statuses in Dev. Main app unchanged.
