//! Runs a plan's verification steps as programs, never through a shell.
//!
//! A step is a command line such as `cargo test -p monocode-core`. It is
//! split with shell quoting rules (`shlex`) and executed directly in the
//! workspace, so a step that needs a shell (pipes, `&&`, redirection,
//! variables, globs) or is prose (`Run the tests`) cannot run: review hands it
//! to Mono instead of guessing. A program named by path must stay inside the
//! workspace, and so must every path argument; shells, wrappers and inline
//! interpreter code are refused, and Git may only read (never commit).
//! This is best effort (`confine`): a script the project itself
//! runs can do anything that script does.
pub(super) use super::confine::confine;
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
/// How long output readers may outlive the check before they are abandoned
/// (a daemonised descendant can keep the pipes open).
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
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
/// shell or a person reading it, or may not run here ([`confine`]).
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
    confine(&argv, cwd)?;
    let program = resolve(&argv[0], cwd)?;
    Ok((program, argv))
}

/// A bare name is looked up on PATH; a path must be relative and resolve
/// inside the workspace.
fn resolve(name: &str, cwd: &Path) -> Result<PathBuf, String> {
    let missing = || format!("`{name}` is not a program in the workspace or on PATH.");
    if name.contains('/') || name.contains('\\') {
        if Path::new(name).is_absolute() {
            return Err(format!(
                "`{name}` is outside the workspace; name a program inside it or on PATH."
            ));
        }
        let root = cwd.canonicalize().map_err(|e| e.to_string())?;
        let path = root.join(name).canonicalize().map_err(|_| missing())?;
        if !path.starts_with(&root) {
            return Err(format!("`{name}` resolves outside the workspace."));
        }
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err(missing())
        };
    }
    let search = crate::scm::shell::cached_gui_search_path();
    std::env::split_paths(std::ffi::OsStr::new(&search))
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
        .ok_or_else(missing)
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
    let grace = Instant::now() + OUTPUT_GRACE;
    let collect = |pipe: std::sync::mpsc::Receiver<(String, bool)>| {
        pipe.recv_timeout(grace.saturating_duration_since(Instant::now()))
            .unwrap_or((String::new(), true))
    };
    let (out, out_cut) = collect(stdout);
    let (err, err_cut) = collect(stderr);
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

/// Reads a pipe to its end on a thread, keeping only the last bytes. The
/// result arrives on the channel; a reader stuck on an inherited pipe is
/// abandoned after [`OUTPUT_GRACE`].
fn tail(pipe: Option<Box<dyn Read + Send>>) -> std::sync::mpsc::Receiver<(String, bool)> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Some(mut pipe) = pipe else {
            let _ = send.send((String::new(), false));
            return;
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
        let _ = send.send((String::from_utf8_lossy(&kept).into_owned(), cut));
    });
    receive
}

/// The checks of one review or verification round and what they did to
/// the workspace.
#[derive(Debug, Default)]
pub struct Batch {
    /// One per check, in order: the run, or why it could not run.
    pub results: Vec<Result<CheckRun, String>>,
    /// Files the checks changed relative to the snapshot taken before them.
    pub modified: Vec<String>,
    /// Git could not snapshot the workspace after the checks.
    pub error: Option<String>,
}

impl Batch {
    /// Whether the runs describe the snapshot taken before them: nothing
    /// changed the workspace while they ran.
    pub fn trustworthy(&self) -> bool {
        self.modified.is_empty() && self.error.is_none()
    }
}

/// Runs `checks` in the workspace at `path`, whose snapshot before them is
/// `before`, then snapshots it again at `after_ref` and reports any change.
pub fn run_batch(path: &str, checks: &[String], before: &str, after_ref: &str) -> Batch {
    let timeout = Duration::from_millis(CHECK_TIMEOUT_MS);
    let mut batch = Batch {
        results: checks
            .iter()
            .map(|check| run(check, Path::new(path), timeout))
            .collect(),
        ..Batch::default()
    };
    if !batch.results.iter().any(Result::is_ok) {
        return batch;
    }
    let after = crate::checkpoint::snapshot_workspace(path, after_ref).and_then(|after| {
        if crate::checkpoint::same_snapshot(path, before, &after)? {
            return Ok(Vec::new());
        }
        let files = crate::checkpoint::changed_paths(path, before, &after)?;
        Ok(if files.is_empty() {
            vec!["(the workspace)".into()]
        } else {
            files
        })
    });
    match after {
        Ok(modified) => batch.modified = modified,
        Err(error) => batch.error = Some(error),
    }
    batch
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

#[cfg(test)]
#[path = "checks_tests.rs"]
mod tests;
