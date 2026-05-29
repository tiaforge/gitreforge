use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

struct TestRepo {
    root: PathBuf,
    bare: PathBuf,
    work: PathBuf,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl TestRepo {
    fn create(name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gitreforge-remove-regression-{}-{}-{}",
            name,
            std::process::id(),
            nonce
        ));
        let bare = root.join("repo.git");
        let work = root.join("work");

        fs::create_dir_all(&root)?;
        run(Command::new("git").arg("init").arg("--bare").arg(&bare))?;
        run(Command::new("git").arg("clone").arg(&bare).arg(&work))?;
        run(Command::new("git")
            .args(["config", "user.email", "a@example.com"])
            .current_dir(&work))?;
        run(Command::new("git")
            .args(["config", "user.name", "A"])
            .current_dir(&work))?;
        run(Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&work))?;
        run(Command::new("git")
            .args(["config", "tag.gpgsign", "false"])
            .current_dir(&work))?;

        Ok(Self { root, bare, work })
    }

    fn commit_file(&self, contents: &str, message: &str) -> Result<(), Box<dyn std::error::Error>> {
        fs::write(self.work.join("keep.txt"), contents)?;
        run(Command::new("git")
            .arg("add")
            .arg("keep.txt")
            .current_dir(&self.work))?;
        run(Command::new("git")
            .arg("commit")
            .arg("-m")
            .arg(message)
            .current_dir(&self.work))?;
        run(Command::new("git")
            .arg("push")
            .arg("origin")
            .arg("HEAD:main")
            .current_dir(&self.work))?;
        Ok(())
    }
}

fn run(command: &mut Command) -> Result<(), Box<dyn std::error::Error>> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(format!(
        "command failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        command,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn run_gitreforge_remove_dry_run(repo: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run(Command::new(env!("CARGO_BIN_EXE_gitreforge"))
        .arg("--dry-run")
        .arg(repo)
        .arg("remove")
        .arg("--file")
        .arg("does-not-exist"))
}

#[test]
fn remove_no_match_does_not_overflow_on_single_commit_repo()
-> Result<(), Box<dyn std::error::Error>> {
    let repo = TestRepo::create("single")?;
    repo.commit_file("one\n", "one")?;

    run_gitreforge_remove_dry_run(&repo.bare)
}

#[test]
fn remove_no_match_does_not_overflow_on_two_commit_repo() -> Result<(), Box<dyn std::error::Error>>
{
    let repo = TestRepo::create("two")?;
    repo.commit_file("one\n", "one")?;
    repo.commit_file("two\n", "two")?;

    run_gitreforge_remove_dry_run(&repo.bare)
}
