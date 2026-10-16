mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use common::{Commit, TestRepo, commits, file_path, fsck, git, refs};

/// Several versions of a long file, so that a repacked mirror stores
/// deltas.
fn long_file(version: usize) -> String {
    (0..200)
        .map(|line| {
            if line % 50 == version % 50 {
                format!("changed line {line} in version {version}\n")
            } else {
                format!("line {line} with some content to compress\n")
            }
        })
        .collect()
}

/// History with a side branch, a merge, nested directories, a directory that
/// only contains a file to be removed, and lightweight and annotated tags.
fn create_fixture(name: &str) -> TestRepo {
    let repo = TestRepo::create(name);
    repo.write("README.md", &long_file(0));
    repo.write("src/main.rs", &long_file(0));
    repo.write("src/secret.key", "key 1\n");
    repo.write("src/debug-2.log", "log\n");
    repo.write("config/secret.txt", "password\n");
    repo.write("docs/a.md", &long_file(0));
    repo.write("docs/sub/b.md", "b\n");
    repo.write("build/out.o", "o\n");
    repo.write("build-x/out.o", "x\n");
    repo.write("rebuild/out.o", "r\n");
    repo.write("logs/debug-1.log", "log\n");
    repo.write("vendor/private/lib.c", "private\n");
    repo.write("vendor/public/lib.c", "public\n");
    repo.write(
        "vendor/public/private/readme",
        "not the private vendor dir\n",
    );
    repo.commit("initial");

    repo.write("src/main.rs", &long_file(1));
    repo.commit("edit main");
    repo.git(&["tag", "v1"]);

    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("secret.txt", "root secret\n");
    repo.write("docs/a.md", &long_file(2));
    repo.commit("feature");
    repo.write("docs/sub/secret.txt", "nested secret\n");
    repo.commit("feature 2");

    repo.git(&["checkout", "-q", "main"]);
    repo.write("tools/secret.key", "key 2\n");
    repo.write("README.md", &long_file(3));
    repo.commit("more");
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"],
    );
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["tag", "-a", "v2", "-m", "release 2"],
    );

    repo.delete("config");
    repo.write("src/main.rs", &long_file(4));
    repo.commit("remove config");
    repo.write("src/main.rs", &long_file(5));
    repo.commit("last");
    repo
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Loose objects in, new pack out
    Loose,
    /// Delta-compressed pack in, new pack out
    Packed,
    /// Delta-compressed pack in, loose objects out
    PackedToLoose,
}

const MODES: [Mode; 3] = [Mode::Loose, Mode::Packed, Mode::PackedToLoose];

impl Mode {
    fn mirror(self, repo: &TestRepo) -> PathBuf {
        match self {
            Mode::Loose => repo.loose_mirror(),
            Mode::Packed | Mode::PackedToLoose => repo.packed_mirror(),
        }
    }

    fn env(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Mode::PackedToLoose => &[("GITREFORGE_PACK", "0")],
            _ => &[],
        }
    }
}

/// All directories that hold at least one of the files.
fn directories_of(files: &[String]) -> Vec<String> {
    let mut directories = BTreeSet::new();
    for file in files {
        let path = file_path(file);
        for (i, _) in path.match_indices('/') {
            directories.insert(format!("{}/", &path[..i]));
        }
    }
    directories.into_iter().collect()
}

/// Whether a commit's own tree or any of its ancestors changed.
fn changed(
    subject: &str,
    before: &BTreeMap<String, Commit>,
    after: &BTreeMap<String, Commit>,
) -> bool {
    before[subject].files != after[subject].files
        || before[subject]
            .parents
            .iter()
            .any(|parent| changed(parent, before, after))
}

/// Runs a removal and checks the rewritten history against the original:
/// every commit is kept with its metadata and parents, its tree has exactly
/// the files that `removed` doesn't select, no empty directories are left
/// behind, and only commits whose history changed get a new id.
fn check_remove(repo: &TestRepo, mode: Mode, args: &[&str], removed: impl Fn(&str) -> bool) {
    let mirror = mode.mirror(repo);
    let before = commits(&mirror);
    let refs_before = refs(&mirror);

    let mut command = vec!["remove"];
    command.extend_from_slice(args);
    repo.reforge_env_ok(&mirror, mode.env(), &command);

    fsck(&mirror);
    let after = commits(&mirror);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "{args:?} {mode:?}"
    );

    let commit_map = repo.commit_map();
    for (subject, old) in &before {
        let new = &after[subject];
        let context = format!("{args:?} {mode:?} in commit {subject:?}");
        let expected: Vec<String> = old
            .files
            .iter()
            .filter(|f| !removed(file_path(f)))
            .cloned()
            .collect();
        assert_eq!(new.files, expected, "{context}");
        assert_eq!(new.directories, directories_of(&new.files), "{context}");
        assert_eq!(new.metadata, old.metadata, "{context}");
        assert_eq!(new.parents, old.parents, "{context}");

        if changed(subject, &before, &after) {
            assert_ne!(new.id, old.id, "{context}");
            assert_eq!(commit_map.get(&old.id), Some(&new.id), "{context}");
        } else {
            assert_eq!(new.id, old.id, "{context}");
            assert!(!commit_map.contains_key(&old.id), "{context}");
        }
    }

    // every ref still exists and resolves to the rewritten commit
    let refs_after = refs(&mirror);
    assert_eq!(
        refs_before.keys().collect::<Vec<_>>(),
        refs_after.keys().collect::<Vec<_>>()
    );
    for name in refs_after.keys() {
        let subject = git(&mirror, &["log", "-1", "--format=%s", name]);
        assert_eq!(
            git(&mirror, &["rev-parse", &format!("{name}^{{commit}}")]),
            after[&subject].id,
            "{name}"
        );
    }
    assert_eq!(git(&mirror, &["cat-file", "-t", "v2"]), "tag");
    assert_eq!(
        git(&mirror, &["cat-file", "-p", "v2"]).lines().last(),
        Some("release 2")
    );

    // the result can be rewritten again, which reads the pack written above
    repo.reforge_env_ok(&mirror, mode.env(), &["prune-empty"]);
    fsck(&mirror);
}

fn check_remove_all_modes(name: &str, args: &[&str], removed: impl Fn(&str) -> bool + Copy) {
    for mode in MODES {
        let repo = create_fixture(name);
        check_remove(&repo, mode, args, removed);
    }
}

#[test]
fn remove_file_by_name() {
    check_remove_all_modes("file-name", &["--file", "secret.txt"], |p| {
        p.rsplit('/').next() == Some("secret.txt")
    });
}

#[test]
fn remove_file_by_absolute_path() {
    check_remove_all_modes("file-absolute", &["--file", "/config/secret.txt"], |p| {
        p == "config/secret.txt"
    });
    check_remove_all_modes("file-absolute-root", &["--file", "/secret.txt"], |p| {
        p == "secret.txt"
    });
}

#[test]
fn remove_file_by_relative_path() {
    check_remove_all_modes("file-relative", &["--file", "sub/secret.txt"], |p| {
        p == "docs/sub/secret.txt"
    });
    check_remove_all_modes("file-relative-wildcard", &["--file", "src/debug-*"], |p| {
        p == "src/debug-2.log"
    });
}

#[test]
fn remove_file_with_leading_wildcard() {
    check_remove_all_modes("file-suffix", &["--file", "*.key"], |p| p.ends_with(".key"));
    check_remove_all_modes("file-suffix-dir", &["--file", "*/src/secret.key"], |p| {
        p == "src/secret.key"
    });
}

#[test]
fn remove_file_with_trailing_wildcard() {
    check_remove_all_modes("file-prefix", &["--file", "debug-*"], |p| {
        p.rsplit('/').next().unwrap().starts_with("debug-")
    });
    check_remove_all_modes("file-prefix-absolute", &["--file", "/logs/debug-*"], |p| {
        p == "logs/debug-1.log"
    });
}

#[test]
fn remove_directory_by_relative_path() {
    check_remove_all_modes("dir-relative", &["--directory", "vendor/private"], |p| {
        p.starts_with("vendor/private/")
    });
    check_remove_all_modes("dir-name", &["--directory", "private/"], |p| {
        p.starts_with("vendor/private/") || p.starts_with("vendor/public/private/")
    });
}

#[test]
fn remove_directory_by_absolute_path() {
    check_remove_all_modes("dir-absolute", &["--directory", "/docs"], |p| {
        p.starts_with("docs/")
    });
    check_remove_all_modes("dir-absolute-nested", &["--directory", "/docs/sub/"], |p| {
        p.starts_with("docs/sub/")
    });
}

#[test]
fn remove_directory_with_wildcards() {
    check_remove_all_modes("dir-prefix", &["--directory", "build*"], |p| {
        p.starts_with("build/") || p.starts_with("build-x/")
    });
    check_remove_all_modes(
        "dir-prefix-absolute",
        &["--directory", "/vendor/pr*"],
        |p| p.starts_with("vendor/private/"),
    );
    check_remove_all_modes("dir-suffix", &["--directory", "*build"], |p| {
        p.starts_with("build/") || p.starts_with("rebuild/")
    });
}

#[test]
fn remove_by_regex() {
    check_remove_all_modes("regex", &["--regex", r"(^|/)debug-[^/]+\.log$"], |p| {
        p.ends_with(".log")
    });
    // paths are matched with a leading slash
    check_remove_all_modes(
        "regex-anchored",
        &["--regex", r"^/src/.*\.(key|log)$"],
        |p| p.starts_with("src/") && (p.ends_with(".key") || p.ends_with(".log")),
    );
}

#[test]
fn remove_with_combined_matchers() {
    check_remove_all_modes(
        "combined",
        &[
            "--file",
            "secret.txt",
            "--file",
            "README.md",
            "--directory",
            "/vendor",
            "--regex",
            r"\.key$",
            "--regex",
            r"^/docs/",
        ],
        |p| {
            p.ends_with("secret.txt")
                || p == "README.md"
                || p.starts_with("vendor/")
                || p.ends_with(".key")
                || p.starts_with("docs/")
        },
    );
}

#[test]
fn remove_everything_leaves_empty_root_trees() {
    check_remove_all_modes("everything", &["--regex", "."], |_| true);
}

#[test]
fn remove_without_matches_changes_nothing() {
    let repo = create_fixture("no-match");
    let mirror = repo.packed_mirror();
    let refs_before = refs(&mirror);
    repo.reforge_ok(&mirror, &["remove", "--file", "does-not-exist"]);
    fsck(&mirror);
    assert_eq!(refs(&mirror), refs_before);
    assert!(repo.commit_map().is_empty());
}

fn snapshot(repo: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(repo, &mut out);
    out.sort();
    out
}

#[test]
fn dry_run_does_not_change_the_repository() {
    let repo = create_fixture("dry-run");
    for mirror in [repo.loose_mirror(), repo.packed_mirror()] {
        let before = snapshot(&mirror);
        repo.reforge_ok(
            &mirror,
            &[
                "--dry-run",
                "remove",
                "--file",
                "secret.txt",
                "--directory",
                "docs",
            ],
        );
        repo.reforge_ok(&mirror, &["-d", "prune-empty"]);
        assert!(snapshot(&mirror) == before);
        assert!(!repo.root.join("object-id-map.old-new.txt").exists());
    }
}

#[test]
fn remove_keeps_tags_on_trees_and_blobs() {
    let repo = create_fixture("object-tags");
    repo.git(&["tag", "blob-light", "HEAD:README.md"]);
    repo.git(&["tag", "tree-light", "HEAD:docs"]);
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["tag", "-a", "tree-annotated", "-m", "a tree", "HEAD^{tree}"],
    );

    for mode in MODES {
        let mirror = mode.mirror(&repo);
        let before = refs(&mirror);
        repo.reforge_env_ok(&mirror, mode.env(), &["remove", "--file", "secret.txt"]);
        fsck(&mirror);
        let after = refs(&mirror);
        for name in [
            "refs/tags/blob-light",
            "refs/tags/tree-light",
            "refs/tags/tree-annotated",
        ] {
            assert_eq!(after[name], before[name], "{name} {mode:?}");
        }
        assert_ne!(after["refs/heads/main"], before["refs/heads/main"]);
        let _ = fs::remove_dir_all(&mirror);
    }
}

#[test]
fn remove_keeps_symbolic_refs() {
    let repo = create_fixture("symbolic");
    let mirror = repo.loose_mirror();
    git(
        &mirror,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/heads/main",
        ],
    );
    repo.reforge_ok(&mirror, &["remove", "--file", "secret.txt"]);
    fsck(&mirror);
    assert_eq!(
        git(&mirror, &["symbolic-ref", "refs/remotes/origin/HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(git(&mirror, &["symbolic-ref", "HEAD"]), "refs/heads/main");
}

#[test]
fn remove_rejects_invalid_regex() {
    let repo = create_fixture("bad-regex");
    let mirror = repo.loose_mirror();
    let refs_before = refs(&mirror);
    let output = repo.reforge(&mirror, &["remove", "--regex", "("]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("regex"), "{stderr}");
    assert_eq!(refs(&mirror), refs_before);
}
