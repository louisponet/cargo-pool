<img src="assets/icon.svg" width="64" height="64" alt="">

# cargo-pool

Share warm cargo `target/` directories between git worktrees, so a fresh worktree
(e.g. one an agent just created) doesn't build the whole dependency graph from scratch.

## How it works

- Every repository (keyed by its git common dir) gets a pool of **slots** in
  `~/.cache/cargo-pool/<repo>-<hash>/slots/<n>/target`.
- `cargo-pool attach` replaces a worktree's `target` with a symlink to a slot. Each slot
  is leased to one worktree at a time, so parallel agents never block on each other's
  cargo build lock.
- A free slot is picked by the commit it was last built at: the closest to the new
  worktree's `HEAD` wins.
- If no slot is free, a new one is created and **seeded** by copying the closest existing
  target dir (another slot or the main checkout's `target/`). It holds cargo's build lock
  during the copy, so it never copies a half-written build. There is no limit on the
  number of slots: disk usage is the only limit.
- **Disk limit**: if the disk holding the pool is above `maxDiskPercent` (default 90%),
  pooled target dirs are deleted, across all repos' pools, until usage is back under the
  limit. This happens on every attach, on every pooled build (with the rustc wrapper
  installed, see below) and on `gc`. Free slots go first, then slots of worktrees that
  haven't built anything for `idleHours`, least recently used first. An idle worktree
  whose slot is deleted loses its `target` link, so it starts over in its own directory
  if it ever builds again. Slots with a build running are never deleted, and neither is
  the main checkout's `target/`. A new slot is only seeded if the copy fits under the
  limit; otherwise it starts empty.
- Leases end on their own when the worktree is deleted (`git worktree remove`),
  or explicitly with `cargo-pool detach`.
- `/target` is added to `.git/info/exclude`, because `target/` patterns in `.gitignore`
  don't match symlinks.

Cargo hashes path dependencies relative to the workspace root, so all third-party
crates are reused as-is across worktrees at different paths. Workspace crates are
recompiled once because a fresh checkout has new file mtimes, but they reuse the slot's
incremental cache.

## Install

```sh
cargo install --git https://github.com/louisponet/cargo-pool
cargo pool install-wrapper                          # once per machine, see below
cd /path/to/rust/repo && cargo pool install-hook    # once per repo
```

Worktrees that already exist aren't attached by the hook; run `cargo pool attach`
inside them. Linux only for now.

`install-hook` appends a guarded block to the repo's `post-checkout` hook (it respects
`core.hooksPath` and keeps existing hooks such as git-lfs). Git runs `post-checkout` on
`git worktree add`, so every tool that creates worktrees picks it up automatically.
The hook never fails the checkout and ignores repos without a top-level `Cargo.toml`.

### Disk check on every build

```sh
cargo pool install-wrapper   # adds build.rustc-wrapper to ~/.cargo/config.toml
```

Cargo then runs every compiler call through `cargo-pool`. Builds that don't write into a
pooled target dir pass straight through to the compiler. For pooled builds, `cargo-pool`
checks disk usage before handing off. If usage is over the limit, a single compiler
process runs the cleanup, at most once a minute. The slot being built is never deleted,
because cargo holds its build lock. A `RUSTC_WRAPPER` env var overrides the config; to
keep using sccache, set `CARGO_POOL_RUSTC_WRAPPER=sccache` instead and `cargo-pool`
runs the compiler through it.

## Commands

| command | |
|---|---|
| `cargo pool attach [path] [--force] [--no-seed] [--allow-main]` | attach a worktree (idempotent) |
| `cargo pool detach [path]` | release the slot and remove the symlink |
| `cargo pool status [--size]` | show slots for this repo (all pools outside a repo) |
| `cargo pool gc [--max-age-days D] [--dry-run]` | free disk space if over the limit; optionally delete free slots older than D days |
| `cargo pool install-hook` / `uninstall-hook` | manage the post-checkout hook |
| `cargo pool install-wrapper` / `uninstall-wrapper` | manage the global rustc wrapper |

## Settings

Each setting comes from an environment variable or git config, with the env var taking precedence:

| env | git config | default |
|---|---|---|
| `CARGO_POOL_DIR` | – | `$XDG_CACHE_HOME/cargo-pool` or `~/.cache/cargo-pool` |
| `CARGO_POOL_IDLE_HOURS` | `cargo-pool.idleHours` | 48 |
| `CARGO_POOL_MAX_DISK_PERCENT` | `cargo-pool.maxDiskPercent` | 90 |
| `CARGO_POOL_RUSTC_WRAPPER` | – | none (e.g. `sccache`) |

Keep the pool on the same filesystem as your repos. On btrfs/XFS, seeding uses
reflinks and is instant; on ext4 it's a full copy.
