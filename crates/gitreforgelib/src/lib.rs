use core::panic;
use std::{
    collections::HashMap,
    error::Error,
    hash::BuildHasher,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

use commits::{CommitsFifoIter, CommitsLifoIter};
use compression::Decompression;

use objs::{CommitBase, CommitEditable, CommitHash, GitObject, Tag, Tree};
use packreader::PackReader;
use rayon::prelude::{ParallelBridge, ParallelIterator};
use refs::GitRef;
use sha1::{Digest, Sha1};
use rustc_hash::FxBuildHasher;
use shared::ObjectHash;

mod commits;
mod compression;
// pub mod ffi;
mod idx_reader;
mod pack_diff;
pub mod pack_writer;
mod delta_cache;
mod packreader;
mod refs;
mod shared;

pub mod objs;

pub struct Repository {
    path: PathBuf,
    pack_reader: PackReader,
    decompression: Decompression,
}

impl Clone for Repository {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            pack_reader: self.pack_reader.clone(),
            decompression: Decompression::default(),
        }
    }
}

#[derive(Debug)]
pub struct WriteBytes {
    bytes: Box<[u8]>,
    start: usize,
}

impl WriteBytes {
    pub fn get_bytes(&self) -> &[u8] {
        &self.bytes[self.start..]
    }
}

pub struct WriteObject {
    pub hash: ObjectHash,
    prefix: String,
    bytes: WriteBytes,
    delta_path: Option<Box<[u8]>>,
}

impl WriteObject {
    /// Path of a tree, used to pick a delta base in the pack writer
    pub fn with_delta_path(mut self, path: &[u8]) -> Self {
        self.delta_path = Some(path.into());
        self
    }
}

impl From<CommitEditable> for WriteObject {
    fn from(value: CommitEditable) -> Self {
        let wb = value.to_bytes();
        Self {
            hash: calculate_hash(&wb.bytes, b"commit"),
            prefix: String::from("commit"),
            bytes: wb,
            delta_path: None,
        }
    }
}

impl From<Tag> for WriteObject {
    fn from(value: Tag) -> Self {
        Self {
            hash: value.hash().clone(),
            prefix: String::from("tag"),
            bytes: value.bytes(),
            delta_path: None,
        }
    }
}

impl From<Tree> for WriteObject {
    fn from(value: Tree) -> Self {
        Self {
            hash: value.hash().0.clone(),
            prefix: String::from("tree"),
            bytes: value.bytes(),
            delta_path: None,
        }
    }
}

/// Boolean switch from the environment: "0" turns it off, "1" on, and
/// anything else (or unset) keeps the default.
pub(crate) fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => default,
    }
}

pub fn calculate_hash(data: &[u8], prefix: &[u8]) -> ObjectHash {
    let mut hasher = Sha1::new();
    hasher.update(prefix);
    hasher.update(b" ");
    hasher.update(data.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(data);
    let bytes: [u8; 20] = hasher.finalize().into();
    ObjectHash::from(bytes)
}

impl Repository {
    pub fn create(path: PathBuf) -> Self {
        let pack_reader = PackReader::create(&path).unwrap();
        let decompression = Decompression::default();

        Self {
            path,
            pack_reader,
            decompression,
        }
    }

    pub fn read_object(&mut self, hash: ObjectHash) -> Option<GitObject> {
        commits::read_object_from_hash(&mut self.decompression, &self.path, &self.pack_reader, hash)
    }

    pub fn write(mut repo_path: PathBuf, object: WriteObject, dry_run: bool) {
        if dry_run {
            return;
        }

        if let Some(writer) = pack_writer::get(&repo_path) {
            writer.add(
                &object.hash,
                &object.prefix,
                object.bytes.get_bytes(),
                object.delta_path.as_deref(),
            );
            return;
        }

        let hash = object.hash.to_string();
        let data = object.bytes;
        let prefix = object.prefix;

        repo_path.push("objects");
        repo_path.push(&hash[0..2]);

        std::fs::create_dir_all(&repo_path).unwrap();

        repo_path.push(&hash[2..]);
        if !Path::new(&repo_path).exists() {
            match compression::pack_file(&repo_path, prefix.as_str(), &data) {
                Ok(_) => {}
                Err(e) => match e.kind() {
                    io::ErrorKind::AlreadyExists => {}
                    _ => panic!("Error writing object: {}", e),
                },
            }
        }
    }

    pub fn write_commits(
        repository_path: PathBuf,
        commits: impl Iterator<Item = WriteObject> + Send,
        dry_run: bool,
    ) {
        commits.par_bridge().for_each(|commit| {
            Self::write(repository_path.clone(), commit, dry_run);
        });
    }

    pub fn write_trees(
        repository_path: PathBuf,
        trees: impl Iterator<Item = objs::Tree> + Send,
        dry_run: bool,
    ) {
        trees.par_bridge().for_each(|tree| {
            Self::write(repository_path.clone(), tree.into(), dry_run);
        });
    }

    pub fn commits_topo(&self) -> impl Iterator<Item = CommitBase> + '_ {
        CommitsFifoIter::create(&self.path, &self.pack_reader, Decompression::default())
    }

    pub fn commits_lifo(&self) -> impl Iterator<Item = CommitBase> + '_ {
        CommitsLifoIter::create(&self.path, &self.pack_reader, Decompression::default())
    }

    pub fn refs(&self) -> Result<Vec<GitRef>, Box<dyn Error>> {
        GitRef::read_all(&self.path)
    }

    pub fn update_refs<T: BuildHasher>(
        &mut self,
        rewritten_commits: &HashMap<CommitHash, CommitHash, T>,
        dry_run: bool,
    ) {
        if dry_run {
            return;
        }

        let t = std::time::Instant::now();
        let refs = refs::GitRef::update(self, rewritten_commits, dry_run);
        let t2 = std::time::Instant::now();
        // the refs must only point to objects that are safely stored
        pack_writer::finish_if_enabled();
        let t3 = std::time::Instant::now();
        refs::GitRef::write_all(&self.path, &refs, env_flag("GITREFORGE_PACKED_REFS", true));
        if std::env::var_os("GITREFORGE_TIMINGS").is_some() {
            eprintln!(
                "TIMING refs={:.3} pack_finish={:.3} refs_write={:.3}",
                (t2 - t).as_secs_f64(),
                (t3 - t2).as_secs_f64(),
                t3.elapsed().as_secs_f64()
            );
        }
    }

    pub fn write_rewritten_commits_file(
        rewritten_commits: HashMap<CommitHash, CommitHash, FxBuildHasher>,
        dry_run: bool,
    ) {
        if dry_run {
            return;
        }

        let file = std::fs::File::create("object-id-map.old-new.txt").unwrap();
        let mut writer = BufWriter::new(file);
        for (old, new) in rewritten_commits.iter() {
            writer.write_fmt(format_args!("{old} {new}\n")).unwrap();
        }

        println!("object-id-map.old-new.txt written");
    }
}
