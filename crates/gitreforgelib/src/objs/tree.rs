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
                start: position - null_terminator_index - 21,
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
    /// offset of the entry in the tree's bytes
    start: usize,
}

impl TreeLineIndex {
    fn end(&self) -> usize {
        self.hash_start() + 20
    }

    fn hash_start(&self) -> usize {
        self.end_of_text() + 1
    }

    fn end_of_text(&self) -> usize {
        match &self.text {
            shared::RefSlice::Referenced(indexes) => indexes.end(),
            shared::RefSlice::Owned(_) => unreachable!("tree lines reference the tree's bytes"),
        }
    }
}

/// Builds a modified copy of a tree by removing entries or replacing their
/// hashes, copying the unchanged bytes in between as whole ranges. Edits
/// must be made in entry order.
pub struct TreeEdit<'a> {
    tree: &'a Tree,
    bytes: Vec<u8>,
    copied_to: usize,
}

impl<'a> TreeEdit<'a> {
    pub fn new(tree: &'a Tree) -> Self {
        Self {
            tree,
            bytes: Vec::with_capacity(tree.bytes.len() - tree.bytes_start),
            copied_to: tree.bytes_start,
        }
    }

    fn copy_until(&mut self, position: usize) {
        self.bytes
            .extend_from_slice(&self.tree.bytes[self.copied_to..position]);
    }

    pub fn remove(&mut self, index: usize) {
        let line = &self.tree.lines[index];
        self.copy_until(line.start);
        self.copied_to = line.end();
    }

    pub fn replace_hash(&mut self, index: usize, hash: &TreeHash) {
        let line = &self.tree.lines[index];
        self.copy_until(line.hash_start());
        self.bytes.extend_from_slice(hash.as_bytes());
        self.copied_to = line.end();
    }

    pub fn finish(mut self) -> Tree {
        self.copy_until(self.tree.bytes.len());
        let object_hash = TreeHash(crate::calculate_hash(&self.bytes, b"tree"));
        Tree::create(object_hash, self.bytes.into_boxed_slice(), false)
    }
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

#[cfg(test)]
mod test {
    use std::borrow::Cow;

    use bstr::ByteSlice;

    use super::{Tree, TreeEdit, TreeHash, TreeLine};
    use crate::shared::ObjectHash;

    fn hash(byte: u8) -> TreeHash {
        TreeHash(ObjectHash::from([byte; 20]))
    }

    fn tree(entries: &[(&str, u8)]) -> Tree {
        entries
            .iter()
            .map(|(text, byte)| TreeLine {
                hash: Cow::Owned(hash(*byte)),
                text: text.as_bytes().as_bstr(),
            })
            .collect()
    }

    #[test]
    fn edit_matches_rebuilt_tree() {
        let original = tree(&[
            ("100644 a.txt", 1),
            ("40000 dir", 2),
            ("100644 m.txt", 3),
            ("40000 sub", 4),
            ("100644 z.txt", 5),
        ]);

        let mut edit = TreeEdit::new(&original);
        edit.remove(0);
        edit.replace_hash(1, &hash(9));
        edit.remove(3);
        let edited = edit.finish();

        let expected = tree(&[("40000 dir", 9), ("100644 m.txt", 3), ("100644 z.txt", 5)]);
        assert_eq!(edited.hash(), expected.hash());
        assert_eq!(edited.lines().count(), 3);
    }

    #[test]
    fn edit_can_remove_everything() {
        let original = tree(&[("100644 a.txt", 1), ("40000 dir", 2)]);
        let mut edit = TreeEdit::new(&original);
        edit.remove(0);
        edit.remove(1);
        assert_eq!(edit.finish().hash(), tree(&[]).hash());
    }
}
