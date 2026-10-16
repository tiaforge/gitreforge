use std::{
    error::Error,
    path::PathBuf,
    sync::mpsc::{Sender, channel},
    thread,
};

use rustc_hash::{FxHashMap, FxHashSet};

use gitreforgelib::{
    Repository, WriteObject,
    objs::{CommitEditable, CommitHash, TreeHash},
};

use crate::signing::SigningPolicy;

/// A commit of the rewritten history.
struct NewCommit {
    tree: TreeHash,
    parents: Vec<CommitHash>,
    /// One more than the highest generation of the parents, so an ancestor
    /// always has a lower generation than its descendants.
    generation: u32,
}

/// Whether `ancestor` is reachable from `descendant` in the rewritten history.
fn is_ancestor(
    commits: &FxHashMap<CommitHash, NewCommit>,
    ancestor: &CommitHash,
    descendant: &CommitHash,
) -> bool {
    let min_generation = commits[ancestor].generation;
    let mut stack = vec![descendant];
    let mut seen: FxHashSet<&CommitHash> = FxHashSet::default();
    while let Some(hash) = stack.pop() {
        if hash == ancestor {
            return true;
        }
        let commit = &commits[hash];
        if commit.generation <= min_generation {
            continue;
        }
        for parent in &commit.parents {
            if seen.insert(parent) {
                stack.push(parent);
            }
        }
    }
    false
}

fn find_empty_commits(
    repository: &mut Repository,
    tx: Sender<WriteObject>,
    signing_policy: &SigningPolicy,
) -> FxHashMap<CommitHash, CommitHash> {
    let mut rewritten_commits: FxHashMap<CommitHash, CommitHash> = FxHashMap::default();
    let mut pruned_commits: FxHashSet<CommitHash> = FxHashSet::default();
    let mut new_commits: FxHashMap<CommitHash, NewCommit> = FxHashMap::default();

    for mut commit in repository
        .commits_topo_prefetched()
        .map(CommitEditable::create)
    {
        let base_hash = commit.base_hash().clone();
        let original_parents = commit.parents();

        // parents that were pruned can collapse into the same commit
        let mut parents: Vec<CommitHash> = Vec::with_capacity(original_parents.len());
        let mut pruned_parents: Vec<CommitHash> = Vec::new();
        for parent in &original_parents {
            let was_pruned = pruned_commits.contains(parent);
            let parent = rewritten_commits.get(parent).unwrap_or(parent);
            if !parents.contains(parent) {
                parents.push(parent.clone());
            }
            if was_pruned {
                pruned_parents.push(parent.clone());
            }
        }

        // a parent that pruning replaced by an ancestor of another parent
        // merges nothing anymore
        for pruned_parent in &pruned_parents {
            if parents.len() > 1
                && parents.iter().any(|other| {
                    other != pruned_parent && is_ancestor(&new_commits, pruned_parent, other)
                })
            {
                parents.retain(|parent| parent != pruned_parent);
            }
        }

        let commit_tree = commit.tree();
        if parents.len() == 1 && new_commits[&parents[0]].tree == commit_tree {
            rewritten_commits.insert(base_hash.clone(), parents.pop().unwrap());
            pruned_commits.insert(base_hash);
            continue;
        }

        if parents != original_parents {
            commit.set_parents(parents.clone());
        }

        signing_policy
            .sign_if_selected(&mut commit)
            .expect("failed to sign commit");

        let w: WriteObject = commit.into();

        let new_hash: CommitHash = w.hash.clone().into();
        let generation = parents
            .iter()
            .map(|parent| new_commits[parent].generation)
            .max()
            .unwrap_or(0)
            + 1;
        new_commits.insert(
            new_hash.clone(),
            NewCommit {
                tree: commit_tree,
                parents,
                generation,
            },
        );

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
