use std::{borrow::Cow, fmt::Display};

use bstr::{BStr, ByteSlice, ByteVec};

use crate::{
    shared::{self, RefSlice},
    WriteBytes,
};

use super::{ObjectHash, Tree, TreeHash};

impl Tree {
    pub fn create(object_hash: TreeHash, bytes: Box<[u8]>, skip_first_null: bool) -> Tree {
        let start_index = if skip_first_null {
            memchr::memchr(0, &bytes).unwrap() + 1
        } else {
            0
        };

        let mut position = start_index;
        // entries are at least 28 bytes: "40000 x\0" + 20 byte hash
        let mut lines = Vec::with_capacity((bytes.len() - start_index) / 28);

        while let Some(null_terminator_index) = memchr::memchr(0, &bytes[position..]) {
            let text = RefSlice::new(position, null_terminator_index);
            let hash_start = position + null_terminator_index + 1;
            let tree_hash: TreeHash = bytes[hash_start..hash_start + 20].try_into().unwrap();

            position = hash_start + 20;

            lines.push(TreeLineIndex {
                hash: tree_hash,
                text,
            });
        }

        Tree {
            object_hash,
            lines,
            bytes,
            bytes_start: start_index,
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = TreeLine<'_>> {
        self.lines.iter().map(|tree_line_index| TreeLine {
            hash: Cow::Borrowed(&tree_line_index.hash),
            text: tree_line_index.text.get(&self.bytes).as_bstr(),
        })
    }

    pub fn bytes(self) -> WriteBytes {
        WriteBytes {
            bytes: self.bytes,
            start: self.bytes_start,
        }
    }
}

impl<'a> FromIterator<TreeLine<'a>> for Tree {
    fn from_iter<T: IntoIterator<Item = TreeLine<'a>>>(iter: T) -> Self {
        let mut buf: Vec<u8> = Vec::new();
        for line in iter {
            buf.push_str(line.text);
            buf.push(b'\0');
            for c in line.hash.0.bytes {
                buf.push(c);
            }
        }

        let object_hash = crate::calculate_hash(&buf, b"tree");

        Self::create(TreeHash(object_hash), buf.into_boxed_slice(), false)
    }
}

pub struct TreeLine<'a> {
    pub hash: Cow<'a, TreeHash>,
    pub text: &'a BStr,
}

impl<'a> TreeLine<'a> {
    pub fn is_tree(&self) -> bool {
        self.text[0] != b'1'
    }

    pub fn filename(&self) -> &[u8] {
        let seperator_index = memchr::memchr(b' ', self.text).unwrap();
        &self.text[seperator_index + 1..]
    }
}

impl<'a> Display for TreeLine<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let null_index = self.text.iter().position(|c| *c == b' ').unwrap();
        f.write_fmt(format_args!(
            "{}{} {} {}\t{}",
            if self.is_tree() {
                b"0".as_bstr()
            } else {
                b"".as_bstr()
            },
            &self.text[0..null_index],
            if self.is_tree() {
                b"tree".as_bstr()
            } else {
                b"blob".as_bstr()
            },
            self.hash,
            &self.text[null_index + 1..]
        ))
    }
}

#[derive(Debug)]
pub struct TreeLineIndex {
    hash: TreeHash,
    text: shared::RefSlice<u8>,
}

impl TreeHash {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0.bytes
    }
}

impl Display for TreeHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

impl TryFrom<&[u8]> for TreeHash {
    type Error = &'static str;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        Ok(ObjectHash::try_from(value)?.into())
    }
}

impl TryFrom<&BStr> for TreeHash {
    type Error = &'static str;

    fn try_from(value: &BStr) -> Result<Self, Self::Error> {
        ObjectHash::try_from_bstr(value)
    }
}

impl From<ObjectHash> for TreeHash {
    fn from(value: ObjectHash) -> Self {
        TreeHash(value)
    }
}

impl Display for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in self.lines.iter() {
            let text = line.text.get(&self.bytes).as_bstr();
            writeln!(f, "{} {}", line.hash, text)?;
        }
        Ok(())
    }
}
