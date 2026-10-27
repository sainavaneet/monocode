//! Checks stay inside their workspace: no path argument outside it, no Git
//! invocation that reaches another repository or a remote, and commits only
//! with commit authority.
use super::*;

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("workspace");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let script = root.join("scripts/x");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, root)
}

fn confined(check: &str, root: &Path, may_commit: bool) -> Result<(), String> {
    let argv = shlex::split(check).unwrap();
    confine(&argv, root, may_commit)
}

#[test]
fn git_that_reaches_a_remote_or_another_repository_is_refused() {
    let (_dir, root) = workspace();
    for refused in [
        "git push",
        "git push origin main",
        "git remote add evil https://example.com/x.git",
        "git fetch origin main:main",
        "git -C /tmp status",
        "git -C .. status",
        "git --git-dir=/tmp/other.git log",
        "git --git-dir /tmp/other.git log",
        "git --work-tree=.. status",
        "git -c core.hooksPath=hooks status",
        "git -c alias.x=!true x",
    ] {
        assert!(confined(refused, &root, true).is_err(), "{refused}");
    }
    for allowed in ["git status", "git diff --stat", "git log -1", "git fetch"] {
        assert!(confined(allowed, &root, false).is_ok(), "{allowed}");
    }
}

#[test]
fn git_commits_need_commit_authority() {
    let (_dir, root) = workspace();
    for check in [
        "git commit -am wip",
        "git merge other",
        "git rebase main",
        "git cherry-pick abc",
        "git tag v1",
    ] {
        let refused = confined(check, &root, false).unwrap_err();
        assert!(refused.contains("commit"), "{check}: {refused}");
        assert!(confined(check, &root, true).is_ok(), "{check}");
    }
}

#[test]
fn arguments_outside_the_workspace_are_refused() {
    let (_dir, root) = workspace();
    for refused in [
        "cat /etc/passwd",
        "cargo test --manifest-path=/tmp/other/Cargo.toml",
        "cat ../secret",
        "ls src/../../",
        "rm -rf src/../../elsewhere",
    ] {
        assert!(confined(refused, &root, true).is_err(), "{refused}");
    }
    for allowed in [
        "cargo test",
        "cargo test -p monocode-core -- --test-threads=1",
        "npm test",
        "./scripts/x",
        "ls src",
        "grep -q fixed src/feature.rs",
        "test -f src/../src/feature.rs",
    ] {
        assert!(confined(allowed, &root, false).is_ok(), "{allowed}");
    }
}

#[test]
fn refused_checks_do_not_run_and_ordinary_ones_do() {
    let (_dir, root) = workspace();
    let timeout = Duration::from_secs(30);
    assert!(run("./scripts/x", &root, timeout).unwrap().passed());
    assert!(run("ls src", &root, timeout).unwrap().passed());
    let refused = run("git push origin main", &root, timeout).unwrap_err();
    assert!(refused.contains("push"), "{refused}");
    assert!(run("cat /etc/hosts", &root, timeout).is_err());
    // A batch reports a refused check as one that could not run.
    let checks = ["git commit -m x".to_owned()];
    let batch = run_batch(&root.to_string_lossy(), &checks, "unused", "unused", false);
    assert!(batch.results[0].is_err());
}
