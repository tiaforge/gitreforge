use std::{
    error::Error,
    path::PathBuf,
    sync::mpsc::{Sender, channel},
    thread,
};

use rustc_hash::FxHashMap;

use gitreforgelib::{
    Repository, WriteObject,
    objs::{CommitEditable, CommitHash, TreeHash},
};

use crate::signing::SigningPolicy;

fn find_empty_commits(
    repository: &mut Repository,
    tx: Sender<WriteObject>,
    signing_policy: &SigningPolicy,
) -> FxHashMap<CommitHash, CommitHash> {
    let mut rewritten_commits: FxHashMap<CommitHash, CommitHash> = FxHashMap::default();
    let mut commit_trees: FxHashMap<CommitHash, TreeHash> = FxHashMap::default();

    for mut commit in repository.commits_topo_prefetched().map(CommitEditable::create) {
        let base_hash = commit.base_hash().clone();
        let original_parents = commit.parents();

        // parents that were pruned can collapse into the same commit
        let mut parents: Vec<CommitHash> = Vec::with_capacity(original_parents.len());
        for parent in &original_parents {
            let parent = rewritten_commits.get(parent).unwrap_or(parent);
            if !parents.contains(parent) {
                parents.push(parent.clone());
            }
        }

        let commit_tree = commit.tree();
        if parents.len() == 1 && commit_trees[&parents[0]] == commit_tree {
            rewritten_commits.insert(base_hash, parents.pop().unwrap());
            continue;
        }

        if parents != original_parents {
            commit.set_parents(parents);
        }

        signing_policy
            .sign_if_selected(&mut commit)
            .expect("failed to sign commit");

        let w: WriteObject = commit.into();

        let new_hash: CommitHash = w.hash.clone().into();
        commit_trees.insert(new_hash.clone(), commit_tree);

        if base_hash != new_hash {
            rewritten_commits.insert(base_hash, new_hash.clone());
            tx.send(w).unwrap();
        }
    }

    rewritten_commits
}

pub fn remove_empty_commits(
    repository_path: PathBuf,
    dry_run: bool,
    signing_policy: &SigningPolicy,
) -> Result<(), Box<dyn Error>> {
    let write_path = repository_path.clone();
    let (tx, rx) = channel();

    let thread =
        thread::spawn(move || Repository::write_commits(write_path, rx.into_iter(), dry_run));

    let mut repository = Repository::create(repository_path);
    let rewritten_commits = find_empty_commits(&mut repository, tx, signing_policy);

    thread.join().unwrap();

    if !rewritten_commits.is_empty() {
        repository.update_refs(&rewritten_commits, dry_run);
        Repository::write_rewritten_commits_file(rewritten_commits, dry_run);
    }

    Ok(())
}
