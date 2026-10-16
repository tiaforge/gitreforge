use core::panic;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    ops::Deref,
    path::{Path, PathBuf},
    sync::{RwLock, mpsc::channel},
};

use gitreforgelib::{
    Repository, WriteObject,
    objs::{CommitBase, CommitEditable, CommitHash, Tree, TreeEdit, TreeHash},
};
use rayon::prelude::*;
use regex::bytes::RegexSet;
use rustc_hash::{FxBuildHasher, FxHashMap};
use smallvec::SmallVec;

use crate::signing::SigningPolicy;

macro_rules! b {
    ( $x:expr ) => {
        Box::new($x)
    };
}

fn last_index_of(path: &[u8], needle: u8) -> Option<usize> {
    for (i, c) in path.iter().rev().enumerate() {
        if *c == needle {
            return Some(path.len() - i - 1);
        }
    }
    None
}

type DynFn<'a> = Box<dyn Fn(&[u8]) -> bool + Sync + Send + 'a>;
type DynFn2<'a> = Box<dyn Fn(&[u8], &[u8]) -> bool + Sync + Send + 'a>;

fn build_folder_delete_patterns(folders: &[String]) -> DynFn<'_> {
    let mut delete_folder: DynFn = Box::new(|_path| false);

    for folder in folders.iter().map(|f| f.as_bytes()) {
        if folder[0] == b'*' {
            if folder[folder.len() - 1] == b'/' {
                delete_folder = b!(move |path| delete_folder(path) || path.ends_with(&folder[1..]));
            } else {
                // handles trailing slash
                delete_folder = b!(move |path| delete_folder(path)
                    || path[0..path.len() - 1].ends_with(&folder[1..]));
            }
        } else if folder[0] == b'/' && folder[folder.len() - 1] == b'*' {
            delete_folder =
                b!(move |path| delete_folder(path)
                    || path.starts_with(&folder[0..folder.len() - 1]));
        } else if folder[folder.len() - 1] == b'*' {
            // relative path with wildcard: the last components of the path
            // start with the pattern, "build*" matches "/src/build-x/"
            let mut pattern = b"/".to_vec();
            pattern.extend_from_slice(&folder[0..folder.len() - 1]);
            let components = pattern.iter().filter(|c| **c == b'/').count();
            delete_folder = b!(move |path| delete_folder(path) || {
                let path = &path[..path.len() - 1];
                path.iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, c)| **c == b'/')
                    .nth(components - 1)
                    .is_some_and(|(start, _)| path[start..].starts_with(&pattern))
            });
        } else if folder[0] == b'/' {
            // absolute path, no wildcard
            if folder[folder.len() - 1] == b'/' {
                delete_folder = b!(move |path| delete_folder(path) || path.eq(folder));
            } else {
                // handles missing trailing slash
                delete_folder = b!(move |path| delete_folder(path)
                    || path.len() == folder.len() + 1 && path[0..path.len() - 1].eq(folder));
            }
        } else {
            // relative path, no wildcard
            let mut folder: Vec<u8> = folder.to_owned();
            if folder[folder.len() - 1] != b'/' {
                folder.push(b'/');
            }
            if folder[0] != b'/' {
                folder.insert(0, b'/');
            }

            delete_folder = b!(move |path| delete_folder(path) || path.ends_with(&folder));
        }
    }

    delete_folder
}

fn build_regex_pattern(patterns: &[String]) -> DynFn2<'_> {
    if patterns.is_empty() {
        return b!(|_, _| false);
    }

    let regexes = RegexSet::new(patterns).expect("regexes are validated");
    b!(move |folder, file| {
        let path = [folder, file].concat();
        regexes.is_match(&path)
    })
}

fn build_file_delete_patterns(files: &[String]) -> DynFn2<'_> {
    let mut delete_file: DynFn2 = b!(|_path, _filename| false);
    for file in files.iter().map(|f| f.as_bytes()) {
        if file[0] == b'*' {
            match last_index_of(file, b'/') {
                // */bin/test.txt
                Some(last_slash) => {
                    delete_file = b!(move |path, filename| delete_file(path, filename)
                        || (path.ends_with(&file[1..last_slash + 1])
                            && filename.eq(&file[last_slash + 1..])));
                }
                // *mytest.txt
                None => {
                    delete_file = b!(move |path, filename| delete_file(path, filename)
                        || filename.ends_with(&file[1..]));
                }
            }
        } else {
            // "[/][some/folder/]name[*]": an absolute folder must be the
            // file's folder, a relative one must be at the end of it
            let (folder, name) = match last_index_of(file, b'/') {
                Some(last_slash) => (&file[..last_slash + 1], &file[last_slash + 1..]),
                None => (&b""[..], file),
            };
            let folder = if folder.is_empty() || folder[0] == b'/' {
                folder.to_vec()
            } else {
                [b"/", folder].concat()
            };
            let absolute = file[0] == b'/';
            let folder_matches = move |path: &[u8]| {
                if absolute {
                    path == folder
                } else {
                    path.ends_with(&folder)
                }
            };

            delete_file = match name.strip_suffix(b"*") {
                Some(prefix) => b!(move |path, filename| delete_file(path, filename)
                    || (filename.starts_with(prefix) && folder_matches(path))),
                None => b!(move |path, filename| delete_file(path, filename)
                    || (filename == name && folder_matches(path))),
            };
        }
    }

    delete_file
}

/// Directories that can contain something to remove, when every pattern is
/// anchored at an absolute path. A subtree whose path isn't a prefix of one
/// of these can't change, so update_tree doesn't need to read it. Returns
/// None when any pattern can match anywhere (relative, leading '*', regex).
fn build_descend_filter<'a>(
    files: &'a [String],
    directories: &'a [String],
    regexes: &[String],
) -> Option<Vec<&'a [u8]>> {
    if !regexes.is_empty() {
        return None;
    }

    let mut anchors = Vec::new();
    for folder in directories.iter().map(|f| f.as_bytes()) {
        if folder[0] != b'/' {
            return None;
        }
        // "/x/y*" matches every path starting with "/x/y"
        anchors.push(folder.strip_suffix(b"*").unwrap_or(folder));
    }

    for file in files.iter().map(|f| f.as_bytes()) {
        if file[0] != b'/' {
            return None;
        }
        // "/some/folder/file" and "/some/folder/file*" only match in "/some/folder/"
        let last_slash = last_index_of(file, b'/').unwrap();
        anchors.push(&file[..last_slash + 1]);
    }

    Some(anchors)
}

fn build_descend_pattern<'a>(
    files: &'a [String],
    directories: &'a [String],
    regexes: &[String],
) -> DynFn<'a> {
    if std::env::var_os("GITREFORGE_NO_PRUNE").is_some() {
        return b!(|_| true);
    }

    match build_descend_filter(files, directories, regexes) {
        None => b!(|_| true),
        Some(anchors) => b!(move |path: &[u8]| anchors.iter().any(|a| a.starts_with(path))),
    }
}

/// Whether removing with these patterns can treat the same tree differently
/// depending on where it sits. Only patterns that look at nothing but the
/// last path component (a file name, or a directory name without slashes)
/// give the same result for a tree at every path.
fn patterns_depend_on_path(files: &[String], directories: &[String], regexes: &[String]) -> bool {
    if !regexes.is_empty() {
        return true;
    }

    let file_depends = |file: &String| file.contains('/');
    let directory_depends = |folder: &String| {
        if folder.starts_with('/') {
            return true;
        }
        let name = folder.trim_start_matches('*').trim_end_matches('/');
        name.contains('/')
    };

    files.iter().any(file_depends) || directories.iter().any(directory_depends)
}

const EMPTY_TREE: [u8; 20] = [
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88,
    0xfb, 0xee, 0x49, 0x04,
];

type RewrittenEntries = SmallVec<[(Box<[u8]>, Option<TreeHash>); 1]>;

/// Old tree hash to its rewritten hash (None: unchanged), sharded by the
/// first hash byte so that the worker threads rarely contend on a lock.
///
/// The same tree can appear at several paths (moved directories, merged
/// subprojects). When the patterns depend on the path, each path gets its
/// own entry, otherwise the result for the first path would be reused for
/// the others.
pub(crate) struct RewrittenTrees {
    shards: Vec<RwLock<FxHashMap<TreeHash, RewrittenEntries>>>,
    path_dependent: bool,
}

impl RewrittenTrees {
    const SHARDS: usize = 64;

    fn new(path_dependent: bool) -> Self {
        Self {
            shards: (0..Self::SHARDS)
                .map(|_| RwLock::new(FxHashMap::default()))
                .collect(),
            path_dependent,
        }
    }

    fn shard(&self, hash: &TreeHash) -> &RwLock<FxHashMap<TreeHash, RewrittenEntries>> {
        &self.shards[hash.as_bytes()[0] as usize % Self::SHARDS]
    }

    fn key<'a>(&self, path: &'a [u8]) -> &'a [u8] {
        if self.path_dependent { path } else { b"" }
    }

    fn get(&self, hash: &TreeHash, path: &[u8]) -> Option<Option<TreeHash>> {
        let key = self.key(path);
        self.shard(hash)
            .read()
            .unwrap()
            .get(hash)?
            .iter()
            .find(|(p, _)| &**p == key)
            .map(|(_, new)| new.clone())
    }

    fn insert(&self, old: TreeHash, path: &[u8], new: Option<TreeHash>) {
        let key = self.key(path);
        let mut shard = self.shard(&old).write().unwrap();
        let entries = shard.entry(old).or_default();
        if !entries.iter().any(|(p, _)| &**p == key) {
            entries.push((key.into(), new));
        }
    }
}

/// `path` is the tree's path with a trailing slash; it's used as a scratch
/// buffer for the subtree paths and restored before returning.
fn update_tree(
    tree_hash: TreeHash,
    path: &mut Vec<u8>,
    repository: &mut Repository,
    should_delete_file: &DynFn2,
    should_delete_folder: &DynFn,
    should_remove: &DynFn2,
    should_descend: &DynFn,
    rewritten_trees: &RewrittenTrees,
    write_tree: &(impl Fn(Tree, &[u8]) + Sync + Send),
) -> Option<TreeHash> {
    if let Some(rewritten_hash_option) = rewritten_trees.get(&tree_hash, path) {
        return rewritten_hash_option;
    }

    let tree: Tree = match repository.read_object(tree_hash.into()).unwrap() {
        gitreforgelib::objs::GitObject::Tree(tree) => tree,
        _ => panic!("Expected a tree, found something else"),
    };

    let old_hash = tree.hash();

    // only started once the first line changes
    let mut edit: Option<TreeEdit> = None;
    for (index, line) in tree.lines().enumerate() {
        if line.is_tree() {
            let path_len = path.len();
            path.extend_from_slice(line.filename());
            path.push(b'/');

            if should_delete_folder(path) {
                path.truncate(path_len);
                edit.get_or_insert_with(|| TreeEdit::new(&tree)).remove(index);
            } else if !should_descend(path) {
                path.truncate(path_len);
            } else {
                let new_tree_hash = update_tree(
                    line.hash.deref().clone(),
                    path,
                    repository,
                    should_delete_file,
                    should_delete_folder,
                    should_remove,
                    should_descend,
                    rewritten_trees,
                    write_tree,
                );
                path.truncate(path_len);
                if let Some(new_tree_hash) = new_tree_hash {
                    let edit = edit.get_or_insert_with(|| TreeEdit::new(&tree));
                    // like git, don't keep directories without files
                    if new_tree_hash.as_bytes() == &EMPTY_TREE {
                        edit.remove(index);
                    } else {
                        edit.replace_hash(index, &new_tree_hash);
                    }
                }
            }
        } else if should_delete_file(path, line.filename()) || should_remove(path, line.filename())
        {
            edit.get_or_insert_with(|| TreeEdit::new(&tree)).remove(index);
        }
    }

    match edit {
        None => {
            rewritten_trees.insert(old_hash.clone(), path, None);
            None
        }
        Some(edit) => {
            let new_tree = edit.finish();
            let new_hash = new_tree.hash().clone();
            rewritten_trees.insert(old_hash.clone(), path, Some(new_hash.clone()));
            write_tree(new_tree, path);
            Some(new_hash)
        }
    }
}

struct OrderedCommit {
    commit: CommitBase,
    index: usize,
}

impl Eq for OrderedCommit {}

impl PartialEq for OrderedCommit {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl PartialOrd for OrderedCommit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.index.cmp(&other.index))
    }
}

impl Ord for OrderedCommit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}

pub fn remove(
    repository_path: PathBuf,
    files: Vec<String>,
    directories: Vec<String>,
    regexes: Vec<String>,
    dry_run: bool,
    signing_policy: &SigningPolicy,
) -> Result<(), String> {
    if files.iter().chain(&directories).any(|p| p.is_empty()) {
        return Err("--file and --directory patterns must not be empty".into());
    }
    RegexSet::new(&regexes).map_err(|e| format!("invalid --regex: {e}"))?;

    let mut rewritten_commits: HashMap<CommitHash, CommitHash, _> = FxHashMap::default();
    let rewritten_trees =
        RewrittenTrees::new(patterns_depend_on_path(&files, &directories, &regexes));

    let t0 = std::time::Instant::now();
    let mut repository = rayon::scope(|scope| {
        let (tx, rx) = channel::<OrderedCommit>();
        scope.spawn(|_| {
            let mut heap: BinaryHeap<Reverse<OrderedCommit>> = BinaryHeap::new();
            let mut commit_index = 0usize;
            for ordered_commit in rx.into_iter() {
                if ordered_commit.index == commit_index {
                    commit_index += 1;

                    let commit = CommitEditable::create(ordered_commit.commit);
                    let (old_hash, new_hash) = update_commit(
                        &repository_path,
                        commit,
                        &rewritten_commits,
                        &rewritten_trees,
                        dry_run,
                        signing_policy,
                    );
                    if old_hash != new_hash {
                        rewritten_commits.insert(old_hash, new_hash);
                    }

                    while let Some(commit) = heap.pop() {
                        if commit.0.index == commit_index {
                            commit_index += 1;

                            let commit = CommitEditable::create(commit.0.commit);
                            let (old_hash, new_hash) = update_commit(
                                &repository_path,
                                commit,
                                &rewritten_commits,
                                &rewritten_trees,
                                dry_run,
                                signing_policy,
                            );
                            if old_hash != new_hash {
                                rewritten_commits.insert(old_hash, new_hash);
                            }
                        } else {
                            heap.push(commit);
                            break;
                        }
                    }
                } else {
                    heap.push(Reverse(ordered_commit));
                }
            }
        });

        let repository = Repository::create(repository_path.clone());
        let file_delete_patterns = build_file_delete_patterns(&files);
        let folder_delete_patterns = build_folder_delete_patterns(&directories);
        let should_remove_line = build_regex_pattern(&regexes);
        let should_descend = build_descend_pattern(&files, &directories, &regexes);
        repository
            .commits_topo()
            .enumerate()
            .map(|(index, commit)| OrderedCommit { index, commit })
            .par_bridge()
            .for_each_with(repository.clone(), |repository, commit| {
                let old_tree_hash = commit.commit.tree();
                update_tree(
                    old_tree_hash,
                    &mut b"/".to_vec(),
                    repository,
                    &file_delete_patterns,
                    &folder_delete_patterns,
                    &should_remove_line,
                    &should_descend,
                    &rewritten_trees,
                    &|tree, path| {
                        if !dry_run {
                            // TODO write out on different thread
                            Repository::write(
                                repository_path.clone(),
                                WriteObject::from(tree).with_delta_path(path),
                                dry_run,
                            );
                        }
                    },
                );

                tx.send(commit).unwrap();
            });

        std::mem::drop(tx);

        repository
    });

    let t1 = std::time::Instant::now();
    repository.update_refs(&rewritten_commits, dry_run);
    let t2 = std::time::Instant::now();
    Repository::write_rewritten_commits_file(rewritten_commits, dry_run);
    let t3 = std::time::Instant::now();
    if std::env::var_os("GITREFORGE_TIMINGS").is_some() {
        eprintln!("TIMING rewrite={:.3} refs+finish={:.3} mapfile={:.3}", (t1 - t0).as_secs_f64(), (t2 - t1).as_secs_f64(), (t3 - t2).as_secs_f64());
    }
    Ok(())
}

fn update_commit(
    repo_path: &Path,
    mut commit: CommitEditable,
    rewritten_commits: &HashMap<CommitHash, CommitHash, FxBuildHasher>,
    rewritten_trees: &RewrittenTrees,
    dry_run: bool,
    signing_policy: &SigningPolicy,
) -> (CommitHash, CommitHash) {
    let old_hash = commit.base_hash().clone();

    update_parents(&mut commit, rewritten_commits);
    // update tree
    if let Some(Some(new_tree_hash)) = rewritten_trees.get(&commit.tree(), b"/") {
        commit.set_tree(new_tree_hash);
    }

    signing_policy
        .sign_if_selected(&mut commit)
        .expect("failed to sign commit");

    if commit.has_changes() {
        let write_object: WriteObject = commit.into();
        let new_hash = write_object.hash.clone();
        Repository::write(repo_path.into(), write_object, dry_run);
        return (old_hash, new_hash.into());
    }

    (old_hash.clone(), old_hash)
}

fn update_parents(
    commit: &mut CommitEditable,
    rewritten_commits: &HashMap<CommitHash, CommitHash, FxBuildHasher>,
) {
    for (i, parent) in commit.parents().iter().enumerate() {
        if let Some(new_parent) = rewritten_commits.get(parent) {
            if new_parent != parent {
                commit.parents[i] = Some(new_parent.clone());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{build_descend_filter, build_folder_delete_patterns, patterns_depend_on_path};

    #[test]
    pub fn path_dependent_patterns() {
        let none: Vec<String> = vec![];
        for files in [&["*.zip"][..], &["secret*"], &["passwords.txt"]] {
            assert!(!patterns_depend_on_path(&strings(files), &none, &none), "{files:?}");
        }
        for directories in [&["docs"][..], &["*build"], &["*build/"], &["bin/"], &["build*"]] {
            assert!(!patterns_depend_on_path(&none, &strings(directories), &none), "{directories:?}");
        }
        for files in [&["/Makefile"][..], &["*bin/test.txt"], &["/a/b*"]] {
            assert!(patterns_depend_on_path(&strings(files), &none, &none), "{files:?}");
        }
        for directories in [&["/docs"][..], &["bin/debug"], &["*my/directory"], &["/x/y*"], &["a/b*"]] {
            assert!(patterns_depend_on_path(&none, &strings(directories), &none), "{directories:?}");
        }
        assert!(patterns_depend_on_path(&none, &none, &strings(&["x"])));
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    pub fn descend_filter_only_for_absolute_patterns() {
        let none: Vec<String> = vec![];
        assert!(build_descend_filter(&none, &strings(&["docs"]), &none).is_none());
        assert!(build_descend_filter(&none, &strings(&["*docs"]), &none).is_none());
        assert!(build_descend_filter(&strings(&["*.zip"]), &none, &none).is_none());
        assert!(build_descend_filter(&strings(&["secret*"]), &none, &none).is_none());
        assert!(build_descend_filter(&strings(&["/a/b"]), &none, &strings(&["x"])).is_none());
        assert!(
            build_descend_filter(&strings(&["/a/b"]), &strings(&["docs"]), &none).is_none()
        );
    }

    #[test]
    pub fn descend_filter_paths() {
        let none: Vec<String> = vec![];
        let files = strings(&["/src/main.rs", "/etc/conf/secret*", "/root.txt"]);
        let directories = strings(&["/Documentation", "/x/y*"]);
        let anchors = build_descend_filter(&files, &directories, &none).unwrap();
        let descend = |path: &str| anchors.iter().any(|a| a.starts_with(path.as_bytes()));

        for path in ["/src/", "/etc/", "/etc/conf/", "/x/"] {
            assert!(descend(path), "{path}");
        }
        for path in ["/t/", "/src/bin/", "/etc/other/", "/Doc/", "/y/", "/etc/conf/sub/"] {
            assert!(!descend(path), "{path}");
        }
    }

    #[test]
    pub fn folder_deletion_patterns() {
        let patterns: Vec<String> = vec![
            "/some/folder".into(),
            "/another/folder/".into(),
            "*some_folder".into(),
            "*my/directory".into(),
            "/x/y*".into(),
            "bin/debug".into(),
            "foo/bar/".into(),
            "build*".into(),
            "out/gen*".into(),
        ];

        let matches = build_folder_delete_patterns(&patterns);

        assert!(matches(b"/some/folder/"));
        assert!(matches(b"/another/folder/"));
        assert!(matches(b"/this/is_some_folder/"));
        assert!(matches(b"/this/is/some_folder/"));
        assert!(matches(b"/my/directory/"));
        assert!(matches(b"/_my/directory/"));
        assert!(matches(b"/x/y/"));
        assert!(matches(b"/x/y/z/"));
        assert!(matches(b"/src/bin/debug/"));
        assert!(matches(b"/bin/debug/"));
        assert!(matches(b"/baz/foo/bar/"));
        assert!(matches(b"/foo/bar/"));

        assert!(!matches(b"/_bin/debug/"));
        assert!(!matches(b"/bin/debug_/"));
        assert!(!matches(b"/a/some/folder/"));
        assert!(!matches(b"/this/is_some_folder/b/"));
        assert!(!matches(b"/my/directory/b/"));

        assert!(matches(b"/build/"));
        assert!(matches(b"/src/build-x/"));
        assert!(!matches(b"/src/rebuild/"));
        assert!(!matches(b"/build-x/sub/"));
        assert!(matches(b"/out/generated/"));
        assert!(matches(b"/a/out/gen/"));
        assert!(!matches(b"/a/xout/gen/"));
        assert!(!matches(b"/out/x/gen/"));
    }

    #[test]
    pub fn file_deletion_patterns() {
        let patterns = vec![
            "/some/folder/removeme.txt".into(),
            "test.txt".into(),
            "*/bin/test_with_folder.txt".into(),
            "*test1.txt".into(),
            "/var/opt/myfile*".into(),
            "thisfile*".into(),
            "bin/relative.txt".into(),
            "etc/relative*".into(),
        ];
        let should_delete = super::build_file_delete_patterns(&patterns);

        assert!(should_delete(b"/some/folder/", b"removeme.txt"));
        assert!(!should_delete(b"/some/folder/", b"1removeme.txt"));
        assert!(!should_delete(b"/some/folder/", b"removeme.txt1"));
        assert!(!should_delete(b"/some/folder/", b"removeme.tx"));
        assert!(!should_delete(b"/some/folder_/", b"removeme.txt"));

        assert!(should_delete(b"/", b"test.txt"));
        assert!(should_delete(b"/hello/world/", b"test.txt"));

        assert!(should_delete(b"/test/bin/", b"test_with_folder.txt"));
        assert!(!should_delete(
            b"/test/bin/another_folder",
            b"test_with_folder.txt"
        ));

        assert!(should_delete(b"/some/folder/", b"test1.txt"));
        assert!(should_delete(b"/", b"test1.txt"));
        assert!(should_delete(b"/some/folder/", b"more_to_this_test1.txt"));
        assert!(should_delete(b"/", b"more_to_this_test1.txt"));

        assert!(should_delete(b"/var/opt/", b"myfile.txt"));
        assert!(should_delete(b"/var/opt/", b"myfile"));
        assert!(!should_delete(b"/var/opt/", b"_myfile.txt"));

        assert!(should_delete(b"/some/folder/", b"thisfile.txt"));
        assert!(should_delete(b"/another/folder/", b"thisfile.txt"));
        assert!(should_delete(b"/some/folder/", b"thisfile"));
        assert!(should_delete(b"/", b"thisfile"));

        assert!(!should_delete(b"/", b"_thisfile"));
        assert!(!should_delete(b"/", b"test.txt1"));
        assert!(!should_delete(b"/hello/world", b"1test.txt"));

        assert!(should_delete(b"/bin/", b"relative.txt"));
        assert!(should_delete(b"/a/bin/", b"relative.txt"));
        assert!(!should_delete(b"/a/xbin/", b"relative.txt"));
        assert!(!should_delete(b"/bin/a/", b"relative.txt"));
        assert!(!should_delete(b"/", b"relative.txt"));
        assert!(should_delete(b"/etc/", b"relative.conf"));
        assert!(should_delete(b"/x/etc/", b"relative"));
        assert!(!should_delete(b"/x/etc/", b"_relative"));
        assert!(!should_delete(b"/x/etcetera/", b"relative"));
    }
}

