mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

use common::{TestRepo, commits, fsck, git, isolate, refs};

const ALICE: &str = "Alice <alice@example.com>";
const BOB: &str = "Bob <bob@example.com>";

struct Key {
    private: PathBuf,
    public: PathBuf,
    allowed_signers: PathBuf,
}

fn create_key(repo: &TestRepo) -> Key {
    let private = repo.root.join("key");
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
        .arg(&private)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let public = repo.root.join("key.pub");
    let allowed_signers = repo.root.join("allowed_signers");
    let public_key = fs::read_to_string(&public).unwrap();
    fs::write(
        &allowed_signers,
        format!(
            "alice@example.com {public_key}bob@example.com {public_key}robert@example.com {public_key}"
        ),
    )
    .unwrap();
    Key {
        private,
        public,
        allowed_signers,
    }
}

/// `G` for a good signature, `N` for none, as `git log --format=%G?`.
fn signature_status(repo: &Path, key: &Key, rev: &str) -> String {
    git(
        repo,
        &[
            "-c",
            &format!(
                "gpg.ssh.allowedSignersFile={}",
                key.allowed_signers.display()
            ),
            "log",
            "-1",
            "--format=%G?",
            rev,
        ],
    )
}

fn has_signature_header(repo: &Path, rev: &str) -> bool {
    git(repo, &["cat-file", "commit", rev]).contains("\ngpgsig ")
}

/// Commits by Alice and Bob, some of them signed by git itself, with a
/// secret added after the first commit.
fn create_fixture(name: &str) -> (TestRepo, Key) {
    let repo = TestRepo::create(name);
    let key = create_key(&repo);
    let sign = [
        "-c".to_owned(),
        "gpg.format=ssh".to_owned(),
        "-c".to_owned(),
        format!("user.signingKey={}", key.private.display()),
    ];
    let signed_commit = |message: &str, identity: &str| {
        repo.git(&["add", "-A"]);
        let mut args: Vec<&str> = sign.iter().map(String::as_str).collect();
        args.extend(["commit", "-q", "-S", "-m", message]);
        repo.with_identity(identity, identity, &args);
    };

    repo.write("a.txt", "a\n");
    signed_commit("signed base", ALICE);
    repo.write("secret.txt", "secret\n");
    repo.write("e.txt", "e\n");
    repo.commit_as("alice adds secret", ALICE, ALICE);
    repo.write("b.txt", "b\n");
    signed_commit("signed by bob", BOB);
    repo.write("a.txt", "a2\n");
    signed_commit("signed by alice", ALICE);
    repo.write("c.txt", "c\n");
    repo.commit_as("bob commits for alice", ALICE, BOB);
    repo.write("d.txt", "d\n");
    repo.commit_as("alice commits for bob", BOB, ALICE);

    for subject in ["signed base", "signed by bob", "signed by alice"] {
        let rev = format!(":/{subject}");
        assert_eq!(signature_status(&repo.work, &key, &rev), "G", "{subject}");
    }
    (repo, key)
}

fn configure_private_key(mirror: &Path, key: &Key) {
    git(mirror, &["config", "gpg.format", "ssh"]);
    git(
        mirror,
        &["config", "user.signingKey", key.private.to_str().unwrap()],
    );
}

/// Signatures after a rewrite that changed every commit except the first.
fn check_signatures(
    mirror: &Path,
    key: &Key,
    before: &std::collections::BTreeMap<String, common::Commit>,
) {
    fsck(mirror);
    let after = commits(mirror);
    assert_eq!(after["signed base"].id, before["signed base"].id);
    assert_eq!(signature_status(mirror, key, &after["signed base"].id), "G");
    for (subject, expected) in [
        ("alice adds secret", "G"),
        ("signed by bob", "N"),
        ("signed by alice", "G"),
        ("bob commits for alice", "N"),
        ("alice commits for bob", "G"),
    ] {
        let id = &after[subject].id;
        assert_ne!(id, &before[subject].id, "{subject}");
        assert_eq!(signature_status(mirror, key, id), expected, "{subject}");
        assert_eq!(
            has_signature_header(mirror, id),
            expected == "G",
            "{subject}"
        );
        assert_eq!(
            after[subject].metadata, before[subject].metadata,
            "{subject}"
        );
        assert!(!after[subject].paths().contains(&"secret.txt"), "{subject}");
    }
}

#[test]
fn sign_with_private_key_file() {
    let (repo, key) = create_fixture("sign-private");
    for mirror in [repo.loose_mirror(), repo.packed_mirror()] {
        configure_private_key(&mirror, &key);
        let before = commits(&mirror);
        repo.reforge_ok(
            &mirror,
            &[
                "--sign-committer",
                "alice@example.com",
                "remove",
                "--file",
                "secret.txt",
            ],
        );
        check_signatures(&mirror, &key, &before);
    }
}

#[test]
fn sign_contributor_rewrite_and_prune() {
    let (repo, key) = create_fixture("sign-rewrite");
    let mirror = repo.packed_mirror();
    configure_private_key(&mirror, &key);
    repo.reforge_ok(&mirror, &["remove", "--file", "secret.txt"]);

    let before = commits(&mirror);
    // renaming Bob changes every commit from "signed by bob" on
    repo.reforge_stdin_ok(
        &mirror,
        &[
            "--sign-committer",
            "alice@example.com",
            "--sign-committer",
            "robert@example.com",
            "contributor",
            "rewrite",
        ],
        &format!("{BOB} = Robert <robert@example.com>\n"),
    );
    fsck(&mirror);
    let after = commits(&mirror);
    for (subject, expected, changed) in [
        ("signed base", "G", false),
        // unsigned by the remove above, not changed by this rewrite
        ("alice adds secret", "N", false),
        ("signed by bob", "G", true),
        ("signed by alice", "G", true),
        ("bob commits for alice", "G", true),
        ("alice commits for bob", "G", true),
    ] {
        let id = &after[subject].id;
        assert_eq!(id != &before[subject].id, changed, "{subject}");
        assert_eq!(signature_status(&mirror, &key, id), expected, "{subject}");
    }

    // nothing to prune: signed commits keep their id and signature
    let refs_before = refs(&mirror);
    repo.reforge_ok(
        &mirror,
        &["--sign-committer", "alice@example.com", "prune-empty"],
    );
    assert_eq!(refs(&mirror), refs_before);
}

#[test]
fn rewrite_drops_signatures_of_unselected_committers() {
    let (repo, key) = create_fixture("sign-dropped");
    let mirror = repo.packed_mirror();
    repo.reforge_ok(&mirror, &["remove", "--file", "secret.txt"]);
    fsck(&mirror);
    let after = commits(&mirror);
    assert_eq!(
        signature_status(&mirror, &key, &after["signed base"].id),
        "G"
    );
    for subject in ["signed by bob", "signed by alice"] {
        assert!(
            !has_signature_header(&mirror, &after[subject].id),
            "{subject}"
        );
    }
}

#[test]
fn sign_with_key_from_global_config() {
    let (repo, key) = create_fixture("sign-global");
    let config = format!(
        "[gpg]\n\tformat = ssh\n[user]\n\tsigningKey = {}\n",
        key.private.display()
    );

    // the file GIT_CONFIG_GLOBAL points to
    fs::write(repo.root.join("gitconfig"), &config).unwrap();
    let mirror = repo.loose_mirror();
    let before = commits(&mirror);
    repo.reforge_ok(
        &mirror,
        &[
            "--sign-committer",
            "alice@example.com",
            "remove",
            "--file",
            "secret.txt",
        ],
    );
    check_signatures(&mirror, &key, &before);

    // XDG_CONFIG_HOME/git/config without GIT_CONFIG_GLOBAL
    fs::write(repo.root.join("gitconfig"), "").unwrap();
    fs::create_dir_all(repo.root.join("xdg/git")).unwrap();
    fs::write(repo.root.join("xdg/git/config"), &config).unwrap();
    let mirror = repo.packed_mirror();
    let before = commits(&mirror);
    let output = Command::new(env!("CARGO_BIN_EXE_gitreforge"))
        .tap(|c| isolate(c, &repo.root))
        .env_remove("GIT_CONFIG_GLOBAL")
        .args(["--sign-committer", "alice@example.com"])
        .arg(&mirror)
        .args(["remove", "--file", "secret.txt"])
        .current_dir(&repo.root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    check_signatures(&mirror, &key, &before);
}

trait Tap {
    fn tap(&mut self, f: impl FnOnce(&mut Self)) -> &mut Self;
}

impl Tap for Command {
    fn tap(&mut self, f: impl FnOnce(&mut Self)) -> &mut Self {
        f(self);
        self
    }
}

struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_agent(repo: &TestRepo, key: &Key) -> Agent {
    let socket = repo.root.join("agent.sock");
    let child = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let agent = Agent { child, socket };
    let start = Instant::now();
    while !agent.socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "ssh-agent did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = Command::new("ssh-add")
        .arg("-q")
        .arg(&key.private)
        .env("SSH_AUTH_SOCK", &agent.socket)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    agent
}

#[test]
fn sign_with_ssh_agent() {
    let (repo, key) = create_fixture("sign-agent");
    let agent = start_agent(&repo, &key);
    let socket = agent.socket.to_str().unwrap();

    // configured public key, from a file and inline, or the agent's first key
    let public_key = fs::read_to_string(&key.public).unwrap();
    let public_key = public_key.trim();
    for signing_key in [
        Some(key.public.to_str().unwrap().to_owned()),
        Some(format!("key::{public_key}")),
        Some(public_key.to_owned()),
        None,
    ] {
        let mirror = repo.packed_mirror();
        git(&mirror, &["config", "gpg.format", "ssh"]);
        if let Some(signing_key) = &signing_key {
            git(&mirror, &["config", "user.signingKey", signing_key]);
        }
        let before = commits(&mirror);
        repo.reforge_env_ok(
            &mirror,
            &[("SSH_AUTH_SOCK", socket)],
            &[
                "--sign-committer",
                "alice@example.com",
                "remove",
                "--file",
                "secret.txt",
            ],
        );
        check_signatures(&mirror, &key, &before);
        fs::remove_dir_all(&mirror).unwrap();
    }

    // a configured key that the agent doesn't have
    let other = repo.root.join("other");
    Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&other)
        .output()
        .unwrap();
    let mirror = repo.packed_mirror();
    git(&mirror, &["config", "gpg.format", "ssh"]);
    git(
        &mirror,
        &[
            "config",
            "user.signingKey",
            &format!("{}.pub", other.display()),
        ],
    );
    let before = refs(&mirror);
    let output = repo.reforge_with_env(
        &mirror,
        &[("SSH_AUTH_SOCK", socket)],
        &[
            "--sign-committer",
            "alice@example.com",
            "remove",
            "--file",
            "secret.txt",
        ],
    );
    assert!(!output.status.success());
    assert_eq!(refs(&mirror), before);
}

#[test]
fn signing_configuration_errors() {
    let (repo, key) = create_fixture("sign-errors");
    let mirror = repo.loose_mirror();
    let before = refs(&mirror);
    let run = || {
        let output = repo.reforge(
            &mirror,
            &[
                "--sign-committer",
                "alice@example.com",
                "remove",
                "--file",
                "secret.txt",
            ],
        );
        assert!(!output.status.success());
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // OpenPGP is the default format
    assert!(run().contains("gpg.format=ssh"));

    git(&mirror, &["config", "gpg.format", "ssh"]);
    git(&mirror, &["config", "gpg.ssh.program", "/usr/bin/false"]);
    assert!(run().contains("gpg.ssh.program"));

    git(&mirror, &["config", "--unset", "gpg.ssh.program"]);
    let missing = key.private.with_file_name("missing-key");
    git(
        &mirror,
        &["config", "user.signingKey", missing.to_str().unwrap()],
    );
    assert!(run().contains("missing-key"));

    // without a key and without an agent
    git(&mirror, &["config", "--unset", "user.signingKey"]);
    assert!(run().contains("SSH_AUTH_SOCK"));

    assert_eq!(refs(&mirror), before);
}
