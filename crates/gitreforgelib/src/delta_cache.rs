//! Prototype: per-thread cache of resolved pack objects, keyed by pack offset.
//!
//! Without it, every read of an OFS_DELTA object inflates and composes the
//! whole delta chain down to the base. Neighbouring trees in history share
//! most of their chain, so caching resolved objects lets a read start from
//! the nearest cached ancestor and apply only the deltas above it. Like git's
//! `delta_base_cache`, it's direct-mapped: a slot holds one object and a
//! colliding insert evicts it.
//!
//! `GITREFORGE_DELTA_CACHE=0` disables it, `GITREFORGE_DELTA_CACHE=<n>` sets
//! the number of slots per thread (default 256). `GITREFORGE_DELTA_CACHE_MB`
//! caps the bytes held per thread (default 64).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use memmap2::Mmap;

use crate::compression::Decompression;
use crate::pack_diff::{read_base_offset, read_varint};
use crate::packreader::PackObject;

const OFS_DELTA: u8 = 6;

struct Config {
    slots: usize,
    max_bytes: usize,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let slots = std::env::var("GITREFORGE_DELTA_CACHE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);
        let mb: usize = std::env::var("GITREFORGE_DELTA_CACHE_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64);
        Config {
            slots,
            max_bytes: mb << 20,
        }
    })
}

pub fn enabled() -> bool {
    config().slots > 0
}

struct Slot {
    key: usize,
    object_type: u8,
    base_offset: usize,
    data: Rc<[u8]>,
}

struct Cache {
    slots: Vec<Option<Slot>>,
    bytes: usize,
    max_bytes: usize,
}

impl Cache {
    fn new(slots: usize, max_bytes: usize) -> Self {
        Self {
            slots: (0..slots).map(|_| None).collect(),
            bytes: 0,
            max_bytes,
        }
    }

    fn index(&self, key: usize) -> usize {
        // offsets of consecutive objects differ in the low bits, mix them a bit
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20) % self.slots.len()
    }

    fn get(&self, key: usize) -> Option<&Slot> {
        self.slots[self.index(key)]
            .as_ref()
            .filter(|slot| slot.key == key)
    }

    fn insert(&mut self, key: usize, object_type: u8, base_offset: usize, data: Rc<[u8]>) {
        if data.len() > self.max_bytes / 4 {
            return;
        }

        let index = self.index(key);
        if let Some(old) = self.slots[index].take() {
            self.bytes -= old.data.len();
        }

        // over budget: drop everything, simpler than LRU and rare for trees
        if self.bytes + data.len() > self.max_bytes {
            self.slots.iter_mut().for_each(|slot| *slot = None);
            self.bytes = 0;
        }

        self.bytes += data.len();
        self.slots[index] = Some(Slot {
            key,
            object_type,
            base_offset,
            data,
        });
    }
}

thread_local! {
    static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
}

/// Resolve the OFS_DELTA object at `pack_object` to its full content.
/// Returns the content and the PackObject of the chain's base, which
/// carries the real object type, the same as the uncached path.
pub fn read(
    decompression: &mut Decompression,
    mmap: &Mmap,
    pack_object: PackObject,
) -> (Box<[u8]>, PackObject) {
    CACHE.with_borrow_mut(|cache| {
        let cache = cache.get_or_insert_with(|| {
            let config = config();
            Cache::new(config.slots, config.max_bytes)
        });
        resolve(cache, decompression, mmap, pack_object)
    })
}

fn resolve(
    cache: &mut Cache,
    decompression: &mut Decompression,
    mmap: &Mmap,
    pack_object: PackObject,
) -> (Box<[u8]>, PackObject) {
    let pack_key = mmap.as_ptr() as usize;

    // walk down the chain until we hit a cached object or the base
    let mut chain = Vec::new();
    let mut current = pack_object;
    let (object_type, base_offset, mut data) = loop {
        if let Some(slot) = cache.get(pack_key + current.offset) {
            break (slot.object_type, slot.base_offset, slot.data.clone());
        }

        if current.object_type != OFS_DELTA {
            let data: Rc<[u8]> = decompression.unpack(mmap, &current, 0).into();
            cache.insert(
                pack_key + current.offset,
                current.object_type,
                current.offset,
                data.clone(),
            );
            break (current.object_type, current.offset, data);
        }

        let (negative_offset, _) = read_base_offset(mmap, &current);
        let next = PackObject::create(mmap, current.offset - negative_offset);
        chain.push(current);
        current = next;
    };

    // apply the deltas from the bottom up, caching every intermediate result
    while let Some(delta_object) = chain.pop() {
        let (_, ofs_len) = read_base_offset(mmap, &delta_object);
        let delta = decompression.unpack(mmap, &delta_object, ofs_len);
        let result: Rc<[u8]> = apply_delta(&data, &delta).into();
        cache.insert(
            pack_key + delta_object.offset,
            object_type,
            base_offset,
            result.clone(),
        );
        data = result;
    }

    let base = PackObject::create(mmap, base_offset);
    debug_assert_eq!(base.object_type, object_type);
    (Box::from(&*data), base)
}

/// Apply a git delta (base size, result size, copy/insert ops) to `base`.
pub(crate) fn apply_delta(base: &[u8], delta: &[u8]) -> Vec<u8> {
    let (base_len, pos) = read_varint(delta, 0);
    let (result_len, mut pos) = read_varint(delta, pos);
    assert_eq!(base_len, base.len(), "delta base size mismatch");

    let mut result = Vec::with_capacity(result_len);
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;

        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    offset |= (delta[pos] as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (delta[pos] as usize) << (8 * i);
                    pos += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            result.extend_from_slice(&base[offset..offset + len]);
        } else if op != 0 {
            let len = op as usize;
            result.extend_from_slice(&delta[pos..pos + len]);
            pos += len;
        } else {
            panic!("invalid delta opcode 0");
        }
    }

    assert_eq!(result.len(), result_len, "delta result size mismatch");
    result
}

#[cfg(test)]
mod test {
    use std::path::Path;
    use std::process::Command;

    use memmap2::Mmap;

    use super::{Cache, apply_delta, resolve};
    use crate::compression::Decompression;
    use crate::idx_reader::get_pack_offsets;
    use crate::packreader::PackObject;
    use crate::pack_writer::encode_tree_delta;

    #[test]
    fn apply_delta_copies_and_inserts() {
        let base = b"hello world";
        // base len 11, result len 13, copy 6 bytes from 0, insert "there", copy 2 from 9
        let delta = [
            11, 13, 0x90, 6, 5, b't', b'h', b'e', b'r', b'e', 0x91, 9, 2,
        ];
        assert_eq!(apply_delta(base, &delta), b"hello thereld");
    }

    #[test]
    fn apply_delta_reverses_tree_delta_encoding() {
        let entry = |name: &str, byte: u8| {
            let mut e = format!("100644 {name}\0").into_bytes();
            e.extend_from_slice(&[byte; 20]);
            e
        };
        let base: Vec<u8> = (0..200).flat_map(|i| entry(&format!("f{i:03}"), 1)).collect();
        let target: Vec<u8> = (0..200)
            .flat_map(|i| entry(&format!("f{i:03}"), if i == 77 { 2 } else { 1 }))
            .chain(entry("zzz", 3))
            .collect();

        let delta = encode_tree_delta(&base, &target, target.len()).unwrap();
        assert!(delta.len() < target.len() / 10);
        assert_eq!(apply_delta(&base, &delta), target);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// Every object resolved through the cache must hash to its own id,
    /// no matter which part of its chain is cached or evicted.
    #[test]
    fn cached_reads_match_object_ids() {
        let dir = std::env::temp_dir().join(format!(
            "gitreforge-delta-cache-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q"]);
        for i in 0..80 {
            for f in 0..40 {
                let content = format!("file {f} changed in {}\n", i - i % (f + 1));
                std::fs::write(dir.join("src").join(format!("f{f}")), content).unwrap();
            }
            git(&dir, &["add", "-A"]);
            git(&dir, &["commit", "-qm", &format!("commit {i}")]);
        }
        git(&dir, &["gc", "-q", "--aggressive"]);

        let pack_dir = dir.join(".git/objects/pack");
        let idx = std::fs::read_dir(&pack_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "idx"))
            .unwrap();
        let mmap = unsafe { Mmap::map(&std::fs::File::open(idx.with_extension("pack")).unwrap()) }
            .unwrap();
        let mut offsets = get_pack_offsets(&idx).unwrap();

        let mut decompression = Decompression::default();
        let mut deltas = 0;
        for (slots, reverse) in [(1, false), (7, true), (4096, false), (4096, true)] {
            let mut cache = Cache::new(slots, 64 << 20);
            offsets.reverse();
            for offset in offsets.iter() {
                let object = PackObject::create(&mmap, offset.offset);
                if object.object_type != super::OFS_DELTA {
                    continue;
                }
                deltas += 1;
                let (bytes, base) = resolve(&mut cache, &mut decompression, &mmap, object);
                let prefix = match base.object_type {
                    1 => "commit",
                    2 => "tree",
                    3 => "blob",
                    4 => "tag",
                    t => panic!("unexpected type {t}"),
                };
                assert_eq!(
                    crate::calculate_hash(&bytes, prefix.as_bytes()),
                    offset.hash,
                    "slots {slots} reverse {reverse}"
                );
            }
        }

        std::fs::remove_dir_all(&dir).unwrap();
        assert!(deltas > 100, "test repo has too few deltas: {deltas}");
    }
}
