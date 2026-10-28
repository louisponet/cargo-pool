<img src="assets/icon.svg" width="64" height="64" alt="">

# cargo-pool

Share warm cargo `target/` directories between git worktrees, so a fresh worktree
(e.g. one an agent just created) doesn't build the whole dependency graph from scratch.

## How it works

- Every repository (keyed by its git common dir) gets a pool of **slots** in
  `<pool-directory>/<repo>-<hash>/slots/<n>/target`. The default pool directory is
  `~/.cache/cargo-pool`.
- `cargo-pool attach` replaces a worktree's `target` with a symlink to a slot. Each slot
  is leased to one worktree at a time, so parallel agents never block on each other's
  cargo build lock. The main checkout uses the same rules. Existing build artifacts are
  moved into a new slot, with a copy across filesystems only if it fits below the disk
  limit. Explicit `attach` refuses while a build is running. A deferred worktree's first
  cargo command waits for it. Detach to make the slot available for reuse.
- Worktrees created through the hook take a slot only on their first cargo command, so
  worktrees that never build hold none. Cargo asks the compiler for its version
  (`rustc -vV`, through the rustc wrapper) before it touches the target dir, and the slot is
  leased and linked then. Without the rustc wrapper installed, the hook attaches right away.
- A free slot is picked across all configured pool directories by the commit it was
  last built at: the closest to the new worktree's `HEAD` wins. Directories at or above
  `maxDiskPercent` are excluded from reuse. New slots use the directory with the most
  available bytes. Existing attachments stay in their current directory.
- If no slot is free, a new, empty one is created. It's **seeded** on the worktree's first
  cargo command, by copying the closest existing target dir (another slot or the main
  checkout's `target/`); that first command waits for the copy. The copy holds the source's
  build lock, so it never copies a half-written build. The `incremental/` caches are left
  out: they're often most of the bytes, and only speed up the first rebuild of a crate
  edited after seeding. Without the rustc wrapper installed, new slots stay empty. There is
  no limit on the number of slots: disk usage is the only limit.
- **Pruning by Cargo.lock**: cargo never deletes artifacts of dependency versions it no longer
  uses, so slots only grow. cargo-pool drops every build unit that the slot's worktree can't
  use at its current `Cargo.lock` and toolchain: units built from a crate version or git
  revision the lock doesn't contain, units built by another toolchain, unfinished units, and
  everything depending on them. It handles both cargo's classic target layout and the new
  build-dir layout. A seed copies only the units the new worktree can use. A slot is pruned in
  place when a worktree takes it over, and after each build once its worktree's `Cargo.lock` or
  toolchain has changed. It never runs during a build. If a deleted unit was needed after all,
  cargo rebuilds it.
- **Disk limit**: if the disk holding the pool is above `maxDiskPercent` (default 90%),
  pooled target dirs on that filesystem are deleted, across all repos' pools and configured
  directories, until usage is back under the limit. This happens on every attach, on every pooled build (with the rustc wrapper
  installed, see below) and on `gc`. The least recently used slots go first, whether or
  not a worktree holds them. A worktree whose slot is deleted loses its `target` link and
  gets a new slot on its next cargo command. Slots with a build running or being seeded
  are never deleted. Once attached, the main checkout's slot follows the same eviction rules.
  A new slot is only seeded if the copy fits under the limit. Making room for it only
  deletes slots no worktree holds; if that isn't enough, nothing is deleted and the slot
  starts empty.
- Leases end on their own when the worktree is deleted (`git worktree remove`),
  or explicitly with `cargo-pool detach`.
- `/target` is added to `.git/info/exclude`, because `target/` patterns in `.gitignore`
  don't match symlinks.

Cargo hashes path dependencies relative to the workspace root, so third-party crates are
reused as-is across worktrees at different paths (unless the repo's config depends on the
checkout path, see below).

### As if the changes were made in the old checkout

A fresh checkout gives every file a new mtime, which would make cargo rebuild every
workspace crate. To avoid that, cargo-pool snapshots each slot's worktree after every build
into it: size, mtime and content hash of every file, plus the mtime and entry list of every
directory (for `rerun-if-changed=<dir>`). The rustc wrapper starts a background process
that waits for cargo's build lock to be released, then takes the snapshot.

When the slot is attached to a new worktree, files and directories whose content matches
the snapshot get their recorded mtime back. Cargo then sees exactly what it saw after the
last build: only the files that actually differ count as changes. Copying exact
(content, mtime) pairs inherits cargo's own correctness, including edits made during a
build. A snapshot is only used if nothing has been compiled into the slot since it was
taken; otherwise nothing is backdated and you get the normal rebuild. Disable it with
`cargo-pool.backdate = false`.

On attach, cargo-pool also resets build units whose recorded inputs are another checkout's
git metadata, e.g. `git_version!()` tracking `.git/worktrees/<other>/HEAD`. Those would
otherwise stay fresh and keep embedding the other checkout's commit.

Measured on a 1,316-crate workspace: 150 s from scratch; a new worktree on a pooled slot
builds in under a second with no edits, or in about a second plus the edited crates.

## Install

```sh
curl -sSf https://raw.githubusercontent.com/louisponet/cargo-pool/main/install.sh | sh
```

That runs `cargo install --git https://github.com/louisponet/cargo-pool` followed by
`cargo pool install`, which sets up two things:

- **The worktree hook.** Git runs `post-checkout` on `git worktree add`, so every tool
  that creates worktrees (T3, Claude Code, plain git) gets a pooled target dir. If your git
  supports config-based hooks (2.54+), this is a single entry in `~/.gitconfig` that covers
  every repository, including ones cloned later, and runs alongside each repository's own
  `.git/hooks` (git-lfs and similar keep working):

  ```ini
  [hook "cargo-pool"]
  	event = post-checkout
  	command = "f=\"/home/you/.cargo/bin/cargo-pool\"; [ -x \"$f\" ] || exit 0; exec \"$f\" post-checkout"
  ```

  On older git it falls back to appending a guarded block to the current repository's
  `.git/hooks/post-checkout`; run `cargo pool install-hook` in each other repo too. The hook
  never fails the checkout and ignores repos without a top-level `Cargo.toml`.
- **The rustc wrapper** (`build.rustc-wrapper` in `~/.cargo/config.toml`, via a small shim
  script), which checks
  disk usage on every pooled build. Cargo runs every compiler call through `cargo-pool`.
  Builds that don't write into a pooled target dir pass straight through to the compiler.
  For pooled builds, `cargo-pool` checks disk usage before handing off; if usage is over the
  limit, a single compiler process runs the cleanup, at most once a minute. The slot being
  built is never deleted, because cargo holds its build lock. Skip it with
  `| sh -s -- --no-wrapper`.

The wrapper also caps compilers machine-wide. Each cargo invocation runs up to one compiler
per CPU, so two concurrent builds oversubscribe the CPUs twice over. Every compile first
takes one of `cargo-pool.jobs` slots (default: the CPU count) and holds it until the compiler
exits, so concurrent builds share the CPUs instead. A killed build releases its slots, and a
daemon the compiler starts, such as an sccache server, doesn't keep one. A compile nested in
another, such as a proc macro building a crate, uses its parent's slot. The cap counts
compiler processes, not the codegen threads inside them or C compilers run by build scripts.
The slots are lock files in `~/.cache/cargo-pool-jobs`. After waiting 30 seconds, a compiler
says so on the terminal, and `lslocks` shows who holds them. Set `cargo-pool.jobs` to 0 to
turn the cap off.

A `RUSTC_WRAPPER` env var overrides the config entry. To keep using sccache, set
`CARGO_POOL_RUSTC_WRAPPER=sccache` instead and `cargo-pool` runs the compiler through it.

Worktrees that already exist aren't attached by the hook; run `cargo pool attach` inside
them, including the main checkout. Stray files such as logs and rust-analyzer output are
preserved when attaching and seeding. If a name conflicts, the old slot file is kept beside
it with a `.cargo-pool-saved-<timestamp>` suffix. Stray files stay in the ignored
`target.cargo-pool-files/` directory and the slot links to them, so open log
handles keep working. These retained files stay with their original worktree when a slot
is released or seeded. A failed deferred attach keeps its marker and stops the compiler
probe, so the next build can retry. An interrupted attach retains its lease while the
worktree exists. `detach` can cancel an unfinished copy while keeping its source target.
Once the copy is complete, rerun `attach` before detaching it. Linux only for now.

Cross-filesystem adoption keeps Cargo's lock inodes in the repository's Git metadata,
so commands already waiting for a build lock still use the same lock after adoption.
The Git metadata must share the original target's filesystem. Seeds get independent locks.
After moving the whole repository, run `cargo pool attach --force` to reset its pool
association and lock paths. The previous artifacts remain in the old pool.

To remove it, run `cargo pool uninstall`, then `cargo uninstall cargo-pool`. Skipping the
first step leaves nothing broken: cargo's config points at a small script
(`~/.cargo/cargo-pool-rustc-wrapper`) that runs the compiler directly once cargo-pool is
gone, and the git hook does nothing if the binary is missing. Only the two leftover config
entries remain.

## Path-dependent cargo config

Sharing a target dir only helps if cargo's fingerprints don't depend on where the
checkout lives. A relative linker in the repo's own config breaks that:

```toml
[target.x86_64-unknown-linux-gnu]
linker = "scripts/my-linker.sh"
```

Cargo resolves the path inside each worktree and hashes the absolute linker path into every
crate's fingerprint, so every worktree recompiles everything (`Dirty …: the config settings
changed` in `cargo build -v`). `cargo-pool attach` warns about this. Build scripts that print
absolute `rerun-if-changed` paths outside their package have the same effect for everything
depending on them ("the rerun-if-changed instructions changed"); print paths relative to
the package instead. To fix it, give the
linker a path that's identical in every checkout, e.g. the main checkout's copy:

```sh
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/path/to/main/checkout/scripts/my-linker.sh
```

## Metrics

Every pool keeps an append-only log, `<pool-directory>/<repo>-<hash>/events.jsonl`, with one
JSON line per attach, slot preparation (backdating), seed, build, snapshot, prune and evicted slot,
including how long it took. `cargo pool stats` sums it up:

```
cargo-pool-ac7e5f46c861e3b5 (/home/you/cargo-pool/.git): 4 slots, 2 in use, all time
  attached   42 worktrees: 35 to a warm slot, 7 to a new one; 3.1s
  seeded     6 of 7 new slots, 45.2G copied, 80.1G left out; 3m12s
  prepared   41 slots: 98112 of 99840 files backdated, 12 units reset; 8.4s
  setup      3m24s in total (attach + seed + prepare)
  built      120 cargo commands, 3412 units compiled; 1h02m
  snapshots  118 in the background; 1m03s
  pruned     2870 units in 52 passes, 61.0G freed; 9.6s
  evicted    5 slots, 23.1G freed
```

Builds are recorded by the rustc wrapper: a build runs from its first compile into the slot
until cargo releases the build lock, and counts the compiler calls in between. Cargo commands
with nothing to compile don't show up, and without the wrapper no builds are recorded.

## Commands

| command | |
|---|---|
| `cargo pool attach [path] [--force] [--no-seed]` | attach a worktree and preserve its target (idempotent); `--force` discards it |
| `cargo pool detach [path]` | release the slot and remove the symlink, or cancel a deferred attach or unfinished copy |
| `cargo pool status [--size]` | show slots for this repo (all pools outside a repo) |
| `cargo pool stats [--days D]` | show recorded metrics for this repo (all pools outside a repo) |
| `cargo pool gc [--max-age-days D] [--dry-run]` | free disk space if over the limit; optionally delete free slots older than D days |
| `cargo pool install [--no-wrapper]` / `uninstall` | set up / remove the hook and the rustc wrapper |
| `cargo pool install-hook [path] [--repo]` / `uninstall-hook [path]` | only the hook (global if supported, else per repo) |
| `cargo pool install-wrapper` / `uninstall-wrapper` | only the rustc wrapper |

## Settings

Each setting comes from an environment variable or git config, with the env var taking precedence:

| env | git config | default |
|---|---|---|
| `CARGO_POOL_DIRS` | global `cargo-pool.directory` entries | unset |
| `CARGO_POOL_DIR` | – | `$XDG_CACHE_HOME/cargo-pool` or `~/.cache/cargo-pool` |
| `CARGO_POOL_MAX_DISK_PERCENT` | `cargo-pool.maxDiskPercent` | 90 |
| `CARGO_POOL_BACKDATE` | `cargo-pool.backdate` | true |
| `CARGO_POOL_JOBS` | global `cargo-pool.jobs` | number of CPUs (0 turns the cap off) |
| `CARGO_POOL_RUSTC_WRAPPER` | – | none (e.g. `sccache`) |

### Use multiple pool directories

Set a colon-separated list of absolute paths for a command:

```sh
CARGO_POOL_DIRS=/mnt/fast/cargo-pool:/mnt/large/cargo-pool cargo pool attach
```

To use the same directories for hooks and builds in every shell, save them in global git config:

```sh
git config --global --add cargo-pool.directory /mnt/fast/cargo-pool
git config --global --add cargo-pool.directory /mnt/large/cargo-pool
```

`CARGO_POOL_DIRS` overrides `CARGO_POOL_DIR`, which overrides the global directory list.
Without these settings, the existing default directory is used. Missing directories are
created when a repository is opened by cargo-pool. All configured paths must be writable.

Include the old pool directory in the list to keep reusing its slots. Existing slots are
not moved. `status` and `gc` cover every configured directory, and new slots can be seeded
from any of them. Cleanup affects only the filesystem that needs space.

Seeding uses reflinks where supported on the same filesystem, such as btrfs or XFS.
Copies between filesystems, or on ext4, require a full copy.
