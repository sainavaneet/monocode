//! What a check may reach: its workspace only. Every argument that is a
//! path must stay inside the workspace, and Git may neither reach a remote
//! or another repository nor (without commit authority) make commits.
//! Pushing is never part of a check.
use std::path::{Component, Path};

/// Git subcommands that write a remote or another repository's
/// configuration.
const GIT_REFUSED: &[&str] = &["push", "remote", "send-pack", "send-email", "svn", "p4"];
/// Git subcommands that make commits or move refs to them.
const GIT_COMMITS: &[&str] = &[
    "commit",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "am",
    "pull",
    "tag",
    "commit-tree",
    "update-ref",
    "notes",
    "stash",
];
/// Git options that point it at another repository or override its
/// configuration (which can run programs).
const GIT_GLOBAL_REFUSED: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
    "--exec-path",
    "--super-prefix",
];

/// Refuses `argv` when it would reach outside the workspace `cwd`, or commit
/// without `may_commit`.
pub(super) fn confine(argv: &[String], cwd: &Path, may_commit: bool) -> Result<(), String> {
    let root = cwd.canonicalize().map_err(|e| e.to_string())?;
    for arg in &argv[1..] {
        inside(arg, &root)?;
    }
    if program_name(&argv[0]) == "git" {
        git(&argv[1..], may_commit)?;
    }
    Ok(())
}

fn program_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// An argument (or the value of `--flag=value`) that is a path must name
/// something inside `root`.
fn inside(arg: &str, root: &Path) -> Result<(), String> {
    let value = match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with('-') => value,
        _ => arg,
    };
    if value.is_empty() || value.contains("://") {
        return Ok(());
    }
    let outside = || {
        Err(format!(
            "`{arg}` is outside the workspace; checks stay inside it."
        ))
    };
    let path = Path::new(value);
    if path.is_absolute() {
        return outside();
    }
    let looks_like_path = value.contains('/') || value.contains('\\') || value == "..";
    let joined = root.join(path);
    if let Ok(real) = joined.canonicalize() {
        return if real.starts_with(root) {
            Ok(())
        } else {
            outside()
        };
    }
    if looks_like_path && !lexically_inside(path) {
        return outside();
    }
    Ok(())
}

/// Whether a relative path that does not exist stays below its base.
fn lexically_inside(path: &Path) -> bool {
    let mut depth: usize = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Git's global options and subcommand, checked.
fn git(args: &[String], may_commit: bool) -> Result<(), String> {
    let mut rest = args.iter();
    let subcommand = loop {
        let Some(arg) = rest.next() else {
            return Ok(());
        };
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        let attached = GIT_GLOBAL_REFUSED
            .iter()
            .any(|refused| refused.len() == 2 && arg.starts_with(refused) && arg.len() > 2);
        if GIT_GLOBAL_REFUSED.contains(&name) || attached {
            return Err(format!(
                "`git {arg}` points Git at another repository or overrides its configuration; checks use the workspace's own."
            ));
        }
        if !arg.starts_with('-') {
            break arg.as_str();
        }
    };
    if GIT_REFUSED.contains(&subcommand) {
        return Err(format!(
            "`git {subcommand}` reaches a remote or another repository; a check may not push or change remotes."
        ));
    }
    if subcommand == "fetch" && rest.any(|arg| !arg.starts_with('-') && arg.contains(':')) {
        return Err("`git fetch` with a refspec writes refs; a check may not.".into());
    }
    if GIT_COMMITS.contains(&subcommand) && !may_commit {
        return Err(format!(
            "`git {subcommand}` makes commits, and the task has no commit authority."
        ));
    }
    Ok(())
}
