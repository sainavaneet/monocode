//! Which programs a check may run: not a shell, not a wrapper that runs
//! another program, not an interpreter given code on the command line, and
//! Git only for reading. A runner that wraps a program is judged by the
//! program it wraps (`runner`).

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
    "bunx",
    "uvx",
    "command",
    "exec",
    "busybox",
    "stdbuf",
    "setsid",
    "chroot",
    "osascript",
    "arch",
    "caffeinate",
    "watch",
    "flock",
    "parallel",
];
/// `find` actions that run programs or write files.
const FIND_ACTIONS: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls",
];
/// Programs whose first operand is program text unless `-f` names a file.
const AWKS: &[&str] = &["awk", "gawk", "mawk", "nawk"];
/// awk options whose value is the next argument.
const AWK_VALUED: &[&str] = &["-F", "-v"];

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

/// Refuses `argv` when its program is a shell, wrapper, fetcher, `find`
/// action, inline code or more than reading Git, looking through runners
/// to the program they wrap.
pub(super) fn check(argv: &[String]) -> Result<(), String> {
    let Some(first) = argv.first() else {
        return Ok(());
    };
    let name = base_name(program_name(first));
    let args = &argv[1..];
    if WRAPPERS.contains(&name) {
        return Err(format!(
            "`{name}` runs a shell or another program; name the check's program directly."
        ));
    }
    super::runner::fetcher(name, args)?;
    if let Some(line) = super::runner::wrapped(name, args)? {
        return if line.is_empty() {
            Ok(())
        } else {
            check(&line)
        };
    }
    if name == "find" {
        if let Some(action) = args.iter().find(|a| FIND_ACTIONS.contains(&a.as_str())) {
            return Err(format!(
                "`find {action}` runs programs or writes files; a check may not."
            ));
        }
    }
    let inline = match INTERPRETERS.iter().find(|i| (i.is)(name)) {
        Some(interpreter) => inline_code(interpreter, args),
        None => AWKS.contains(&name) && awk_inline(args),
    };
    if inline {
        return Err(format!(
            "`{name}` with inline code is refused; run a file in the workspace instead."
        ));
    }
    if name == "git" {
        return super::git::git(args);
    }
    Ok(())
}

fn program_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// `python3.12` → `python`, `php8.2` → `php`, `perl5.34` → `perl`.
fn base_name(name: &str) -> &str {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if base.is_empty() {
        name
    } else {
        base
    }
}

/// Whether awk is given its program as an operand rather than with `-f`.
fn awk_inline(args: &[String]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-f" || arg.starts_with("-f") || arg.starts_with("--file") {
            return false;
        }
        if AWK_VALUED.contains(&arg.as_str()) {
            iter.next();
            continue;
        }
        if arg == "--" {
            return iter.next().is_some();
        }
        if !arg.starts_with('-') {
            return true;
        }
    }
    false
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
