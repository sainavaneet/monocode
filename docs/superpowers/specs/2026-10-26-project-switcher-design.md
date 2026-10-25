# Project switcher design

A keyboard-driven popup that switches the active project, built into the
native GPUI app's existing Quick Open popup.

## Goal

Press one chord, type a few letters of a project name, press Enter, and the
workspace switches to that project. Same feel as the Cmd+P file search.

## Behaviour

- **Chord.** Cmd+Shift+P on macOS, Ctrl+Shift+P on Linux and Windows. The
  action is named `SwitchProject`, appears in Settings → Keybindings as
  "App: Switch Project", and can be rebound through the existing keymap
  override path.
- **Command palette.** It gives up Cmd+Shift+P. Its new default is
  Cmd+Option+P (Ctrl+Alt+P elsewhere), which is unused today, so it keeps a
  default chord and stays rebindable. It also remains reachable by typing `>`
  in the Cmd+P popup, and the Settings label "App: Command Palette" is unchanged.
- **Opening.** The chord opens the Quick Open popup in project mode: the search
  field contains `@`, the placeholder reads "Switch project…". Typing `@` as the
  first character in the Cmd+P popup enters the same mode, matching the `>`
  convention for commands. Deleting the `@` returns to file search.
- **Rows.** Open projects in rail order: pinned, then grouped, then the rest.
  Remote projects are included with their rail label. Archived projects are
  excluded. Each row shows the project's mark (logo or mascot, as the rail
  draws it), the project label, and the path as the detail line. The active
  project shows an "Active" hint on the right.
- **Ranking.** The query after `@` is scored with the same fuzzy scorer as the
  command list, first against the label and then against the path; the better
  score wins. An empty query keeps rail order.
- **Selection.** The initial highlight is the first row that is not the active
  project, so chord then Enter always switches. Up and Down cycle, Enter or
  click picks, Escape closes and restores the previous focus. Picking calls the
  shell's existing `select_project`, so tabs, the sidebar slide and session
  restore behave exactly as clicking the rail.
- **Discoverability.** The `>` command list gets a "Switch project" entry with
  the chord hint.

## Code changes

All in `crates/monocode-gpui/src`.

- `quick_open.rs`
  - Replace the `palette: bool` constructor flag with `Mode { Files, Commands,
    Projects }`. Mode is derived from the query prefix on every change, so the
    constructor only seeds the text.
  - Add `ProjectRow { path, label, active, mark }` and a `projects: Vec<ProjectRow>`
    field passed in by the shell. `mark` is a boxed element builder so the view
    has no shell dependency.
  - Add `Picked::Project(String)`.
  - Add `Command::SwitchProject` to `COMMANDS`.
  - Project mode skips the file listing work when opened directly; the file
    list still loads lazily if the user deletes the `@`.
- `shell/quick_open.rs`
  - `open_quick_open` takes the `Mode`. The shell builds `ProjectRow`s from the
    prepared rail order using `project_label` and `project_mark`.
  - Map `Picked::Project` to `select_project`, and `Command::SwitchProject` to
    the new action.
- `shell.rs`
  - Add `SwitchProject` to the `workspace` actions, bind `cmd-shift-p`
    (`ctrl-shift-p` elsewhere) in the Workspace context, and dispatch it to
    `open_quick_open(Mode::Projects)`.
  - Rebind `OpenCommandPalette` to `cmd-alt-p` (`ctrl-alt-p` elsewhere).
- `settings/keymap.rs`
  - Map `workspace::SwitchProject` to "App: Switch Project".

## Error handling

Project mode has no I/O, so the only failure is an empty project list, which
renders the existing "No results" row. Opening the popup never blocks on the
file listing.

## Tests

- `quick_open.rs` unit tests: project rows rank by label then path; the initial
  highlight skips the active project; Enter emits `Picked::Project`; `@` and
  `>` prefixes switch modes in both directions.
- Shell tests, next to the existing Quick Open test: the action opens the popup
  in project mode; picking a row changes `workspace.project`; Escape restores
  focus to the composer and keeps its draft.
- Keymap test: the new label resolves, an override chord round-trips, and the
  palette's moved default still maps to "App: Command Palette".

## Out of scope

Recency ordering, archived projects, worktree rows, and a Tauri port. Each can
be a follow-up.
