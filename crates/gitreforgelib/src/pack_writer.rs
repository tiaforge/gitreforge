//! PROTOTYPE: write rewritten objects into a single pack file instead of loose
//! objects. Enabled with GITREFORGE_PACK=1. Objects are stored undeltified
//! (whole zlib streams); compression happens on the calling thread, only the
//! append to the file is serialized.

use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use libdeflater::{CompressionLvl, Compressor, Crc};
use rustc_hash::FxHashSet;

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

pub struct PackWriter {
    pack_dir: PathBuf,
    inner: Mutex<Inner>,
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
        .unwrap_or(6);
    CompressionLvl::new(lvl).unwrap()
}

pub fn enabled() -> bool {
    std::env::var_os("GITREFORGE_PACK").is_some_and(|v| v == "1")
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
            inner: Mutex::new(Inner {
                file,
                offset: 12,
                entries: Vec::new(),
                seen: FxHashSet::default(),
            }),
        }
    }

    pub fn add(&self, hash: &ObjectHash, prefix: &str, data: &[u8]) {
        if self.inner.lock().unwrap().seen.contains(hash) {
            return;
        }

        COMPRESSOR.with_borrow_mut(|(compressor, buf)| {
            // entry header: type + size varint
            buf.clear();
            let mut size = data.len();
            let mut byte = (type_code(prefix) << 4) | (size & 0x0f) as u8;
            size >>= 4;
            while size != 0 {
                buf.push(byte | 0x80);
                byte = (size & 0x7f) as u8;
                size >>= 7;
            }
            buf.push(byte);
            let header_len = buf.len();

            buf.resize(header_len + compressor.zlib_compress_bound(data.len()), 0);
            let n = compressor
                .zlib_compress(data, &mut buf[header_len..])
                .unwrap();
            buf.truncate(header_len + n);

            let mut crc = Crc::new();
            crc.update(buf);

            let mut inner = self.inner.lock().unwrap();
            if !inner.seen.insert(hash.clone()) {
                return;
            }
            let offset = inner.offset;
            inner.file.write_all(buf).unwrap();
            inner.offset += buf.len() as u64;
            inner.entries.push(Entry {
                hash: hash.bytes,
                offset,
                crc: crc.sum(),
            });
        });
    }

    pub fn finish(&self) {
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
