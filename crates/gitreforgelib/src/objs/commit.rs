use std::fmt::Display;

use bstr::{BStr, BString, ByteSlice, ByteVec};

use crate::shared::SliceIndexes;

use super::{CommitBase, CommitEditable, CommitHash, ObjectHash, TreeHash, WriteBytes};
use memchr::memchr;

impl Display for CommitHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

impl From<ObjectHash> for CommitHash {
    fn from(value: ObjectHash) -> Self {
        CommitHash(value)
    }
}

impl TryFrom<&BStr> for CommitHash {
    type Error = &'static str;

    fn try_from(value: &BStr) -> Result<Self, Self::Error> {
        ObjectHash::try_from_bstr(value)
    }
}

impl Display for CommitBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.hash))?;
        Ok(())
    }
}

fn time_index(line: &[u8]) -> usize {
    let mut spaces = 0;
    for (i, b) in line.iter().rev().enumerate() {
        let index_from_back = line.len() - i - 1;
        if *b == b' ' {
            spaces += 1;
        }

        if spaces == 2 {
            return index_from_back;
        }
    }

    line.len()
}

impl CommitBase {
    pub fn create(hash: CommitHash, bytes: Box<[u8]>, skip_first_null: bool) -> Self {
        let mut bytes_start = 0;
        let mut line_reader = if skip_first_null {
            bytes_start = memchr(b'\0', &bytes).unwrap();
            bytes_start += 1;
            bytes[bytes_start..].lines()
        } else {
            bytes.lines()
        };

        let mut line = line_reader.next().unwrap();
        let tree_line = SliceIndexes::from_slice(&bytes, line, 5);

        let mut parents = Vec::with_capacity(1);
        line = line_reader.next().unwrap();
        while line.starts_with(b"parent ") {
            parents.push(SliceIndexes::from_slice(&bytes, line, 7));
            line = line_reader.next().unwrap();
        }

        let author_line = &line[7..];
        let author_time_index = time_index(author_line);
        let author = SliceIndexes::from_slice(&bytes, &author_line[0..author_time_index], 0);
        let author_time = SliceIndexes::from_slice(&bytes, author_line, author_time_index + 1);

        let committer_line = line_reader.next().map(|line| &line[10..]).unwrap();
        let committer_time_index = time_index(committer_line);
        let committer =
            SliceIndexes::from_slice(&bytes, &committer_line[0..committer_time_index], 0);
        let committer_time =
            SliceIndexes::from_slice(&bytes, committer_line, committer_time_index + 1);

        let committer_line_start: usize =
            unsafe { committer_line.as_ptr().offset_from(bytes.as_ptr()) }
                .try_into()
                .unwrap();
        let remainder_start: usize = committer_line_start + committer_line.len() + 1;
        let remainder = SliceIndexes::new(remainder_start, bytes.len() - remainder_start);

        Self {
            hash,
            bytes: WriteBytes {
                bytes,
                start: bytes_start,
            },
            tree_line,
            parents,
            author,
            author_time,
            committer,
            committer_time,
            remainder,
        }
    }

    pub(crate) fn get_str(&self, f: impl Fn(&CommitBase) -> &SliceIndexes) -> &BStr {
        f(self).get(&self.bytes.bytes).as_bstr()
    }

    pub fn parents(&self) -> Vec<CommitHash> {
        self.parents
            .iter()
            .enumerate()
            .map(|(i, _)| self.get_str(|c| &c.parents[i]).try_into().unwrap())
            .collect()
    }

    pub fn author(&self) -> &bstr::BStr {
        self.get_str(|c| &c.author)
    }

    pub fn committer(&self) -> &bstr::BStr {
        self.get_str(|c| &c.committer)
    }

    pub fn tree(&self) -> TreeHash {
        self.get_str(|c| &c.tree_line).try_into().unwrap()
    }
}

impl CommitEditable {
    pub fn create(base: CommitBase) -> Self {
        let parents = vec![None; base.parents.len()];
        CommitEditable {
            base,
            tree: None,
            author: None,
            committer: None,
            parents,
            signature: None,
        }
    }

    pub fn has_changes(&self) -> bool {
        self.tree.is_some()
            || self.author.is_some()
            || self.committer.is_some()
            || self.signature.is_some()
            || self.parents.iter().any(|p| p.is_some())
    }

    pub fn parents(&self) -> Vec<CommitHash> {
        self.parents
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if let Some(p) = p {
                    p.clone()
                } else {
                    self.base.parents[i]
                        .get(&self.base.bytes.bytes)
                        .as_bstr()
                        .try_into()
                        .unwrap()
                }
            })
            .collect()
    }

    pub fn base_hash(&self) -> &CommitHash {
        &self.base.hash
    }

    pub fn tree(&self) -> TreeHash {
        if let Some(t) = &self.tree {
            t.clone()
        } else {
            self.base.get_str(|c| &c.tree_line).try_into().unwrap()
        }
    }

    pub fn set_tree(&mut self, value: TreeHash) {
        self.tree = Some(value);
    }

    pub fn set_parent(&mut self, index: usize, value: CommitHash) {
        self.parents[index] = Some(value);
    }

    // pub fn author(&self) -> &bstr::BStr {
    //     self.author.get(&self.bytes).as_bstr()
    // }

    pub fn author_bytes(&self) -> &[u8] {
        if let Some(author) = &self.author {
            author
        } else {
            self.base.get_str(|c| &c.author).as_bytes()
        }
    }

    pub fn author(&self) -> &bstr::BStr {
        if let Some(author) = &self.author {
            author.as_bstr()
        } else {
            self.base.get_str(|c| &c.author)
        }
    }

    pub fn set_author(&mut self, author: Vec<u8>) {
        self.author = Some(author);
    }

    // pub fn committer(&self) -> &bstr::BStr {
    //     self.committer.get(&self.bytes).as_bstr()
    // }

    pub fn committer_bytes(&self) -> &[u8] {
        if let Some(committer) = &self.committer {
            committer
        } else {
            self.base.get_str(|c| &c.committer).as_bytes()
        }
        // self.committer.get(&self.bytes)
    }

    pub fn set_committer(&mut self, committer: Vec<u8>) {
        self.committer = Some(committer);
    }

    pub fn committer_email(&self) -> Option<&[u8]> {
        let committer = self.committer_bytes();
        let start = committer.iter().position(|b| *b == b'<')?;
        let end = committer[start + 1..].iter().position(|b| *b == b'>')?;
        Some(&committer[start + 1..start + 1 + end])
    }

    pub fn set_signature(&mut self, signature: Vec<u8>) {
        self.signature = Some(signature);
    }

    // pub fn tree_str(&self) -> &BStr {
    //     if let Some(t) = self.tree {
    //         format!("{}", t).as_bytes().as_bstr()
    //     } else {
    //         self.get_base_str(|commit_base| &commit_base.tree_line).as_bstr()
    //     }
    // }

    fn get_str(
        &self,
        self_getter: impl Fn(&Self) -> &Option<Vec<u8>>,
        base_getter: impl Fn(&CommitBase) -> &SliceIndexes,
    ) -> &BStr {
        if let Some(v) = self_getter(self) {
            v.as_bstr()
        } else {
            self.base.get_str(base_getter)
        }
    }

    pub fn unsigned_bytes(&self) -> WriteBytes {
        self.to_bytes_inner(None, true)
    }

    pub fn to_bytes(self) -> WriteBytes {
        let has_changes = self.has_changes();
        if !has_changes {
            return self.base.bytes;
        }

        self.to_bytes_inner(self.signature.as_deref(), true)
    }

    fn to_bytes_inner(&self, signature: Option<&[u8]>, force_rewrite: bool) -> WriteBytes {
        let has_changes = force_rewrite || self.has_changes();
        if !has_changes {
            return WriteBytes {
                bytes: self.base.bytes.bytes.clone(),
                start: self.base.bytes.start,
            };
        }

        let tree: BString = //self.get_str(|c| &c.tree, |c| &c.tree_line);
            if let Some(tree) = &self.tree {
                tree.to_string().as_bytes().as_bstr().to_owned()
            } else {
                self.base.get_str(|c| &c.tree_line).to_owned()
            };

        let parents: Vec<_> = self.parents().iter().map(|p| format!("{}", p)).collect();

        let author = self.get_str(|c| &c.author, |c| &c.author);
        let author_time = self.base.get_str(|c| &c.author_time);
        let committer = self.get_str(|c| &c.committer, |c| &c.committer);
        let committer_time = self.base.get_str(|c| &c.committer_time);
        let remainder = self.base.get_str(|c| &c.remainder);
        let filtered_remainder = strip_signature_headers(remainder.as_bytes());
        let signature_len = signature.map(folded_signature_len).unwrap_or(0);

        let mut result: Vec<u8> = Vec::with_capacity(
            b"tree \n".len()
                + tree.len()
                + parents
                    .iter()
                    .map(|parent| b"parent \n".len() + parent.len())
                    .sum::<usize>()
                + b"author  \n".len()
                + author.len()
                + committer_time.len()
                + b"committer  \n".len()
                + committer.len()
                + author_time.len()
                + filtered_remainder.len()
                + signature_len,
        );

        result.push_str(b"tree ");
        result.push_str(tree);
        result.push_str(b"\n");

        for parent in parents {
            result.push_str(b"parent ");
            result.push_str(parent);
            result.push_str(b"\n");
        }

        result.push_str(b"author ");
        result.push_str(author);
        result.push_str(b" ");
        result.push_str(author_time);
        result.push_str(b"\n");

        result.push_str(b"committer ");
        result.push_str(committer);
        result.push_str(b" ");
        result.push_str(committer_time);
        result.push_str(b"\n");

        if let Some(signature) = signature {
            push_folded_signature(&mut result, signature);
        }

        result.push_str(&filtered_remainder);

        debug_assert_eq!(result.capacity(), result.len());

        WriteBytes {
            bytes: result.into_boxed_slice(),
            start: 0,
        }
    }
}

fn folded_signature_len(signature: &[u8]) -> usize {
    signature
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(i, line)| {
            line.len()
                + if i == 0 {
                    b"gpgsig \n".len()
                } else {
                    b" \n".len()
                }
        })
        .sum()
}

fn push_folded_signature(result: &mut Vec<u8>, signature: &[u8]) {
    for (i, line) in signature
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if i == 0 {
            result.push_str(b"gpgsig ");
        } else {
            result.push_str(b" ");
        }
        result.push_str(line);
        result.push_str(b"\n");
    }
}

fn strip_signature_headers(remainder: &[u8]) -> Vec<u8> {
    let message_start = remainder
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 1)
        .unwrap_or(0);

    if message_start == 0 {
        return remainder.to_vec();
    }

    let headers = &remainder[..message_start];
    let message = &remainder[message_start..];
    let mut result = Vec::with_capacity(remainder.len());
    let mut index = 0;

    while index < headers.len() {
        let line_end = headers[index..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|offset| index + offset + 1)
            .unwrap_or(headers.len());
        let line = &headers[index..line_end];
        if line == b"\n" {
            break;
        }

        let is_signature = line.starts_with(b"gpgsig ") || line.starts_with(b"gpgsig-sha256 ");
        let block_start = index;
        index = line_end;
        while index < headers.len() && headers[index] == b' ' {
            let continuation_end = headers[index..]
                .iter()
                .position(|b| *b == b'\n')
                .map(|offset| index + offset + 1)
                .unwrap_or(headers.len());
            index = continuation_end;
        }

        if !is_signature {
            result.extend_from_slice(&headers[block_start..index]);
        }
    }

    result.extend_from_slice(message);
    result
}

#[cfg(test)]
mod tests {
    use bstr::ByteSlice;

    use super::*;

    fn hash() -> CommitHash {
        b"53dd2e51161a4eebd8baacd17383c9af35a8283e"
            .as_bstr()
            .try_into()
            .unwrap()
    }

    #[test]
    fn changed_commit_drops_existing_signature_headers() {
        let bytes = b"tree 31aa860596f003d69b896943677e9fe5ff208233\n\
parent 5eec99927bb6058c8180e5dac871c89c7d01b0ab\n\
author A User <a@example.com> 1688207675 +0200\n\
committer C User <c@example.com> 1688209149 +0200\n\
gpgsig -----BEGIN SSH SIGNATURE-----\n\
 abc\n\
 -----END SSH SIGNATURE-----\n\
encoding UTF-8\n\
\n\
message\n";
        let mut commit =
            CommitEditable::create(CommitBase::create(hash(), bytes.to_vec().into(), false));
        commit.set_author(b"Other User <other@example.com>".to_vec());

        let result = commit.to_bytes();
        let result = result.get_bytes();

        assert!(!result.as_bstr().contains_str("gpgsig"));
        assert!(result.as_bstr().contains_str("encoding UTF-8\n\nmessage\n"));
    }

    #[test]
    fn new_signature_is_folded_before_message() {
        let bytes = b"tree 31aa860596f003d69b896943677e9fe5ff208233\n\
author A User <a@example.com> 1688207675 +0200\n\
committer C User <c@example.com> 1688209149 +0200\n\
\n\
message\n";
        let mut commit =
            CommitEditable::create(CommitBase::create(hash(), bytes.to_vec().into(), false));
        commit.set_signature(b"-----BEGIN SSH SIGNATURE-----\nabc\n-----END SSH SIGNATURE-----\n".to_vec());

        let result = commit.to_bytes();
        let result = result.get_bytes();

        assert!(result.as_bstr().contains_str(
            "gpgsig -----BEGIN SSH SIGNATURE-----\n abc\n -----END SSH SIGNATURE-----\n\nmessage\n"
        ));
    }
}
