# gitrw

`gitrw` rewrites Git history in bare or mirrored repositories. Use it when you
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

`gitrw` is built for high-throughput history rewrites and performs
significantly better than most general-purpose alternatives on large
repositories.

That performance comes from working directly with Git objects and refs instead
of driving each change through Git commands. `gitrw` reads pack files through
memory-mapped I/O, parallelizes rewrite work, uses a release allocator tuned for
throughput, and avoids external command execution in the core rewrite and
signing paths.

## Installation

Build from source with Cargo:

```sh
cargo build --release
```

The binary is written to `target/release/gitrw`.

## Basic Workflow

Start from a mirror clone so tags, branches, and refs are available without
using a working tree:

```sh
git clone --mirror git@example.com:org/project.git project.git
cd project.git
```

Run `gitrw` against the mirror repository:

```sh
gitrw . contributor list
gitrw --dry-run . remove --file secrets.env
gitrw . prune-empty
```

After a successful rewrite, validate the repository with normal Git tooling.
When the result is correct, push the rewritten refs according to your repository
hosting policy.

## Contributor Cleanup

List all identities:

```sh
gitrw /path/to/project.git contributor list
```

Rewrite identities by piping mappings into `contributor rewrite`. Each input
line maps the full old identity to the full new identity:

```sh
cat mappings.txt | gitrw /path/to/project.git contributor rewrite
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
gitrw /path/to/project.git remove --file secrets.env
```

Remove a directory:

```sh
gitrw /path/to/project.git remove --directory vendor/private
```

Use a regular expression when the simpler file or directory matchers are not
enough:

```sh
gitrw /path/to/project.git remove --regex '(^|/)debug-[^/]+\.log$'
```

You can pass each matcher more than once, and you can combine matcher types in
one command.

## Pruning Empty Commits

After removing paths, some commits may no longer change the tree. Remove those
non-merge commits with:

```sh
gitrw /path/to/project.git prune-empty
```

Merge commits are preserved.

## Commit Signing

History rewrites invalidate existing commit signatures. `gitrw` can add new SSH
signatures to rewritten commits whose committer email matches
`--sign-committer`.

Configure Git for SSH signing before running the rewrite:

```sh
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519.pub
```

Then select the committer email addresses that should be signed:

```sh
gitrw --sign-committer alice@example.com /path/to/project.git remove --file secrets.env
gitrw --sign-committer alice@example.com --sign-committer bob@example.com /path/to/project.git prune-empty
```

Signing is applied by committer email, not author email. Commits with other
committer emails are rewritten without a new signature.

Supported signing configuration:

- `gpg.format` must be `ssh`.
- `user.signingkey` may point to a public key file, a private OpenSSH key file,
  an inline `ssh-...` public key, or a `key::ssh-...` value.
- If `user.signingkey` is a public key or omitted, signing uses the first
  suitable key from `SSH_AUTH_SOCK`.
- Private key files are read and used directly.

Current limitations:

- OpenPGP signing is not supported.
- `gpg.ssh.program` is not supported because `gitrw` does not execute external
  signing commands.
- SSH agent signing currently requires a Unix socket.

## Dry Runs

Add `--dry-run` before the repository path to verify which operations can run
without writing rewritten objects or updating refs:

```sh
gitrw --dry-run /path/to/project.git remove --directory generated
```

## Command Reference

```text
gitrw [OPTIONS] [REPOSITORY] <COMMAND>
```

`REPOSITORY` is the path to a bare or mirrored repository. If omitted, `gitrw`
uses the current directory.

Global options:

- `-d`, `--dry-run`: do not change the repository.
- `--sign-committer <EMAIL>`: re-sign rewritten commits whose committer email
  matches this value. Can be specified multiple times.

Commands:

- `contributor list`: list all author and committer identities.
- `contributor rewrite`: rewrite identities from stdin mappings in the form
  `Old User <old@example.com> = New User <new@example.com>`.
- `remove --file <FILE>`: remove matching files from history. `*` can be used
  as a wildcard at the beginning or end.
- `remove --directory <DIRECTORY>`: remove matching directories from history.
  `*` can be used as a wildcard at the beginning or end.
- `remove --regex <REGEX>`: remove paths whose full repository path matches the
  regular expression.
- `prune-empty`: remove empty non-merge commits.

## Safety Notes

- Do not run `gitrw` on a repository with a working copy.
- Keep a backup or fresh mirror clone until you have verified the rewritten
  history.
- Coordinate force-pushes with other users of the repository.
- Treat old clones, forks, pull requests, and CI caches as possible places where
  removed data can still exist after a history rewrite.
