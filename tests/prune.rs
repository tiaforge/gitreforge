mod common;

use std::path::PathBuf;

use common::{TestRepo, commits, fsck, git, refs};

fn mirrors(repo: &TestRepo) -> [(&'static str, PathBuf); 2] {
    [
        ("loose", repo.loose_mirror()),
        ("packed", repo.packed_mirror()),
    ]
}

/// Subject of the commit a ref points to, through tags.
fn subject(repo: &std::path::Path, rev: &str) -> String {
    git(repo, &["log", "-1", "--format=%s", rev])
}

#[test]
fn prune_removes_empty_commits_and_moves_refs() {
    let repo = TestRepo::create("prune-linear");
    repo.write("a.txt", "a\n");
    repo.commit("a");
    repo.commit("empty 1");
    repo.git(&["tag", "light-on-empty"]);
    repo.write("c.txt", "c\n");
    repo.commit("c");
    repo.commit("empty 2");
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["tag", "-a", "annotated-on-empty", "-m", "tag message"],
    );
    repo.write("c.txt", "c2\n");
    repo.commit("c2");
    repo.commit("empty 3");
    repo.commit("empty 4");

    for (kind, mirror) in mirrors(&repo) {
        let before = commits(&mirror);
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        let after = commits(&mirror);

        assert_eq!(after.keys().collect::<Vec<_>>(), ["a", "c", "c2"], "{kind}");
        // nothing before the first empty commit changes
        assert_eq!(after["a"].id, before["a"].id, "{kind}");
        assert_eq!(after["c"].parents, ["a"], "{kind}");
        assert_eq!(after["c2"].parents, ["c"], "{kind}");
        for (subject, commit) in &after {
            assert_eq!(commit.metadata, before[subject].metadata, "{kind}");
            assert_eq!(commit.files, before[subject].files, "{kind}");
        }

        assert_eq!(subject(&mirror, "main"), "c2", "{kind}");
        assert_eq!(subject(&mirror, "light-on-empty"), "a", "{kind}");
        assert_eq!(subject(&mirror, "annotated-on-empty"), "c", "{kind}");
        assert_eq!(
            git(&mirror, &["cat-file", "-t", "annotated-on-empty"]),
            "tag"
        );
        assert!(
            git(&mirror, &["cat-file", "-p", "annotated-on-empty"]).ends_with("tag message"),
            "{kind}"
        );
    }
}

#[test]
fn prune_without_empty_commits_changes_nothing() {
    let repo = TestRepo::create("prune-nothing");
    repo.write("a.txt", "a\n");
    repo.commit("a");
    repo.write("b.txt", "b\n");
    repo.commit("b");
    repo.git(&["checkout", "-q", "-b", "side", "HEAD~1"]);
    repo.write("s.txt", "s\n");
    repo.commit("side");
    repo.git(&["checkout", "-q", "main"]);
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "--no-ff", "-m", "merge", "side"],
    );

    for (kind, mirror) in mirrors(&repo) {
        let before = refs(&mirror);
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        assert_eq!(refs(&mirror), before, "{kind}");
    }
}

#[test]
fn prune_keeps_root_commits_and_merges() {
    let repo = TestRepo::create("prune-merges");
    repo.commit("empty root");
    repo.write("a.txt", "a\n");
    repo.commit("a");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("s.txt", "s\n");
    repo.commit("side change");
    repo.commit("side empty");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("m.txt", "m\n");
    repo.commit("main change");
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    // a merge whose tree equals its first parent is still a merge
    repo.git(&["checkout", "-q", "-b", "ours", "main~1"]);
    repo.write("o.txt", "o\n");
    repo.commit("ours change");
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "-s", "ours", "-m", "merge ours", "main"],
    );

    for (kind, mirror) in mirrors(&repo) {
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        let after = commits(&mirror);
        assert!(!after.contains_key("side empty"), "{kind}");
        assert_eq!(after["empty root"].parents, Vec::<String>::new(), "{kind}");
        assert_eq!(
            after["merge side"].parents,
            ["main change", "side change"],
            "{kind}"
        );
        assert_eq!(
            after["merge ours"].parents,
            ["ours change", "merge side"],
            "{kind}"
        );
        assert_eq!(subject(&mirror, "side"), "side change", "{kind}");
    }
}

/// A merge of a branch that only had empty commits merges nothing after
/// pruning; it must not end up with the same parent twice.
#[test]
fn prune_merge_of_only_empty_commits() {
    let repo = TestRepo::create("prune-empty-merge");
    repo.write("a.txt", "a\n");
    repo.commit("a");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.commit("side empty");
    repo.git(&["checkout", "-q", "main"]);
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "--no-ff", "-m", "merge empty side", "side"],
    );
    repo.write("b.txt", "b\n");
    repo.commit("b");

    for (kind, mirror) in mirrors(&repo) {
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        let after = commits(&mirror);
        assert_eq!(after.keys().collect::<Vec<_>>(), ["a", "b"], "{kind}");
        assert_eq!(after["b"].parents, ["a"], "{kind}");
        assert_eq!(subject(&mirror, "side"), "a", "{kind}");
    }
}

/// When the merged branch only had empty commits but the other side moved on,
/// pruning makes the merge's second parent an ancestor of its first. The
/// merge then merges nothing and is pruned as well.
#[test]
fn prune_merge_of_only_empty_commits_after_main_moved() {
    let repo = TestRepo::create("prune-empty-merge-moved");
    repo.write("a.txt", "a\n");
    repo.commit("a");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.commit("side empty 1");
    repo.commit("side empty 2");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("m.txt", "m\n");
    repo.commit("main change");
    repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &["merge", "-q", "--no-ff", "-m", "merge empty side", "side"],
    );
    repo.write("b.txt", "b\n");
    repo.commit("b");
    // a merge that was already redundant before pruning stays
    let redundant = repo.with_identity(
        common::AUTHOR,
        common::AUTHOR,
        &[
            "commit-tree",
            "-p",
            "main~1",
            "-p",
            "main~3",
            "-m",
            "merge redundant",
            "main~1^{tree}",
        ],
    );
    repo.git(&["branch", "redundant", &redundant]);

    for (kind, mirror) in mirrors(&repo) {
        let before = commits(&mirror);
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        let after = commits(&mirror);
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            ["a", "b", "main change", "merge redundant"],
            "{kind}"
        );
        assert_eq!(after["b"].parents, ["main change"], "{kind}");
        assert_eq!(after["main change"].id, before["main change"].id, "{kind}");
        assert_eq!(subject(&mirror, "side"), "a", "{kind}");
        assert_eq!(subject(&mirror, "main"), "b", "{kind}");
        assert_eq!(
            after["merge redundant"].parents,
            ["main change", "a"],
            "{kind}"
        );
    }
}

#[test]
fn prune_after_remove() {
    let repo = TestRepo::create("prune-after-remove");
    repo.write("keep.txt", "1\n");
    repo.commit("one");
    repo.write("secret.txt", "secret\n");
    repo.commit("add secret");
    repo.write("secret.txt", "secret 2\n");
    repo.commit("change secret");
    repo.write("keep.txt", "2\n");
    repo.commit("two");

    for (kind, mirror) in mirrors(&repo) {
        repo.reforge_ok(&mirror, &["remove", "--file", "secret.txt"]);
        repo.reforge_ok(&mirror, &["prune-empty"]);
        fsck(&mirror);
        let after = commits(&mirror);
        assert_eq!(after.keys().collect::<Vec<_>>(), ["one", "two"], "{kind}");
        assert_eq!(after["two"].parents, ["one"], "{kind}");
        assert_eq!(after["two"].paths(), ["keep.txt"], "{kind}");
    }
}
