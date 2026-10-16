use std::{
    collections::HashMap,
    error::Error,
    io::{BufRead, stdin},
    path::PathBuf,
    sync::mpsc::channel,
    thread::spawn,
};

use bstr::{BString, ByteSlice, io::BufReadExt};
use gitreforgelib::{
    Repository, WriteObject,
    objs::{CommitEditable, CommitHash},
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::signing::SigningPolicy;

fn split_index(line: &[u8]) -> Option<usize> {
    for (pos, c) in line.iter().enumerate() {
        if *c == b'=' {
            return Some(pos);
        }
    }

    None
}

/// "Name <email>", as in commit headers
fn is_identity(identity: &[u8]) -> bool {
    let Some(start) = identity.find_byte(b'<') else {
        return false;
    };
    start > 1
        && identity[start - 1] == b' '
        && identity.ends_with(b">")
        && identity[start + 1..identity.len() - 1]
            .find_byteset(b"<>")
            .is_none()
}

fn parse_mappings(
    input: impl BufRead,
) -> Result<FxHashMap<Vec<u8>, Vec<u8>>, Box<dyn Error + Send + Sync>> {
    let mut mappings = FxHashMap::default();

    for line in input.byte_lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let malformed = || {
            format!(
                "Line is malformed: `{}`. Pattern: Old Name <old@example.com> = New Name <new@example.com>",
                line.as_bstr()
            )
        };
        let split_pos = split_index(&line).ok_or_else(malformed)?;

        let old = line[0..split_pos].trim().to_owned();
        let new = line[split_pos + 1..].trim().to_owned();
        if !is_identity(&old) || !is_identity(&new) {
            return Err(malformed().into());
        }

        if old != new {
            mappings.insert(old, new);
        }
    }

    Ok(mappings)
}

pub fn rewrite(
    repository_path: PathBuf,
    dry_run: bool,
    signing_policy: &SigningPolicy,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mappings = parse_mappings(stdin().lock())?;

    let (tx, rx) = channel();
    let write_path = repository_path.clone();
    let write_thread =
        spawn(move || Repository::write_commits(write_path, rx.into_iter(), dry_run));

    let mut repository = Repository::create(repository_path);
    let mut rewritten_commits: HashMap<CommitHash, CommitHash, _> = FxHashMap::default();
    for mut commit in repository.commits_topo_prefetched().map(CommitEditable::create) {
        if let Some(new_author) = mappings.get(commit.author_bytes()) {
            commit.set_author(new_author.clone());
        }

        if let Some(new_committer) = mappings.get(commit.committer_bytes()) {
            commit.set_committer(new_committer.clone());
        }

        for (i, parent) in commit.parents().iter().enumerate() {
            if let Some(new_commit_hash) = rewritten_commits.get(parent) {
                commit.set_parent(i, new_commit_hash.clone());
            }
        }

        signing_policy.sign_if_selected(&mut commit)?;

        if commit.has_changes() {
            let old_hash = commit.base_hash().clone();
            let w: WriteObject = commit.into();
            rewritten_commits.insert(old_hash, CommitHash::from(w.hash.clone()));
            tx.send(w).unwrap();
        }
    }

    drop(tx);
    write_thread.join().expect("Failed to write commits");

    if !rewritten_commits.is_empty() {
        repository.update_refs(&rewritten_commits, dry_run);
        Repository::write_rewritten_commits_file(rewritten_commits, dry_run);
    }

    Ok(())
}

pub fn get_contributors(repository_path: PathBuf) -> Result<Vec<BString>, Box<dyn Error>> {
    let mut committers = FxHashSet::default();
    let repository = Repository::create(repository_path);

    for commit in repository.commits_lifo() {
        committers.insert(commit.committer().to_owned());
        committers.insert(commit.author().to_owned());
    }

    let mut committers: Vec<_> = committers.into_iter().collect();
    committers.sort();

    Ok(committers)
}

#[cfg(test)]
mod test {
    use super::parse_mappings;

    #[test]
    fn mappings() {
        let mappings = parse_mappings(
            &b"\n Old <old@example.com>  =  New Name <new@example.com>\r\n\nSame <s@example.com> = Same <s@example.com>"[..],
        )
        .unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(
            mappings[&b"Old <old@example.com>"[..]],
            b"New Name <new@example.com>"
        );
    }

    #[test]
    fn malformed_mappings() {
        for line in [
            "Old <old@example.com> New <new@example.com>",
            "Old <old@example.com> = New",
            "Old <old@example.com> = ",
            "Old = New <new@example.com>",
            "Old <old@example.com> = New <new@example.com> x",
            "Old <old@example.com> = <new@example.com>",
            "Old <old@example.com> = New<new@example.com>",
        ] {
            assert!(parse_mappings(line.as_bytes()).is_err(), "{line}");
        }
    }
}
