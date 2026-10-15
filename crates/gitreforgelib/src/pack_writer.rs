//! Writes rewritten objects into a single new pack file instead of one loose
//! object file each. Compression happens on the calling thread, only the
//! append to the file is serialized. Trees are stored as OFS_DELTA against
//! the last tree written at the same path when that's smaller.
//!
//! `GITREFORGE_PACK=0` writes loose objects instead, `GITREFORGE_DELTA=0`
//! stores every tree whole, `GITREFORGE_LEVEL` sets the zlib level (1).

use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use libdeflater::{CompressionLvl, Compressor, Crc};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::shared::ObjectHash;

struct Entry {
    hash: [u8; 20],
    offset: u64,
    crc: u32,
}

struct Inner {
    file: BufWriter<File>,
    offset: u64,
    entries: Vec<Entry>,
    seen: FxHashSet<ObjectHash>,
}

/// Last tree written at a path; candidate delta base for the next one
#[derive(Clone)]
struct Base {
    offset: u64,
    bytes: Arc<[u8]>,
    depth: u32,
}

const MAX_DEPTH: u32 = 50;

pub struct PackWriter {
    pack_dir: PathBuf,
    inner: Mutex<Inner>,
    bases: Mutex<FxHashMap<Box<[u8]>, Base>>,
    delta: bool,
    stats_deltas: AtomicU64,
    stats_full: AtomicU64,
    stats_raw_bytes: AtomicU64,
    stats_payload_bytes: AtomicU64,
}

static WRITER: OnceLock<Option<PackWriter>> = OnceLock::new();

thread_local! {
    static COMPRESSOR: RefCell<(Compressor, Vec<u8>)> =
        RefCell::new((Compressor::new(compression_level()), Vec::new()));
}

pub(crate) fn compression_level() -> CompressionLvl {
    let lvl = std::env::var("GITREFORGE_LEVEL")
        .ok()
        .and_then(|l| l.parse().ok())
        // most of the pack is delta data, which level 1 compresses about as
        // well as higher levels, in less time
        .unwrap_or(1);
    CompressionLvl::new(lvl).unwrap()
}

pub fn enabled() -> bool {
    crate::env_flag("GITREFORGE_PACK", true)
}

pub fn get(repo_path: &Path) -> Option<&'static PackWriter> {
    WRITER
        .get_or_init(|| enabled().then(|| PackWriter::create(repo_path)))
        .as_ref()
}

fn type_code(prefix: &str) -> u8 {
    match prefix {
        "commit" => 1,
        "tree" => 2,
        "blob" => 3,
        "tag" => 4,
        _ => panic!("unknown type"),
    }
}

impl PackWriter {
    fn create(repo_path: &Path) -> Self {
        let pack_dir = repo_path.join("objects/pack");
        let file = File::create(pack_dir.join("tmp_gitreforge.pack")).unwrap();
        let mut file = BufWriter::with_capacity(1 << 20, file);
        file.write_all(b"PACK\0\0\0\x02\0\0\0\0").unwrap();
        Self {
            pack_dir,
            bases: Mutex::new(FxHashMap::default()),
            delta: crate::env_flag("GITREFORGE_DELTA", true),
            stats_deltas: AtomicU64::new(0),
            stats_full: AtomicU64::new(0),
            stats_raw_bytes: AtomicU64::new(0),
            stats_payload_bytes: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                file,
                offset: 12,
                entries: Vec::new(),
                seen: FxHashSet::default(),
            }),
        }
    }

    pub fn add(&self, hash: &ObjectHash, prefix: &str, data: &[u8], delta_path: Option<&[u8]>) {
        if self.inner.lock().unwrap().seen.contains(hash) {
            return;
        }

        let delta_path = delta_path.filter(|_| self.delta);
        let base = delta_path.and_then(|p| self.bases.lock().unwrap().get(p).cloned());
        let delta = base
            .as_ref()
            .filter(|b| b.depth < MAX_DEPTH)
            .and_then(|b| encode_tree_delta(&b.bytes, data, data.len() / 2));

        let (type_code, payload) = match &delta {
            Some(d) => (6u8, &d[..]),
            None => (type_code(prefix), data),
        };

        COMPRESSOR.with_borrow_mut(|(compressor, buf)| {
            // entry header: type + size varint
            buf.clear();
            let mut size = payload.len();
            let mut byte = (type_code << 4) | (size & 0x0f) as u8;
            size >>= 4;
            while size != 0 {
                buf.push(byte | 0x80);
                byte = (size & 0x7f) as u8;
                size >>= 7;
            }
            buf.push(byte);
            let header_len = buf.len();

            buf.resize(header_len + compressor.zlib_compress_bound(payload.len()), 0);
            let n = compressor
                .zlib_compress(payload, &mut buf[header_len..])
                .unwrap();
            buf.truncate(header_len + n);

            let mut inner = self.inner.lock().unwrap();
            if !inner.seen.insert(hash.clone()) {
                return;
            }
            let offset = inner.offset;
            let ofs = match (&delta, &base) {
                (Some(_), Some(b)) => encode_ofs(offset - b.offset),
                _ => Vec::new(),
            };

            let mut crc = Crc::new();
            crc.update(&buf[..header_len]);
            crc.update(&ofs);
            crc.update(&buf[header_len..]);

            inner.file.write_all(&buf[..header_len]).unwrap();
            inner.file.write_all(&ofs).unwrap();
            inner.file.write_all(&buf[header_len..]).unwrap();
            inner.offset += (buf.len() + ofs.len()) as u64;
            inner.entries.push(Entry {
                hash: hash.bytes,
                offset,
                crc: crc.sum(),
            });

            // still holding the pack lock: the new base is in the file before
            // anybody can reference its offset
            if let Some(path) = delta_path {
                let depth = match (&delta, &base) {
                    (Some(_), Some(b)) => b.depth + 1,
                    _ => 0,
                };
                self.bases.lock().unwrap().insert(
                    path.into(),
                    Base {
                        offset,
                        bytes: Arc::from(data),
                        depth,
                    },
                );
            }
        });

        if delta.is_some() {
            self.stats_deltas.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats_full.fetch_add(1, Ordering::Relaxed);
        }
        self.stats_raw_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.stats_payload_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
    }

    pub fn finish(&self) {
        if std::env::var_os("GITREFORGE_TIMINGS").is_some() {
            let bases = self.bases.lock().unwrap();
            eprintln!(
                "PACK full={} deltas={} raw_bytes={} payload_bytes={} base_paths={} base_bytes={}",
                self.stats_full.load(Ordering::Relaxed),
                self.stats_deltas.load(Ordering::Relaxed),
                self.stats_raw_bytes.load(Ordering::Relaxed),
                self.stats_payload_bytes.load(Ordering::Relaxed),
                bases.len(),
                bases.values().map(|b| b.bytes.len()).sum::<usize>(),
            );
        }
        let mut inner = self.inner.lock().unwrap();
        let count = inner.entries.len() as u32;
        inner.file.flush().unwrap();
        let file = inner.file.get_mut();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&count.to_be_bytes()).unwrap();

        let tmp_pack = self.pack_dir.join("tmp_gitreforge.pack");
        let content = unsafe { memmap2::Mmap::map(&File::open(&tmp_pack).unwrap()).unwrap() };
        let pack_sha = sha1(&content);
        drop(content);
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(&pack_sha).unwrap();
        file.flush().unwrap();

        let entries = &mut inner.entries;
        entries.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));

        let mut idx = Vec::with_capacity(8 + 1024 + entries.len() * 28 + 40);
        idx.extend_from_slice(b"\xfftOc\0\0\0\x02");
        let mut fanout = [0u32; 256];
        for e in entries.iter() {
            fanout[e.hash[0] as usize] += 1;
        }
        let mut acc = 0;
        for f in fanout {
            acc += f;
            idx.extend_from_slice(&acc.to_be_bytes());
        }
        for e in entries.iter() {
            idx.extend_from_slice(&e.hash);
        }
        for e in entries.iter() {
            idx.extend_from_slice(&e.crc.to_be_bytes());
        }
        let mut large = Vec::new();
        for e in entries.iter() {
            if e.offset < 0x8000_0000 {
                idx.extend_from_slice(&(e.offset as u32).to_be_bytes());
            } else {
                idx.extend_from_slice(&(0x8000_0000 | large.len() as u32).to_be_bytes());
                large.push(e.offset);
            }
        }
        for l in large {
            idx.extend_from_slice(&l.to_be_bytes());
        }
        idx.extend_from_slice(&pack_sha);
        let idx_sha = sha1(&idx);
        idx.extend_from_slice(&idx_sha);

        let name = hex::encode(pack_sha);
        std::fs::write(self.pack_dir.join(format!("pack-{name}.idx")), idx).unwrap();
        std::fs::rename(tmp_pack, self.pack_dir.join(format!("pack-{name}.pack"))).unwrap();
    }
}

fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::Digest;
    sha1::Sha1::digest(data).into()
}

pub fn finish_if_enabled() {
    if let Some(Some(w)) = WRITER.get() {
        w.finish();
    }
}

/// OFS_DELTA base distance, git's "offset encoding"
fn encode_ofs(mut distance: u64) -> Vec<u8> {
    let mut buf = [0u8; 10];
    let mut pos = buf.len() - 1;
    buf[pos] = (distance & 0x7f) as u8;
    distance >>= 7;
    while distance != 0 {
        distance -= 1;
        pos -= 1;
        buf[pos] = 0x80 | (distance & 0x7f) as u8;
        distance >>= 7;
    }
    buf[pos..].to_vec()
}

/// Splits raw tree bytes into entries ("<mode> <name>\0<20 byte hash>")
fn tree_entries(data: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos >= data.len() {
            return None;
        }
        let nul = memchr::memchr(0, &data[pos..]).unwrap() + pos;
        let entry = (pos, nul + 21);
        pos = nul + 21;
        Some(entry)
    })
}

fn push_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn push_copy(out: &mut Vec<u8>, mut offset: usize, mut len: usize) {
    while len > 0 {
        let chunk = len.min(0x10000);
        let op_pos = out.len();
        let mut op = 0x80u8;
        out.push(0);
        for i in 0..4 {
            let b = (offset >> (i * 8)) as u8;
            if b != 0 {
                op |= 1 << i;
                out.push(b);
            }
        }
        if chunk != 0x10000 {
            for i in 0..3 {
                let b = (chunk >> (i * 8)) as u8;
                if b != 0 {
                    op |= 0x10 << i;
                    out.push(b);
                }
            }
        }
        out[op_pos] = op;
        offset += chunk;
        len -= chunk;
    }
}

fn push_insert(out: &mut Vec<u8>, mut data: &[u8]) {
    while !data.is_empty() {
        let chunk = data.len().min(127);
        out.push(chunk as u8);
        out.extend_from_slice(&data[..chunk]);
        data = &data[chunk..];
    }
}

/// Entry-level delta between two trees: unchanged entries become copies from
/// the base (merged while they are contiguous there), everything else is
/// inserted literally. Gives up once the delta exceeds `max_len`.
pub(crate) fn encode_tree_delta(base: &[u8], target: &[u8], max_len: usize) -> Option<Vec<u8>> {
    let mut index: FxHashMap<&[u8], usize> = FxHashMap::default();
    index.reserve(base.len() / 40);
    for (start, end) in tree_entries(base) {
        index.entry(&base[start..end]).or_insert(start);
    }

    let mut out = Vec::with_capacity(64);
    push_varint(&mut out, base.len());
    push_varint(&mut out, target.len());

    let mut copy: Option<(usize, usize)> = None;
    let mut insert_start: Option<usize> = None;
    for (start, end) in tree_entries(target) {
        match index.get(&target[start..end]) {
            Some(&base_pos) => {
                if let Some(s) = insert_start.take() {
                    push_insert(&mut out, &target[s..start]);
                }
                copy = match copy {
                    Some((c_off, c_len)) if c_off + c_len == base_pos => {
                        Some((c_off, c_len + end - start))
                    }
                    other => {
                        if let Some((c_off, c_len)) = other {
                            push_copy(&mut out, c_off, c_len);
                        }
                        Some((base_pos, end - start))
                    }
                };
            }
            None => {
                if let Some((c_off, c_len)) = copy.take() {
                    push_copy(&mut out, c_off, c_len);
                }
                insert_start.get_or_insert(start);
            }
        }
        if out.len() > max_len {
            return None;
        }
    }
    if let Some((c_off, c_len)) = copy {
        push_copy(&mut out, c_off, c_len);
    }
    if let Some(s) = insert_start {
        push_insert(&mut out, &target[s..]);
    }

    (out.len() <= max_len).then_some(out)
}
