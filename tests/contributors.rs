mod common;

use std::{collections::BTreeSet, path::PathBuf};

use common::{TestRepo, commits, fsck, git, refs};

const ALICE: &str = "Alice <alice@example.com>";
const BOB: &str = "Bob <bob@example.com>";
const CAROL: &str = "Carol Old <carol@old.example.com>";

fn create_fixture(name: &str) -> TestRepo {
    let repo = TestRepo::create(name);
    repo.write("a.txt", "a\n");
    repo.commit_as("by alice", ALICE, ALICE);
    repo.write("b.txt", "b\n");
    repo.commit_as("bob for alice", ALICE, BOB);
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("c.txt", "c\n");
    repo.commit_as("by carol", CAROL, CAROL);
    repo.git(&["checkout", "-q", "main"]);
    repo.write("b.txt", "b2\n");
    repo.commit_as("by bob", BOB, BOB);
    repo.with_identity(
        BOB,
        BOB,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "merge carol\n\nwith a body",
            "side",
        ],
    );
    repo.with_identity(BOB, BOB, &["tag", "-a", "v1", "-m", "release"]);
    repo.write("d.txt", "d\n");
    repo.git(&["add", "d.txt"]);
    // extra header that has to survive the rewrite
    repo.with_identity(
        CAROL,
        ALICE,
        &[
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "-q",
            "-m",
            "encoded",
        ],
    );
    repo.with_identity(
        ALICE,
        ALICE,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "--allow-empty-message",
            "-m",
            "",
        ],
    );
    repo
}

fn mirrors(repo: &TestRepo) -> [(&'static str, PathBuf); 2] {
    [
        ("loose", repo.loose_mirror()),
        ("packed", repo.packed_mirror()),
    ]
}

fn identities(repo: &std::path::Path) -> BTreeSet<String> {
    git(repo, &["log", "--all", "--format=%an <%ae>%n%cn <%ce>"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn list_contributors() {
    let repo = create_fixture("contributors-list");
    for (kind, mirror) in mirrors(&repo) {
        let output = repo.reforge_ok(&mirror, &["contributor", "list"]);
        let listed: Vec<&str> = output.lines().collect();
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted, "{kind}");
        assert_eq!(
            listed
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
            identities(&mirror),
            "{kind}"
        );
        assert_eq!(output.lines().count(), 3, "{kind}");
    }
}

#[test]
fn rewrite_contributors() {
    let repo = create_fixture("contributors-rewrite");
    for (kind, mirror) in mirrors(&repo) {
        let before = commits(&mirror);
        // blank lines and surrounding spaces are fine, identities that map
        // to themselves are ignored
        let mappings = format!(
            "\n  {CAROL}   =   Carol New <carol@new.example.com>  \n\n{BOB} = Robert <robert@example.com>\n{ALICE} = {ALICE}"
        );
        repo.reforge_stdin_ok(&mirror, &["contributor", "rewrite"], &mappings);
        fsck(&mirror);
        let after = commits(&mirror);

        let rename = |identity: &str| {
            identity
                .replace(CAROL, "Carol New <carol@new.example.com>")
                .replace(BOB, "Robert <robert@example.com>")
        };
        assert_eq!(
            before.keys().collect::<Vec<_>>(),
            after.keys().collect::<Vec<_>>()
        );
        for (subject, old) in &before {
            let new = &after[subject];
            assert_eq!(new.metadata, rename(&old.metadata), "{kind} {subject}");
            assert_eq!(new.files, old.files, "{kind} {subject}");
            assert_eq!(new.parents, old.parents, "{kind} {subject}");
        }
        // only Alice before the first renamed commit
        assert_eq!(after["by alice"].id, before["by alice"].id, "{kind}");
        assert_ne!(
            after["bob for alice"].id, before["bob for alice"].id,
            "{kind}"
        );

        let raw = git(&mirror, &["cat-file", "commit", &after["encoded"].id]);
        assert!(
            raw.contains("\nencoding ISO-8859-1\n\nencoded"),
            "{kind} {raw}"
        );

        assert_eq!(git(&mirror, &["cat-file", "-t", "v1"]), "tag");
        assert_eq!(
            git(&mirror, &["rev-parse", "v1^{commit}"]),
            after["merge carol"].id,
            "{kind}"
        );
        assert_eq!(
            identities(&mirror),
            [
                ALICE,
                "Carol New <carol@new.example.com>",
                "Robert <robert@example.com>"
            ]
            .map(str::to_owned)
            .into()
        );

        let map = repo.commit_map();
        assert_eq!(map.len(), before.len() - 1, "{kind}");
        for (subject, old) in &before {
            if subject != "by alice" {
                assert_eq!(map[&old.id], after[subject].id, "{kind} {subject}");
            }
        }
    }
}

#[test]
fn rewrite_without_matching_identity_changes_nothing() {
    let repo = create_fixture("contributors-no-match");
    for (kind, mirror) in mirrors(&repo) {
        let before = refs(&mirror);
        repo.reforge_stdin_ok(
            &mirror,
            &["contributor", "rewrite"],
            "Nobody <nobody@example.com> = Somebody <somebody@example.com>\n",
        );
        fsck(&mirror);
        assert_eq!(refs(&mirror), before, "{kind}");
    }
}

#[test]
fn rewrite_dry_run_changes_nothing() {
    let repo = create_fixture("contributors-dry-run");
    let mirror = repo.packed_mirror();
    let before = refs(&mirror);
    let packs = std::fs::read_dir(mirror.join("objects/pack"))
        .unwrap()
        .count();
    repo.reforge_stdin_ok(
        &mirror,
        &["--dry-run", "contributor", "rewrite"],
        &format!("{BOB} = Robert <robert@example.com>\n"),
    );
    assert_eq!(refs(&mirror), before);
    assert_eq!(
        std::fs::read_dir(mirror.join("objects/pack"))
            .unwrap()
            .count(),
        packs
    );
    assert!(!repo.root.join("object-id-map.old-new.txt").exists());
}

#[test]
fn rewrite_rejects_malformed_mappings() {
    let repo = create_fixture("contributors-malformed");
    let mirror = repo.loose_mirror();
    let before = refs(&mirror);
    let output = repo.reforge_with_stdin(
        &mirror,
        &["contributor", "rewrite"],
        &format!("{BOB} = Robert <robert@example.com>\n{ALICE} Alicia <alicia@example.com>\n"),
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("malformed"), "{stderr}");
    assert_eq!(refs(&mirror), before);
}
