use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs::File,
    hash::BuildHasher,
    io::BufReader,
    path::{Path, PathBuf},
};

use bstr::{
    io::{BufReadExt, ByteLines},
    BStr, BString, ByteSlice,
};

use crate::{
    objs::{CommitHash, Tag, TagTargetType},
    shared::ObjectHash,
    Repository,
};

static PACKED: std::sync::Mutex<Option<Vec<(String, String)>>> = std::sync::Mutex::new(None);

trait RefName {
    fn get_name(&self) -> &BStr;
    fn get_target(&self) -> &BStr;
}

#[derive(Debug)]
pub enum GitRef {
    Simple(SimpleRef),
    Tag(TagRef),
}

impl RefName for GitRef {
    fn get_name(&self) -> &BStr {
        match self {
            GitRef::Simple(simple) => simple.name[..].as_bstr(),
            GitRef::Tag(tag) => tag.name[..].as_bstr(),
        }
    }

    fn get_target(&self) -> &BStr {
        match self {
            GitRef::Simple(simple) => simple.hash[..].as_bstr(),
            GitRef::Tag(tag) => tag.hash[..].as_bstr(),
        }
    }
}

#[derive(Debug)]
pub struct SimpleRef {
    pub name: bstr::BString,
    pub hash: bstr::BString,
}

#[derive(Debug)]
pub struct TagRef {
    pub name: BString,
    pub hash: BString,
    pub obj_hash: BString,
}

impl GitRef {
    pub fn read_all(base_path: &std::path::Path) -> Result<Vec<GitRef>, Box<dyn Error>> {
        let packed_refs_path = base_path.join("packed-refs");

        let file = File::open(packed_refs_path);
        let packed_refs = match file {
            Ok(file) => {
                let reader = BufReader::new(file);

                let packed_refs = get_packed_refs(&mut reader.byte_lines())?;
                Some(packed_refs)
            }
            Err(_) => None,
        };

        let mut refs = get_loose_refs(base_path, "refs");

        if let Some(packed_refs) = packed_refs {
            // a loose ref is newer than a packed one with the same name
            let loose_names: HashSet<BString> =
                refs.iter().map(|r| r.get_name().to_owned()).collect();
            refs.extend(
                packed_refs
                    .into_iter()
                    .filter(|r| !loose_names.contains(r.get_name())),
            );
        }

        Ok(refs)
    }

    pub fn update<T: BuildHasher>(
        repository: &mut Repository,
        rewritten_commits: &HashMap<CommitHash, CommitHash, T>,
        dry_run: bool,
    ) {
        let packed = std::env::var_os("GITREFORGE_PACKED_REFS").is_some();
        if packed {
            *PACKED.lock().unwrap() = Some(Vec::new());
        }
        for r in repository.refs().unwrap() {
            Self::rewrite_ref(
                repository,
                r.get_name(),
                r.get_target(),
                rewritten_commits,
                dry_run,
            );
        }

        let mut path = repository.path.clone();
        path.push("packed-refs");
        if packed {
            // PROTOTYPE: one packed-refs file instead of one loose file per ref
            let mut refs = PACKED.lock().unwrap().take().unwrap();
            refs.sort();
            let mut out = String::from("# pack-refs with: sorted \n");
            for (name, target) in refs {
                let loose = repository.path.join(&name);
                if loose.is_file() {
                    std::fs::remove_file(loose).unwrap();
                }
                out.push_str(&target);
                out.push(' ');
                out.push_str(&name);
                out.push('\n');
            }
            let tmp = repository.path.join("packed-refs.new");
            std::fs::write(&tmp, out).unwrap();
            std::fs::rename(tmp, path).unwrap();
            return;
        }
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }

    fn write_ref(repository_path: &str, ref_name: &str, ref_target: &str) {
        if let Some(refs) = PACKED.lock().unwrap().as_mut() {
            refs.push((ref_name.to_owned(), ref_target.to_owned()));
            return;
        }
        let path: PathBuf = [repository_path, ref_name].iter().collect();

        let file_name = path.file_name().unwrap();
        let ref_path = path.to_str().unwrap();
        let dir_path = &ref_path[0..ref_path.len() - file_name.len()];
        std::fs::create_dir_all(dir_path).unwrap();
        std::fs::write(path, format!("{ref_target}\n")).unwrap();
    }

    fn rewrite_ref<T: BuildHasher>(
        repository: &mut Repository,
        ref_name: &BStr,
        ref_target: &BStr,
        rewritten_commits: &HashMap<CommitHash, CommitHash, T>,
        dry_run: bool,
    ) -> ObjectHash {
        let tag_target_obj = repository
            .read_object(ref_target.try_into().unwrap())
            .unwrap();
        match tag_target_obj {
            crate::objs::GitObject::Commit(_) => {
                let tag_target: CommitHash = ref_target.try_into().unwrap();
                let rewritten_target = rewritten_commits.get(&tag_target).unwrap_or(&tag_target);
                Self::write_ref(
                    repository.path.clone().to_str().unwrap(),
                    ref_name.to_str().unwrap(),
                    rewritten_target.to_string().as_str(),
                );

                rewritten_target.clone().0
            }
            crate::objs::GitObject::Tree(tree) => {
                Self::write_ref(
                    repository.path.to_str().unwrap(),
                    ref_name.to_str().unwrap(),
                    ref_target.to_str().unwrap(),
                );

                tree.hash().0.clone()
            }
            crate::objs::GitObject::Tag(target_tag) => match target_tag.target_type() {
                TagTargetType::Commit | TagTargetType::Tag => {
                    let target_hash =
                        Self::rewrite_tag(repository, target_tag, rewritten_commits, dry_run);

                    Self::write_ref(
                        repository.path.to_str().unwrap(),
                        ref_name.to_str().unwrap(),
                        target_hash.to_string().as_str(),
                    );

                    target_hash
                }
                TagTargetType::Tree | TagTargetType::Blob => {
                    let target_tag_hash = target_tag.hash().clone();
                    Repository::write(repository.path.clone(), target_tag.into(), dry_run);
                    Self::write_ref(
                        repository.path.to_str().unwrap(),
                        ref_name.to_str().unwrap(),
                        target_tag_hash.to_string().as_str(),
                    );
                    target_tag_hash
                }
            },
        }
    }

    /// Rewrites an annotated tag that points to a commit or (recursively) to
    /// another tag and returns the hash of the rewritten tag.
    fn rewrite_tag<T: BuildHasher>(
        repository: &mut Repository,
        mut tag: Tag,
        rewritten_commits: &HashMap<CommitHash, CommitHash, T>,
        dry_run: bool,
    ) -> ObjectHash {
        let new_object = match tag.target_type() {
            TagTargetType::Commit => {
                let target_hash = CommitHash(tag.object());
                rewritten_commits
                    .get(&target_hash)
                    .unwrap_or(&target_hash)
                    .clone()
                    .0
            }
            TagTargetType::Tag => match repository.read_object(tag.object()).unwrap() {
                crate::objs::GitObject::Tag(inner) => {
                    Self::rewrite_tag(repository, inner, rewritten_commits, dry_run)
                }
                _ => panic!("Tag {} does not point to a tag object", tag.name()),
            },
            TagTargetType::Tree | TagTargetType::Blob => tag.object(),
        };

        tag.set_object(new_object);
        let tag = Tag::create(None, tag.to_bytes(), false);
        let tag_hash = tag.hash().clone();
        Repository::write(repository.path.clone(), tag.into(), dry_run);
        tag_hash
    }
}

fn get_loose_refs(base_path: &Path, current_path: &str) -> Vec<GitRef> {
    let mut result: Vec<GitRef> = Vec::new();

    let full_path = base_path.join(current_path);
    for dir_entry in std::fs::read_dir(full_path).unwrap().map(|x| x.unwrap()) {
        let file_type = dir_entry.file_type().unwrap();
        if file_type.is_dir() {
            let mut next_path = String::new();
            next_path.push_str(current_path);
            next_path.push('/');
            next_path.push_str(dir_entry.path().file_name().unwrap().to_str().unwrap());
            result.append(&mut get_loose_refs(base_path, &next_path));
        } else {
            let hash = BString::from(
                std::fs::read_to_string(&dir_entry.path())
                    .unwrap()
                    .trim_end(),
            );

            let mut name = String::new();
            name.push_str(current_path);
            name.push('/');
            name.push_str(dir_entry.file_name().to_str().unwrap());

            if !hash.starts_with(b"ref: ") {
                result.push(GitRef::Simple(SimpleRef {
                    name: BString::from(name),
                    hash,
                }))
            }
        }
    }

    result
}

fn get_packed_refs(lines: &mut ByteLines<BufReader<File>>) -> Result<Vec<GitRef>, Box<dyn Error>> {
    let mut result: Vec<GitRef> = Vec::new();

    let Some(first_line) = lines.next() else {
        return Ok(result);
    };
    let mut previous_line = Some(first_line?);
    let mut line_started = previous_line
        .as_ref()
        .map(|x| !x.starts_with(b"#"))
        .unwrap();

    for current_line in lines.by_ref().flatten() {
        if current_line.starts_with(b"^") {
            if let Some(x) = previous_line.take() {
                let split = x.split_at(41);
                result.push(GitRef::Tag(TagRef {
                    hash: split.0[0..split.0.len() - 1].as_bstr().to_owned(),
                    name: split.1.as_bstr().to_owned(),
                    obj_hash: current_line.split_at(1).1.as_bstr().to_owned(),
                }));
            };

            line_started = false;
        } else {
            if line_started {
                if let Some(x) = previous_line.take() {
                    let split = x.split_at(41);
                    result.push(GitRef::Simple(SimpleRef {
                        hash: split.0[0..split.0.len() - 1].as_bstr().to_owned(),
                        name: split.1.as_bstr().to_owned(),
                    }));
                };
            }

            line_started = !current_line.starts_with(b"#");
            previous_line = Some(current_line);
        }
    }

    if line_started {
        let previous_line = previous_line.unwrap();
        let split = previous_line.split_at(41);
        let hash = split.0[..split.0.len() - 1].as_bstr().to_owned();
        let name = split.1.as_bstr().to_owned();
        result.push(GitRef::Simple(SimpleRef { hash, name }));
    }

    Ok(result)
}

#[cfg(test)]
mod test {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn read_packed_and_loose_refs() {
        let dir = std::env::temp_dir().join(format!("gitreforge-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&dir, &["tag", "-a", "-m", "annotated", "v1"]);
        git(&dir, &["branch", "a-branch"]);
        git(&dir, &["pack-refs", "--all"]);
        // main moves on: the loose ref is current, the packed one is stale
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "two"]);
        git(&dir, &["branch", "z-loose-only"]);

        let mut refs: Vec<(String, String)> = GitRef::read_all(&dir.join(".git"))
            .expect("Cannot read refs")
            .iter()
            .map(|r| (r.get_name().to_string(), r.get_target().to_string()))
            .collect();
        refs.sort();

        let expected: Vec<(String, String)> = git(
            &dir,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
        .lines()
        .map(|l| {
            let (name, hash) = l.split_once(' ').unwrap();
            (name.to_string(), hash.to_string())
        })
        .collect();

        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(refs, expected);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn read_refs_with_empty_packed_refs() {
        let dir = std::env::temp_dir().join(format!("gitreforge-refs-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        std::fs::write(dir.join(".git/packed-refs"), "").unwrap();

        let refs = GitRef::read_all(&dir.join(".git")).expect("Cannot read refs");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(refs.len(), 1);
    }
}
