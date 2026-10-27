//! What a check may reach, best effort: its workspace only. Paths a check
//! names must stay inside the workspace; shells, wrappers that run another
//! program (or code given inline) are refused, and so is a runner such as
//! `uv run` whose wrapped program would be; Git may only read the
//! repository, never commit or push (`program`, `git`).
//!
//! This is not a sandbox: a program the project itself runs (`npm run`,
//! `make`, `cargo run`, `./gradlew`) can do anything its scripts do.
use std::path::{Component, Path, PathBuf};

#[path = "confine_git.rs"]
mod git;
#[path = "confine_program.rs"]
mod program;

/// Options whose value is a path, in every program a check might run.
const PATH_FLAGS: &[&str] = &[
    "-C",
    "-o",
    "-f",
    "-d",
    "--manifest-path",
    "--output",
    "--out-dir",
    "--target-dir",
    "--git-dir",
    "--work-tree",
    "--file",
    "--dir",
    "--directory",
    "--config",
    "--cwd",
    "--prefix",
    "--rootdir",
    "--basetemp",
];
/// Short path options also written with the value attached (`-o/tmp/x`).
const SHORT_PATH_FLAGS: &[&str] = &["-C", "-o", "-f", "-d"];

/// Refuses `argv` when it would reach outside the workspace `cwd`, run a
/// shell, wrapper or inline code, or use Git for more than reading.
pub(super) fn confine(argv: &[String], cwd: &Path) -> Result<(), String> {
    let root = cwd.canonicalize().map_err(|e| e.to_string())?;
    program::check(argv)?;
    paths(&argv[1..], &root)
}

/// Every path a check names must stay inside `root`: the value of a path
/// option strictly, any other argument when it is (or would be created
/// beside) something outside, or climbs out with `..`.
fn paths(args: &[String], root: &Path) -> Result<(), String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if PATH_FLAGS.contains(&arg.as_str()) {
            if let Some(value) = iter.next_if(|value| !value.starts_with('-')) {
                assigned_value(value, root, true)?;
            }
            continue;
        }
        if let Some((flag, value)) = arg.split_once('=').filter(|(f, _)| f.starts_with('-')) {
            assigned_value(value, root, PATH_FLAGS.contains(&flag))?;
            continue;
        }
        let attached = SHORT_PATH_FLAGS
            .iter()
            .find(|flag| arg.len() > 2 && arg.starts_with(**flag));
        if let Some(flag) = attached {
            assigned_value(&arg[flag.len()..], root, true)?;
            continue;
        }
        if !arg.starts_with('-') {
            assigned_value(arg, root, false)?;
        }
    }
    Ok(())
}

/// Checks `value` and, for a `name=value` form (`of=/tmp/x`,
/// `PREFIX=/usr/local`, `cache_dir=/tmp/x`), the part after the first `=`.
fn assigned_value(value: &str, root: &Path, strict: bool) -> Result<(), String> {
    path_value(value, root, strict)?;
    match value.split_once('=') {
        Some((_, assigned)) => path_value(assigned, root, strict),
        None => Ok(()),
    }
}

/// Checks one value. `strict`: it is in a path position, so an absolute
/// path anywhere outside is refused; otherwise an absolute value counts as
/// a path only when it, or the directory it would be created in, exists
/// (so patterns such as `/health` or `// SAFETY` pass).
fn path_value(value: &str, root: &Path, strict: bool) -> Result<(), String> {
    if value.is_empty() || value.contains("://") {
        return Ok(());
    }
    let outside = || {
        Err(format!(
            "`{value}` is outside the workspace; checks stay inside it."
        ))
    };
    let path = Path::new(value);
    if !path.is_absolute() && !lexically_inside(path) {
        return outside();
    }
    let Some(existing) = nearest_existing(&root.join(path)) else {
        return outside();
    };
    if existing.starts_with(root) {
        return Ok(());
    }
    let is_pattern = path.is_absolute() && !strict && existing.parent().is_none();
    if is_pattern {
        Ok(())
    } else {
        outside()
    }
}

/// The canonical form of `path`, or of its nearest ancestor that exists.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|p| p.canonicalize().ok())
}

/// Whether a relative path stays below its base, `..` by `..`.
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
