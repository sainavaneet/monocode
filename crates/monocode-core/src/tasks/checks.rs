//! Runs a plan's verification steps as programs, never through a shell.
//!
//! A step is a command line such as `cargo test -p monocode-core`. It is
//! split with shell quoting rules (`shlex`) and executed directly in the
//! workspace, so a step that needs a shell (pipes, `&&`, redirection,
//! variables, globs) or is prose (`Run the tests`) cannot run: review hands it
//! to Mono instead of guessing.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one check may run before it is killed and recorded as timed out.
pub const CHECK_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// How much of a check's output a verification keeps: the end, where
/// failures are reported.
pub const CHECK_OUTPUT_BYTES: usize = 8 * 1024;
const POLL: Duration = Duration::from_millis(25);
/// Tokens that only mean something to a shell.
const SHELL_TOKENS: &[&str] = &["&&", "||", "|", ";", "&", ">", ">>", "<", "<<", "2>&1"];
const SHELL_CHARS: &[char] = &['$', '`', '*', '?', '~', '>', '<', '|', ';'];

/// One executed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRun {
    pub check: String,
    pub argv: Vec<String>,
    /// None when the process was killed (timeout or signal).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The end of stdout then stderr, at most [`CHECK_OUTPUT_BYTES`].
    pub output: String,
    pub truncated: bool,
    pub duration_ms: u64,
}

impl CheckRun {
    pub fn passed(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// The program and arguments of `check`, or why it cannot run without a
/// shell or a person reading it.
pub fn command_line(check: &str, cwd: &Path) -> Result<(PathBuf, Vec<String>), String> {
    let argv = shlex::split(check.trim())
        .filter(|argv| !argv.is_empty())
        .ok_or("It is not a command line.")?;
    if argv
        .iter()
        .any(|token| SHELL_TOKENS.contains(&token.as_str()))
        || check.contains(SHELL_CHARS)
    {
        return Err("It needs a shell; checks run without one.".into());
    }
    let program = resolve(&argv[0], cwd).ok_or_else(|| {
        format!(
            "`{}` is not a program in the workspace or on PATH.",
            argv[0]
        )
    })?;
    Ok((program, argv))
}

fn resolve(name: &str, cwd: &Path) -> Option<PathBuf> {
    if name.contains('/') {
        let path = cwd.join(name);
        return is_executable(&path).then_some(path);
    }
    let search = crate::scm::shell::cached_gui_search_path();
    std::env::split_paths(std::ffi::OsStr::new(&search))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Runs `check` in `cwd` until it exits or `timeout` passes. Errs only when
/// it cannot run at all ([`command_line`]).
pub fn run(check: &str, cwd: &Path, timeout: Duration) -> Result<CheckRun, String> {
    let (program, argv) = command_line(check, cwd)?;
    let mut command = Command::new(&program);
    crate::scm::shell::apply_gui_env(&mut command);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process::isolate(&mut command);
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| format!("`{}` could not start: {e}", argv[0]))?;
    let _registration = crate::process::register(child.id());
    let stdout = tail(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = tail(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = started + timeout;
    let (exit_code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if Instant::now() >= deadline => break (None, true),
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => break (None, false),
        }
    };
    crate::process::terminate(&mut child);
    let (out, out_cut) = stdout.join().unwrap_or_default();
    let (err, err_cut) = stderr.join().unwrap_or_default();
    let joined = [out, err]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let (output, cut) = keep_end(joined.trim());
    Ok(CheckRun {
        check: check.to_owned(),
        argv,
        exit_code,
        timed_out,
        output,
        truncated: out_cut || err_cut || cut,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// Reads a pipe to its end on a thread, keeping only the last bytes.
fn tail(pipe: Option<Box<dyn Read + Send>>) -> std::thread::JoinHandle<(String, bool)> {
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            return (String::new(), false);
        };
        let mut kept = Vec::new();
        let mut cut = false;
        let mut chunk = [0; 4096];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&chunk[..n]);
            if kept.len() > CHECK_OUTPUT_BYTES {
                kept.drain(..kept.len() - CHECK_OUTPUT_BYTES);
                cut = true;
            }
        }
        (String::from_utf8_lossy(&kept).into_owned(), cut)
    })
}

/// The last [`CHECK_OUTPUT_BYTES`] of `text`, on a character boundary.
fn keep_end(text: &str) -> (String, bool) {
    if text.len() <= CHECK_OUTPUT_BYTES {
        return (text.to_owned(), false);
    }
    let mut start = text.len() - CHECK_OUTPUT_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_owned(), true)
}
