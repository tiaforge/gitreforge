//! Shared helpers for the integration tests: a scratch working repository
//! that is turned into a bare mirror, and wrappers to run git and gitreforge
//! isolated from the user's configuration.

#![allow(dead_code)]

use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

pub const AUTHOR: &str = "A U Thor <author@example.com>";

pub struct TestRepo {
    pub root: PathBuf,
    pub work: PathBuf,
    time: Cell<u64>,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        if std::env::var_os("GITREFORGE_KEEP_TEST_REPOS").is_none() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

impl TestRepo {
    pub fn create(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "gitreforge-test-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("gitconfig"), "").unwrap();
        let work = root.join("work");
        git(&root, &["init", "-q", "-b", "main", "work"]);

        Self {
            root,
            work,
            time: Cell::new(1_700_000_000),
        }
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.work, args)
    }

    pub fn write(&self, path: &str, contents: &str) {
        let path = self.work.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn delete(&self, path: &str) {
        self.git(&["rm", "-q", "-r", path]);
    }

    /// Commits everything in the working directory, empty commits included.
    pub fn commit(&self, message: &str) {
        self.commit_as(message, AUTHOR, AUTHOR);
    }

    pub fn commit_as(&self, message: &str, author: &str, committer: &str) {
        self.git(&["add", "-A"]);
        self.with_identity(
            author,
            committer,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
    }

    /// Runs a git command that creates a commit, with a fixed identity and a
    /// date that increases with every call.
    pub fn with_identity(&self, author: &str, committer: &str, args: &[&str]) -> String {
        let (author_name, author_email) = split_identity(author);
        let (committer_name, committer_email) = split_identity(committer);
        let time = self.time.get();
        self.time.set(time + 60);
        let date = format!("{time} +0200");
        command_output(
            git_command(&self.work)
                .args(args)
                .env("GIT_AUTHOR_NAME", author_name)
                .env("GIT_AUTHOR_EMAIL", author_email)
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_NAME", committer_name)
                .env("GIT_COMMITTER_EMAIL", committer_email)
                .env("GIT_COMMITTER_DATE", &date),
        )
    }

    /// Bare mirror of the working repository, its objects loose
    /// (`git push` unpacks small pushes).
    pub fn loose_mirror(&self) -> PathBuf {
        let bare = self.root.join("loose.git");
        git(
            &self.root,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        self.git(&["push", "-q", "--mirror", bare.to_str().unwrap()]);
        assert_eq!(fs::read_dir(bare.join("objects/pack")).unwrap().count(), 0);
        bare
    }

    /// Bare mirror of the working repository with all objects in one pack,
    /// stored as deltas wherever git finds one.
    pub fn packed_mirror(&self) -> PathBuf {
        let bare = self.root.join("packed.git");
        git(
            &self.root,
            &[
                "clone",
                "-q",
                "--mirror",
                "--no-local",
                "work",
                bare.to_str().unwrap(),
            ],
        );
        git(
            &bare,
            &[
                "repack",
                "-q",
                "-a",
                "-d",
                "-f",
                "--depth=50",
                "--window=250",
            ],
        );
        bare
    }

    /// Runs gitreforge in the repository's root directory, where it writes
    /// its commit map. Global options go before the repository, the
    /// subcommand and its arguments after it.
    fn command(&self, repo: &Path, env: &[(&str, &str)], args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gitreforge"));
        isolate(&mut command, &self.root);
        let global = args
            .iter()
            .position(|a| ["contributor", "remove", "prune-empty"].contains(a))
            .unwrap_or(args.len());
        command
            .envs(env.iter().copied())
            .args(&args[..global])
            .arg(repo)
            .args(&args[global..])
            .current_dir(&self.root);
        command
    }

    pub fn reforge(&self, repo: &Path, args: &[&str]) -> Output {
        self.reforge_with_env(repo, &[], args)
    }

    pub fn reforge_with_env(&self, repo: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
        self.command(repo, env, args).output().unwrap()
    }

    pub fn reforge_ok(&self, repo: &Path, args: &[&str]) -> String {
        self.reforge_env_ok(repo, &[], args)
    }

    pub fn reforge_env_ok(&self, repo: &Path, env: &[(&str, &str)], args: &[&str]) -> String {
        success(self.reforge_with_env(repo, env, args), args)
    }

    pub fn reforge_with_stdin(&self, repo: &Path, args: &[&str], stdin: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = self
            .command(repo, &[], args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn reforge_stdin_ok(&self, repo: &Path, args: &[&str], stdin: &str) -> String {
        success(self.reforge_with_stdin(repo, args, stdin), args)
    }

    /// Old to new commit ids, as written by the last rewrite.
    pub fn commit_map(&self) -> BTreeMap<String, String> {
        fs::read_to_string(self.root.join("object-id-map.old-new.txt"))
            .unwrap()
            .lines()
            .map(|line| {
                let (old, new) = line.split_once(' ').unwrap();
                (old.to_owned(), new.to_owned())
            })
            .collect()
    }
}

fn success(output: Output, args: &[&str]) -> String {
    assert!(
        output.status.success(),
        "gitreforge {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn split_identity(identity: &str) -> (&str, &str) {
    let (name, email) = identity.split_once(" <").unwrap();
    (name, email.trim_end_matches('>'))
}

/// Keeps the user's and system's git configuration away from a command.
pub fn isolate(command: &mut Command, root: &Path) {
    command
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("xdg"))
        .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("GITREFORGE_PACK")
        .env_remove("GITREFORGE_DELTA")
        .env_remove("GITREFORGE_NO_PRUNE");
}

fn git_command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // the global config lives in the test root, the parent of every repo
    let root = dir
        .ancestors()
        .find(|p| p.join("gitconfig").is_file())
        .unwrap_or(dir);
    isolate(&mut command, root);
    command.current_dir(dir);
    command
}

fn command_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    command_output(git_command(dir).args(args))
}

/// Fails unless git finds the repository fully consistent.
pub fn fsck(repo: &Path) {
    git(repo, &["fsck", "--full", "--strict", "--no-dangling"]);
}

/// All refs and their targets.
pub fn refs(repo: &Path) -> BTreeMap<String, String> {
    git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .map(|l| {
            let (name, hash) = l.split_once(' ').unwrap();
            (name.to_owned(), hash.to_owned())
        })
        .collect()
}

/// Every commit reachable from any ref, keyed by its subject (which the
/// tests keep unique).
pub fn commits(repo: &Path) -> BTreeMap<String, Commit> {
    let ids = git(repo, &["rev-list", "--all"]);
    let mut result = BTreeMap::new();
    for id in ids.lines() {
        let commit = Commit::read(repo, id);
        assert!(
            result.insert(commit.subject.clone(), commit).is_none(),
            "duplicate subject"
        );
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub subject: String,
    /// Everything except the tree and parents: identities, dates, message
    pub metadata: String,
    pub parents: Vec<String>,
    /// `ls-tree -r` output: mode, type, blob id and path of every file
    pub files: Vec<String>,
    /// Paths of all directories in the tree, with a trailing slash
    pub directories: Vec<String>,
}

impl Commit {
    pub fn read(repo: &Path, id: &str) -> Self {
        let subject = git(repo, &["log", "-1", "--format=%s", id]);
        let metadata = git(
            repo,
            &[
                "log",
                "-1",
                "--format=%an <%ae> %ad%n%cn <%ce> %cd%n%B",
                "--date=raw",
                id,
            ],
        );
        let parents = git(repo, &["log", "-1", "--format=%P", id])
            .split_whitespace()
            .map(|p| git(repo, &["log", "-1", "--format=%s", p]))
            .collect();
        let files = git(repo, &["ls-tree", "-r", id])
            .lines()
            .map(str::to_owned)
            .collect();
        let directories = git(repo, &["ls-tree", "-r", "-d", "--name-only", id])
            .lines()
            .map(|d| format!("{d}/"))
            .collect();
        Self {
            id: id.to_owned(),
            subject,
            metadata,
            parents,
            files,
            directories,
        }
    }

    pub fn paths(&self) -> Vec<&str> {
        self.files.iter().map(|f| file_path(f)).collect()
    }
}

/// Path of an `ls-tree` line
pub fn file_path(line: &str) -> &str {
    line.split_once('\t').unwrap().1
}
