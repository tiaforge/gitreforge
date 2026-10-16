# gitreforge

`gitreforge` rewrites Git history in bare or mirrored repositories. Use it when you
need to clean sensitive files out of history, fix author or committer identities,
or remove commits that became empty after a rewrite.

The tool works directly on Git objects and refs. Because every rewrite creates
new commit hashes, run it on a mirror clone or another disposable copy first,
inspect the result, and then force-push the rewritten refs when you are ready.

## What You Can Do

- List every author and committer identity found in the repository.
- Rewrite author and committer names and email addresses in existing commits.
- Remove files, directories, or path patterns from every commit.
- Prune non-merge commits whose tree is identical to their parent.
- Re-sign selected rewritten commits with SSH commit signatures.
- Preview changes with `--dry-run` before modifying the repository.

## Performance

`gitreforge` is built for high-throughput history rewrites and performs
significantly better than most general-purpose alternatives on large
repositories.

That performance comes from working directly with Git objects and refs instead
of driving each change through Git commands. `gitreforge` reads pack files through
memory-mapped I/O, parallelizes rewrite work, uses a release allocator tuned for
throughput, and avoids external command execution in the core rewrite and
signing paths. Rewritten objects go into a single new pack file, with trees
stored as deltas, and the refs into `packed-refs`, instead of creating one file
per object and ref.

### Benchmarks

Wall-clock time to rewrite the full history of a mirror clone, compared with
[git filter-repo](https://github.com/newren/git-filter-repo) 2.47 and
[BFG Repo-Cleaner](https://rtyley.github.io/bfg-repo-cleaner/) 1.15:

| Repository | Task | gitreforge | git filter-repo | BFG |
|:---|:---|---:|---:|---:|
| [tokio](https://github.com/tokio-rs/tokio) (24k commits) | Remove `tokio-util/` | **0.26 s** | 2.0 s (7.6×) | 5.1 s (19×) |
| [tokio](https://github.com/tokio-rs/tokio) (24k commits) | Remove `tokio-*/` (wildcard) | **0.27 s** | 2.1 s (7.7×) | 8.6 s (32×) |
| [git](https://github.com/git/git) (205k commits) | Remove `Documentation/` | **2.8 s** | 69 s (25×) | fails after 94 s¹ |
| [git](https://github.com/git/git) (205k commits) | Remove every `Makefile` | **5.5 s** | 78 s (14×) | fails after 142 s¹ |
| [git](https://github.com/git/git) (205k commits) | Remove every `*.po` file (regex) | **5.3 s** | 72 s (14×) | 103 s (20×) |
| [git](https://github.com/git/git) (205k commits) | Rename an author (7 identities) | **0.86 s** | 61 s (71×) | not supported |

¹ BFG aborts with a `NullPointerException` at 95% of the commits on two old
tags in git/git that have no `tagger` line, and leaves all refs unchanged.

All tools only rewrite history: no `git gc` or repack, and empty commits are
kept, so every tool that finishes produces the same trees and identities. Measured with
[hyperfine](https://github.com/sharkdp/hyperfine) on an AMD Ryzen Z2 Extreme
with NVMe storage. Reproduce with `bench/rewrite.sh <scenario>`; the script
downloads the other tools and lists the available scenarios.

The other commands on the same git/git mirror:

| Task | gitreforge | Reference |
|:---|---:|:---|
| `contributor list` | **0.43 s** | `git log --all` piped into `sort -u`: 1.17 s (2.7×) |
| `--dry-run remove --directory /Documentation` | 2.3 s | the same rewrite without `--dry-run`: 3.0 s |
| `prune-empty` after removing `Documentation/` | **1.43 s** | git filter-repo `--prune-empty always`: 68 s (48×)² |

² `gitreforge` removes 11,485 commits, filter-repo 11,292: every commit
filter-repo removes, plus merges that merge nothing once pruning makes one
parent an ancestor of the other.

Reproduce with `bench/features.sh` after `bench/rewrite.sh git-docs`.

## Installation

Build from source with Cargo:

```sh
cargo build --release
```

The binary is written to `target/release/gitreforge`.

## Basic Workflow

Start from a mirror clone so tags, branches, and refs are available without
using a working tree:

```sh
git clone --mirror git@example.com:org/project.git project.git
cd project.git
```

Run `gitreforge` against the mirror repository:

```sh
gitreforge . contributor list
gitreforge --dry-run . remove --file secrets.env
gitreforge . prune-empty
```

After a successful rewrite, validate the repository with normal Git tooling.
When the result is correct, push the rewritten refs according to your repository
hosting policy.

## Contributor Cleanup

List all identities:

```sh
gitreforge /path/to/project.git contributor list
```

Rewrite identities by piping mappings into `contributor rewrite`. Each input
line maps the full old identity to the full new identity:

```sh
cat mappings.txt | gitreforge /path/to/project.git contributor rewrite
```

`mappings.txt`:

```text
Old Name <old@example.com> = New Name <new@example.com>
Another Old Name <old2@example.com> = Another New Name <new2@example.com>
```

Only commits matching the old author or committer identity are changed.

## Removing Paths From History

Remove a specific file everywhere it appears:

```sh
gitreforge /path/to/project.git remove --file secrets.env
```

Paths that start with `/` are absolute and only match at that location.
Other paths match at any depth: `config/secrets.env` removes
`config/secrets.env` and `app/config/secrets.env`, but not
`myconfig/secrets.env`.

Remove a directory:

```sh
gitreforge /path/to/project.git remove --directory vendor/private
```

Use a regular expression when the simpler file or directory matchers are not
enough. It is matched against the full path of each file, which starts with a
`/` (`/src/debug-1.log`):

```sh
gitreforge /path/to/project.git remove --regex '(^|/)debug-[^/]+\.log$'
gitreforge /path/to/project.git remove --regex '^/logs/.*\.log$'
```

You can pass each matcher more than once, and you can combine matcher types in
one command. Directories that are left without any files are removed as well.

## Pruning Empty Commits

After removing paths, some commits may no longer change the tree. Remove those
non-merge commits with:

```sh
gitreforge /path/to/project.git prune-empty
```

Merge commits are preserved, unless pruning leaves them with only a single
parent: a merge of a branch that only had empty commits is pruned like any
other empty commit.

## Commit Signing

History rewrites invalidate existing commit signatures. `gitreforge` can add new SSH
signatures to rewritten commits whose committer email matches
`--sign-committer`.

Configure Git for SSH signing before running the rewrite:

```sh
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
```

Then select the committer email addresses that should be signed:

```sh
gitreforge --sign-committer alice@example.com /path/to/project.git remove --file secrets.env
gitreforge --sign-committer alice@example.com --sign-committer bob@example.com /path/to/project.git prune-empty
```

Signing is applied by committer email, not author email. Commits with other
committer emails are rewritten without a new signature. Commits that the
rewrite doesn't change keep their id and their existing signature.

Supported signing configuration:

- The configuration is read from the repository's `config` and the global git
  configuration (`$GIT_CONFIG_GLOBAL`, or `$XDG_CONFIG_HOME/git/config` and
  `~/.gitconfig`).
- `gpg.format` must be `ssh`.
- `user.signingkey` may point to a public key file, a private OpenSSH key file,
  an inline `ssh-...` public key, or a `key::ssh-...` value.
- If `user.signingkey` is a public key or omitted, signing uses the first
  suitable key from `SSH_AUTH_SOCK`.
- Private key files are read and used directly. Encrypted private keys must be
  added to the SSH agent instead.

Each commit's id depends on the signature of its parent, so the signatures are
made one after another. An Ed25519 key file is the fastest option, about 10 µs
per commit; the SSH agent needs about 100 µs per commit, RSA keys a few
milliseconds.

Current limitations:

- OpenPGP signing is not supported.
- `gpg.ssh.program` is not supported because `gitreforge` does not execute external
  signing commands.
- SSH agent signing currently requires a Unix socket.

## Dry Runs

Add `--dry-run` before the repository path to verify which operations can run
without writing rewritten objects or updating refs:

```sh
gitreforge --dry-run /path/to/project.git remove --directory generated
```

## Command Reference

```text
gitreforge [OPTIONS] [REPOSITORY] <COMMAND>
```

`REPOSITORY` is the path to a bare or mirrored repository. If omitted, `gitreforge`
uses the current directory.

Every rewrite writes the old and new id of each changed commit to
`object-id-map.old-new.txt` in the current directory.

Global options:

- `-d`, `--dry-run`: do not change the repository.
- `--sign-committer <EMAIL>`: re-sign rewritten commits whose committer email
  matches this value. Can be specified multiple times.

Commands:

- `contributor list`: list all author and committer identities.
- `contributor rewrite`: rewrite identities from stdin mappings in the form
  `Old User <old@example.com> = New User <new@example.com>`. Blank lines are
  ignored.
- `remove --file <FILE>`: remove matching files from history. `*` can be used
  as a wildcard at the beginning or end.
- `remove --directory <DIRECTORY>`: remove matching directories from history.
  `*` can be used as a wildcard at the beginning or end.
- `remove --regex <REGEX>`: remove files whose full repository path, starting
  with `/`, matches the regular expression.
- `prune-empty`: remove empty non-merge commits.

## Safety Notes

- Do not run `gitreforge` on a repository with a working copy.
- Keep a backup or fresh mirror clone until you have verified the rewritten
  history.
- Coordinate force-pushes with other users of the repository.
- Treat old clones, forks, pull requests, and CI caches as possible places where
  removed data can still exist after a history rewrite.

## License

gitreforge is free for personal use, hobby projects, research and
noncommercial organizations under the
[PolyForm Noncommercial License 1.0.0](LICENSE.md). It is source-available
rather than open source: commercial use needs a separate license.
