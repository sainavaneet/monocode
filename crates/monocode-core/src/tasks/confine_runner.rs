//! Runners that wrap a program (`uv run pytest`, `bundle exec rspec`,
//! `pnpm exec vitest`, `npx vitest`): the check is judged by the wrapped
//! program. Each runner's options are known exactly, so an option's value
//! is never mistaken for the program (or the program for an argument); an
//! unknown option fails closed, and an option that runs a string through a
//! shell (`npx -c`) is refused. A runner subcommand that fetches a tool
//! first (`pnpm dlx`, `bun x`) is refused.

/// Runner subcommands that fetch and run an arbitrary tool.
const FETCHERS: &[(&str, &str)] = &[
    ("pnpm", "dlx"),
    ("yarn", "dlx"),
    ("bun", "x"),
    ("pipx", "run"),
];

/// A runner of the program named after its subcommand and options.
struct Runner {
    name: &'static str,
    /// None: the runner itself runs the program (`npx`).
    subcommand: Option<&'static str>,
    /// Options whose value is the next argument.
    valued: &'static [&'static str],
    /// Options that take no value.
    bare: &'static [&'static str],
    /// Options that run a string through a shell.
    shell: &'static [&'static str],
}

const JS_VALUED: &[&str] = &[
    "-w",
    "--workspace",
    "--filter",
    "-C",
    "--dir",
    "--prefix",
    "--package",
    "-p",
];
const NPM_BARE: &[&str] = &[
    "-y",
    "--yes",
    "--no",
    "-q",
    "--quiet",
    "-s",
    "--silent",
    "--ws",
    "--workspaces",
    "--include-workspace-root",
    "--prefer-offline",
    "--prefer-online",
    "--offline",
];
const NPM_SHELL: &[&str] = &["-c", "--call"];
const UV_VALUED: &[&str] = &[
    "--python",
    "-p",
    "--with",
    "-w",
    "--with-requirements",
    "--group",
    "--extra",
    "--project",
    "--directory",
    "--package",
    "--env-file",
];
const UV_BARE: &[&str] = &[
    "--no-sync",
    "--frozen",
    "--locked",
    "--isolated",
    "--no-project",
    "--all-extras",
    "--all-groups",
    "--all-packages",
    "--no-dev",
    "--dev",
    "--active",
    "--exact",
    "--offline",
    "--no-cache",
    "-q",
    "--quiet",
    "-v",
    "--verbose",
    "-m",
    "--module",
    "-s",
    "--script",
];

const RUNNERS: &[Runner] = &[
    Runner {
        name: "uv",
        subcommand: Some("run"),
        valued: UV_VALUED,
        bare: UV_BARE,
        shell: &[],
    },
    Runner {
        name: "poetry",
        subcommand: Some("run"),
        valued: &["-P", "--project", "-C", "--directory"],
        bare: &[
            "-q",
            "--quiet",
            "-v",
            "-vv",
            "-vvv",
            "--verbose",
            "-n",
            "--no-interaction",
        ],
        shell: &[],
    },
    Runner {
        name: "pdm",
        subcommand: Some("run"),
        valued: &["-p", "--project", "--venv"],
        bare: &["-v", "-q", "-s", "--site-packages", "-g", "--global"],
        shell: &[],
    },
    Runner {
        name: "pipenv",
        subcommand: Some("run"),
        valued: &[],
        bare: &[],
        shell: &[],
    },
    Runner {
        name: "pnpm",
        subcommand: Some("exec"),
        valued: JS_VALUED,
        bare: &["-r", "--recursive", "--parallel", "-s", "--silent"],
        shell: &["-c", "--shell-mode"],
    },
    Runner {
        name: "yarn",
        subcommand: Some("exec"),
        valued: JS_VALUED,
        bare: &[],
        shell: &["-c"],
    },
    Runner {
        name: "bundle",
        subcommand: Some("exec"),
        valued: &["--gemfile"],
        bare: &["--keep-file-descriptors"],
        shell: &[],
    },
    Runner {
        name: "npm",
        subcommand: Some("exec"),
        valued: JS_VALUED,
        bare: NPM_BARE,
        shell: NPM_SHELL,
    },
    Runner {
        name: "npm",
        subcommand: Some("x"),
        valued: JS_VALUED,
        bare: NPM_BARE,
        shell: NPM_SHELL,
    },
    Runner {
        name: "npx",
        subcommand: None,
        valued: JS_VALUED,
        bare: NPM_BARE,
        shell: NPM_SHELL,
    },
];

/// Refuses a runner subcommand that fetches a tool before running it.
pub(super) fn fetcher(name: &str, args: &[String]) -> Result<(), String> {
    match subcommand(name, args, |s| FETCHERS.contains(&(name, s))) {
        Some(sub) => Err(format!(
            "`{name} {}` fetches or runs an arbitrary program; checks run the project's own.",
            args[sub]
        )),
        None => Ok(()),
    }
}

/// The program line a runner wraps (empty when it names none), or None
/// when `name args` is not a runner of a program. Errs on an option that
/// runs a shell, an unknown option before the program, or a program token
/// with whitespace in it.
pub(super) fn wrapped(name: &str, args: &[String]) -> Result<Option<Vec<String>>, String> {
    let found = RUNNERS.iter().find_map(|runner| {
        if runner.name != name {
            return None;
        }
        match runner.subcommand {
            None => Some((runner, 0)),
            Some(wanted) => subcommand(name, args, |s| s == wanted).map(|i| (runner, i + 1)),
        }
    });
    let Some((runner, start)) = found else {
        return Ok(None);
    };
    let mut rest = args[start..].iter().enumerate();
    while let Some((index, arg)) = rest.next() {
        let program = if arg == "--" {
            index + 1
        } else if arg.starts_with('-') {
            if option(runner, arg)? {
                rest.next();
            }
            continue;
        } else {
            index
        };
        let line = args[start + program.min(args.len() - start)..].to_vec();
        if line
            .first()
            .is_some_and(|p| p.contains(char::is_whitespace))
        {
            return Err(format!(
                "`{}` is not a program name; name the program and its arguments separately.",
                line[0]
            ));
        }
        return Ok(Some(line));
    }
    Ok(Some(Vec::new()))
}

/// Whether the runner option `arg` takes the next argument as its value;
/// errs on a shell option or an unknown one.
fn option(runner: &Runner, arg: &str) -> Result<bool, String> {
    let (flag, attached) = match arg.split_once('=') {
        Some((flag, _)) => (flag, true),
        None => (arg, false),
    };
    let label = match runner.subcommand {
        Some(sub) => format!("{} {sub}", runner.name),
        None => runner.name.to_owned(),
    };
    if runner.shell.contains(&flag) {
        return Err(format!(
            "`{label} {flag}` runs its command through a shell; checks run without one."
        ));
    }
    if runner.valued.contains(&flag) {
        return Ok(!attached);
    }
    if runner.bare.contains(&flag) && !attached {
        return Ok(false);
    }
    Err(format!(
        "`{label} {arg}` is an unrecognised runner option; put the program first."
    ))
}

/// The index in `args` of the runner's subcommand matching `wanted`,
/// skipping its global options first (`npm -w pkg exec`).
fn subcommand(name: &str, args: &[String], wanted: impl Fn(&str) -> bool) -> Option<usize> {
    let known = FETCHERS.iter().any(|(r, _)| *r == name) || RUNNERS.iter().any(|r| r.name == name);
    if !known {
        return None;
    }
    let mut after_bare_flag = false;
    for (index, arg) in args.iter().enumerate() {
        if arg.starts_with('-') {
            after_bare_flag = !arg.contains('=');
            continue;
        }
        if wanted(arg) {
            return Some(index);
        }
        if !after_bare_flag {
            return None;
        }
        after_bare_flag = false;
    }
    None
}
