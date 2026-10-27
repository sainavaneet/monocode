//! Git in a check: reading only. Read-only subcommands (and the listing
//! forms of `branch`, `tag`, `remote`, `stash` and `worktree`) run;
//! commit-making ones only with commit authority; everything else — config,
//! ref moves, remotes, fetch and push, submodules, unknown names (which may
//! be aliases that run programs) — is refused, as are global options that
//! point Git elsewhere or override its configuration.

/// Subcommands that only read the repository.
const READ_ONLY: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "grep",
    "describe",
    "merge-base",
    "cat-file",
    "rev-list",
    "name-rev",
    "blame",
    "shortlog",
    "show-ref",
    "for-each-ref",
    "check-ignore",
];
/// Subcommands that make commits: allowed only with commit authority.
const COMMITS: &[&str] = &["commit", "merge", "rebase", "cherry-pick", "revert", "am"];
/// Global options that change nothing outside the command's own output.
const GLOBAL_ALLOWED: &[&str] = &[
    "--no-pager",
    "-P",
    "--no-optional-locks",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--no-replace-objects",
];
/// `branch` options that only list.
const BRANCH_LIST: &[&str] = &[
    "--list",
    "-l",
    "-a",
    "--all",
    "-r",
    "--remotes",
    "--show-current",
    "-v",
    "-vv",
    "--verbose",
    "--color",
    "--no-color",
    "--column",
    "--no-column",
    "-i",
    "--ignore-case",
];
/// `tag` options that only list.
const TAG_LIST: &[&str] = &[
    "--list",
    "-l",
    "-i",
    "--ignore-case",
    "--column",
    "--no-column",
    "--color",
];
/// Listing options of `branch` and `tag` whose value is the next argument.
const LIST_VALUED: &[&str] = &[
    "--contains",
    "--no-contains",
    "--merged",
    "--no-merged",
    "--points-at",
    "--sort",
    "--format",
];

/// Git's global options and subcommand, checked.
pub(super) fn git(args: &[String], may_commit: bool) -> Result<(), String> {
    if let Some(bang) = args.iter().find(|a| a.starts_with('!')) {
        return Err(format!(
            "`git {bang}` runs a program through Git; a check may not."
        ));
    }
    let mut rest = args.iter();
    let subcommand = loop {
        let Some(arg) = rest.next() else {
            return Ok(());
        };
        if !arg.starts_with('-') {
            break arg.as_str();
        }
        if !GLOBAL_ALLOWED.contains(&arg.as_str()) {
            return Err(format!(
                "`git {arg}` points Git at another repository or overrides its configuration; checks use the workspace's own."
            ));
        }
    };
    let rest: Vec<&str> = rest.map(String::as_str).collect();
    if READ_ONLY.contains(&subcommand) {
        return reads(subcommand, &rest);
    }
    if COMMITS.contains(&subcommand) {
        return if may_commit {
            Ok(())
        } else {
            Err(format!(
                "`git {subcommand}` makes commits, and the task has no commit authority."
            ))
        };
    }
    let listing = match subcommand {
        "branch" => lists(&rest, BRANCH_LIST, false),
        "tag" => lists(&rest, TAG_LIST, true),
        "remote" => rest.iter().all(|a| matches!(*a, "-v" | "--verbose")),
        "stash" => matches!(rest.first(), Some(&"list")),
        "worktree" => matches!(rest.first(), Some(&"list")),
        _ => false,
    };
    if listing {
        return Ok(());
    }
    Err(format!(
        "`git {subcommand}` is not a read-only Git command; a check may only read the repository (no config, ref, remote, fetch or push changes, and no aliases)."
    ))
}

/// A read-only subcommand, refused only for options that open a pager
/// program or a diff driver of the caller's choosing.
fn reads(subcommand: &str, rest: &[&str]) -> Result<(), String> {
    let runs = rest.iter().find(|a| {
        a.starts_with("--open-files-in-pager") || (subcommand == "grep" && a.starts_with("-O"))
    });
    match runs {
        Some(arg) => Err(format!(
            "`git {subcommand} {arg}` runs a program; a check may not."
        )),
        None => Ok(()),
    }
}

/// Whether `branch`/`tag` arguments only list: every option is a listing
/// one, and names appear only as values of listing options or as patterns
/// after `--list`/`-l` (or anywhere for `tag`, which needs `--list`/`-l`
/// whenever it has arguments).
fn lists(rest: &[&str], allowed: &[&str], needs_list_flag: bool) -> bool {
    let list_mode = rest.iter().any(|a| matches!(*a, "--list" | "-l"));
    if needs_list_flag && !rest.is_empty() && !list_mode {
        return false;
    }
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let flag = arg.split_once('=').map_or(*arg, |(flag, _)| flag);
        if LIST_VALUED.contains(&flag) {
            if !arg.contains('=') {
                iter.next();
            }
            continue;
        }
        if arg.starts_with('-') {
            if !allowed.contains(&flag) {
                return false;
            }
            continue;
        }
        if !list_mode {
            return false;
        }
    }
    true
}
