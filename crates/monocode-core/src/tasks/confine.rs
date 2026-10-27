//! What a check may reach, best effort: its workspace only. Paths a check
//! names must stay inside the workspace; shells, wrappers that run another
//! program and interpreters given inline code are refused; Git may only
//! read the repository (and commit with commit authority, never push).
//!
//! This is not a sandbox: a program the project itself runs (`npm run`,
//! `make`, `cargo run`, `./gradlew`) can do anything its scripts do.
use std::path::{Component, Path, PathBuf};

#[path = "confine_git.rs"]
mod git;

/// Programs that run a shell or another program given as arguments.
const WRAPPERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "env",
    "xargs",
    "nohup",
    "time",
    "sudo",
    "doas",
    "su",
    "nice",
    "ionice",
    "timeout",
    "script",
    "expect",
    "npx",
    "bunx",
    "uvx",
    "command",
    "exec",
    "busybox",
    "stdbuf",
    "setsid",
    "chroot",
    "osascript",
];
/// Package runners whose subcommand downloads or runs an arbitrary program.
const RUNNER_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("pnpm", &["dlx"]),
    ("yarn", &["dlx"]),
    ("npm", &["exec", "x"]),
    ("bun", &["x"]),
    ("pipx", &["run"]),
];
/// `find` actions that run programs or write files.
const FIND_ACTIONS: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls",
];
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

/// An interpreter and the options that hand it code on the command line.
struct Interpreter {
    /// Whether a program name is this interpreter.
    is: fn(&str) -> bool,
    /// Short options (letters) that take inline code or load a module.
    short: &'static str,
    /// Short options (letters) that take no value and may precede them.
    plain: &'static str,
    /// Short options (letters) whose value is the next argument.
    valued: &'static str,
    long: &'static [&'static str],
    subcommands: &'static [&'static str],
}

const INTERPRETERS: &[Interpreter] = &[
    Interpreter {
        is: |name| name.starts_with("python") || name.starts_with("pypy"),
        short: "c",
        plain: "bBdEhiIOqsSuvVx",
        valued: "WX",
        long: &[],
        subcommands: &[],
    },
    Interpreter {
        is: |name| matches!(name, "node" | "nodejs" | "bun"),
        short: "epr",
        plain: "ci",
        valued: "",
        long: &["--eval", "--print", "--require", "--import", "--loader"],
        subcommands: &[],
    },
    Interpreter {
        is: |name| name == "deno",
        short: "",
        plain: "",
        valued: "",
        long: &["--eval"],
        subcommands: &["eval", "repl"],
    },
    Interpreter {
        is: |name| name == "ruby",
        short: "er",
        plain: "acdlnpsvwyW",
        valued: "CEIF",
        long: &["--require"],
        subcommands: &[],
    },
    Interpreter {
        is: |name| name == "perl",
        short: "eEmM",
        plain: "acdlnpsStTuUvwWX",
        valued: "I",
        long: &[],
        subcommands: &[],
    },
    Interpreter {
        is: |name| name == "php",
        short: "rBRE",
        plain: "aHnqv",
        valued: "cdz",
        long: &[
            "--run",
            "--process-begin",
            "--process-code",
            "--process-end",
        ],
        subcommands: &[],
    },
];

/// Refuses `argv` when it would reach outside the workspace `cwd`, run a
/// shell, wrapper or inline code, or use Git for more than reading
/// (committing only with `may_commit`).
pub(super) fn confine(argv: &[String], cwd: &Path, may_commit: bool) -> Result<(), String> {
    let root = cwd.canonicalize().map_err(|e| e.to_string())?;
    let name = program_name(&argv[0]);
    let args = &argv[1..];
    program(name, args)?;
    paths(args, &root)?;
    if name == "git" {
        git::git(args, may_commit)?;
    }
    Ok(())
}

fn program_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// Refuses shells, wrappers, package runners, `find` actions and inline
/// interpreter code.
fn program(name: &str, args: &[String]) -> Result<(), String> {
    if WRAPPERS.contains(&name) {
        return Err(format!(
            "`{name}` runs a shell or another program; name the check's program directly."
        ));
    }
    let first = args.first().map(String::as_str);
    let runner = RUNNER_SUBCOMMANDS
        .iter()
        .any(|(runner, subs)| *runner == name && first.is_some_and(|sub| subs.contains(&sub)));
    if runner {
        return Err(format!(
            "`{name} {}` fetches or runs an arbitrary program; checks run the project's own.",
            first.unwrap_or_default()
        ));
    }
    if name == "find" {
        if let Some(action) = args.iter().find(|a| FIND_ACTIONS.contains(&a.as_str())) {
            return Err(format!(
                "`find {action}` runs programs or writes files; a check may not."
            ));
        }
    }
    if let Some(interpreter) = INTERPRETERS.iter().find(|i| (i.is)(name)) {
        if inline_code(interpreter, args) {
            return Err(format!(
                "`{name}` with inline code is refused; run a file in the workspace instead."
            ));
        }
    }
    Ok(())
}

/// Whether the interpreter's options (those before the script or module)
/// hand it code or a module to load.
fn inline_code(interpreter: &Interpreter, args: &[String]) -> bool {
    if args
        .first()
        .is_some_and(|sub| interpreter.subcommands.contains(&sub.as_str()))
    {
        return true;
    }
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-m" || arg == "--" || !arg.starts_with('-') {
            return false;
        }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if interpreter.long.contains(&flag) {
            return true;
        }
        if arg.starts_with("--") {
            continue;
        }
        let letters = arg[1..].trim_start_matches(|c| interpreter.plain.contains(c));
        match letters.chars().next() {
            Some(c) if interpreter.short.contains(c) => return true,
            Some(c) if interpreter.valued.contains(c) && letters.len() == 1 => {
                iter.next();
            }
            _ => {}
        }
    }
    false
}

/// Every path a check names must stay inside `root`: the value of a path
/// option strictly, any other argument when it is (or would be created
/// beside) something outside, or climbs out with `..`.
fn paths(args: &[String], root: &Path) -> Result<(), String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if PATH_FLAGS.contains(&arg.as_str()) {
            if let Some(value) = iter.next_if(|value| !value.starts_with('-')) {
                path_value(value, root, true)?;
            }
            continue;
        }
        if let Some((flag, value)) = arg.split_once('=').filter(|(f, _)| f.starts_with('-')) {
            path_value(value, root, PATH_FLAGS.contains(&flag))?;
            continue;
        }
        let attached = SHORT_PATH_FLAGS
            .iter()
            .find(|flag| arg.len() > 2 && arg.starts_with(**flag));
        if let Some(flag) = attached {
            path_value(&arg[flag.len()..], root, true)?;
            continue;
        }
        if !arg.starts_with('-') {
            path_value(arg, root, false)?;
        }
    }
    Ok(())
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
