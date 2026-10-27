//! Checks stay inside their workspace, best effort: no path argument outside
//! it, no shell, wrapper or inline interpreter code, Git only for reading
//! (and committing with commit authority).
use super::*;

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("workspace");
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
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

fn refused(checks: &[&str], root: &Path) {
    for check in checks {
        assert!(confined(check, root, true).is_err(), "{check}");
    }
}

fn allowed(checks: &[&str], root: &Path) {
    for check in checks {
        let result = confined(check, root, false);
        assert!(result.is_ok(), "{check}: {result:?}");
    }
}

#[test]
fn git_that_reaches_a_remote_or_another_repository_is_refused() {
    let (_dir, root) = workspace();
    refused(
        &[
            "git push",
            "git push origin main",
            "git remote add evil https://example.com/x.git",
            "git fetch origin main:main",
            "git fetch",
            "git -C /tmp status",
            "git -C .. status",
            "git --git-dir=/tmp/other.git log",
            "git --git-dir /tmp/other.git log",
            "git --work-tree=.. status",
            "git -c core.hooksPath=hooks status",
            "git -c alias.x=!true x",
        ],
        &root,
    );
    allowed(
        &[
            "git status",
            "git diff --stat",
            "git log -1",
            "git show HEAD",
            "git rev-parse HEAD",
            "git ls-files src",
            "git grep -n todo",
            "git describe --tags",
            "git merge-base HEAD main",
            "git cat-file -t HEAD",
            "git branch",
            "git branch --show-current",
        ],
        &root,
    );
}

#[test]
fn git_that_changes_configuration_refs_or_runs_programs_is_refused() {
    let (_dir, root) = workspace();
    refused(
        &[
            "git config core.hooksPath hooks",
            "git branch -f main HEAD~1",
            "git branch -D old",
            "git branch -m new",
            "git branch topic",
            "git submodule foreach make",
            "git update-ref refs/heads/main HEAD",
            "git my-alias",
            "git tag v1",
            "git stash",
        ],
        &root,
    );
}

#[test]
fn git_commits_need_commit_authority() {
    let (_dir, root) = workspace();
    for check in [
        "git commit -am wip",
        "git merge other",
        "git rebase main",
        "git cherry-pick abc",
    ] {
        let refused = confined(check, &root, false).unwrap_err();
        assert!(refused.contains("commit"), "{check}: {refused}");
        assert!(confined(check, &root, true).is_ok(), "{check}");
    }
}

#[test]
fn shells_wrappers_and_inline_code_are_refused() {
    let (_dir, root) = workspace();
    refused(
        &[
            "sh -c true",
            "bash scripts/x",
            "zsh -c true",
            "fish -c true",
            "env FOO=1 cargo test",
            "xargs rm",
            "nohup cargo test",
            "time cargo test",
            "sudo cargo test",
            "/usr/bin/env cargo test",
            "python3 -c print(1)",
            "python -cprint(1)",
            "node -e 1",
            "node --eval 1",
            "ruby -e 1",
            "perl -e 1",
            "npx some-tool",
        ],
        &root,
    );
    allowed(&["python3 -m pytest tests/", "node scripts/x"], &root);
}

#[test]
fn arguments_outside_the_workspace_are_refused() {
    let (_dir, root) = workspace();
    refused(
        &[
            "cat /etc/passwd",
            "cargo test --manifest-path=/tmp/other/Cargo.toml",
            "cargo test --manifest-path /tmp/other/Cargo.toml",
            "cargo build -o/tmp/x",
            "cargo build --output /tmp/x",
            "make -f /tmp/Makefile",
            "cat ../secret",
            "ls src/../../",
            "rm -rf src/../../elsewhere",
        ],
        &root,
    );
}

#[test]
fn patterns_that_look_like_paths_are_not_refused() {
    let (_dir, root) = workspace();
    allowed(
        &[
            "grep -q /health src/routes.rs",
            "grep -q \"// SAFETY\" src/lib.rs",
            "jest --testPathIgnorePatterns=/node_modules/",
            "grep -rq /api/v1/ src",
        ],
        &root,
    );
}

#[test]
fn ordinary_checks_are_allowed() {
    let (_dir, root) = workspace();
    allowed(
        &[
            "cargo test",
            "cargo test -p monocode-core -- --test-threads=1",
            "cargo test --manifest-path ./Cargo.toml",
            "npm test",
            "npm test -- --watch=false",
            "pytest tests/",
            "./gradlew test",
            "make test",
            "go test ./...",
            "./scripts/x",
            "ls src",
            "grep -q fixed src/feature.rs",
            "test -f src/../src/feature.rs",
        ],
        &root,
    );
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

#[test]
fn every_wrapper_runner_and_inline_interpreter_is_refused() {
    let (_dir, root) = workspace();
    refused(
        &[
            "dash -c true",
            "ksh -c true",
            "doas cargo test",
            "su -c true",
            "nice cargo test",
            "ionice cargo test",
            "timeout 5 cargo test",
            "script -q out cargo",
            "expect run.exp",
            "pnpm dlx some-tool",
            "yarn dlx some-tool",
            "npm exec some-tool",
            "bunx some-tool",
            "python -B -c print(1)",
            "python -Bc print(1)",
            "python -W ignore -c print(1)",
            "node -pe 1",
            "node -r ./scripts/x test.js",
            "bun --eval 1",
            "deno eval 1",
            "php -r echo(1);",
            "perl -Mstrict -e 1",
            "find . -exec rm {} +",
            "find . -delete",
        ],
        &root,
    );
    allowed(
        &[
            "python -m pytest",
            "python3 -B -m pytest tests/",
            "node test.js",
            "pnpm test",
            "npm run lint",
            "find src -name feature.rs",
        ],
        &root,
    );
}

#[test]
fn git_is_read_only_beyond_its_listing_forms() {
    let (_dir, root) = workspace();
    refused(
        &[
            "git reset --hard",
            "git checkout main",
            "git switch -c x",
            "git pull",
            "git fetch --all",
            "git remote set-url origin https://example.com/x.git",
            "git remote show origin",
            "git clean -fdx",
            "git gc",
            "git filter-branch",
            "git replace a b",
            "git notes add",
            "git reflog expire --all",
            "git apply x.patch",
            "git submodule update",
            "git worktree add ../x",
            "git stash drop",
            "git tag -d v1",
            "git branch --delete old",
            "git --exec-path=bin status",
            "git --namespace=x log",
            "git log !x",
            "git grep -Ocat todo",
            "git grep --open-files-in-pager=cat todo",
        ],
        &root,
    );
    allowed(
        &[
            "git branch --list",
            "git branch -a",
            "git branch -r",
            "git branch --list feat/*",
            "git branch --contains HEAD",
            "git tag",
            "git tag --list",
            "git tag -l v1.*",
            "git remote -v",
            "git stash list",
            "git worktree list",
            "git rev-list --count HEAD",
            "git name-rev HEAD",
            "git blame src/feature.rs",
            "git shortlog -sn",
            "git --no-pager log -1",
        ],
        &root,
    );
    for commits in ["git revert HEAD", "git am x.patch"] {
        assert!(confined(commits, &root, false).is_err(), "{commits}");
        assert!(confined(commits, &root, true).is_ok(), "{commits}");
    }
}

#[test]
fn path_options_are_held_to_the_workspace() {
    let (_dir, root) = workspace();
    refused(
        &[
            "make -C ..",
            "make -C/tmp",
            "cargo test --target-dir /tmp/target",
            "pytest --rootdir=/tmp",
            "cp src/feature.rs /tmp/copied",
        ],
        &root,
    );
    allowed(
        &[
            "cargo test -p x",
            "make -C src",
            "grep -C 3 fixed src/feature.rs",
            "cargo test --target-dir target/check",
        ],
        &root,
    );
}
