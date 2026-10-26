//! No test may start a real provider CLI on the user's machine and account.
//!
//! Under a test harness (this crate's own tests, or a test binary of a crate
//! that links it, recognised as cargo's `deps/<name>-<hash>` executable),
//! resolving or spawning a provider is allowed only for test fixtures: an
//! executable the test registered with [`register_fixture`] or created
//! inside the system temporary directory. Anything else is refused and
//! counted ([`refused`]); in this crate's own tests it panics.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

static REFUSED: AtomicUsize = AtomicUsize::new(0);
static FIXTURES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Whether this process is a cargo test binary.
pub fn under_test() -> bool {
    static HARNESS: OnceLock<bool> = OnceLock::new();
    cfg!(test)
        || *HARNESS.get_or_init(|| {
            let Ok(exe) = std::env::current_exe() else {
                return false;
            };
            let in_deps = exe
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|d| d == "deps");
            let hashed = exe
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit_once('-'))
                .is_some_and(|(_, hash)| {
                    hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit())
                });
            in_deps && hashed
        })
}

/// Allows `path` as a provider fixture for this test process.
pub fn register_fixture(path: impl Into<PathBuf>) {
    FIXTURES.lock().unwrap().push(path.into());
}

/// How many real provider launches this process refused.
pub fn refused() -> usize {
    REFUSED.load(Ordering::SeqCst)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn fixture(binary: &Path) -> bool {
    let binary = canonical(binary);
    if FIXTURES
        .lock()
        .unwrap()
        .iter()
        .any(|f| canonical(f) == binary)
    {
        return true;
    }
    binary.is_absolute() && binary.starts_with(canonical(&std::env::temp_dir()))
}

/// Whether `binary` may be considered at all in this process (no count, no
/// panic): always outside tests, only fixtures under one.
pub fn permitted(binary: &Path) -> bool {
    !under_test() || fixture(binary)
}

/// Refuses `binary` under a test harness unless it is a fixture.
pub fn check(binary: &Path) -> Result<(), String> {
    if !under_test() || fixture(binary) {
        return Ok(());
    }
    REFUSED.fetch_add(1, Ordering::SeqCst);
    let message = format!(
        "A test tried to start the real provider {}. Tests must use a fixture (a script in a temporary directory or provider::test_guard::register_fixture).",
        binary.display()
    );
    eprintln!("{message}");
    if cfg!(test) {
        panic!("{message}");
    }
    Err(message)
}

/// `resolved` if it may run in this process.
pub fn allow(resolved: Option<PathBuf>) -> Option<PathBuf> {
    resolved.filter(|path| check(path).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Kind;

    #[test]
    fn tests_cannot_resolve_or_start_a_real_provider() {
        assert!(under_test());
        // Real installations are neither probed nor returned.
        for kind in Kind::ALL {
            if let Some(path) = crate::provider::resolve(kind) {
                assert!(fixture(&path), "{kind:?} resolved to {}", path.display());
            }
        }
        let before = refused();
        let real = std::panic::catch_unwind(|| check(Path::new("/opt/homebrew/bin/codex")));
        assert!(real.is_err(), "a real provider path must be refused");
        assert_eq!(refused(), before + 1);
        // A script the test made in a temporary directory, or a registered
        // path, is a fixture.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        assert!(check(&script).is_ok());
        let registered = PathBuf::from("/usr/bin/true");
        register_fixture(&registered);
        assert!(check(&registered).is_ok());
    }
}
