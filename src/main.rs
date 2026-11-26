//! cargo-pool: share warm cargo `target/` directories between git worktrees.
//!
//! Each cargo workspace of a repository (identified by its git common dir) gets a pool of slots
//! under the configured pool directories (default `~/.cache/cargo-pool`). A worktree attaches to a slot
//! by having its `target` replaced with a symlink into the slot. A slot is leased
//! to at most one worktree at a time, so concurrent agents never fight over the
//! same cargo build lock. Leases are released explicitly with `detach`, or
//! implicitly when the worktree disappears (e.g. `git worktree remove`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const HOOK_BEGIN: &str = "# >>> cargo-pool >>>";
const HOOK_END: &str = "# <<< cargo-pool <<<";
/// Set for a compiler holding a job slot, so compiles nested in it don't wait for one. Not
/// `CARGO_*`: sccache hashes those into its cache keys.
const IN_JOB_SLOT: &str = "CARGOPOOL_IN_JOB_SLOT";
const NULL_SHA: &str = "0000000000000000000000000000000000000000";
/// An unfinished lease whose worktree has disappeared is honoured for this long.
const PENDING_TIMEOUT_SECS: u64 = 3 * 3600;
/// Subdirectory under each pool root storing globally shared external dependency build units.
const SHARED_DIR: &str = "shared";
/// Linux `FICLONE` ioctl (`_IOW(0x94, 9, int)`) for copy-on-write reflinks on btrfs/XFS.
const FICLONE: libc::Ioctl = 0x4004_9409 as libc::Ioctl;

#[derive(Parser)]
#[command(version, about = "Share warm cargo target dirs between git worktrees")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach a worktree's `target` to a pooled target dir (idempotent).
    Attach {
        /// Worktree path (default: current directory).
        path: Option<PathBuf>,
        /// Don't seed a newly created slot by copying the closest existing target dir.
        #[arg(long)]
        no_seed: bool,
        /// Replace an existing non-empty `target` directory (its contents are deleted).
        #[arg(long)]
        force: bool,
        /// Accepted for compatibility; the main worktree is pooled by default.
        #[arg(long, hide = true)]
        allow_main: bool,
        /// Invoked from the git hook: never fail, only act on cargo workspaces.
        #[arg(long, hide = true)]
        from_hook: bool,
    },
    /// Release a worktree's slot back to the pool and remove the `target` symlink.
    Detach {
        /// Worktree path (default: current directory).
        path: Option<PathBuf>,
    },
    /// Show the pool for the current repository (or all pools outside a repo).
    Status {
        /// Compute on-disk size of every slot (slow for big targets).
        #[arg(long)]
        size: bool,
    },
    /// Summarize recorded metrics for the current repository (or all pools outside a repo).
    Stats {
        /// Only count events from the last this many days.
        #[arg(long)]
        days: Option<u64>,
    },
    /// Reclaim stale leases and free disk space if usage is above the limit.
    Gc {
        /// Also delete free slots unused for longer than this many days.
        #[arg(long)]
        max_age_days: Option<u64>,
        /// Release held leases idle for longer than this many days back to the free pool.
        #[arg(long)]
        detach_idle_days: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Set everything up: the worktree hook (for every repository if git supports
    /// config-based hooks, otherwise the current one) and the rustc wrapper.
    Install {
        /// Don't install the rustc wrapper (no disk check during builds).
        #[arg(long)]
        no_wrapper: bool,
    },
    /// Remove the hooks and rustc wrapper installed by cargo-pool (slots are kept).
    Uninstall,
    /// Install the post-checkout hook that attaches every newly created worktree.
    /// Global (all repositories) when git supports config-based hooks (2.54+),
    /// otherwise for the current repository.
    InstallHook {
        /// Install into this repository's hooks instead of globally.
        path: Option<PathBuf>,
        /// Install into the current repository's hooks instead of globally.
        #[arg(long)]
        repo: bool,
    },
    /// Remove the global hook and the current repository's hook block (or only `path`'s).
    UninstallHook {
        path: Option<PathBuf>,
    },
    /// Make cargo run every compiler call through cargo-pool (`build.rustc-wrapper` in
    /// `$CARGO_HOME/config.toml`), so disk usage is checked during every pooled build and
    /// all cargo invocations share `cargo-pool.jobs` compiler slots.
    InstallWrapper,
    /// Remove the `build.rustc-wrapper` entry added by `install-wrapper`.
    UninstallWrapper,
    /// Snapshot a slot's worktree once the running build finishes (spawned by the wrapper).
    #[command(hide = true)]
    SnapshotAfterBuild { slot: PathBuf },
    /// Hook entry point (called by the installed post-checkout hook).
    #[command(hide = true)]
    PostCheckout { prev: String, new: String, flag: String },
}

fn main() {
    // Support `cargo pool ...`, which invokes us as `cargo-pool pool ...`.
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "pool") {
        args.remove(1);
    }
    if is_rustc_wrapper(&args) {
        rustc_wrapper(args);
    }
    // Die quietly on a closed stdout (`cargo pool status | head`) instead of panicking.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse_from(args);
    let from_hook = matches!(
        cli.cmd,
        Cmd::PostCheckout { .. } | Cmd::Attach { from_hook: true, .. }
    );
    if let Err(e) = run(cli) {
        eprintln!("cargo-pool: error: {e:#}");
        // A failing hook must never break `git worktree add`.
        if !from_hook {
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Attach { path, no_seed, force, allow_main: _, from_hook } => {
            attach(
                &path_or_cwd(path)?,
                AttachOpts { seed: !no_seed, force, from_hook, wait_for_build: false, rustc_bin: None },
            )
        }
        Cmd::Detach { path } => detach(&path_or_cwd(path)?),
        Cmd::Status { size } => status(size),
        Cmd::Stats { days } => stats(days),
        Cmd::Gc { max_age_days, detach_idle_days, dry_run } => gc(max_age_days, detach_idle_days, dry_run),
        Cmd::Install { no_wrapper } => {
            install_hook_auto(None, false)?;
            if !no_wrapper {
                install_wrapper()?;
            }
            Ok(())
        }
        Cmd::Uninstall => {
            uninstall_hook_auto(None)?;
            if wrapper_installed() {
                uninstall_wrapper()?;
            }
            for root in pool_roots()? {
                println!("pooled target dirs are kept in {}; delete it to free their space", root.display());
            }
            Ok(())
        }
        Cmd::InstallHook { path, repo } => install_hook_auto(path, repo),
        Cmd::UninstallHook { path } => uninstall_hook_auto(path),
        Cmd::SnapshotAfterBuild { slot } => snapshot_after_build(&slot),
        Cmd::InstallWrapper => install_wrapper(),
        Cmd::UninstallWrapper => uninstall_wrapper(),
        Cmd::PostCheckout { prev, .. } => {
            // A null previous HEAD means a fresh checkout: `git worktree add` or clone.
            if prev != NULL_SHA {
                return Ok(());
            }
            let opts = AttachOpts { seed: true, force: false, from_hook: true, wait_for_build: false, rustc_bin: None };
            attach(&std::env::current_dir()?, opts)
        }
    }
}

fn path_or_cwd(p: Option<PathBuf>) -> Result<PathBuf> {
    let p = match p {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    fs::canonicalize(&p).with_context(|| format!("{} does not exist", p.display()))
}

// ---------------------------------------------------------------------------
// git helpers

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Hooks run with GIT_DIR & co. pointing at the new worktree; we always
        // address repositories explicitly via `-C`, so drop them.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Number of commits separating two revisions (sum of both sides of `a...b`).
fn distance(dir: &Path, a: Option<&str>, b: &str) -> u64 {
    let Some(a) = a else { return u64::MAX };
    if a == b {
        return 0;
    }
    git(dir, &["rev-list", "--count", "--left-right", &format!("{a}...{b}")])
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace().map(|n| n.parse::<u64>().ok());
            Some(it.next()?? + it.next()??)
        })
        .unwrap_or(u64::MAX)
}

/// A cargo workspace of a git worktree, with the pool its `target` dir is leased from.
#[derive(Clone)]
struct Repo {
    /// Top level of the worktree.
    worktree: PathBuf,
    /// Directory of the workspace's root `Cargo.toml`, where its `target` lives: the worktree's
    /// top level, or a directory one level down (see `workspaces`).
    workspace: PathBuf,
    common_dir: PathBuf,
    /// The same workspace in the main worktree, if the repository isn't bare.
    main_workspace: Option<PathBuf>,
    /// Pool directory for this workspace.
    pool: PathBuf,
}

impl Repo {
    /// The workspaces of the worktree holding `path` that contain `path` or lie below it.
    fn discover(path: &Path) -> Result<Vec<Repo>> {
        let worktree = PathBuf::from(git(path, &["rev-parse", "--show-toplevel"])?);
        let common = git(&worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common_dir = fs::canonicalize(&common)?;
        let main_worktree = (common_dir.file_name().is_some_and(|n| n == ".git"))
            .then(|| common_dir.parent().unwrap().to_path_buf());
        let name = main_worktree
            .as_deref()
            .unwrap_or(&common_dir)
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
            .unwrap_or_else(|| "repo".into());
        let roots = pool_roots()?;
        let path = fs::canonicalize(path)?;
        workspaces(&worktree)
            .into_iter()
            .filter(|ws| path.starts_with(ws) || ws.starts_with(&path))
            .map(|workspace| {
                let rel = workspace.strip_prefix(&worktree)?;
                // Workspaces of one repository build different crates: each gets its own pool.
                let key = if rel.as_os_str().is_empty() {
                    format!("{name}-{:016x}", fnv1a(common_dir.as_os_str().as_encoded_bytes()))
                } else {
                    let id = common_dir.join(rel);
                    format!("{name}-{}-{:016x}", rel.display(), fnv1a(id.as_os_str().as_encoded_bytes()))
                };
                let pool = select_pool(&roots, &key, &workspace, &common_dir)?;
                let main_workspace = main_worktree.as_ref().map(|m| m.join(rel));
                Ok(Repo { worktree: worktree.clone(), workspace, common_dir: common_dir.clone(), main_workspace, pool })
            })
            .collect()
    }

    /// The workspace at `workspace`.
    fn of_workspace(workspace: &Path) -> Result<Repo> {
        Repo::discover(workspace)?
            .into_iter()
            .find(|r| r.workspace == workspace)
            .ok_or_else(|| anyhow!("{} is not a cargo workspace", workspace.display()))
    }

    fn head(&self) -> Result<String> {
        git(&self.workspace, &["rev-parse", "HEAD"])
    }

    fn max_disk_percent(&self) -> f64 {
        setting(&self.common_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0)
    }

    fn idle_lease_days(&self) -> Option<u64> {
        idle_lease_days(&self.common_dir)
    }

    fn slots_dir(&self) -> PathBuf {
        self.pool.join("slots")
    }

    fn lock(&self) -> Result<FileLock> {
        fs::create_dir_all(self.slots_dir())?;
        fs::write(self.pool.join("repo"), self.common_dir.as_os_str().as_encoded_bytes())?;
        FileLock::acquire(&self.pool.join(".lock"), true)?
            .ok_or_else(|| anyhow!("could not lock pool"))
    }
}

/// The cargo workspaces of a worktree: its top level if that holds a `Cargo.toml`, else every
/// directory one level down that holds one (a monorepo of projects). Without a manifest above
/// it, cargo makes each of those the root of its own workspace, with its own `target`.
fn workspaces(worktree: &Path) -> Vec<PathBuf> {
    if worktree.join("Cargo.toml").is_file() {
        return vec![worktree.to_path_buf()];
    }
    let mut out: Vec<PathBuf> = fs::read_dir(worktree)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    out.sort();
    out
}

/// Env var, else git config of the repository at `git_dir` (any dir inside it), else default.
fn setting<T: std::str::FromStr>(git_dir: &Path, env: &str, git_key: &str, default: T) -> T {
    std::env::var(env)
        .ok()
        .or_else(|| git(git_dir, &["config", "--get", git_key]).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Idle lease timeout in days (`CARGO_POOL_IDLE_LEASE_DAYS` / `cargo-pool.idleLeaseDays`).
/// `None` when unset or `0` (disabled).
fn idle_lease_days(git_dir: &Path) -> Option<u64> {
    let raw = std::env::var("CARGO_POOL_IDLE_LEASE_DAYS")
        .ok()
        .or_else(|| git(git_dir, &["config", "--get", "cargo-pool.idleLeaseDays"]).ok())?;
    match raw.trim().parse::<u64>().ok()? {
        0 => None,
        days => Some(days),
    }
}

/// Repository git dir recorded for a pool, or `/` (reads only global git config) if absent/gone.
fn pool_git_dir(pool: &Path) -> PathBuf {
    fs::read_to_string(pool.join("repo"))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn pool_roots() -> Result<Vec<PathBuf>> {
    let configured = if let Some(dirs) = std::env::var_os("CARGO_POOL_DIRS") {
        std::env::split_paths(&dirs).collect::<Vec<_>>()
    } else if let Some(dir) = std::env::var_os("CARGO_POOL_DIR") {
        vec![PathBuf::from(dir)]
    } else {
        let output = Command::new("git")
            .args(["config", "--global", "--includes", "--path", "--get-all", "cargo-pool.directory"])
            .output()?;
        String::from_utf8(output.stdout)?.lines().map(PathBuf::from).collect()
    };
    let configured = if configured.is_empty() {
        let root = if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(d).join("cargo-pool")
        } else {
            let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
            PathBuf::from(home).join(".cache/cargo-pool")
        };
        vec![root]
    } else {
        configured
    };
    let mut roots = Vec::new();
    for root in configured {
        if root.as_os_str().is_empty() || !root.is_absolute() {
            bail!("pool directories must be nonempty absolute paths: {}", root.display());
        }
        let root = fs::canonicalize(&root).unwrap_or(root);
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn select_pool(roots: &[PathBuf], key: &str, worktree: &Path, common_dir: &Path) -> Result<PathBuf> {
    let attached = fs::read_link(worktree.join("target")).ok();
    let head = git(worktree, &["rev-parse", "HEAD"]).ok();
    let max_disk = setting(common_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    let idle_cutoff = idle_lease_days(common_dir).map(|d| now().saturating_sub(d.saturating_mul(86400)));
    let mut warm = Vec::new();
    let mut idle_warm = Vec::new();
    let mut capacity = Vec::new();
    for root in roots {
        fs::create_dir_all(root).with_context(|| format!("creating pool directory {}", root.display()))?;
    }
    for root in roots {
        let pool = fs::canonicalize(root)?.join(key);
        if attached.as_ref().is_some_and(|target| target.starts_with(pool.join("slots"))) {
            return Ok(pool);
        }
        if load_slots(&pool.join("slots"))?
            .iter()
            .any(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == worktree))
        {
            return Ok(pool);
        }
        capacity.push((available_bytes(root)?, pool.clone()));
        if disk_percent(root, 0)? < max_disk {
            for slot in load_slots(&pool.join("slots"))? {
                if slot.state() != State::Held {
                    warm.push((
                        distance(worktree, slot.meta.last_head.as_deref(), head.as_deref().unwrap_or("")),
                        u64::MAX - slot.meta.last_used,
                        pool.clone(),
                    ));
                } else if let Some(cutoff) = idle_cutoff
                    && !slot.meta.seed_pending
                    && is_idle_held(&slot, cutoff)
                {
                    let slot_head = slot.meta.lease.as_ref().map(|l| l.head.as_str()).or(slot.meta.last_head.as_deref());
                    idle_warm.push((
                        slot.last_active(),
                        distance(worktree, slot_head, head.as_deref().unwrap_or("")),
                        pool.clone(),
                    ));
                }
            }
        }
    }
    if let Some((_, _, pool)) = warm.into_iter().min() {
        return Ok(pool);
    }
    if let Some((_, _, pool)) = idle_warm.into_iter().min() {
        return Ok(pool);
    }
    Ok(capacity.into_iter().max_by_key(|(bytes, _)| *bytes).unwrap().1)
}

fn available_bytes(path: &Path) -> Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

fn containing_pool_root(roots: &[PathBuf], path: &Path) -> Result<Option<PathBuf>> {
    let path = fs::canonicalize(path)?;
    Ok(roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count()))
}

/// The first of `roots` on each filesystem: `free_disk` on it covers every pool on that filesystem.
fn pool_filesystems(roots: &[PathBuf]) -> Vec<(u64, &Path)> {
    let mut devices = HashSet::new();
    roots
        .iter()
        .filter_map(|root| Some((fs::metadata(root).ok()?.dev(), root.as_path())))
        .filter(|(dev, _)| devices.insert(*dev))
        .collect()
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;

/// Stable hash for directory names and file contents (std's hasher isn't guaranteed stable).
fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_update(FNV_OFFSET, bytes)
}

fn fnv1a_update(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// locking

/// Released when dropped (closing the fd drops the flock).
struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    /// `blocking == false` returns `Ok(None)` if the lock is held elsewhere.
    fn acquire(path: &Path, blocking: bool) -> Result<Option<FileLock>> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        let op = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("flock {}", path.display()));
        }
        Ok(Some(FileLock(f)))
    }
}

/// Cargo's build-directory locks (`target/<profile>/.cargo-lock`) inside a target dir.
fn cargo_locks(target: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name() == ".cargo-lock" {
                out.push(p);
            } else if depth < 2 && e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((p, depth + 1));
            }
        }
    }
    out
}

/// Try to take every cargo lock in `target` so no build runs while we copy it.
fn lock_target(target: &Path, blocking: bool) -> Option<Vec<FileLock>> {
    cargo_locks(target)
        .iter()
        .filter(|p| p.exists() || !fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink()))
        .map(|p| FileLock::acquire(p, blocking).ok().flatten())
        .collect()
}

/// Last time cargo produced output in this target dir.
fn last_activity(target: &Path) -> u64 {
    last_activity_ns(target) / 1_000_000_000
}

/// Newest mtime of cargo's output dirs (not of `target` itself: moving it changes that).
fn last_activity_ns(target: &Path) -> u64 {
    let mut newest = 0;
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            if name == "deps" || name == ".fingerprint" || name == "incremental" || name == "build" {
                newest = newest.max(mtime_ns(&e.path()));
            } else if depth < 2 {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    newest
}

fn mtime_ns(p: &Path) -> u64 {
    fs::metadata(p).map_or(0, |m| modified_ns(&m))
}

fn modified_ns(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}

// ---------------------------------------------------------------------------
// slots

#[derive(Serialize, Deserialize, Clone)]
struct Lease {
    /// The workspace dir holding the `target` link (see `Repo::workspace`).
    worktree: PathBuf,
    head: String,
    acquired_at: u64,
    /// Attach has not finished; source cleanup may still be pending after linking.
    #[serde(default)]
    pending: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Adoption {
    Copying,
    Ready { source: u64 },
}

#[derive(Serialize, Deserialize, Default)]
struct SlotMeta {
    lease: Option<Lease>,
    #[serde(default)]
    adoption: Option<Adoption>,
    /// New slot that will be seeded on the first cargo command in its worktree.
    #[serde(default)]
    seed_pending: bool,
    /// Commit the target dir was last built for (best effort).
    last_head: Option<String>,
    last_used: u64,
}

struct Slot {
    id: u32,
    dir: PathBuf,
    meta: SlotMeta,
}

#[derive(PartialEq, Debug)]
enum State {
    Free,
    Held,
    Stale,
}

impl Slot {
    fn target(&self) -> PathBuf {
        self.dir.join("target")
    }

    /// When the slot was last used: released, or compiled into.
    fn last_used(&self) -> u64 {
        self.meta.last_used.max(last_compile_ns(&self.dir) / 1_000_000_000)
    }

    /// When a slot's current lease (if held) or previous lease was last active.
    fn last_active(&self) -> u64 {
        let used = self.last_used();
        match &self.meta.lease {
            Some(l) => used.max(l.acquired_at),
            None => used,
        }
    }

    fn saved_files(&self, worktree: &Path) -> PathBuf {
        worktree.join("target.cargo-pool-files").join(format!("{:016x}", fnv1a(self.dir.as_os_str().as_encoded_bytes())))
    }

    fn discard_partial_copy(&self) -> Result<()> {
        let tmp = self.dir.join("target.adopting");
        if self.meta.adoption == Some(Adoption::Copying) && tmp.exists() {
            fs::remove_dir_all(tmp)?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let tmp = self.dir.join("slot.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.meta)?)?;
        fs::rename(tmp, self.dir.join("slot.json"))?;
        Ok(())
    }

    fn state(&self) -> State {
        let Some(l) = &self.meta.lease else { return State::Free };
        if l.pending && (l.worktree.join("Cargo.toml").exists() || now().saturating_sub(l.acquired_at) < PENDING_TIMEOUT_SECS) {
            return State::Held;
        }
        if fs::read_link(l.worktree.join("target")).is_ok_and(|t| t == self.target()) {
            State::Held
        } else {
            State::Stale
        }
    }

    /// Turn a lease into a free slot, remembering what it was built for.
    fn release(&mut self, head: Option<String>) -> Result<()> {
        self.release_at(head, now())
    }

    fn release_at(&mut self, head: Option<String>, min_last_used: u64) -> Result<()> {
        self.discard_partial_copy()?;
        if matches!(self.meta.adoption, Some(Adoption::Ready { .. })) {
            merge_target(&self.dir.join("target.adopting"), &self.target(), Conflict::Source)?;
        }
        remove_saved_links(&self.target())?;
        for lock in cargo_locks(&self.target()) {
            if !lock.exists() && fs::symlink_metadata(&lock).is_ok_and(|m| m.is_symlink()) {
                fs::remove_file(lock)?;
            }
        }
        if let Some(l) = self.meta.lease.take() {
            self.meta.adoption = None;
            self.meta.seed_pending = false;
            self.meta.last_head = head.or(Some(l.head));
            self.meta.last_used = min_last_used.max(last_activity(&self.target()));
        }
        Ok(())
    }
}

fn load_slots(slots_dir: &Path) -> Result<Vec<Slot>> {
    let mut slots = vec![];
    let Ok(rd) = fs::read_dir(slots_dir) else { return Ok(slots) };
    for e in rd.flatten() {
        let Some(id) = e.file_name().to_str().and_then(|s| s.parse().ok()) else { continue };
        let dir = e.path();
        let meta = fs::read(dir.join("slot.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        slots.push(Slot { id, dir, meta });
    }
    slots.sort_by_key(|s| s.id);
    Ok(slots)
}

/// Free every lease whose worktree is gone or no longer links to its slot.
fn reclaim_stale(slots: &mut [Slot]) -> Result<()> {
    for s in slots.iter_mut().filter(|s| s.state() == State::Stale) {
        let Some(_seed_lock) = FileLock::acquire(&s.dir.join("seed.lock"), false)? else { continue };
        s.release(None)?;
        s.save()?;
    }
    Ok(())
}

/// Whether `s` is a held slot (not pending attach or adoption) last active before `cutoff`.
fn is_idle_held(s: &Slot, cutoff: u64) -> bool {
    let Some(l) = &s.meta.lease else { return false };
    !l.pending && s.meta.adoption.is_none() && s.state() == State::Held && s.last_active() < cutoff
}

/// Replace `worktree/target` (if it still links to `slot_target`) with an empty directory and
/// write its deferred `attach_marker` so the worktree transparently re-attaches on its next
/// cargo command.
fn rearm_deferred_attach(worktree: &Path, slot_target: &Path) -> Result<bool> {
    let link = worktree.join("target");
    if fs::read_link(&link).is_ok_and(|t| t == slot_target) {
        if let Some(marker) = attach_marker(worktree) {
            fs::write(marker, "")?;
        }
        fs::remove_file(&link)?;
        fs::create_dir_all(&link)?;
        return Ok(true);
    }
    Ok(false)
}

/// Release an idle held slot back to the free pool without deleting its artifacts, restoring the
/// worktree's deferred attach marker so its next cargo command re-attaches transparently.
/// Skips slots currently attaching, seeding, snapshotting, or building. Caller holds pool `.lock`.
fn release_idle_slot(s: &mut Slot, cutoff: u64) -> Result<Option<PathBuf>> {
    if !is_idle_held(s, cutoff) {
        return Ok(None);
    }
    let worktree = s.meta.lease.as_ref().unwrap().worktree.clone();
    let Some(marker) = attach_marker(&worktree) else {
        return Ok(None);
    };
    let Some(_attach_lock) = FileLock::acquire(&marker.with_added_extension("lock"), false).ok().flatten() else {
        return Ok(None);
    };
    let Some(_seed_lock) = FileLock::acquire(&s.dir.join("seed.lock"), false)? else {
        return Ok(None);
    };
    let Some(_snap_lock) = FileLock::acquire(&s.dir.join("snapshot.lock"), false)? else {
        return Ok(None);
    };
    let Some(_build_locks) = lock_target(&s.target(), false) else {
        return Ok(None);
    };
    if !is_idle_held(s, cutoff) {
        return Ok(None);
    }
    let last_active = s.last_active();
    if !rearm_deferred_attach(&worktree, &s.target())? {
        return Ok(None);
    }
    let head = git(&worktree, &["rev-parse", "HEAD"]).ok();
    s.release_at(head, last_active)?;
    s.save()?;
    Ok(Some(worktree))
}

// ---------------------------------------------------------------------------
// disk pressure

/// Bytes used and bytes available to us on the filesystem holding `path`, as `df` counts them.
fn disk_usage(path: &Path) -> Result<(i64, i64)> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    let bs = st.f_frsize as i64;
    Ok(((st.f_blocks - st.f_bfree) as i64 * bs, st.f_bavail as i64 * bs))
}

/// Disk usage (in percent, as `df` reports it) of the filesystem holding `path`,
/// pretending `extra` more bytes had already been written (or freed, if negative).
fn disk_percent(path: &Path, extra: i64) -> Result<f64> {
    let (used, avail) = disk_usage(path)?;
    Ok(100.0 * (used + extra) as f64 / (used + avail).max(1) as f64)
}

/// Bytes allocated on disk by everything under `roots` (symlinks not followed, hard links once).
fn dir_size(roots: impl IntoIterator<Item = impl AsRef<Path>>, excluded: &[PathBuf]) -> u64 {
    let mut total = 0;
    let mut linked = HashSet::new();
    let mut stack: Vec<PathBuf> = roots.into_iter().map(|r| r.as_ref().to_path_buf()).collect();
    while let Some(p) = stack.pop() {
        if excluded.contains(&p) {
            continue;
        }
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if !m.is_dir() && m.nlink() > 1 && !linked.insert((m.dev(), m.ino())) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            stack.extend(fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
        }
    }
    total
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / (1u64 << 30) as f64;
    if gb >= 1.0 { format!("{gb:.1}G") } else { format!("{:.0}M", bytes as f64 / (1u64 << 20) as f64) }
}

/// Probe whether `src_dir` and `dst_dir` support copy-on-write reflinks (`FICLONE`).
fn supports_reflink(src_dir: &Path, dst_dir: &Path) -> bool {
    let (Ok(sm), Ok(dm)) = (fs::metadata(src_dir), fs::metadata(dst_dir)) else { return false };
    if sm.dev() != dm.dev() {
        return false;
    }
    let tag = format!(".cargo-pool-reflink-{}-{}", std::process::id(), now_ns());
    let src = src_dir.join(format!("{tag}.src"));
    let dst = dst_dir.join(format!("{tag}.dst"));
    let ok = (|| -> std::io::Result<bool> {
        fs::write(&src, [0u8; 4096])?;
        let sf = File::open(&src)?;
        let df = File::create(&dst)?;
        Ok(unsafe { libc::ioctl(df.as_raw_fd(), FICLONE, sf.as_raw_fd()) } == 0)
    })()
    .unwrap_or(false);
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    ok
}

/// Copy one file or symlink from `src` to `dst`, using `FICLONE` reflink when supported and
/// preserving permissions, mtime, and intra-copy hard links tracked in `linked`.
fn copy_file_cow(src: &Path, dst: &Path, linked: &mut HashMap<(u64, u64), PathBuf>) -> Result<()> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if meta.is_symlink() {
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        symlink(&target, dst)?;
        return Ok(());
    }
    if meta.nlink() > 1 {
        let key = (meta.dev(), meta.ino());
        if let Some(existing) = linked.get(&key) {
            let _ = fs::remove_file(dst);
            if fs::hard_link(existing, dst).is_ok() {
                return Ok(());
            }
        } else {
            linked.insert(key, dst.to_path_buf());
        }
    }
    let mut sf = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let _ = fs::remove_file(dst);
    let mut df = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let cloned = unsafe { libc::ioctl(df.as_raw_fd(), FICLONE, sf.as_raw_fd()) } == 0;
    if !cloned {
        std::io::copy(&mut sf, &mut df).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    }
    let _ = fs::set_permissions(dst, meta.permissions());
    if let Ok(mtime) = meta.modified() {
        let _ = df.set_modified(mtime);
    }
    Ok(())
}

/// Recursively copy `src` into `dst` (creating `dst` if needed) with `copy_file_cow`.
/// Preserves directory mtimes after populating them.
fn copy_tree_cow(src: &Path, dst: &Path, linked: &mut HashMap<(u64, u64), PathBuf>) -> Result<()> {
    let meta = fs::symlink_metadata(src).with_context(|| format!("stat {}", src.display()))?;
    if !meta.is_dir() {
        return copy_file_cow(src, dst, linked);
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        copy_tree_cow(&entry.path(), &dst.join(entry.file_name()), linked)?;
    }
    if let Ok(mtime) = meta.modified()
        && let Ok(d) = File::open(dst)
    {
        let _ = d.set_modified(mtime);
    }
    Ok(())
}

/// Which slots `free_disk` may delete. Any slot's incremental caches may go first.
#[derive(PartialEq, Clone, Copy)]
enum Evict {
    Any,
    /// Only slots no worktree holds: a warm slot is worth more than a copy of one.
    Unheld,
}

/// A slot `free_disk` may take space from: its incremental caches, and the whole slot if `deletable`.
struct Reclaim {
    pool: PathBuf,
    slot: Slot,
    /// Active lease (held and not past `idleLeaseDays`).
    held: bool,
    /// If `Some(cutoff)`, this slot was held but past `idleLeaseDays` at scan time and is treated
    /// at unheld priority once verified still idle under lock.
    idle_cutoff: Option<u64>,
    deletable: bool,
    /// Older unheld (or expired idle-held) slot in a pool with more than one such slot
    /// (keep the newest warm reserve).
    excess_free: bool,
}

/// The `incremental/` caches of a target dir's profiles: they only speed up rebuilding a crate
/// after an edit, and rustc starts one over if it's gone. Not through a symlink: what it points
/// to isn't the slot's.
fn incremental_caches(target: &Path) -> Vec<PathBuf> {
    profile_dirs(target)
        .into_iter()
        .map(|p| p.join("incremental"))
        .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect()
}

/// The most recently modified non-empty `incremental/` cache in `target`: the profile this
/// worktree is actively compiling, whether `debug`, `release-with-debug`, or another profile.
fn active_incremental_cache(target: &Path) -> Option<PathBuf> {
    incremental_caches(target)
        .into_iter()
        .filter(|p| fs::read_dir(p).is_ok_and(|mut d| d.next().is_some()))
        .max_by_key(|p| mtime_ns(p))
}

/// What's in the matching `incremental/` caches of a target dir, one entry per crate.
fn incremental_entries_matching(target: &Path, pred: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    incremental_caches(target)
        .into_iter()
        .filter(|c| pred(c))
        .flat_map(fs::read_dir)
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect()
}

/// Cached external units in `<root>/shared/units/` on `root`'s filesystem, least recently used first.
fn shared_reclaim_candidates(root: &Path, protect: &[&Path]) -> Result<Vec<PathBuf>> {
    let mut units = vec![];
    let device = fs::metadata(root)?.dev();
    for directory in pool_roots()? {
        if !fs::metadata(&directory).is_ok_and(|m| m.dev() == device) {
            continue;
        }
        let shared_units = directory.join(SHARED_DIR).join("units");
        for kind in ["reg", "git"] {
            for origin_dir in fs::read_dir(shared_units.join(kind)).into_iter().flatten().flatten() {
                if !origin_dir.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                for unit in fs::read_dir(origin_dir.path()).into_iter().flatten().flatten() {
                    let path = unit.path();
                    if !unit.file_type().is_ok_and(|t| t.is_dir()) || protect.contains(&path.as_path()) {
                        continue;
                    }
                    units.push((mtime_ns(&path), path));
                }
            }
        }
    }
    units.sort_by_key(|(used, _)| *used);
    Ok(units.into_iter().map(|(_, p)| p).collect())
}

/// Remove a shared unit directory and prune its parent origin directory if it becomes empty.
fn remove_shared_unit(unit_dir: &Path) -> Result<()> {
    if unit_dir.exists() {
        fs::remove_dir_all(unit_dir).with_context(|| format!("removing {}", unit_dir.display()))?;
    }
    if let Some(origin_dir) = unit_dir.parent() {
        let _ = fs::remove_dir(origin_dir);
    }
    Ok(())
}

/// Slots on `root`'s filesystem, across configured directories, that `free_disk` may take space
/// from, least recently used first. Discards abandoned partial copies on the way.
fn reclaim_candidates(root: &Path, protect: &[&Path], evict: Evict) -> Result<Vec<Reclaim>> {
    // (last used, candidate)
    let mut candidates = vec![];
    let device = fs::metadata(root)?.dev();
    for directory in pool_roots()? {
        if !fs::metadata(&directory).is_ok_and(|m| m.dev() == device) {
            continue;
        }
        for pool in fs::read_dir(directory)?.flatten().map(|e| e.path()) {
            if pool.file_name().is_some_and(|n| n == SHARED_DIR) || !pool.join("slots").is_dir() {
                continue;
            }
            let pool_idle_cutoff = idle_lease_days(&pool_git_dir(&pool)).map(|d| now().saturating_sub(d.saturating_mul(86400)));
            for s in load_slots(&pool.join("slots"))? {
                if protect.contains(&s.dir.as_path()) {
                    continue;
                }
                if s.meta.adoption == Some(Adoption::Copying) && s.dir.join("target.adopting").exists()
                    && let Some(_pool_lock) = FileLock::acquire(&pool.join(".lock"), false)?
                    && let Some(_seed_lock) = FileLock::acquire(&s.dir.join("seed.lock"), false)?
                    && let Some(current) = load_slots(&pool.join("slots"))?.into_iter().find(|slot| slot.id == s.id)
                {
                    current.discard_partial_copy()?;
                }
                let used = s.last_used();
                let (used, held, idle_cutoff, deletable) = match (s.state(), &s.meta.lease) {
                    (State::Held, Some(l)) if l.pending => continue,
                    (State::Held, Some(l)) => {
                        let active = used.max(l.acquired_at);
                        if pool_idle_cutoff.is_some_and(|c| active < c) && s.meta.adoption.is_none() {
                            (active, false, pool_idle_cutoff, true)
                        } else {
                            (active, true, None, evict == Evict::Any)
                        }
                    }
                    _ => (used, false, None, true),
                };
                candidates.push((
                    used,
                    Reclaim { pool: pool.clone(), slot: s, held, idle_cutoff, deletable, excess_free: false },
                ));
            }
        }
    }
    candidates.sort_by_key(|(used, _)| *used);
    let mut out: Vec<Reclaim> = candidates.into_iter().map(|(_, c)| c).collect();
    // Keep the newest free slot per pool as the warm reserve; older free slots are excess and go
    // before active held slots' `debug/incremental` caches.
    let mut newest_free: HashMap<PathBuf, usize> = HashMap::new();
    for (i, c) in out.iter().enumerate() {
        if !c.held && c.deletable {
            newest_free.insert(c.pool.clone(), i);
        }
    }
    for (i, c) in out.iter_mut().enumerate() {
        if !c.held && c.deletable && newest_free.get(&c.pool) != Some(&i) {
            c.excess_free = true;
        }
    }
    Ok(out)
}

/// Lock a candidate against attaches, seeds and builds, and reload it: things may have changed
/// since the scan. None if one of those holds it, or it's gone or attaching.
fn lock_candidate(c: &Reclaim) -> Result<Option<(Slot, Vec<FileLock>)>> {
    let Some(pool_lock) = FileLock::acquire(&c.pool.join(".lock"), false)? else { return Ok(None) };
    let Some(s) = load_slots(&c.pool.join("slots"))?.into_iter().find(|s| s.id == c.slot.id) else { return Ok(None) };
    if s.state() == State::Held && s.meta.lease.as_ref().is_some_and(|l| l.pending) {
        return Ok(None);
    }
    let Some(seed_lock) = FileLock::acquire(&s.dir.join("seed.lock"), false).ok().flatten() else { return Ok(None) };
    let Some(mut locks) = lock_target(&s.target(), false) else { return Ok(None) };
    locks.extend([seed_lock, pool_lock]);
    Ok(Some((s, locks)))
}

/// Bytes `free_disk` could take from `candidates` and `shared` right now, in its exact order:
/// 1. unheld slots' incremental caches
/// 2. held slots' inactive (secondary) incremental caches (keeping each held slot's newest cache)
/// 3. excess free slots (keeping the newest free slot per pool)
/// 4. held slots' active incremental caches
/// 5. LRU shared dependency units
/// 6. remaining deletable slots (last free slot per pool, then held slots if `Evict::Any`)
fn reclaimable(
    candidates: &[Reclaim],
    shared: &[PathBuf],
    mut enough: impl FnMut(u64) -> Result<bool>,
) -> Result<u64> {
    let mut freed = 0;
    let mut trimmed = vec![0u64; candidates.len()];
    let mut deleted = vec![false; candidates.len()];
    let active_caches: Vec<Option<PathBuf>> = candidates
        .iter()
        .map(|c| if c.held { active_incremental_cache(&c.slot.target()) } else { None })
        .collect();
    // The probe lets its locks go before the walk, which would otherwise stall a build starting
    // in the slot.
    let probe_trim = |c: &Reclaim, pred: &dyn Fn(&Path) -> bool| -> Result<u64> {
        if lock_candidate(c)?.is_some() {
            Ok(dir_size(incremental_entries_matching(&c.slot.target(), pred), &[]))
        } else {
            Ok(0)
        }
    };
    for i in 0..candidates.len() {
        if enough(freed)? {
            return Ok(freed);
        }
        if !candidates[i].held {
            let sz = probe_trim(&candidates[i], &|_| true)?;
            trimmed[i] += sz;
            freed += sz;
        }
    }
    for i in 0..candidates.len() {
        if enough(freed)? {
            return Ok(freed);
        }
        if candidates[i].held {
            let active = active_caches[i].as_deref();
            let sz = probe_trim(&candidates[i], &|p| active != Some(p))?;
            trimmed[i] += sz;
            freed += sz;
        }
    }
    for i in 0..candidates.len() {
        if enough(freed)? {
            return Ok(freed);
        }
        if candidates[i].excess_free && lock_candidate(&candidates[i])?.is_some() {
            freed += dir_size([&candidates[i].slot.dir], &[]).saturating_sub(trimmed[i]);
            deleted[i] = true;
        }
    }
    for i in 0..candidates.len() {
        if enough(freed)? {
            return Ok(freed);
        }
        if candidates[i].held {
            let active = active_caches[i].as_deref();
            let sz = probe_trim(&candidates[i], &|p| active == Some(p))?;
            trimmed[i] += sz;
            freed += sz;
        }
    }
    for unit in shared {
        if enough(freed)? {
            return Ok(freed);
        }
        freed += dir_size([unit], &[]);
    }
    for i in 0..candidates.len() {
        if enough(freed)? {
            return Ok(freed);
        }
        if candidates[i].deletable && !deleted[i] && lock_candidate(&candidates[i])?.is_some() {
            freed += dir_size([&candidates[i].slot.dir], &[]).saturating_sub(trimmed[i]);
        }
    }
    Ok(freed)
}

fn trim_slot_caches(
    c: &Reclaim,
    pred: impl Fn(&Path) -> bool,
    max_percent: f64,
    quiet: bool,
) -> Result<bool> {
    let Some((s, _locks)) = lock_candidate(c)? else { return Ok(false) };
    if c.idle_cutoff.is_some_and(|cutoff| s.state() == State::Held && !is_idle_held(&s, cutoff)) {
        return Ok(false);
    }
    let caches: Vec<PathBuf> = incremental_caches(&s.target()).into_iter().filter(|p| pred(p)).collect();
    let entries: Vec<PathBuf> = caches.iter().flat_map(fs::read_dir).flatten().flatten().map(|e| e.path()).collect();
    let size = dir_size(&entries, &[]);
    if size == 0 {
        return Ok(false);
    }
    // Empty each cache but keep it with its mtime: it dates the slot's last compile, and
    // cargo would recreate a missing one on its next command, compiling or not.
    for cache in caches {
        let mtime = fs::metadata(&cache)?.modified()?;
        for e in fs::read_dir(&cache)?.flatten() {
            if e.file_type()?.is_dir() {
                fs::remove_dir_all(e.path())?;
            } else {
                fs::remove_file(e.path())?;
            }
        }
        File::open(&cache)?.set_modified(mtime)?;
    }
    record(&s.dir, Event::Trim { bytes: size });
    if !quiet {
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted the incremental caches of slot {} of {} ({})",
            s.id,
            c.pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
    }
    Ok(true)
}

fn evict_slot(c: &Reclaim, evict: Evict, max_percent: f64, quiet: bool) -> Result<bool> {
    let Some((s, _locks)) = lock_candidate(c)? else { return Ok(false) };
    if s.state() == State::Held {
        let still_idle = c.idle_cutoff.is_some_and(|cutoff| is_idle_held(&s, cutoff));
        if c.idle_cutoff.is_some() && !still_idle {
            return Ok(false);
        }
        if !still_idle && (evict == Evict::Unheld || c.excess_free) {
            return Ok(false);
        }
    }
    let mut from = String::new();
    let mut _attach_lock = None;
    if let (State::Held, Some(l)) = (s.state(), &s.meta.lease) {
        if let Some(marker) = attach_marker(&l.worktree) {
            let Some(lock) = FileLock::acquire(&marker.with_added_extension("lock"), false).ok().flatten() else {
                return Ok(false);
            };
            _attach_lock = Some(lock);
        }
        rearm_deferred_attach(&l.worktree, &s.target())?;
        from = format!(", taken from worktree {}", l.worktree.display());
    }
    let size = dir_size([&s.dir], &[]);
    fs::remove_dir_all(&s.dir).with_context(|| format!("removing {}", s.dir.display()))?;
    record(&s.dir, Event::Evict { bytes: size });
    if !quiet {
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted slot {} of {} ({}{from})",
            s.id,
            c.pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
    }
    Ok(true)
}

/// Free space on `root`'s filesystem, across configured directories, until disk usage (plus
/// `extra` bytes about to be written) is below `max_percent`.
///
/// Order: unheld incremental caches, held inactive (secondary) incremental caches, excess free
/// slots (keeping one newest free slot per pool), held active incremental caches, LRU globally
/// shared dependency units, then remaining slots (`evict` says whether held ones may go too).
/// Slots in `protect`, slots being attached, seeded or built and pools locked by another
/// cargo-pool process are never touched. Must be called without holding any pool lock.
///
/// With `extra` bytes to make room for, nothing is deleted unless they fit once everything
/// eligible is gone: a copy that can't fit anyway isn't worth a single warm slot.
fn free_disk(root: &Path, max_percent: f64, extra: u64, protect: &[&Path], evict: Evict) -> Result<bool> {
    free_disk_impl(root, max_percent, extra, protect, evict, false)
}

fn free_disk_impl(
    root: &Path,
    max_percent: f64,
    extra: u64,
    protect: &[&Path],
    evict: Evict,
    quiet: bool,
) -> Result<bool> {
    fs::create_dir_all(root)?;
    let fits = |freed: u64| disk_percent(root, extra as i64 - freed as i64).map(|p| p < max_percent);
    if fits(0)? {
        return Ok(true);
    }
    let candidates = reclaim_candidates(root, protect, evict)?;
    let shared = shared_reclaim_candidates(root, protect)?;
    if fits(0)? {
        return Ok(true);
    }
    if extra > 0 && !fits(reclaimable(&candidates, &shared, &fits)?)? {
        return Ok(false);
    }

    for c in candidates.iter().filter(|c| !c.held) {
        if trim_slot_caches(c, |_| true, max_percent, quiet)? && fits(0)? {
            return Ok(true);
        }
    }
    for c in candidates.iter().filter(|c| c.held) {
        let active = active_incremental_cache(&c.slot.target());
        if trim_slot_caches(c, |p| active.as_deref() != Some(p), max_percent, quiet)? && fits(0)? {
            return Ok(true);
        }
    }
    for c in candidates.iter().filter(|c| c.excess_free) {
        if evict_slot(c, evict, max_percent, quiet)? && fits(0)? {
            return Ok(true);
        }
    }
    for c in candidates.iter().filter(|c| c.held) {
        if trim_slot_caches(c, |_| true, max_percent, quiet)? && fits(0)? {
            return Ok(true);
        }
    }

    let (mut shared_evicted, mut shared_bytes) = (0u64, 0u64);
    for unit in &shared {
        let size = dir_size([unit], &[]);
        if size == 0 && !unit.exists() {
            continue;
        }
        if remove_shared_unit(unit).is_ok() {
            shared_evicted += 1;
            shared_bytes += size;
        }
        if fits(0)? {
            if !quiet && shared_evicted > 0 {
                eprintln!(
                    "cargo-pool: disk above {max_percent}%: evicted {shared_evicted} shared dependency units ({})",
                    human(shared_bytes)
                );
            }
            return Ok(true);
        }
    }
    if !quiet && shared_evicted > 0 {
        eprintln!(
            "cargo-pool: disk above {max_percent}%: evicted {shared_evicted} shared dependency units ({})",
            human(shared_bytes)
        );
    }

    for c in candidates.iter().filter(|c| c.deletable && !c.excess_free) {
        if evict_slot(c, evict, max_percent, quiet)? && fits(0)? {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// rustc wrapper

/// Cargo runs a `build.rustc-wrapper` as `<wrapper> <rustc> <args...>`: a compiler path
/// (or name) in place of one of our subcommands means we're wrapping a compiler call.
fn is_rustc_wrapper(args: &[OsString]) -> bool {
    let Some(first) = args.get(1) else { return false };
    let first_str = first.to_string_lossy();
    let is_subcommand = matches!(
        first_str.as_ref(),
        "attach"
            | "detach"
            | "status"
            | "stats"
            | "gc"
            | "install"
            | "uninstall"
            | "install-hook"
            | "uninstall-hook"
            | "install-wrapper"
            | "uninstall-wrapper"
            | "snapshot-after-build"
            | "post-checkout"
            | "help"
            | "--help"
            | "-h"
            | "--version"
            | "-V"
    );
    if is_subcommand {
        return false;
    }
    let name = Path::new(first).file_name().unwrap_or_default().to_string_lossy();
    Path::new(first).is_file() || name.starts_with("rustc") || name.starts_with("clippy-driver")
}

/// Check disk usage if this compile writes into a pooled target dir, then exec the
/// compiler (through `CARGO_POOL_RUSTC_WRAPPER`, e.g. sccache, if set).
fn rustc_wrapper(args: Vec<OsString>) -> ! {
    if args[2..].iter().any(|a| a == "-vV") {
        if let Err(e) = attach_on_first_use(&args[1]) {
            eprintln!("cargo-pool: attaching to a pooled target dir failed: {e:#}");
            std::process::exit(1);
        }
        if let Err(e) = seed_on_first_use(&args[1]) {
            eprintln!("cargo-pool: warning: seeding the slot failed, building from scratch: {e:#}");
        }
        if let Err(e) = hydrate_on_probe(&args[1]) {
            eprintln!("cargo-pool: warning: hydrating shared dependencies failed: {e:#}");
        }
    }
    if let Err(e) = check_disk_during_build(&args[1], &args[2..]) {
        eprintln!("cargo-pool: warning: disk check failed: {e:#}");
    }
    let held = hold_job_slot(&args[2..]).unwrap_or_else(|e| {
        eprintln!("cargo-pool: warning: compiling without a job slot: {e:#}");
        false
    });
    let (program, rest) = match std::env::var_os("CARGO_POOL_RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
        Some(inner) => (inner, &args[1..]),
        None => (args[1].clone(), &args[2..]),
    };
    let mut cmd = Command::new(&program);
    cmd.args(rest);
    if held {
        cmd.env(IN_JOB_SLOT, "1");
    }
    let err = cmd.exec();
    eprintln!("cargo-pool: failed to run {}: {err}", program.to_string_lossy());
    std::process::exit(1);
}

/// Take one of `cargo-pool.jobs` machine-wide slots for a compile (cargo passes `--out-dir`
/// to every compile and to none of its probes), so concurrent cargo invocations, each running
/// up to nproc compilers, share the CPUs. The slot is a POSIX record lock: it survives the
/// exec but isn't inherited by forks, so it's released when the compiler exits, however it
/// exits, and never kept by a daemon it starts (an sccache server). Returns whether it took one.
fn hold_job_slot(rustc_args: &[OsString]) -> Result<bool> {
    // A compile nested in one holding a slot (a proc macro building a crate) would deadlock
    // waiting for the slot its parent holds.
    if !rustc_args.iter().any(|a| a == "--out-dir") || std::env::var_os(IN_JOB_SLOT).is_some() {
        return Ok(false);
    }
    let nproc = std::thread::available_parallelism().map_or(1, |n| n.get());
    // From `/` git reads no repository config: a repo's setting would cap just its crates.
    let jobs: usize = setting(Path::new("/"), "CARGO_POOL_JOBS", "cargo-pool.jobs", nproc);
    if jobs == 0 {
        return Ok(false);
    }
    // Not under `CARGO_HOME` or `TMPDIR`: environments that set those differently must share it.
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".cache/cargo-pool-jobs");
    fs::create_dir_all(&dir)?;
    let first = std::process::id() as usize;
    let try_lock = |slot: &File| -> Result<bool> {
        let lock = libc::flock { l_type: libc::F_WRLCK as _, l_whence: libc::SEEK_SET as _, l_start: 0, l_len: 0, l_pid: 0 };
        if unsafe { libc::fcntl(slot.as_raw_fd(), libc::F_SETLK, &lock) } != 0 {
            let err = std::io::Error::last_os_error();
            if matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
                return Ok(false);
            }
            return Err(err).context("lock a job slot");
        }
        // Closing any fd to the slot would drop the lock, so the compiler inherits this one.
        if unsafe { libc::fcntl(slot.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
            return Err(std::io::Error::last_os_error()).context("clear FD_CLOEXEC on the job slot");
        }
        Ok(true)
    };
    // Open slots lazily on the first pass: uncontended compiles only touch one lock file.
    let mut slots = Vec::with_capacity(jobs);
    for i in 0..jobs {
        let slot = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(((first + i) % jobs).to_string()))?;
        if try_lock(&slot)? {
            std::mem::forget(slot);
            std::mem::forget(slots);
            return Ok(true);
        }
        slots.push(slot);
    }
    let since = Instant::now();
    let mut notified = false;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        for slot in &slots {
            if try_lock(slot)? {
                std::mem::forget(slots);
                return Ok(true);
            }
        }
        // To the terminal: cargo caches a compiler's stderr and replays it on every later build.
        // Once a minute machine-wide: a stalled build has up to nproc compilers waiting.
        if !notified && since.elapsed() > Duration::from_secs(30) {
            notified = true;
            let marker = dir.join("notice");
            let last = fs::metadata(&marker).and_then(|m| m.modified()).ok();
            if last.is_none_or(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(60))
                && let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty")
            {
                let _ = OpenOptions::new().create(true).truncate(false).write(true).open(&marker).and_then(|m| m.set_modified(SystemTime::now()));
                let _ = writeln!(tty, "cargo-pool: waited 30s for one of {jobs} compiler job slots (`lslocks | grep cargo-pool-jobs` lists their holders)");
            }
        }
    }
}

fn check_disk_during_build(rustc_bin: &OsString, rustc_args: &[OsString]) -> Result<()> {
    let Some(out_dir) = rustc_args.iter().position(|a| a == "--out-dir").and_then(|i| rustc_args.get(i + 1)) else {
        return Ok(());
    };
    let Ok(out_dir) = fs::canonicalize(out_dir) else { return Ok(()) };
    let roots = pool_roots()?;
    let Some(root) = containing_pool_root(&roots, &out_dir)? else { return Ok(()) };
    let Ok(rel) = out_dir.strip_prefix(&root) else { return Ok(()) };
    let mut parts = rel.components().map(|c| c.as_os_str());
    let Some(pool) = parts.next() else { return Ok(()) };
    let mut slot_dir = None;
    if let (Some(slots), Some(id)) = (parts.next(), parts.next())
        && slots == "slots"
    {
        let dir = root.join(pool).join(slots).join(id);
        if let Err(e) = note_pooled_compile(&dir, rustc_bin, rustc_args) {
            eprintln!("cargo-pool: warning: can't track build for mtime snapshots: {e:#}");
        }
        slot_dir = Some(dir);
    }
    let git_dir = pool_git_dir(&root.join(pool));
    let max_disk = setting(&git_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    // Every filesystem, not just this build's: new slots go where there's room, so a full one
    // might otherwise never see another build.
    for (_, root) in pool_filesystems(&roots) {
        if disk_percent(root, 0)? < max_disk {
            continue;
        }
        // Over the limit. Many rustc processes run in parallel: let one of them clean up,
        // and don't rescan more than once a minute if the disk can't be brought under.
        // The marker holds the time of the last cleanup (its mtime changes when locking it).
        let marker = root.join(".disk-cleanup");
        let recent = || {
            let last: u64 = fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            now().saturating_sub(last) < 60
        };
        if recent() {
            continue;
        }
        let Some(_lock) = FileLock::acquire(&marker, false)? else { continue };
        if recent() {
            continue;
        }
        // Quiet: Cargo caches a compiler's stderr and replays it on every later build.
        let ok = free_disk_impl(root, max_disk, 0, slot_dir.as_deref().as_slice(), Evict::Any, true);
        fs::write(&marker, now().to_string())?;
        let _ = ok?;
    }
    Ok(())
}

fn cargo_home() -> Result<PathBuf> {
    Ok(match std::env::var_os("CARGO_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?).join(".cargo"),
    })
}

fn cargo_config() -> Result<PathBuf> {
    Ok(cargo_home()?.join("config.toml"))
}

/// Cargo's config points at this script rather than at the binary itself: it lives
/// outside `$CARGO_HOME/bin`, so a plain `cargo uninstall cargo-pool` can't leave cargo
/// calling a missing wrapper. Without cargo-pool it runs the compiler directly.
fn wrapper_shim() -> Result<PathBuf> {
    Ok(cargo_home()?.join("cargo-pool-rustc-wrapper"))
}

fn shim_script(exe: &Path) -> String {
    format!(
        r#"#!/bin/sh
# rustc wrapper installed by `cargo pool install`; remove with `cargo pool uninstall`.
# Falls back to running the compiler directly once cargo-pool is uninstalled.
pool={exe:?}
[ -x "$pool" ] || pool=$(command -v cargo-pool) || pool=
[ -n "$pool" ] && exec "$pool" "$@"
exec ${{CARGO_POOL_RUSTC_WRAPPER:+"$CARGO_POOL_RUSTC_WRAPPER"}} "$@"
"#
    )
}

fn install_wrapper() -> Result<()> {
    let config = cargo_config()?;
    let exe = fs::canonicalize(std::env::current_exe()?)?;
    let shim = wrapper_shim()?;
    fs::create_dir_all(shim.parent().unwrap())?;
    fs::write(&shim, shim_script(&exe))?;
    fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let current = fs::read_to_string(&config).unwrap_or_default();
    let entry = format!("rustc-wrapper = {:?}  # cargo-pool: disk check on every pooled build", shim.to_string_lossy());
    if let Some(line) = current.lines().find(|l| l.trim_start().starts_with("rustc-wrapper")) {
        if line.trim() == entry {
            println!("already installed in {}", config.display());
            return Ok(());
        }
        if is_our_wrapper_line(line) {
            // Installed by an older version or from another location: repoint it.
            fs::write(&config, current.replacen(line, &entry, 1))?;
            println!("updated rustc-wrapper in {} to {}", config.display(), shim.display());
            return Ok(());
        }
        bail!(
            "{} already sets `{}`; remove it first (set CARGO_POOL_RUSTC_WRAPPER to keep using it through cargo-pool)",
            config.display(),
            line.trim()
        );
    }
    let updated = match current.lines().position(|l| l.trim() == "[build]") {
        Some(i) => {
            let mut lines: Vec<&str> = current.lines().collect();
            lines.insert(i + 1, &entry);
            lines.join("\n") + "\n"
        }
        None => {
            let sep = if current.is_empty() { "" } else if current.ends_with('\n') { "\n" } else { "\n\n" };
            format!("{current}{sep}[build]\n{entry}\n")
        }
    };
    fs::create_dir_all(config.parent().unwrap())?;
    fs::write(&config, updated)?;
    println!("installed rustc-wrapper in {}", config.display());
    if let Some(w) = std::env::var_os("RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
        eprintln!(
            "cargo-pool: warning: RUSTC_WRAPPER={} is set in this environment and overrides the config",
            w.to_string_lossy()
        );
    }
    Ok(())
}

fn is_our_wrapper_line(l: &str) -> bool {
    l.trim_start().starts_with("rustc-wrapper") && l.contains("cargo-pool")
}

fn uninstall_wrapper() -> Result<()> {
    let config = cargo_config()?;
    let current = fs::read_to_string(&config).with_context(|| format!("reading {}", config.display()))?;
    let kept: Vec<&str> = current
        .lines()
        .filter(|l| !is_our_wrapper_line(l))
        .collect();
    // Drop a `[build]` table we left empty.
    let mut out: Vec<&str> = vec![];
    for (i, l) in kept.iter().enumerate() {
        let next = kept[i + 1..].iter().find(|n| !n.trim().is_empty());
        if l.trim() == "[build]" && next.is_none_or(|n| n.trim_start().starts_with('[')) {
            continue;
        }
        out.push(l);
    }
    let text = out.join("\n").trim().to_string();
    if text.is_empty() {
        fs::remove_file(&config)?;
    } else {
        fs::write(&config, text + "\n")?;
    }
    let _ = fs::remove_file(wrapper_shim()?);
    println!("removed cargo-pool rustc-wrapper from {}", config.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// snapshots: carry file mtimes over to the next worktree
//
// Cargo decides freshness of workspace crates by comparing source mtimes with the time of
// the last build, so a fresh checkout (all mtimes "now") rebuilds every workspace crate.
// After each build into a slot we record (size, mtime, content hash) of every file in its
// worktree. When the slot moves to a new worktree, files whose content matches get their
// recorded mtime back: cargo then sees exactly what it saw after that build, as if the new
// worktree's changes had been made in the old one. Cargo itself treats files modified
// during a build as dirty, so copying exact (content, mtime) pairs inherits its correctness.

/// State of a worktree's files right after a build into the slot.
#[derive(Serialize, Deserialize, Default)]
struct Snapshot {
    /// When the snapshot was started (ns). Only valid while nothing compiled since.
    taken_at: u64,
    /// Path relative to the worktree -> (size, mtime in ns, content hash).
    files: BTreeMap<String, (u64, u64, u64)>,
    /// Directories holding those files -> (mtime in ns, hash of entry names). Cargo checks
    /// directory mtimes for `rerun-if-changed=<dir>`.
    #[serde(default)]
    dirs: BTreeMap<String, (u64, u64)>,
}

fn dir_listing_hash(p: &Path) -> Option<u64> {
    let mut names: Vec<_> = fs::read_dir(p).ok()?.flatten().map(|e| e.file_name()).collect();
    names.sort();
    Some(names.iter().fold(FNV_OFFSET, |h, n| fnv1a_update(fnv1a_update(h, n.as_encoded_bytes()), b"\0")))
}

fn hash_file_buf(p: &Path, buf: &mut [u8]) -> Option<u64> {
    let mut f = File::open(p).ok()?;
    let mut h = FNV_OFFSET;
    loop {
        match f.read(buf).ok()? {
            0 => return Some(h),
            n => h = fnv1a_update(h, &buf[..n]),
        }
    }
}

/// Tracked and untracked (not ignored) files of a worktree, relative to its root.
fn worktree_files(wt: &Path) -> Result<Vec<String>> {
    let out = git(wt, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"])?;
    let mut files: Vec<String> = out.split('\0').filter(|f| !f.is_empty()).map(String::from).collect();
    files.dedup();
    Ok(files)
}

fn take_snapshot(wt: &Path, prev: Option<&Snapshot>) -> Result<Snapshot> {
    let mut snap = Snapshot { taken_at: now_ns(), ..Default::default() };
    let mut buf = vec![0u8; 1 << 16];
    for rel in worktree_files(wt)? {
        let p = wt.join(&rel);
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if !m.is_file() {
            continue;
        }
        let (size, mtime) = (m.len(), modified_ns(&m));
        let hash = match prev.and_then(|s| s.files.get(&rel)) {
            Some(&(s, t, h)) if (s, t) == (size, mtime) => h,
            _ => {
                let Some(h) = hash_file_buf(&p, &mut buf) else { continue };
                // Modified while hashing: the hash may not belong to this mtime.
                match fs::symlink_metadata(&p) {
                    Ok(m) if (m.len(), modified_ns(&m)) == (size, mtime) => h,
                    _ => continue,
                }
            }
        };
        snap.files.insert(rel, (size, mtime, hash));
    }
    let dirs: std::collections::BTreeSet<String> = snap
        .files
        .keys()
        .flat_map(|f| Path::new(f).ancestors().skip(1).map(|d| d.to_string_lossy().into_owned()))
        .collect();
    for rel in dirs {
        let p = wt.join(&rel);
        let mtime = mtime_ns(&p);
        if let Some(listing) = dir_listing_hash(&p)
            && mtime_ns(&p) == mtime
        {
            snap.dirs.insert(rel, (mtime, listing));
        }
    }
    Ok(snap)
}

fn load_snapshot(slot_dir: &Path) -> Option<Snapshot> {
    serde_json::from_slice(&fs::read(slot_dir.join("snapshot.json")).ok()?).ok()
}

fn save_snapshot(slot_dir: &Path, snap: &Snapshot) -> Result<()> {
    let tmp = slot_dir.join("snapshot.json.tmp");
    fs::write(&tmp, serde_json::to_vec(snap)?)?;
    fs::rename(tmp, slot_dir.join("snapshot.json"))?;
    Ok(())
}

/// Last time anything was compiled into the slot: the rustc wrapper's marker, or cargo's
/// output directories for builds that bypassed the wrapper.
fn last_compile_ns(slot_dir: &Path) -> u64 {
    mtime_ns(&slot_dir.join("last-compile")).max(last_activity_ns(&slot_dir.join("target")))
}

/// The slot's snapshot, if nothing has been compiled into the slot since it was taken.
fn valid_snapshot(slot_dir: &Path) -> Option<Snapshot> {
    load_snapshot(slot_dir).filter(|s| s.taken_at > last_compile_ns(slot_dir))
}

fn io_workers(items: usize) -> usize {
    if items < 4 {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, |n| n.get()).min(8).min(items)
}

/// Give every file in `wt` whose content matches the snapshot its recorded mtime.
/// Returns (files matched, files in the worktree).
fn backdate(wt: &Path, snap: &Snapshot) -> Result<(usize, usize)> {
    let files = worktree_files(wt)?;
    let backdate_one = |rel: &String, buf: &mut Vec<u8>| -> bool {
        let Some(&(size, mtime, hash)) = snap.files.get(rel) else { return false };
        let p = wt.join(rel);
        let Ok(m) = fs::symlink_metadata(&p) else { return false };
        if !m.is_file() || m.len() != size {
            return false;
        }
        if modified_ns(&m) != mtime {
            if hash_file_buf(&p, buf) != Some(hash) {
                return false;
            }
            if let Err(e) = File::open(&p).and_then(|f| f.set_modified(UNIX_EPOCH + Duration::from_nanos(mtime))) {
                eprintln!("cargo-pool: warning: cannot backdate {}: {e}", p.display());
                return false;
            }
        }
        true
    };
    let workers = if files.len() >= 64 { io_workers(files.len()) } else { 1 };
    let matched = if workers <= 1 {
        let mut buf = vec![0u8; 1 << 16];
        files.iter().filter(|rel| backdate_one(rel, &mut buf)).count()
    } else {
        let next = AtomicUsize::new(0);
        let matched = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    let mut buf = vec![0u8; 1 << 16];
                    let mut local = 0;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() {
                            break;
                        }
                        if backdate_one(&files[i], &mut buf) {
                            local += 1;
                        }
                    }
                    matched.fetch_add(local, Ordering::Relaxed);
                });
            }
        });
        matched.into_inner()
    };
    // Same for directories whose entries are unchanged (setting a child's mtime doesn't
    // touch its parent's, so the order doesn't matter).
    for (rel, &(mtime, listing)) in &snap.dirs {
        let p = wt.join(rel);
        if mtime_ns(&p) != mtime
            && dir_listing_hash(&p) == Some(listing)
            && let Err(e) = File::open(&p).and_then(|f| f.set_modified(UNIX_EPOCH + Duration::from_nanos(mtime)))
        {
            eprintln!("cargo-pool: warning: cannot backdate {}: {e}", p.display());
        }
    }
    // Refresh git's cached stat info so `git status` doesn't rehash everything.
    let _ = git(wt, &["update-index", "-q", "--refresh"]);
    Ok((matched, files.len()))
}

/// Delete the fingerprints of units whose recorded inputs include git metadata of another
/// checkout, e.g. `git_version!()` tracking `.git/worktrees/<other>/HEAD`. Those inputs
/// don't change when this worktree moves to another commit, so the units would stay fresh
/// and keep embedding the other checkout's version info. Returns how many were reset.
fn invalidate_foreign_git_inputs(repo: &Repo, target: &Path) -> Result<usize> {
    let own_gitdir = git(&repo.workspace, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
    let common = repo.common_dir.as_os_str().as_bytes();
    let own_prefix = format!("{own_gitdir}/");
    let refers_elsewhere = |bytes: &[u8]| {
        bytes.windows(common.len()).enumerate().filter(|(_, w)| *w == common).any(|(i, _)| {
            let input = &bytes[i..];
            if own_gitdir.as_bytes() == common {
                input[common.len()..].starts_with(b"/worktrees/")
            } else {
                !input.starts_with(own_prefix.as_bytes())
            }
        })
    };
    let fingerprints: Vec<PathBuf> = profile_dirs(target)
        .iter()
        .flat_map(|d| profile_units(d))
        .map(|(_, fp, _)| fp)
        .collect();
    let check_one = |fingerprint: &Path| -> Result<bool> {
        let foreign = fs::read_dir(fingerprint)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|f| {
                let name = f.file_name();
                let s = name.to_string_lossy();
                s.ends_with(".json") || s.starts_with("dep-")
            })
            .any(|f| fs::read(f.path()).is_ok_and(|b| refers_elsewhere(&b)));
        if foreign {
            fs::remove_dir_all(fingerprint)?;
            return Ok(true);
        }
        Ok(false)
    };
    let workers = if fingerprints.len() >= 64 { io_workers(fingerprints.len()) } else { 1 };
    if workers <= 1 {
        let mut reset = 0;
        for fp in &fingerprints {
            if check_one(fp)? {
                reset += 1;
            }
        }
        return Ok(reset);
    }
    let next = AtomicUsize::new(0);
    let reset = AtomicUsize::new(0);
    let err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= fingerprints.len() {
                    break;
                }
                match check_one(&fingerprints[i]) {
                    Ok(true) => {
                        reset.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        let mut guard = err.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(e);
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(reset.into_inner())
}

/// A cargo profile dir: one with a `.fingerprint` (classic layout) or `build` (both layouts).
fn is_profile(dir: &Path) -> bool {
    dir.join(".fingerprint").is_dir() || dir.join("build").is_dir()
}

/// Cargo's profile dirs in a target dir (`<profile>` and `<triple>/<profile>`).
fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if is_profile(&dir) {
            out.push(dir);
            continue;
        }
        if depth < 2 {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push((e.path(), depth + 1));
                }
            }
        }
    }
    out
}

/// A profile dir's build units as (hash, fingerprint dir, unit dir in the new layout): classic
/// `.fingerprint/<pkg>-<h>/`, and `build/<pkg>/<h>/fingerprint/` in cargo's new build-dir layout,
/// where `build/<pkg>/<h>/` holds all of the unit.
fn profile_units(profile: &Path) -> Vec<(Option<String>, PathBuf, Option<PathBuf>)> {
    let mut out = vec![];
    for e in fs::read_dir(profile.join(".fingerprint")).into_iter().flatten().flatten() {
        out.push((e.file_name().to_str().and_then(unit_hash).map(str::to_string), e.path(), None));
    }
    for pkg in fs::read_dir(profile.join("build")).into_iter().flatten().flatten() {
        // Classic `build/<pkg>-<h>/` dirs belong to units in `.fingerprint/`.
        if pkg.file_name().to_str().is_none_or(|n| unit_hash(n).is_some()) {
            continue;
        }
        for e in fs::read_dir(pkg.path()).into_iter().flatten().flatten() {
            if let Some(hash) = e.file_name().to_str().filter(|n| is_hash(n)) {
                out.push((Some(hash.to_string()), e.path().join("fingerprint"), Some(e.path())));
            }
        }
    }
    out
}

/// What a worktree's Cargo.lock pins, and a hash of it to tell whether a slot was pruned for it.
struct Lockfile {
    hash: String,
    /// `<name>-<version>` of every package: a registry package's source dir name.
    packages: HashSet<String>,
    /// Commits of git packages.
    commits: Vec<String>,
}

/// Where a build unit's crate came from.
#[derive(Clone)]
enum Origin {
    /// `<name>-<version>`.
    Registry(String),
    /// Abbreviated commit.
    Git(String),
}

impl Lockfile {
    /// None without a readable Cargo.lock: then nothing is pruned.
    fn read(worktree: &Path) -> Option<Lockfile> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            package: Vec<Package>,
        }
        #[derive(Deserialize)]
        struct Package {
            name: String,
            version: String,
            source: Option<String>,
        }
        let bytes = fs::read(worktree.join("Cargo.lock")).ok()?;
        let raw: Raw = toml::from_str(std::str::from_utf8(&bytes).ok()?).ok()?;
        Some(Lockfile {
            hash: format!("{:016x}", fnv1a(&bytes)),
            commits: raw
                .package
                .iter()
                .filter_map(|p| Some(p.source.as_ref()?.strip_prefix("git+")?.rsplit_once('#')?.1.to_string()))
                .collect(),
            packages: raw.package.into_iter().map(|p| format!("{}-{}", p.name, p.version)).collect(),
        })
    }

    fn pins(&self, origin: &Origin) -> bool {
        match origin {
            Origin::Registry(package) => self.packages.contains(package),
            Origin::Git(rev) => self.commits.iter().any(|c| c.starts_with(rev.as_str())),
        }
    }
}

/// A hash of the worktree's toolchain file: worktrees with the same one build with the same compiler.
fn toolchain_hash(worktree: &Path) -> String {
    let bytes = ["rust-toolchain.toml", "rust-toolchain"].iter().find_map(|f| fs::read(worktree.join(f)).ok());
    format!("{:016x}", fnv1a(&bytes.unwrap_or_default()))
}

/// What a slot was last pruned for, in `<slot>/pruned-for` as `<lock hash> <toolchain hash> <rustc>`:
/// the hashes of its worktree's Cargo.lock and toolchain file, and the `rustc` fingerprint of the
/// compiler whose units were kept, or `-` if units of every compiler were kept.
#[derive(PartialEq)]
struct Stamp {
    lock: String,
    toolchain: String,
    rustc: Option<u64>,
}

impl Stamp {
    fn read(slot_dir: &Path) -> Option<Stamp> {
        let text = fs::read_to_string(slot_dir.join("pruned-for")).ok()?;
        let mut words = text.split_whitespace();
        let (lock, toolchain, rustc) = (words.next()?, words.next()?, words.next()?);
        Some(Stamp { lock: lock.to_string(), toolchain: toolchain.to_string(), rustc: rustc.parse().ok() })
    }

    fn write(&self, slot_dir: &Path) -> Result<()> {
        let rustc = self.rustc.map_or("-".to_string(), |r| r.to_string());
        fs::write(slot_dir.join("pruned-for"), format!("{} {} {rustc}\n", self.lock, self.toolchain))?;
        Ok(())
    }
}

/// The origin of a compiled unit, from the first source in its dep-info (`<output>: <src> ...`):
/// `/registry/src/<index>/<name>-<version>/...` or `/git/checkouts/<repo>/<rev>/...`.
fn origin(dep_info: &Path) -> Option<Origin> {
    let mut line = vec![];
    BufReader::new(File::open(dep_info).ok()?).read_until(b'\n', &mut line).ok()?;
    let line = String::from_utf8_lossy(&line);
    let (_, sources) = line.split_once(": ")?;
    // Spaces inside paths are escaped.
    let end = sources.as_bytes().windows(2).position(|w| w[0] != b'\\' && w[1] == b' ').map_or(sources.len(), |i| i + 1);
    let first = &sources[..end];
    if let Some((_, rest)) = first.split_once("/registry/src/") {
        return Some(Origin::Registry(rest.split('/').nth(1)?.to_string()));
    }
    let (_, rest) = first.split_once("/git/checkouts/")?;
    Some(Origin::Git(rest.split('/').nth(1)?.to_string()))
}

fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `<h>` of a unit's entry, named `<name>-<h>` or `<name>-<h>.<ext>` with 16 hex digits.
fn unit_hash(name: &str) -> Option<&str> {
    name.match_indices('-').rev().find_map(|(i, _)| {
        let h = name.get(i + 1..i + 17)?;
        let rest = &name[i + 17..];
        (is_hash(h) && (rest.is_empty() || rest.starts_with('.'))).then_some(h)
    })
}

/// What pruning and shared-cache harvesting read from a unit's fingerprint JSON (`<kind>-<target>.json`).
#[derive(Deserialize)]
struct Fingerprint {
    /// Hash of the compiler that built the unit.
    rustc: Option<u64>,
    /// (package id, name, public, fingerprint)
    deps: Vec<(u64, String, bool, u64)>,
    /// Local fingerprint checks (`RerunIfChanged`, `CheckDepInfo`, `RerunIfEnvChanged`, ...).
    #[serde(default)]
    local: Vec<serde_json::Value>,
}

/// Build units in `target` that a worktree at `lock` can't use: built from a crate version or git
/// revision it doesn't pin, by another compiler than `rustc` (if known), never finished (no
/// fingerprint JSON), or depending on such a unit. Returns their number and the paths to delete:
/// classic units' entries in `.fingerprint/`, `deps/`, `build/` and `examples/`, and new-layout
/// units' `build/<pkg>/<h>/` dirs, or `build/<pkg>/` if none of its units stay.
fn unusable_units(target: &Path, lock: &Lockfile, rustc: Option<u64>) -> (usize, Vec<PathBuf>) {
    unusable_units_for(target, lock, rustc, None)
}

fn unusable_units_for(
    target: &Path,
    lock: &Lockfile,
    rustc: Option<u64>,
    only_profiles: Option<&HashSet<String>>,
) -> (usize, Vec<PathBuf>) {
    struct Unit {
        profile: usize,
        hash: String,
        dir: Option<PathBuf>,
        deps: Vec<u64>,
        dead: bool,
    }
    let profiles: Vec<PathBuf> = profile_dirs(target)
        .into_iter()
        .filter(|p| {
            only_profiles.is_none_or(|set| {
                p.strip_prefix(target)
                    .ok()
                    .and_then(|rel| rel.to_str())
                    .is_some_and(|rel| set.contains(rel))
            })
        })
        .collect();
    let mut units = vec![];
    // Units depend on each other by fingerprint, across profile dirs (build scripts and proc
    // macros are built for the host) and layouts.
    let mut by_fingerprint = HashMap::new();
    for (p, profile) in profiles.iter().enumerate() {
        let mut origins = HashMap::new();
        let build_dirs = fs::read_dir(profile.join("build"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_str().and_then(unit_hash).is_some())
            .map(|e| e.path());
        for dir in std::iter::once(profile.join("deps")).chain(build_dirs) {
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name();
                if let Some(hash) = name.to_str().filter(|n| n.ends_with(".d")).and_then(unit_hash)
                    && let Some(origin) = origin(&e.path())
                {
                    origins.insert(hash.to_string(), origin);
                }
            }
        }
        for (hash, fingerprint, dir) in profile_units(profile) {
            let Some(hash) = hash else { continue };
            let (mut deps, mut finished, mut foreign) = (vec![], false, false);
            for f in fs::read_dir(&fingerprint).into_iter().flatten().flatten() {
                let path = f.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                finished = true;
                if let Some(json) = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Fingerprint>(&b).ok()) {
                    deps.extend(json.deps.iter().map(|d| d.3));
                    foreign |= rustc.is_some() && json.rustc.is_some() && json.rustc != rustc;
                }
                // The unit's own fingerprint: the u64's little-endian bytes in hex.
                if let Some(fp) = fs::read_to_string(path.with_extension("")).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok()) {
                    by_fingerprint.insert(fp.swap_bytes(), units.len());
                }
            }
            // A new-layout unit's `out/` is a build script's OUT_DIR for run units: only its own
            // dep-info counts.
            let source = match &dir {
                None => origins.remove(&hash),
                Some(dir) => fs::read_dir(dir.join("out")).into_iter().flatten().flatten().find_map(|e| {
                    let name = e.file_name();
                    let own = name.to_str().filter(|n| n.ends_with(".d")).and_then(unit_hash) == Some(hash.as_str());
                    own.then(|| origin(&e.path())).flatten()
                }),
            };
            let dead = !finished || foreign || source.is_some_and(|o| !lock.pins(&o));
            units.push(Unit { profile: p, hash, dir, deps, dead });
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..units.len() {
            if !units[i].dead && units[i].deps.iter().any(|f| by_fingerprint.get(f).is_some_and(|&j| units[j].dead)) {
                units[i].dead = true;
                changed = true;
            }
        }
    }
    let mut dead = vec![HashSet::new(); profiles.len()];
    let mut dead_in_pkg: HashMap<&Path, Vec<&Path>> = HashMap::new();
    for u in units.iter().filter(|u| u.dead) {
        match &u.dir {
            None => {
                dead[u.profile].insert(u.hash.as_str());
            }
            Some(dir) => dead_in_pkg.entry(dir.parent().unwrap()).or_default().push(dir),
        }
    }
    let mut entries = vec![];
    for (profile, dead) in profiles.iter().zip(&dead).filter(|(_, d)| !d.is_empty()) {
        for sub in [".fingerprint", "deps", "build", "examples"] {
            for e in fs::read_dir(profile.join(sub)).into_iter().flatten().flatten() {
                if e.file_name().to_str().and_then(unit_hash).is_some_and(|h| dead.contains(h)) {
                    entries.push(e.path());
                }
            }
        }
    }
    for (pkg, dirs) in dead_in_pkg {
        if fs::read_dir(pkg).is_ok_and(|d| d.count() == dirs.len()) {
            entries.push(pkg.to_path_buf());
        } else {
            entries.extend(dirs.into_iter().map(Path::to_path_buf));
        }
    }
    (units.iter().filter(|u| u.dead).count(), entries)
}

/// The `rustc` of the newest fingerprint JSON in `target`: the compiler that last built into it.
fn last_rustc(target: &Path) -> Option<u64> {
    let newest = profile_dirs(target)
        .iter()
        .flat_map(|p| profile_units(p))
        .flat_map(|(_, fingerprint, _)| fs::read_dir(fingerprint).into_iter().flatten().flatten())
        .filter(|f| f.path().extension().is_some_and(|x| x == "json"))
        .max_by_key(|f| f.metadata().map_or(0, |m| modified_ns(&m)))?;
    serde_json::from_slice::<Fingerprint>(&fs::read(newest.path()).ok()?).ok()?.rustc
}

/// Delete the slot's build units that `worktree` can't use: see `unusable_units`. If the worktree
/// `just_built` into the slot, its compiler is the one of the newest fingerprint. Before its first
/// build, it's the one in the slot's stamp if that is for the same toolchain file, and unknown
/// otherwise. Skipped while a build runs, and if the slot was already pruned for the same
/// Cargo.lock, toolchain file and compiler: everything built since then was built for them.
fn prune(worktree: &Path, slot_dir: &Path, just_built: bool) -> Result<()> {
    let Some(lock) = Lockfile::read(worktree) else { return Ok(()) };
    let target = slot_dir.join("target");
    let old = Stamp::read(slot_dir);
    let toolchain = toolchain_hash(worktree);
    let rustc = match &old {
        _ if just_built => last_rustc(&target),
        Some(s) if s.toolchain == toolchain => s.rustc,
        _ => None,
    };
    let stamp = Stamp { lock: lock.hash.clone(), toolchain, rustc };
    if old.as_ref() == Some(&stamp) {
        return Ok(());
    }
    let Some(_build_locks) = lock_target(&target, false) else { return Ok(()) };
    let started = Instant::now();
    let (units, entries) = unusable_units(&target, &lock, rustc);
    let bytes = dir_size(&entries, &[]);
    for p in &entries {
        if fs::symlink_metadata(p)?.is_dir() {
            fs::remove_dir_all(p)?;
        } else {
            fs::remove_file(p)?;
        }
    }
    stamp.write(slot_dir)?;
    record(slot_dir, Event::Prune { units: units as u64, bytes, ms: ms_since(started) });
    if units > 0 {
        eprintln!("cargo-pool: deleted {units} build units ({}) this worktree can't use at its Cargo.lock and toolchain", human(bytes));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// global shared external-unit store (`<pool-root>/shared/`)
//
// Registry and git dependency units whose transitive dependencies are all external produce
// identical Cargo unit hashes across worktrees, monorepo workspaces, and repositories.
// After each build, `snapshot_after_build` harvests finished external units into
// `<pool-root>/shared/units/<reg|git>/<origin>/<profile>__<hash>__<rustc>__<fp>/`.
// New slots (via seed) and warm slots (via attach or `rustc -vV` probe) hydrate missing
// `Cargo.lock`-pinned units from the shared store using reflinks where supported or copies.

/// Schema version for `shared/units/.../meta.json`. Units from older versions (without `abi`
/// / `dep_abis` SVH tracking) are ignored on hydration and replaced on harvest.
const SHARED_META_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct SharedUnitMeta {
    #[serde(default)]
    version: u32,
    profile: String,
    hash: String,
    rustc: u64,
    fp: u64,
    #[serde(default)]
    abi: u64,
    deps: Vec<u64>,
    #[serde(default)]
    dep_abis: Vec<(u64, u64)>,
}

struct ExternalUnit {
    profile_rel: String,
    hash: String,
    origin: Origin,
    rustc: u64,
    fp: u64,
    abi: u64,
    deps: Vec<u64>,
    dep_abis: Vec<(u64, u64)>,
    /// Paths relative to `<target>/<profile_rel>` belonging to this unit.
    files: Vec<PathBuf>,
}

/// Hash the leading bytes of a `.rmeta` file, which hold rustc's metadata header and 16-byte
/// `CrateHash` (SVH). Crates that `include!` from `OUT_DIR` produce a different SVH per worktree
/// even when their Cargo fingerprint is identical; comparing this hash prevents mixing
/// incompatible dependency ABIs across worktrees.
fn rmeta_header_hash(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).ok()?;
    (n > 0).then(|| fnv1a(&buf[..n]))
}

/// Hash the `lib.rmeta` member header inside a `.rlib` `ar` archive (or the leading bytes if not
/// an `ar` archive, e.g. synthetic test artifacts).
fn rlib_abi_hash(path: &Path) -> Option<u64> {
    let mut f = File::open(path).ok()?;
    let mut magic = [0u8; 8];
    let n = f.read(&mut magic).ok()?;
    if n < 8 || &magic != b"!<arch>\n" {
        return rmeta_header_hash(path);
    }
    let mut hdr = [0u8; 60];
    while f.read_exact(&mut hdr).is_ok() {
        if &hdr[58..60] != b"`\n" {
            break;
        }
        let Ok(size_str) = std::str::from_utf8(&hdr[48..58]) else { break };
        let Ok(size) = size_str.trim().parse::<u64>() else { break };
        let raw_name = std::str::from_utf8(&hdr[0..16]).unwrap_or("").trim();
        let (name, payload_size) = if let Some(rest) = raw_name.strip_prefix("#1/")
            && let Ok(name_len) = rest.parse::<usize>()
            && (name_len as u64) <= size
        {
            let mut nbuf = vec![0u8; name_len];
            if f.read_exact(&mut nbuf).is_err() {
                break;
            }
            let s = String::from_utf8_lossy(&nbuf).trim_end_matches('\0').to_string();
            (s, size - name_len as u64)
        } else {
            (raw_name.trim_end_matches('/').to_string(), size)
        };
        if name == "lib.rmeta" {
            let to_read = payload_size.min(64) as usize;
            let mut buf = [0u8; 64];
            if to_read > 0 && f.read_exact(&mut buf[..to_read]).is_ok() {
                return Some(fnv1a(&buf[..to_read]));
            }
            return None;
        }
        let skip = payload_size + (size % 2);
        if f.seek(SeekFrom::Current(skip as i64)).is_err() {
            break;
        }
    }
    None
}

/// Compute a unit's compiled artifact ABI hash for SVH compatibility checks.
/// Prefers `.rmeta`, falls back to `.rlib`'s embedded `lib.rmeta`, and uses `fallback_fp` for
/// non-library units (build scripts, `run-build-script`, proc-macros).
fn unit_abi_hash(dir: Option<&Path>, classic_artifact: Option<&Path>, fallback_fp: u64) -> u64 {
    if let Some(unit_dir) = dir {
        let mut rmeta = None;
        let mut rlib = None;
        for e in fs::read_dir(unit_dir.join("out")).into_iter().flatten().flatten() {
            let p = e.path();
            match p.extension().and_then(|x| x.to_str()) {
                Some("rmeta") => {
                    rmeta = Some(p);
                    break;
                }
                Some("rlib") if rlib.is_none() => rlib = Some(p),
                _ => {}
            }
        }
        if let Some(p) = rmeta {
            return rmeta_header_hash(&p).unwrap_or(fallback_fp);
        }
        if let Some(p) = rlib {
            return rlib_abi_hash(&p).unwrap_or(fallback_fp);
        }
        return fallback_fp;
    }
    if let Some(p) = classic_artifact {
        return match p.extension().and_then(|x| x.to_str()) {
            Some("rmeta") => rmeta_header_hash(p).unwrap_or(fallback_fp),
            Some("rlib") => rlib_abi_hash(p).unwrap_or(fallback_fp),
            _ => fallback_fp,
        };
    }
    fallback_fp
}

/// Pick the best classic-layout artifact (`.rmeta` over `.rlib`) per unit hash in `profile/deps`.
fn classic_deps_artifacts(profile: &Path) -> HashMap<String, PathBuf> {
    let mut out: HashMap<String, PathBuf> = HashMap::new();
    for e in fs::read_dir(profile.join("deps")).into_iter().flatten().flatten() {
        let name = e.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(hash) = unit_hash(s) else { continue };
        let p = e.path();
        match p.extension().and_then(|x| x.to_str()) {
            Some("rmeta") => {
                out.insert(hash.to_string(), p);
            }
            Some("rlib") => {
                out.entry(hash.to_string()).or_insert(p);
            }
            _ => {}
        }
    }
    out
}

fn safe_path_component(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('\\') && s != "." && s != ".."
}

fn encode_profile(rel: &str) -> String {
    rel.replace('/', "%")
}

fn decode_profile(enc: &str) -> String {
    enc.replace('%', "/")
}

fn origin_subpath(origin: &Origin) -> Option<PathBuf> {
    match origin {
        Origin::Registry(pkg) if safe_path_component(pkg) => Some(PathBuf::from("reg").join(pkg)),
        Origin::Git(rev) if safe_path_component(rev) => Some(PathBuf::from("git").join(rev)),
        _ => None,
    }
}

/// Parse `<profile_enc>__<hash>__<rustc>__<fp>` from a shared unit directory name.
fn parse_shared_unit_name(name: &str) -> Option<(String, String, u64, u64)> {
    let mut parts = name.split("__");
    let profile = decode_profile(parts.next()?);
    let hash = parts.next()?.to_string();
    let rustc: u64 = parts.next()?.parse().ok()?;
    let fp: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !is_hash(&hash) {
        return None;
    }
    Some((profile, hash, rustc, fp))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn local_references_worktree(local: &[serde_json::Value], worktree: &[u8], common_dir: &[u8]) -> bool {
    let check_path = |s: &str| {
        let b = s.as_bytes();
        Path::new(s).is_absolute()
            && (b.starts_with(worktree) || b.starts_with(common_dir) || contains_bytes(b, b"/.git/"))
    };
    for item in local {
        if let Some(paths) = item.get("RerunIfChanged").and_then(|v| v.get("paths")).and_then(|v| v.as_array())
            && paths.iter().filter_map(|p| p.as_str()).any(check_path)
        {
            return true;
        }
    }
    false
}

/// Extract finished purely-external build units from `target` (registry/git crates whose
/// transitive dependencies are all external and that don't reference files in `worktree` or `common_dir`).
fn extract_external_units(target: &Path, worktree: &Path, common_dir: &Path) -> Vec<ExternalUnit> {
    struct RawUnit {
        profile_idx: usize,
        profile_rel: String,
        hash: String,
        dir: Option<PathBuf>,
        origin: Option<Origin>,
        is_run_build_script: bool,
        rustc: Option<u64>,
        own_fp: Option<u64>,
        abi: u64,
        deps: Vec<u64>,
        external: bool,
        references_worktree: bool,
    }
    let wt_bytes = worktree.as_os_str().as_encoded_bytes();
    let common_bytes = common_dir.as_os_str().as_encoded_bytes();
    let wt_target = worktree.join("target");
    let wt_target_bytes = wt_target.as_os_str().as_encoded_bytes();
    let target_bytes = target.as_os_str().as_encoded_bytes();
    let path_has_prefix = |p: &[u8], prefix: &[u8]| {
        !prefix.is_empty() && p.starts_with(prefix) && matches!(p.get(prefix.len()), None | Some(b'/'))
    };
    let is_in_target = |p: &[u8]| {
        path_has_prefix(p, wt_target_bytes)
            || path_has_prefix(p, target_bytes)
            || (contains_bytes(p, b"/build/") && contains_bytes(p, b"/out/"))
    };
    let token_refs_wt = |tok: &[u8]| {
        !is_in_target(tok)
            && (path_has_prefix(tok, wt_bytes)
                || path_has_prefix(tok, common_bytes)
                || contains_bytes(tok, b"/.git/"))
    };
    let dep_refs_wt = |dep_info: &Path| {
        let Ok(bytes) = fs::read(dep_info) else { return false };
        bytes.split(|&b| b == b'\n').any(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line.starts_with(b"# env-dep:OUT_DIR=") {
                return false;
            }
            if let Some(rest) = line.strip_prefix(b"# env-dep:") {
                let val = rest.splitn(2, |&b| b == b'=').nth(1).unwrap_or(rest);
                return token_refs_wt(val);
            }
            if line.starts_with(b"#") || line.ends_with(b":") {
                return false;
            }
            let rhs = match line.windows(2).position(|w| w == b": ") {
                Some(i) => &line[i + 2..],
                None => line,
            };
            // Split Makefile dep paths on unescaped spaces.
            let mut start = 0;
            let mut i = 0;
            while i < rhs.len() {
                if rhs[i] == b'\\' && i + 1 < rhs.len() {
                    i += 2;
                    continue;
                }
                if rhs[i] == b' ' {
                    if i > start && token_refs_wt(&rhs[start..i]) {
                        return true;
                    }
                    start = i + 1;
                }
                i += 1;
            }
            start < rhs.len() && token_refs_wt(&rhs[start..])
        })
    };

    let profiles = profile_dirs(target);
    let mut units = vec![];
    let mut by_fingerprint: HashMap<u64, usize> = HashMap::new();
    // Classic layout file index per profile: hash -> Vec<relative path from profile dir>.
    let mut classic_files: Vec<HashMap<String, Vec<PathBuf>>> = vec![HashMap::new(); profiles.len()];

    struct UnitTask {
        profile_idx: usize,
        profile_rel: String,
        hash: String,
        fingerprint: PathBuf,
        dir: Option<PathBuf>,
        classic_origin: Option<(Origin, bool)>,
        classic_artifact: Option<PathBuf>,
    }
    let mut tasks = vec![];

    for (p, profile) in profiles.iter().enumerate() {
        let Ok(profile_rel) = profile.strip_prefix(target) else { continue };
        let profile_rel = profile_rel.to_string_lossy().into_owned();
        if profile_rel.is_empty() {
            continue;
        }
        let mut origins: HashMap<String, (Origin, bool)> = HashMap::new();
        let mut classic_artifacts: HashMap<String, PathBuf> = HashMap::new();
        for sub in [".fingerprint", "deps", "build", "examples"] {
            for e in fs::read_dir(profile.join(sub)).into_iter().flatten().flatten() {
                let name = e.file_name();
                let Some(s) = name.to_str() else { continue };
                let Some(hash) = unit_hash(s) else { continue };
                classic_files[p].entry(hash.to_string()).or_default().push(Path::new(sub).join(&name));
                if sub == "deps" {
                    let path = e.path();
                    if s.ends_with(".d")
                        && let Some(o) = origin(&path)
                    {
                        origins.insert(hash.to_string(), (o, dep_refs_wt(&path)));
                    } else if s.ends_with(".rmeta") {
                        classic_artifacts.insert(hash.to_string(), path);
                    } else if s.ends_with(".rlib") {
                        classic_artifacts.entry(hash.to_string()).or_insert(path);
                    }
                } else if sub == "build" && e.file_type().is_ok_and(|t| t.is_dir()) {
                    for f in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                        let fname = f.file_name();
                        if let Some(fs) = fname.to_str().filter(|n| n.ends_with(".d"))
                            && unit_hash(fs) == Some(hash)
                            && let Some(o) = origin(&f.path())
                        {
                            origins.insert(hash.to_string(), (o, dep_refs_wt(&f.path())));
                        }
                    }
                }
            }
        }
        for (hash, fingerprint, dir) in profile_units(profile) {
            let Some(hash) = hash else { continue };
            let classic_origin = if dir.is_none() { origins.remove(&hash) } else { None };
            let classic_artifact = if dir.is_none() { classic_artifacts.remove(&hash) } else { None };
            tasks.push(UnitTask {
                profile_idx: p,
                profile_rel: profile_rel.clone(),
                hash,
                fingerprint,
                dir,
                classic_origin,
                classic_artifact,
            });
        }
    }

    let parse_one = |t: &UnitTask| -> (RawUnit, Vec<u64>) {
        let (mut deps, mut finished, mut rustc, mut own_fp) = (vec![], false, None, None);
        let (mut is_run_bs, mut is_build_bs, mut refs_wt) = (false, false, false);
        let mut fps = vec![];
        for f in fs::read_dir(&t.fingerprint).into_iter().flatten().flatten() {
            let path = f.path();
            let name = f.file_name();
            let name_str = name.to_string_lossy();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            finished = true;
            is_run_bs |= name_str.starts_with("run-build-script-");
            is_build_bs |= name_str.starts_with("build-script-");
            if let Some(json) = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Fingerprint>(&b).ok()) {
                deps.extend(json.deps.iter().map(|d| d.3));
                rustc = rustc.or(json.rustc);
                refs_wt |= local_references_worktree(&json.local, wt_bytes, common_bytes);
            }
            if let Some(fp) = fs::read_to_string(path.with_extension("")).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok()) {
                let fp = fp.swap_bytes();
                fps.push(fp);
                own_fp = own_fp.or(Some(fp));
            }
        }
        let (origin, dep_wt) = match &t.dir {
            None => t.classic_origin.clone().map_or((None, false), |(o, w)| (Some(o), w)),
            Some(dir) => fs::read_dir(dir.join("out"))
                .into_iter()
                .flatten()
                .flatten()
                .find_map(|e| {
                    let name = e.file_name();
                    let s = name.to_str()?;
                    if !s.ends_with(".d") {
                        return None;
                    }
                    let own = unit_hash(s) == Some(t.hash.as_str()) || (is_build_bs && !is_run_bs);
                    own.then(|| origin(&e.path()).map(|o| (o, dep_refs_wt(&e.path())))).flatten()
                })
                .map_or((None, false), |(o, w)| (Some(o), w)),
        };
        refs_wt |= dep_wt;
        let external = finished && rustc.is_some() && own_fp.is_some() && !refs_wt && origin.is_some();
        let abi = own_fp.map_or(0, |fp| unit_abi_hash(t.dir.as_deref(), t.classic_artifact.as_deref(), fp));
        (
            RawUnit {
                profile_idx: t.profile_idx,
                profile_rel: t.profile_rel.clone(),
                hash: t.hash.clone(),
                dir: t.dir.clone(),
                origin,
                is_run_build_script: is_run_bs,
                rustc,
                own_fp,
                abi,
                deps,
                external,
                references_worktree: refs_wt,
            },
            fps,
        )
    };

    let workers = if tasks.len() >= 64 { io_workers(tasks.len()) } else { 1 };
    let parsed: Vec<(RawUnit, Vec<u64>)> = if workers <= 1 {
        tasks.iter().map(parse_one).collect()
    } else {
        let next = AtomicUsize::new(0);
        let mut out = Vec::with_capacity(tasks.len());
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut local = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= tasks.len() {
                                break;
                            }
                            local.push(parse_one(&tasks[i]));
                        }
                        local
                    })
                })
                .collect();
            for h in handles {
                out.extend(h.join().unwrap());
            }
        });
        out
    };
    for (unit, fps) in parsed {
        let idx = units.len();
        for fp in fps {
            by_fingerprint.insert(fp, idx);
        }
        units.push(unit);
    }

    // `run-build-script` units have no `.d` file of their own; inherit origin from their `build-script-build` dep.
    for i in 0..units.len() {
        if units[i].is_run_build_script && units[i].origin.is_none() && !units[i].references_worktree {
            let inherited = units[i]
                .deps
                .iter()
                .find_map(|fp| by_fingerprint.get(fp).and_then(|&j| units[j].origin.clone()));
            if let Some(o) = inherited {
                units[i].origin = Some(o);
                units[i].external = units[i].rustc.is_some() && units[i].own_fp.is_some();
            }
        }
    }

    // Fixed-point closure: drop any unit whose transitive deps aren't all present and external.
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..units.len() {
            if !units[i].external {
                continue;
            }
            let ok = units[i].deps.iter().all(|fp| by_fingerprint.get(fp).is_some_and(|&j| units[j].external));
            if !ok {
                units[i].external = false;
                changed = true;
            }
        }
    }

    let unit_abis: Vec<u64> = units.iter().map(|u| u.abi).collect();
    let mut out = vec![];
    for u in units.into_iter().filter(|u| u.external) {
        let Some(origin) = u.origin else { continue };
        let (Some(rustc), Some(fp)) = (u.rustc, u.own_fp) else { continue };
        let files = match u.dir {
            Some(dir) => match dir.strip_prefix(&profiles[u.profile_idx]) {
                Ok(rel) => vec![rel.to_path_buf()],
                Err(_) => continue,
            },
            None => classic_files[u.profile_idx].remove(&u.hash).unwrap_or_default(),
        };
        if files.is_empty() {
            continue;
        }
        let dep_abis: Vec<(u64, u64)> = u
            .deps
            .iter()
            .filter_map(|&dep_fp| by_fingerprint.get(&dep_fp).map(|&j| (dep_fp, unit_abis[j])))
            .collect();
        out.push(ExternalUnit {
            profile_rel: u.profile_rel,
            hash: u.hash,
            origin,
            rustc,
            fp,
            abi: u.abi,
            deps: u.deps,
            dep_abis,
            files,
        });
    }
    out
}

/// FNV-1a hex digest of `rustc -vV` output, used to map a compiler binary to Cargo's `Fingerprint.rustc`.
fn rustc_vv_hash(rustc_bin: Option<&OsString>, cwd: &Path) -> Option<String> {
    let program = rustc_bin.map(Path::new).unwrap_or_else(|| Path::new("rustc"));
    let out = Command::new(program).arg("-vV").current_dir(cwd).output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(format!("{:016x}", fnv1a(&out.stdout)))
}

fn record_toolchain(shared_dir: &Path, vv_hash: &str, rustc_fp: u64) -> Result<()> {
    fs::create_dir_all(shared_dir)?;
    let _lock = FileLock::acquire(&shared_dir.join(".lock"), true)?;
    let path = shared_dir.join("toolchains.json");
    let mut map: BTreeMap<String, u64> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if map.get(vv_hash) == Some(&rustc_fp) {
        return Ok(());
    }
    map.insert(vv_hash.to_string(), rustc_fp);
    let tmp = shared_dir.join(format!("toolchains.json.tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(&map)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn lookup_toolchain(roots: &[PathBuf], vv_hash: &str) -> Option<u64> {
    for root in roots {
        let path = root.join(SHARED_DIR).join("toolchains.json");
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(map) = serde_json::from_slice::<BTreeMap<String, u64>>(&bytes) else { continue };
        if let Some(&fp) = map.get(vv_hash) {
            return Some(fp);
        }
    }
    None
}

fn toolchain_recorded(shared_dir: &Path, rustc_fp: u64) -> bool {
    let path = shared_dir.join("toolchains.json");
    fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<BTreeMap<String, u64>>(&b).ok())
        .is_some_and(|map| map.values().any(|&fp| fp == rustc_fp))
}

fn shared_generation(roots: &[PathBuf]) -> u64 {
    roots
        .iter()
        .filter_map(|r| fs::read_to_string(r.join(SHARED_DIR).join("generation")).ok())
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .sum()
}

fn bump_shared_generation(shared_dir: &Path) -> Result<u64> {
    let _lock = FileLock::acquire(&shared_dir.join(".lock"), true)?;
    let path = shared_dir.join("generation");
    let next = fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) + 1;
    let tmp = shared_dir.join(format!("generation.tmp.{}", std::process::id()));
    fs::write(&tmp, format!("{next}\n"))?;
    fs::rename(tmp, path)?;
    Ok(next)
}

/// Remove Cargo's cached `rustc -vV` probe file on other idle slots so their next Cargo command
/// runs the `-vV` hook and hydrates newly published shared dependency units.
fn invalidate_idle_probe_caches(except_slot: &Path) {
    let Ok(roots) = pool_roots() else { return };
    for root in roots {
        for pool in fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()) {
            if pool.file_name().is_some_and(|n| n == SHARED_DIR) || !pool.join("slots").is_dir() {
                continue;
            }
            for slot in load_slots(&pool.join("slots")).unwrap_or_default() {
                if slot.dir == except_slot || slot.state() != State::Held {
                    continue;
                }
                let target = slot.target();
                let cache = target.join(".rustc_info.json");
                // Regular file only: a directory is the failed-probe guard from attach_on_first_use.
                if fs::symlink_metadata(&cache).is_ok_and(|m| m.is_file())
                    && let Some(_locks) = lock_target(&target, false)
                {
                    let _ = fs::remove_file(&cache);
                }
            }
        }
    }
}

/// Publish finished purely-external units from `slot_dir` into `<pool-root>/shared/`.
/// Never mutates `slot_dir/target` and never pushes the disk above `maxDiskPercent`.
fn harvest_shared(worktree: &Path, common_dir: &Path, slot_dir: &Path) -> Result<()> {
    if let Some(lock) = Lockfile::read(worktree)
        && lock.packages.is_empty()
        && lock.commits.is_empty()
    {
        return Ok(());
    }
    let target = slot_dir.join("target");
    let Some(root) = slot_dir.parent().and_then(Path::parent).and_then(Path::parent) else { return Ok(()) };
    let max_disk = setting(common_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    let shared_dir = root.join(SHARED_DIR);
    let tmp_root = shared_dir.join("tmp");
    let reflink = fs::create_dir_all(&tmp_root).is_ok() && supports_reflink(&target, &tmp_root);
    // On non-CoW filesystems leave 1% headroom below max_disk so harvesting never pushes the disk
    // straight back into `free_disk`, which would otherwise thrash shared units and incremental caches.
    let harvest_limit = if !reflink && max_disk > 2.0 { max_disk - 1.0 } else { max_disk };
    if disk_percent(root, 0)? >= harvest_limit {
        return Ok(());
    }
    // Skip if a build is currently running; snapshot_after_build will retry when it finishes.
    if lock_target(&target, false).is_none() {
        return Ok(());
    }
    let compile_marker = slot_dir.join("last-compile");
    let compile_marker_before = mtime_ns(&compile_marker);
    let compile_before = last_compile_ns(slot_dir);
    let started = Instant::now();
    let units = extract_external_units(&target, worktree, common_dir);
    if units.is_empty() {
        let _ = fs::write(slot_dir.join("harvested-once"), "");
        return Ok(());
    }

    let rustc_bin = fs::read_to_string(slot_dir.join("last-rustc")).ok().map(|s| OsString::from(s.trim()));
    if let Some(rustc_fp) = last_rustc(&target).or_else(|| units.first().map(|u| u.rustc))
        && !toolchain_recorded(&shared_dir, rustc_fp)
        && let Some(vv) = rustc_vv_hash(rustc_bin.as_ref(), worktree)
    {
        let _ = record_toolchain(&shared_dir, &vv, rustc_fp);
    }

    let units_root = shared_dir.join("units");
    let all_roots = pool_roots().unwrap_or_default();
    let now_time = SystemTime::now();

    struct Pending {
        unit: ExternalUnit,
        dest_parent: PathBuf,
        dest: PathBuf,
        size: u64,
        replace: bool,
    }
    enum Existing {
        Missing,
        Legacy,
        Match(PathBuf),
        Diverged,
    }
    let read_shared_meta = |dir: &Path| -> Option<SharedUnitMeta> {
        fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice::<SharedUnitMeta>(&b).ok())
    };
    let mut by_fp: HashMap<u64, usize> = HashMap::with_capacity(units.len());
    for (i, u) in units.iter().enumerate() {
        by_fp.insert(u.fp, i);
    }
    let mut existing_states: Vec<Option<(PathBuf, PathBuf, Existing)>> = Vec::with_capacity(units.len());
    let mut diverged = vec![false; units.len()];
    for (i, unit) in units.iter().enumerate() {
        if mtime_ns(&compile_marker) != compile_marker_before {
            existing_states.resize_with(units.len(), || None);
            break;
        }
        let Some(origin_rel) = origin_subpath(&unit.origin) else {
            existing_states.push(None);
            continue;
        };
        let unit_name = format!(
            "{}__{}__{}__{}",
            encode_profile(&unit.profile_rel),
            unit.hash,
            unit.rustc,
            unit.fp
        );
        let dest_parent = units_root.join(&origin_rel);
        let dest = dest_parent.join(&unit_name);
        let state = if dest.exists() {
            match read_shared_meta(&dest) {
                Some(m) if m.version == SHARED_META_VERSION => {
                    if m.abi == unit.abi {
                        Existing::Match(dest.clone())
                    } else {
                        diverged[i] = true;
                        Existing::Diverged
                    }
                }
                _ => Existing::Legacy,
            }
        } else if !reflink {
            let mut found = Existing::Missing;
            for other in all_roots
                .iter()
                .map(|r| r.join(SHARED_DIR).join("units").join(&origin_rel).join(&unit_name))
                .filter(|p| p.exists())
            {
                if let Some(m) = read_shared_meta(&other)
                    && m.version == SHARED_META_VERSION
                {
                    if m.abi == unit.abi {
                        found = Existing::Match(other);
                    } else {
                        diverged[i] = true;
                        found = Existing::Diverged;
                    }
                    break;
                }
            }
            found
        } else {
            Existing::Missing
        };
        existing_states.push(Some((dest_parent, dest, state)));
    }
    // Never publish a unit whose transitive dependency has a different compiled ABI (SVH) than
    // the canonical copy already in `shared/`, or hydration could mix incompatible SVHs.
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..units.len() {
            if !diverged[i] && units[i].deps.iter().any(|fp| by_fp.get(fp).is_some_and(|&j| diverged[j])) {
                diverged[i] = true;
                changed = true;
            }
        }
    }

    let mut pending = vec![];
    let mut planned_extra = 0i64;
    for (i, (unit, slot_state)) in units.into_iter().zip(existing_states).enumerate() {
        if mtime_ns(&compile_marker) != compile_marker_before {
            break;
        }
        if diverged[i] {
            continue;
        }
        let Some((dest_parent, dest, state)) = slot_state else { continue };
        let replace = match state {
            Existing::Match(existing) => {
                if let Ok(d) = File::open(&existing) {
                    let _ = d.set_modified(now_time);
                }
                continue;
            }
            Existing::Diverged => continue,
            Existing::Legacy => true,
            Existing::Missing => false,
        };
        let profile_dir = target.join(&unit.profile_rel);
        let src_paths: Vec<PathBuf> = unit.files.iter().map(|rel| profile_dir.join(rel)).collect();
        let size = dir_size(&src_paths, &[]);
        if !reflink {
            if disk_percent(root, planned_extra + size as i64)? >= harvest_limit {
                continue;
            }
            planned_extra += size as i64;
        }
        pending.push(Pending { unit, dest_parent, dest, size, replace });
    }

    // Pre-create destination origin directories once so workers don't race on mkdir.
    for p in &pending {
        let _ = fs::create_dir_all(&p.dest_parent);
    }

    let published = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let aborted = AtomicBool::new(false);
    let next = AtomicUsize::new(0);
    let pid = std::process::id();
    let stamp = now_ns();
    let workers = io_workers(pending.len());

    let harvest_one = |idx: usize| {
        if aborted.load(Ordering::Relaxed) {
            return;
        }
        if mtime_ns(&compile_marker) != compile_marker_before {
            aborted.store(true, Ordering::Relaxed);
            return;
        }
        let p = &pending[idx];
        let profile_dir = target.join(&p.unit.profile_rel);
        let stage = tmp_root.join(format!("{pid}-{stamp}-{idx}"));
        let files_dir = stage.join("files");
        let meta = SharedUnitMeta {
            version: SHARED_META_VERSION,
            profile: p.unit.profile_rel.clone(),
            hash: p.unit.hash.clone(),
            rustc: p.unit.rustc,
            fp: p.unit.fp,
            abi: p.unit.abi,
            deps: p.unit.deps.clone(),
            dep_abis: p.unit.dep_abis.clone(),
        };
        let mut linked = HashMap::new();
        let copy_ok = (|| -> Result<()> {
            fs::create_dir_all(&files_dir)?;
            for rel in &p.unit.files {
                copy_tree_cow(&profile_dir.join(rel), &files_dir.join(rel), &mut linked)?;
            }
            fs::write(stage.join("meta.json"), serde_json::to_vec(&meta)?)?;
            Ok(())
        })();
        if copy_ok.is_err() || mtime_ns(&compile_marker) != compile_marker_before {
            let _ = fs::remove_dir_all(&stage);
            if mtime_ns(&compile_marker) != compile_marker_before {
                aborted.store(true, Ordering::Relaxed);
            }
            return;
        }
        if p.replace {
            let old = tmp_root.join(format!("old-{pid}-{stamp}-{idx}"));
            if fs::rename(&p.dest, &old).is_ok() {
                let _ = fs::remove_dir_all(&old);
            }
        }
        if fs::rename(&stage, &p.dest).is_ok() {
            published.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(p.size, Ordering::Relaxed);
        } else {
            let _ = fs::remove_dir_all(&stage);
            if let Ok(d) = File::open(&p.dest) {
                let _ = d.set_modified(now_time);
            }
        }
    };

    if workers <= 1 {
        for i in 0..pending.len() {
            harvest_one(i);
            if aborted.load(Ordering::Relaxed) {
                break;
            }
        }
    } else {
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    if aborted.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= pending.len() {
                        break;
                    }
                    harvest_one(i);
                });
            }
        });
    }
    let _ = fs::remove_dir(&tmp_root);

    if !aborted.load(Ordering::Relaxed) && last_compile_ns(slot_dir) == compile_before {
        let _ = fs::write(slot_dir.join("harvested-once"), "");
    }
    let published = published.into_inner();
    let bytes = bytes.into_inner();
    if published > 0 {
        let _ = bump_shared_generation(&shared_dir);
        record(slot_dir, Event::Harvest { units: published, bytes, ms: ms_since(started) });
        invalidate_idle_probe_caches(slot_dir);
    }
    Ok(())
}

/// Configured `build.target` triples from the worktree's `.cargo/config.toml` (if any).
fn configured_targets(worktree: &Path) -> Vec<String> {
    let mut out = vec![];
    for name in ["config.toml", "config"] {
        let Ok(text) = fs::read_to_string(worktree.join(".cargo").join(name)) else { continue };
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        let Some(target) = table.get("build").and_then(|b| b.get("target")) else { continue };
        if let Some(s) = target.as_str() {
            out.push(s.to_string());
        } else if let Some(arr) = target.as_array() {
            out.extend(arr.iter().filter_map(|v| v.as_str()).map(String::from));
        }
    }
    if let Ok(t) = std::env::var("CARGO_BUILD_TARGET")
        && !t.is_empty()
        && !out.contains(&t)
    {
        out.push(t);
    }
    out
}

/// Custom `[profile.<name>]` directory names defined in `<worktree>/Cargo.toml`.
fn configured_profiles(worktree: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(worktree.join("Cargo.toml")) else { return vec![] };
    let Ok(table) = text.parse::<toml::Table>() else { return vec![] };
    let Some(profiles) = table.get("profile").and_then(|v| v.as_table()) else { return vec![] };
    profiles
        .keys()
        .filter(|k| !matches!(k.as_str(), "dev" | "release" | "test" | "bench"))
        .cloned()
        .collect()
}

fn map_cargo_profile(name: &str) -> String {
    match name {
        "dev" | "test" => "debug".to_string(),
        "bench" => "release".to_string(),
        other => other.to_string(),
    }
}

/// Parse the profile dir (`debug`, `release`, `<custom>`, or `<triple>/<profile>`) from a cargo
/// command line argv slice.
fn parse_cargo_cmdline_profile(args: &[String], env_target: Option<&str>) -> Option<String> {
    let first = args.first()?;
    let bin = Path::new(first).file_name().and_then(|n| n.to_str())?;
    if !bin.starts_with("cargo") {
        return None;
    }
    let mut profile: Option<String> = None;
    let mut target: Option<String> = env_target.filter(|t| !t.is_empty()).map(String::from);
    let mut subcmd: Option<&str> = None;
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            break;
        }
        if a == "--release" || a == "-r" {
            profile = Some("release".to_string());
        } else if let Some(p) = a.strip_prefix("--profile=") {
            profile = Some(map_cargo_profile(p));
        } else if a == "--profile" && i + 1 < args.len() {
            i += 1;
            profile = Some(map_cargo_profile(&args[i]));
        } else if let Some(t) = a.strip_prefix("--target=") {
            target = Some(t.to_string());
        } else if a == "--target" && i + 1 < args.len() {
            i += 1;
            target = Some(args[i].clone());
        } else if subcmd.is_none() && !a.starts_with('-') && !a.starts_with('+') {
            subcmd = Some(a);
        }
        i += 1;
    }
    let prof = profile.unwrap_or_else(|| if subcmd == Some("bench") { "release" } else { "debug" }.to_string());
    Some(match target {
        Some(t) => format!("{t}/{prof}"),
        None => prof,
    })
}

/// Detect the active Cargo profile dir by inspecting immediate Cargo/shell ancestor command lines via `/proc`.
fn cargo_invocation_profile() -> Option<String> {
    let env_target = std::env::var("CARGO_BUILD_TARGET").ok();
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..3 {
        if pid <= 1 {
            break;
        }
        if let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) {
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            if let Some(p) = parse_cargo_cmdline_profile(&args, env_target.as_deref()) {
                return Some(p);
            }
            let bin = args.first().and_then(|s| Path::new(s).file_name()).and_then(|n| n.to_str());
            let is_wrapper = matches!(
                bin,
                Some("sh" | "bash" | "dash" | "zsh" | "fish" | "env" | "sccache" | "cargo-pool" | "cargo-pool-rustc-wrapper")
            );
            if !is_wrapper {
                break;
            }
        }
        let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else { break };
        let Some(ppid) = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<libc::pid_t>().ok())
        else {
            break;
        };
        pid = ppid;
    }
    None
}

/// Profiles that make sense to hydrate for this repository: host `debug`/`release`, custom
/// `[profile.<name>]` from `Cargo.toml`, any profile already present in this repo's slots,
/// `<target>/<profile>` for configured targets, and the active Cargo invocation's profile.
fn wanted_profiles(repo: &Repo, roots: &[PathBuf], active: Option<&str>) -> HashSet<String> {
    let mut base_profiles: Vec<String> = vec!["debug".to_string(), "release".to_string()];
    base_profiles.extend(configured_profiles(&repo.workspace));
    let mut wanted: HashSet<String> = base_profiles.iter().cloned().collect();
    for triple in configured_targets(&repo.workspace) {
        for p in &base_profiles {
            wanted.insert(format!("{triple}/{p}"));
        }
    }
    if let Some(a) = active {
        wanted.insert(a.to_string());
    }
    if let Some(pool_name) = repo.pool.file_name() {
        for root in roots {
            let slots_dir = root.join(pool_name).join("slots");
            for slot in load_slots(&slots_dir).unwrap_or_default() {
                let t = slot.target();
                for p in profile_dirs(&t) {
                    if let Ok(rel) = p.strip_prefix(&t) {
                        wanted.insert(rel.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    wanted
}

/// Resolve the `Fingerprint.rustc` u64 for `worktree` before or after its first build.
fn resolve_rustc_fp(worktree: &Path, slot_dir: &Path, rustc_bin: Option<&OsString>, roots: &[PathBuf]) -> Option<u64> {
    let toolchain = toolchain_hash(worktree);
    let same_rustc_bin = match rustc_bin {
        None => true,
        Some(bin) => fs::read(slot_dir.join("last-rustc")).ok().as_deref() == Some(bin.as_encoded_bytes()),
    };
    // Fast path: warm slot already recorded its Fingerprint.rustc for this exact compiler binary.
    if same_rustc_bin
        && let Some(stamp) = Stamp::read(slot_dir)
        && stamp.toolchain == toolchain
        && stamp.rustc.is_some()
    {
        return stamp.rustc;
    }
    if let Some(vv) = rustc_vv_hash(rustc_bin, worktree)
        && let Some(fp) = lookup_toolchain(roots, &vv)
    {
        return Some(fp);
    }
    if let Some(stamp) = Stamp::read(slot_dir)
        && stamp.toolchain == toolchain
        && stamp.rustc.is_some()
    {
        return stamp.rustc;
    }
    last_rustc(&slot_dir.join("target"))
}

/// Profiles required for a Cargo invocation targeting `active`: the profile itself, plus host
/// `debug` when `active` is a cross-target (`<triple>/<profile>`) for build scripts and proc macros.
fn active_profile_set(active: &str) -> HashSet<String> {
    let mut set = HashSet::from([active.to_string()]);
    if active.contains('/') {
        set.insert("debug".to_string());
    }
    set
}

fn read_hydrated_gen(slot_dir: &Path) -> BTreeMap<String, String> {
    let Ok(bytes) = fs::read(slot_dir.join("hydrated-gen")) else { return BTreeMap::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write_hydrated_gen(slot_dir: &Path, map: &BTreeMap<String, String>) {
    if let Ok(bytes) = serde_json::to_vec(map) {
        let _ = fs::write(slot_dir.join("hydrated-gen"), bytes);
    }
}

/// Hydrate missing `Cargo.lock`-pinned external dependency units from `<pool-root>/shared/`
/// into `slot_dir/target`, preserving any valid worktree snapshot mtimes.
fn hydrate_slot(repo: &Repo, slot_dir: &Path, rustc_bin: Option<&OsString>) -> Result<u64> {
    let roots = pool_roots()?;
    let generation = shared_generation(&roots);
    if generation == 0 {
        return Ok(0);
    }
    let Some(lock) = Lockfile::read(&repo.workspace) else { return Ok(0) };
    let active_profile = cargo_invocation_profile();
    let toolchain = toolchain_hash(&repo.workspace);
    let stamp_val = format!("{generation} {} {toolchain}", lock.hash);
    let profile_key = active_profile.as_deref().unwrap_or("*");
    let mut hydrated_map = read_hydrated_gen(slot_dir);
    if hydrated_map.get("*") == Some(&stamp_val) || hydrated_map.get(profile_key) == Some(&stamp_val) {
        return Ok(0);
    }
    let Some(rustc_fp) = resolve_rustc_fp(&repo.workspace, slot_dir, rustc_bin, &roots) else {
        return Ok(0);
    };

    let target = slot_dir.join("target");
    fs::create_dir_all(&target)?;
    let Some(_build_locks) = lock_target(&target, false) else { return Ok(0) };
    let started = Instant::now();

    let mut reflink_by_root: HashMap<PathBuf, bool> = HashMap::new();
    for r in &roots {
        reflink_by_root.insert(r.clone(), supports_reflink(r, &target));
    }
    let any_reflink = reflink_by_root.values().any(|&v| v);

    // On non-CoW filesystems (e.g. ext4), hydrate only the active Cargo profile on demand so
    // `debug` clippy/test slots aren't bloated with 30+ GB of unused release/custom profiles.
    let (wanted, partial_only) = match active_profile.as_deref() {
        Some(active) if !any_reflink => (active_profile_set(active), true),
        other => (wanted_profiles(repo, &roots, other), false),
    };
    if partial_only && !slot_dir.join("partial-profiles").exists() {
        let _ = fs::write(slot_dir.join("partial-profiles"), "");
    }

    // Snapshot existing units, fingerprints, and compiled ABIs already present in the slot.
    let mut existing_units: HashSet<(String, String)> = HashSet::new();
    let mut existing_abis: HashMap<u64, u64> = HashMap::new();
    for profile in profile_dirs(&target) {
        let Ok(rel) = profile.strip_prefix(&target) else { continue };
        let rel = rel.to_string_lossy().into_owned();
        let classic_artifacts = classic_deps_artifacts(&profile);
        for (hash, fingerprint, dir) in profile_units(&profile) {
            let Some(hash) = hash else { continue };
            let mut has_json = false;
            let mut unit_fps = vec![];
            for f in fs::read_dir(&fingerprint).into_iter().flatten().flatten() {
                let path = f.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                has_json = true;
                if let Some(fp) = fs::read_to_string(path.with_extension("")).ok().and_then(|s| u64::from_str_radix(s.trim(), 16).ok()) {
                    unit_fps.push(fp.swap_bytes());
                }
            }
            if has_json {
                if let Some(&first_fp) = unit_fps.first() {
                    let classic = if dir.is_none() { classic_artifacts.get(&hash).map(PathBuf::as_path) } else { None };
                    let abi = unit_abi_hash(dir.as_deref(), classic, first_fp);
                    for fp in unit_fps {
                        existing_abis.insert(fp, abi);
                    }
                }
                existing_units.insert((rel.clone(), hash));
            }
        }
    }

    let slot_dev = fs::metadata(&target).ok().map(|m| m.dev());
    // Prefer pool roots on the same filesystem so reflinks can be used when available.
    let mut ordered_roots = roots.clone();
    ordered_roots.sort_by_key(|r| fs::metadata(r).ok().map(|m| m.dev()) != slot_dev);

    struct Candidate {
        path: PathBuf,
        root: PathBuf,
        mtime: u64,
        meta: SharedUnitMeta,
        size: u64,
        reflink: bool,
    }
    // Keep the newest (or same-FS) candidate per `(profile, hash)`.
    let mut by_key: HashMap<(String, String), Candidate> = HashMap::new();
    let mut scan_origin = |origin_dir: &Path, root: &Path| {
        for e in fs::read_dir(origin_dir).into_iter().flatten().flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            let Some((profile, hash, rustc, _fp)) = name.to_str().and_then(parse_shared_unit_name) else {
                continue;
            };
            if rustc != rustc_fp || !wanted.contains(&profile) || existing_units.contains(&(profile.clone(), hash.clone())) {
                continue;
            }
            let path = e.path();
            let mtime = mtime_ns(&path);
            let key = (profile, hash);
            let same_fs = fs::metadata(root).ok().map(|m| m.dev()) == slot_dev;
            if let Some(prev) = by_key.get(&key) {
                let prev_same_fs = fs::metadata(&prev.root).ok().map(|m| m.dev()) == slot_dev;
                if (prev_same_fs, prev.mtime) >= (same_fs, mtime) {
                    continue;
                }
            }
            let Some(meta) = fs::read(path.join("meta.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<SharedUnitMeta>(&b).ok())
                .filter(|m| m.version == SHARED_META_VERSION)
            else {
                continue;
            };
            by_key.insert(key, Candidate { path, root: root.to_path_buf(), mtime, meta, size: 0, reflink: false });
        }
    };

    for root in &ordered_roots {
        let units_root = root.join(SHARED_DIR).join("units");
        if !units_root.is_dir() {
            continue;
        }
        let reg_root = units_root.join("reg");
        for pkg in &lock.packages {
            if safe_path_component(pkg) {
                let d = reg_root.join(pkg);
                if d.is_dir() {
                    scan_origin(&d, root);
                }
            }
        }
        if !lock.commits.is_empty() {
            for e in fs::read_dir(units_root.join("git")).into_iter().flatten().flatten() {
                let name = e.file_name();
                if let Some(rev) = name.to_str()
                    && lock.commits.iter().any(|c| c.starts_with(rev))
                    && e.file_type().is_ok_and(|t| t.is_dir())
                {
                    scan_origin(&e.path(), root);
                }
            }
        }
    }

    if by_key.is_empty() {
        hydrated_map.insert(if partial_only { profile_key } else { "*" }.to_string(), stamp_val);
        write_hydrated_gen(slot_dir, &hydrated_map);
        return Ok(0);
    }

    let retain_complete = |cands: &mut Vec<Candidate>| {
        loop {
            let mut avail: HashMap<u64, u64> = existing_abis.clone();
            for c in cands.iter() {
                avail.entry(c.meta.fp).or_insert(c.meta.abi);
            }
            let before = cands.len();
            cands.retain(|c| {
                c.meta.deps.iter().all(|d| avail.contains_key(d))
                    && c.meta.dep_abis.iter().all(|&(dep_fp, dep_abi)| avail.get(&dep_fp) == Some(&dep_abi))
            });
            if cands.len() == before {
                break;
            }
        }
    };

    // Keep only dependency-complete sets (every dep is already in the slot or also being hydrated).
    let mut candidates: Vec<Candidate> = by_key.into_values().collect();
    retain_complete(&mut candidates);
    if candidates.is_empty() {
        hydrated_map.insert(if partial_only { profile_key } else { "*" }.to_string(), stamp_val);
        write_hydrated_gen(slot_dir, &hydrated_map);
        return Ok(0);
    }

    // Disk budget check: on CoW filesystems reflinking takes ~0 bytes; on non-CoW filesystems
    // ensure we never push the slot's disk above `maxDiskPercent`. If all profiles together don't
    // fit, fall back to hydrating just the active profile (e.g. `debug`) so the current build wins.
    let slot_root = repo.pool.parent().unwrap();
    let max_disk = repo.max_disk_percent();
    for c in &mut candidates {
        c.reflink = *reflink_by_root.get(&c.root).unwrap_or(&false);
        c.size = dir_size([c.path.join("files")], &[]);
    }
    let extra_of = |cands: &[Candidate]| -> u64 {
        cands.iter().filter(|c| !c.reflink).map(|c| c.size).sum()
    };
    let mut narrowed = false;
    let extra = extra_of(&candidates);
    if extra > 0 {
        let protect_paths: Vec<&Path> = std::iter::once(slot_dir)
            .chain(candidates.iter().map(|c| c.path.as_path()))
            .collect();
        if !free_disk(slot_root, max_disk, extra, &protect_paths, Evict::Unheld)? {
            let focus = active_profile.as_deref().unwrap_or("debug");
            let before = candidates.len();
            candidates.retain(|c| c.meta.profile == focus);
            retain_complete(&mut candidates);
            if candidates.is_empty() || candidates.len() == before {
                return Ok(0);
            }
            let focus_extra = extra_of(&candidates);
            let focus_protect: Vec<&Path> = std::iter::once(slot_dir)
                .chain(candidates.iter().map(|c| c.path.as_path()))
                .collect();
            if focus_extra > 0 && !free_disk(slot_root, max_disk, focus_extra, &focus_protect, Evict::Unheld)? {
                return Ok(0);
            }
            narrowed = true;
        }
    }

    // Record valid snapshot timestamp and existing activity dir mtimes so hydrating into a warm
    // slot never falsely invalidates its snapshot for backdating.
    let valid_snap_at = valid_snapshot(slot_dir).map(|s| s.taken_at);
    let mut saved_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();
    if valid_snap_at.is_some() {
        for profile in profile_dirs(&target) {
            for sub in ["deps", ".fingerprint", "incremental", "build"] {
                let p = profile.join(sub);
                if let Ok(t) = fs::metadata(&p).and_then(|m| m.modified()) {
                    saved_mtimes.insert(p, t);
                }
            }
        }
    }

    // Pre-create shared parent directories per profile so worker threads only write disjoint paths.
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    for c in &candidates {
        let files_src = c.path.join("files");
        let dp = target.join(&c.meta.profile);
        for e in fs::read_dir(&files_src).into_iter().flatten().flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let sub = dp.join(e.file_name());
            if created_dirs.insert(sub.clone()) {
                let _ = fs::create_dir_all(&sub);
            }
            // New build-dir layout nests units under `build/<pkg>/<hash>/`.
            if e.file_name() == "build" {
                for pkg in fs::read_dir(e.path()).into_iter().flatten().flatten() {
                    if pkg.file_type().is_ok_and(|t| t.is_dir()) {
                        let pkg_dir = sub.join(pkg.file_name());
                        if created_dirs.insert(pkg_dir.clone()) {
                            let _ = fs::create_dir_all(&pkg_dir);
                        }
                    }
                }
            }
        }
    }

    let hydrated = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let now_time = SystemTime::now();
    let workers = io_workers(candidates.len());

    let hydrate_one = |c: &Candidate| {
        let files_src = c.path.join("files");
        if !files_src.is_dir() {
            return;
        }
        let dest_profile = target.join(&c.meta.profile);
        let mut linked = HashMap::new();
        if copy_tree_cow(&files_src, &dest_profile, &mut linked).is_ok() {
            hydrated.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(c.size, Ordering::Relaxed);
            if let Ok(d) = File::open(&c.path) {
                let _ = d.set_modified(now_time);
            }
        }
    };

    if workers <= 1 {
        for c in &candidates {
            hydrate_one(c);
        }
    } else {
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= candidates.len() {
                        break;
                    }
                    hydrate_one(&candidates[i]);
                });
            }
        });
    }

    if let Some(snap_at) = valid_snap_at {
        let cap = UNIX_EPOCH + Duration::from_nanos(snap_at.saturating_sub(1));
        for profile in profile_dirs(&target) {
            for sub in ["deps", ".fingerprint", "incremental", "build"] {
                let p = profile.join(sub);
                if p.is_dir() {
                    let restore = saved_mtimes.get(&p).copied().unwrap_or(cap);
                    if let Ok(f) = File::open(&p) {
                        let _ = f.set_modified(restore);
                    }
                }
            }
        }
    }

    if !narrowed {
        hydrated_map.insert(if partial_only { profile_key } else { "*" }.to_string(), stamp_val);
        write_hydrated_gen(slot_dir, &hydrated_map);
    }
    let hydrated = hydrated.into_inner();
    let bytes = bytes.into_inner();
    if hydrated > 0 {
        let ms = ms_since(started);
        eprintln!(
            "cargo-pool: hydrated {hydrated} shared dependency units ({}) in {}",
            human(bytes),
            dur(ms)
        );
        record(slot_dir, Event::Hydrate { units: hydrated, bytes, ms });
    }
    Ok(hydrated)
}

/// Seed a single missing profile (e.g. `release-with-debug`) into an already-warm slot on demand
/// from the closest warm non-busy slot that has it, preserving existing profiles and snapshots.
fn seed_missing_profile(repo: &Repo, slot_dir: &Path, active: &str) -> Result<bool> {
    let target = slot_dir.join("target");
    if target.join(active).exists() {
        return Ok(false);
    }
    let Some(_build_locks) = lock_target(&target, false) else { return Ok(false) };
    let sources = {
        let _pool = repo.lock()?;
        let mut others = Vec::new();
        for root in pool_roots()? {
            let pool = root.join(repo.pool.file_name().unwrap());
            others.extend(load_slots(&pool.join("slots"))?.into_iter().filter(|s| s.dir != slot_dir));
        }
        seed_sources(repo, &others)
    };
    let preferred_src = fs::read_to_string(slot_dir.join("partial-profiles"))
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty());
    let head = repo.head().unwrap_or_default();
    let mut ranked: Vec<_> = sources
        .into_iter()
        .filter(|(t, _)| t.join(active).is_dir())
        .map(|(t, h)| {
            let pref = preferred_src.as_deref() == Some(t.as_path());
            (!pref, distance(&repo.workspace, h.as_deref(), &head), t.parent().map_or(0, last_compile_ns), t)
        })
        .collect();
    ranked.sort_by_key(|(not_pref, d, act, _)| (*not_pref, *d, u64::MAX - act));
    if ranked.is_empty() {
        return Ok(false);
    }

    let only: HashSet<String> = active_profile_set(active)
        .into_iter()
        .filter(|p| !target.join(p).exists())
        .collect();
    if only.is_empty() {
        return Ok(false);
    }
    let lock = Lockfile::read(&repo.workspace);
    let toolchain = toolchain_hash(&repo.workspace);
    let root = repo.pool.parent().unwrap();
    let max_disk = repo.max_disk_percent();
    let valid_snap_at = valid_snapshot(slot_dir).map(|s| s.taken_at);
    let begun = Instant::now();

    for (_, _, _, src) in ranked {
        let from_main = repo.main_workspace.as_ref().is_some_and(|m| src == m.join("target"));
        let rustc = src
            .parent()
            .filter(|_| !from_main)
            .and_then(Stamp::read)
            .filter(|s| s.toolchain == toolchain)
            .and_then(|s| s.rustc);
        let src_compile = src.parent().map_or(0, last_compile_ns);
        let plan = seed_plan_for(&src, lock.as_ref(), rustc, Some(&only), false);
        if plan.parts.is_empty() {
            continue;
        }
        let size = dir_size(plan.parts.iter().map(|p| src.join(p)), &[]);
        let reflink = supports_reflink(&src, slot_dir);
        let extra = if reflink { 0 } else { size };
        let protect: Vec<&Path> = [src.parent(), Some(slot_dir)].into_iter().flatten().collect();
        if !free_disk(root, max_disk, extra, &protect, Evict::Unheld)? {
            continue;
        }
        let Some(_src_locks) = lock_target(&src, false) else { continue };
        let plan = if src.parent().map_or(0, last_compile_ns) == src_compile {
            plan
        } else {
            seed_plan_for(&src, lock.as_ref(), rustc, Some(&only), false)
        };
        if plan.parts.is_empty() {
            continue;
        }
        let tmp = slot_dir.join("target.seeding-profile");
        let _ = fs::remove_dir_all(&tmp);
        if !copy_seed_parts(&src, &tmp, &plan) {
            let _ = fs::remove_dir_all(&tmp);
            continue;
        }
        remove_saved_links(&tmp)?;
        for lock in cargo_locks(&tmp) {
            let _ = fs::remove_file(lock);
        }
        let _ = fs::remove_file(tmp.join(".rustc_info.json"));
        merge_target(&tmp, &target, Conflict::Destination)?;
        if let Some(snap_at) = valid_snap_at {
            let cap = UNIX_EPOCH + Duration::from_nanos(snap_at.saturating_sub(1));
            for prof in &only {
                let pdir = target.join(prof);
                for sub in ["deps", ".fingerprint", "incremental", "build"] {
                    let p = pdir.join(sub);
                    if p.is_dir()
                        && let Ok(f) = File::open(&p)
                    {
                        let _ = f.set_modified(cap);
                    }
                }
            }
        }
        let _ = invalidate_foreign_git_inputs(repo, &target);
        eprintln!(
            "cargo-pool: seeded profile {active} ({}) from {} in {}",
            human(size),
            src.display(),
            dur(ms_since(begun))
        );
        record(
            slot_dir,
            Event::Seed { bytes: Some(size), left_out: dir_size(&plan.left_out, &[]), busy: 0, no_room: 0, failed: 0, ms: ms_since(begun) },
        );
        return Ok(true);
    }
    Ok(false)
}

/// Hydrate newly published shared dependencies (or seed a missing profile on demand) into an
/// already-warm slot on Cargo's `rustc -vV` probe.
fn hydrate_on_probe(rustc_bin: &OsString) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let roots = pool_roots()?;
    let active_profile = cargo_invocation_profile();
    let generation = shared_generation(&roots);
    let Some(target) = cwd
        .ancestors()
        .filter_map(|a| fs::read_link(a.join("target")).ok())
        .find(|t| containing_pool_root(&roots, t).ok().flatten().is_some())
    else {
        return Ok(());
    };
    let Some(slot_dir) = target.parent() else { return Ok(()) };
    let needs_profile_seed = active_profile.as_deref().is_some_and(|p| !target.join(p).exists());
    if generation == 0 && !needs_profile_seed {
        return Ok(());
    }
    let Some(_seed_lock) = FileLock::acquire(&slot_dir.join("seed.lock"), false)? else { return Ok(()) };
    let meta: SlotMeta = serde_json::from_slice(&fs::read(slot_dir.join("slot.json"))?)?;
    let Some(lease) = meta.lease.filter(|l| !l.pending) else { return Ok(()) };
    let Ok(repo) = Repo::of_workspace(&lease.worktree) else { return Ok(()) };
    if let Some(active) = active_profile.as_deref()
        && !target.join(active).exists()
    {
        let _ = seed_missing_profile(&repo, slot_dir, active);
    }
    if generation > 0 {
        hydrate_slot(&repo, slot_dir, Some(rustc_bin))?;
    }
    Ok(())
}

fn backdate_enabled(repo: &Repo) -> bool {
    setting(&repo.common_dir, "CARGO_POOL_BACKDATE", "cargo-pool.backdate", true)
}

fn is_external_compile(rustc_args: &[OsString]) -> bool {
    rustc_args.iter().any(|a| {
        let b = a.as_encoded_bytes();
        contains_bytes(b, b"/registry/src/") || contains_bytes(b, b"/git/checkouts/")
    })
}

/// Called by the rustc wrapper for every compile into a slot: invalidate the slot's
/// snapshot and make sure a waiter will snapshot the worktree once the build ends.
fn note_pooled_compile(slot_dir: &Path, rustc_bin: &OsString, rustc_args: &[OsString]) -> Result<()> {
    let marker = OpenOptions::new().create(true).truncate(false).write(true).open(slot_dir.join("last-compile"))?;
    marker.set_modified(SystemTime::now())?;
    let rustc_path = slot_dir.join("last-rustc");
    let rustc_bytes = rustc_bin.as_encoded_bytes();
    if fs::read(&rustc_path).ok().as_deref() != Some(rustc_bytes) {
        let _ = fs::write(&rustc_path, rustc_bytes);
    }
    // One line per compile (`<ns> <ext>`); the snapshot waiter turns them into a `Build` event.
    let ext = u8::from(is_external_compile(rustc_args));
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(slot_dir.join("compiles"))
        .and_then(|mut f| f.write_all(format!("{} {ext}\n", now_ns()).as_bytes()));
    // A waiter holds this lock; don't start a second one.
    if FileLock::acquire(&slot_dir.join("snapshot.lock"), false)?.is_none() {
        return Ok(());
    }
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("snapshot-after-build")
        .arg(slot_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach: cargo must not wait for it, and it outlives this rustc.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// Wait until no build is running in the slot, then snapshot its worktree and harvest
/// finished external dependency units into `<pool-root>/shared/`. Retries if a build
/// started meanwhile, so the snapshot always reflects the state after the last one.
fn snapshot_after_build(slot_dir: &Path) -> Result<()> {
    let target = slot_dir.join("target");
    loop {
        let Some(_lock) = FileLock::acquire(&slot_dir.join("snapshot.lock"), false)? else { return Ok(()) };
        let mut saw_external: Option<bool> = None;
        let harvested_for = loop {
            for l in cargo_locks(&target) {
                drop(FileLock::acquire(&l, true)?);
            }
            if let Some(ext) = record_build(slot_dir) {
                saw_external = Some(saw_external.unwrap_or(false) || ext);
            }
            let meta: SlotMeta = serde_json::from_slice(&fs::read(slot_dir.join("slot.json"))?)?;
            let Some(lease) = meta.lease.filter(|l| !l.pending) else { return Ok(()) };
            if fs::read_link(lease.worktree.join("target")).ok().as_deref() != Some(target.as_path()) {
                return Ok(());
            }
            // Before the snapshot: deleting units touches the dirs that date the last compile.
            prune(&lease.worktree, slot_dir, true)?;
            let started = Instant::now();
            let snap = take_snapshot(&lease.worktree, load_snapshot(slot_dir).as_ref())?;
            if last_compile_ns(slot_dir) < snap.taken_at {
                save_snapshot(slot_dir, &snap)?;
                record(slot_dir, Event::Snapshot { files: snap.files.len() as u64, ms: ms_since(started) });
                break lease.worktree;
            }
        };
        let should_harvest = saw_external != Some(false) || !slot_dir.join("harvested-once").exists();
        if should_harvest
            && let Some(pool) = slot_dir.parent().and_then(Path::parent)
            && let Ok(common_str) = fs::read_to_string(pool.join("repo"))
        {
            let _ = harvest_shared(&harvested_for, Path::new(common_str.trim()), slot_dir);
        }
        // A slot seeded/hydrated for one profile clears `.rustc_info.json` between builds so
        // switching profiles later triggers the ~5ms `rustc -vV` probe to seed/hydrate on demand.
        if slot_dir.join("partial-profiles").exists() {
            let cache = target.join(".rustc_info.json");
            if fs::symlink_metadata(&cache).is_ok_and(|m| m.is_file())
                && let Some(_locks) = lock_target(&target, false)
            {
                let _ = fs::remove_file(&cache);
            }
        }
        drop(_lock);
        // A compile that saw our lock held didn't start a waiter: if one slipped in, go again.
        if load_snapshot(slot_dir).is_some_and(|s| s.taken_at > last_compile_ns(slot_dir)) {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// metrics: an append-only event log per pool (`<pool>/events.jsonl`), summarized by `stats`

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
enum Event {
    /// A worktree leased a slot: finding it, disk cleanup and linking (not `Prepare`).
    Attach { reused: bool, ms: u64 },
    /// A warm slot was fitted to its new worktree: backdating and fingerprint resets.
    Prepare { backdated: u64, files: u64, reset: u64, ms: u64 },
    /// A new slot was filled on its first cargo command; `bytes` is None if nothing was copied.
    /// `left_out` is the size of the units its worktree can't use at its Cargo.lock. The counts are
    /// the candidates passed over: a build was running in them, their copy wouldn't fit below the
    /// disk limit, or it failed.
    Seed {
        bytes: Option<u64>,
        #[serde(default)]
        left_out: u64,
        #[serde(default)]
        busy: u64,
        #[serde(default)]
        no_room: u64,
        #[serde(default)]
        failed: u64,
        ms: u64,
    },
    /// A cargo command compiled into the slot: from its first compile until it released the build lock.
    Build { units: u64, ms: u64 },
    /// Background snapshot of the worktree after a build.
    Snapshot { files: u64, ms: u64 },
    /// Slot deleted to free disk space, or by `gc --max-age-days`.
    Evict { bytes: u64 },
    /// The slot's incremental caches were deleted to free disk space.
    Trim { bytes: u64 },
    /// Build units the slot's worktree can't use at its Cargo.lock and toolchain were deleted, freeing `bytes`.
    Prune { units: u64, bytes: u64, ms: u64 },
    /// External dependency units published into the global shared cache.
    Harvest { units: u64, bytes: u64, ms: u64 },
    /// External dependency units hydrated from the global shared cache into the slot.
    Hydrate { units: u64, bytes: u64, ms: u64 },
}

#[derive(Serialize, Deserialize)]
struct Record {
    at: u64,
    slot: u32,
    #[serde(flatten)]
    event: Event,
}

/// Append an event for the slot at `slot_dir` to its pool's log. Metrics never fail an operation.
fn record(slot_dir: &Path, event: Event) {
    let Some(pool) = slot_dir.parent().and_then(Path::parent) else { return };
    let Some(slot) = slot_dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse().ok()) else { return };
    let Ok(mut line) = serde_json::to_vec(&Record { at: now(), slot, event }) else { return };
    line.push(b'\n');
    // A single O_APPEND write keeps lines from concurrent processes whole.
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pool.join("events.jsonl"))
        .and_then(|mut f| f.write_all(&line));
}

/// Turn the compiles logged by the rustc wrapper since the last call into a `Build` event.
/// Returns `None` if no compiles were recorded, or `Some(any_external)`.
fn record_build(slot_dir: &Path) -> Option<bool> {
    let taken = slot_dir.join("compiles.taken");
    if fs::rename(slot_dir.join("compiles"), &taken).is_err() {
        return None;
    }
    let mut starts = vec![];
    let mut any_external = false;
    for line in fs::read_to_string(&taken).unwrap_or_default().lines() {
        let mut parts = line.split_whitespace();
        let Some(ts) = parts.next().and_then(|s| s.parse::<u64>().ok()) else { continue };
        starts.push(ts);
        any_external |= parts.next() != Some("0");
    }
    let _ = fs::remove_file(&taken);
    if let Some(first) = starts.iter().min() {
        let ms = now_ns().saturating_sub(*first) / 1_000_000;
        record(slot_dir, Event::Build { units: starts.len() as u64, ms });
    }
    Some(any_external)
}

fn ms_since(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

// ---------------------------------------------------------------------------
// attach / detach

struct AttachOpts {
    seed: bool,
    force: bool,
    from_hook: bool,
    wait_for_build: bool,
    rustc_bin: Option<OsString>,
}

fn attach(path: &Path, opts: AttachOpts) -> Result<()> {
    let repos = Repo::discover(path)?;
    if repos.is_empty() && !opts.from_hook {
        println!("{} is not in a cargo workspace, nothing to do", path.display());
    }
    each_workspace(&repos, |repo| attach_workspace(repo, &opts))
}

/// Run `f` for every workspace: one failing doesn't stop the others.
fn each_workspace(repos: &[Repo], mut f: impl FnMut(&Repo) -> Result<()>) -> Result<()> {
    let mut failed = 0;
    for repo in repos {
        if let Err(e) = f(repo) {
            if repos.len() == 1 {
                return Err(e);
            }
            eprintln!("cargo-pool: error: {}: {e:#}", repo.workspace.display());
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} workspaces failed", repos.len());
    }
    Ok(())
}

fn attach_workspace(repo: &Repo, opts: &AttachOpts) -> Result<()> {
    let _attach_lock =
        attach_marker(&repo.workspace).map(|m| FileLock::acquire(&m.with_added_extension("lock"), true)).transpose()?;
    let link = repo.workspace.join("target");
    // A failed compiler probe leaves an empty directory here to prevent Cargo caching its failure.
    let _ = fs::remove_dir(link.join(".rustc_info.json"));
    let slots_dir = repo.slots_dir();
    match fs::symlink_metadata(&link) {
        Err(_) => {}
        Ok(m) if m.file_type().is_symlink() => {
            let dest = fs::read_link(&link)?;
            if dest.starts_with(&slots_dir) {
                let _lock = repo.lock()?;
                let held = load_slots(&repo.slots_dir())?
                    .into_iter()
                    .find(|s| s.target() == dest && s.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.workspace));
                if let Some(mut slot) = held {
                    if slot.meta.lease.as_ref().is_some_and(|l| l.pending) {
                        drop(_lock);
                        let _build_locks = lock_target(&slot.target(), opts.wait_for_build)
                            .ok_or_else(|| anyhow!("{} is busy building", link.display()))?;
                        let _lock = repo.lock()?;
                        let old = slot.saved_files(&repo.workspace).with_extension("attaching");
                        if old.exists() {
                            fs::remove_dir_all(old)?;
                        }
                        if slot.meta.adoption.is_some() {
                            save_snapshot(&slot.dir, &take_snapshot(&repo.workspace, None)?)?;
                        }
                        if !slot.meta.seed_pending {
                            prepare_worktree(repo, &slot.dir, opts.rustc_bin.as_ref())?;
                        }
                        slot.meta.lease.as_mut().unwrap().pending = false;
                        slot.meta.adoption = None;
                        slot.save()?;
                    }
                    if !opts.from_hook {
                        eprintln!("already attached: {} -> {}", link.display(), dest.display());
                    }
                    return Ok(());
                }
                // Our lease was stolen or lost; drop the dangling link and re-attach.
                fs::remove_file(&link)?;
            } else if opts.force {
                fs::remove_file(&link)?;
            } else {
                bail!("{} is a symlink to {}; use --force to replace it", link.display(), dest.display());
            }
        }
        Ok(m) if m.is_dir() => {
            if opts.force {
                let _pool = repo.lock()?;
                if load_slots(&slots_dir)?.iter().any(|s| {
                    s.meta.adoption.is_some()
                        && s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.workspace)
                }) {
                    bail!("unfinished adoption; rerun attach without --force before discarding its source");
                }
                fs::remove_dir_all(&link)?;
            }
        }
        Ok(_) => bail!("{} exists and is not a directory", link.display()),
    }

    // Worktrees often never build: from the hook, only take a slot on the first cargo
    // command (see `attach_on_first_use`), which needs the rustc wrapper.
    if opts.from_hook
        && !cargo_output(&link)
        && wrapper_installed()
        && let Some(marker) = attach_marker(&repo.workspace)
    {
        fs::write(marker, "")?;
        // An empty dir rather than nothing: other post-checkout hooks may fill a missing
        // `target` (e.g. by copying the main checkout's).
        fs::create_dir_all(&link)?;
        exclude_target(repo)?;
        warn_path_dependent_config(repo);
        eprintln!("cargo-pool: {} gets a pooled target dir on the first cargo command", link.display());
        return Ok(());
    }
    lease_slot(repo, opts.seed, opts.wait_for_build, opts.rustc_bin.as_ref())
}

fn cargo_output(target: &Path) -> bool {
    target.join("CACHEDIR.TAG").exists() || target.join(".rustc_info.json").exists() || !cargo_locks(target).is_empty()
}

#[derive(Clone, Copy)]
enum Conflict {
    Source,
    Destination,
}

/// Move files without overwriting either side. Renames preserve open log descriptors.
fn merge_target(src: &Path, dest: &Path, keep: Conflict) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if fs::read_dir(dest).is_ok_and(|mut d| d.next().is_none()) {
        fs::remove_dir(dest)?;
    }
    if !dest.exists() {
        fs::rename(src, dest)?;
        return Ok(());
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() && fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            merge_target(&entry.path(), &to, keep)?;
        } else {
            if fs::symlink_metadata(&to).is_ok() {
                if fs::read_link(entry.path()).ok().zip(fs::read_link(&to).ok()).is_some_and(|(a, b)| a == b) {
                    fs::remove_file(entry.path())?;
                    continue;
                }
                let saved = dest.join(format!("{}.cargo-pool-saved-{}", entry.file_name().to_string_lossy(), now_ns()));
                match keep {
                    Conflict::Source => fs::rename(&to, saved)?,
                    Conflict::Destination => {
                        fs::rename(entry.path(), saved)?;
                        continue;
                    }
                }
            }
            fs::rename(entry.path(), to)?;
        }
    }
    fs::remove_dir(src)?;
    Ok(())
}

/// Make links into `root` relative and relative links out of it absolute, so both keep
/// their referents when `root` moves.
fn preserve_link_targets(root: &Path) -> Result<()> {
    let real_root = fs::canonicalize(root.parent().unwrap())?.join(root.file_name().unwrap());
    let mut paths = vec![root.to_path_buf()];
    while let Some(from) = paths.pop() {
        if let Ok(referent) = fs::read_link(&from) {
            let dir = fs::canonicalize(from.parent().unwrap())?;
            let joined = dir.join(&referent);
            // Resolve the directories on the way, not the referent: it may be a link itself.
            let resolved = match (joined.parent(), joined.file_name()) {
                (Some(parent), Some(name)) => fs::canonicalize(parent).map(|p| p.join(name)),
                _ => fs::canonicalize(&joined),
            }
            .unwrap_or_else(|_| {
                let components: Vec<_> = joined.components().collect();
                let mut absolute = PathBuf::new();
                for (i, part) in components.iter().enumerate() {
                    match part {
                        std::path::Component::ParentDir => {
                            absolute.pop();
                        }
                        std::path::Component::CurDir => {}
                        _ => absolute.push(part),
                    }
                    if i + 1 < components.len()
                        && let Ok(real) = fs::canonicalize(&absolute)
                    {
                        absolute = real;
                    }
                }
                absolute
            });
            let preserved = if resolved.starts_with(&real_root) {
                let common = dir.components().zip(resolved.components()).take_while(|(a, b)| a == b).count();
                let up = std::iter::repeat_n(std::path::Component::ParentDir, dir.components().count() - common);
                let relative: PathBuf = up.chain(resolved.components().skip(common)).collect();
                if relative.as_os_str().is_empty() { PathBuf::from(".") } else { relative }
            } else if referent.is_relative() {
                resolved
            } else {
                continue;
            };
            if preserved != referent {
                let tmp = from.with_file_name(format!(".cargo-pool-link-{}", now_ns()));
                symlink(preserved, &tmp)?;
                fs::rename(tmp, &from)?;
            }
        } else if from.is_dir() {
            paths.extend(fs::read_dir(&from)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?);
        }
    }
    Ok(())
}

fn saved_link(path: &Path) -> bool {
    fs::read_link(path).is_ok_and(|p| p.components().any(|c| c.as_os_str() == "target.cargo-pool-files"))
}

fn remove_saved_links(target: &Path) -> Result<()> {
    let mut dirs = vec![target.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries {
            let entry = entry?;
            if saved_link(&entry.path()) {
                fs::remove_file(entry.path())?;
            } else if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    Ok(())
}

fn open_writers() -> HashSet<(u64, u64)> {
    let mut files = HashSet::new();
    for process in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if !process.file_name().as_encoded_bytes().iter().all(u8::is_ascii_digit)
            || !process.metadata().is_ok_and(|m| m.uid() == unsafe { libc::geteuid() })
        {
            continue;
        }
        for entry in fs::read_dir(process.path().join("fdinfo")).into_iter().flatten().flatten() {
            let flags = fs::read_to_string(entry.path()).ok().and_then(|s| {
                s.lines().find_map(|l| l.strip_prefix("flags:").and_then(|n| i32::from_str_radix(n.trim(), 8).ok()))
            });
            if flags.is_some_and(|f| f & libc::O_ACCMODE != libc::O_RDONLY)
                && let Ok(m) = fs::metadata(process.path().join("fd").join(entry.file_name()))
            {
                files.insert((m.dev(), m.ino()));
            }
        }
    }
    files
}

#[derive(Clone, Copy, PartialEq)]
enum TargetContents {
    Mixed,
    Cargo,
}

fn stray_files(
    target: &Path,
    contents: TargetContents,
    writers: &HashSet<(u64, u64)>,
    paths: &mut Vec<PathBuf>,
) -> Result<()> {
    let profile = target.join(".cargo-lock").exists() || target.join(".fingerprint").is_dir();
    let placeholder = contents == TargetContents::Mixed && !cargo_output(target) && !profile;
    let Ok(entries) = fs::read_dir(target) else { return Ok(()) };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        if placeholder {
            paths.push(path);
            continue;
        }
        if name == "CACHEDIR.TAG"
            || name == ".rustc_info.json"
            || name == ".rustdoc_fingerprint.json"
            || name == ".future-incompat-report.json"
            || name == ".cargo-lock"
        {
            continue;
        }
        if saved_link(&path) || fs::metadata(&path).is_ok_and(|m| writers.contains(&(m.dev(), m.ino()))) {
            paths.push(path);
            continue;
        }
        let artifact = path
            .extension()
            .is_some_and(|e| ["d", "rlib", "rmeta", "so", "dylib", "dll", "exe", "a", "o"].iter().any(|ext| e == *ext))
            || path.with_extension("d").is_file()
            || target.join(format!("{}.d", name.to_string_lossy())).is_file();
        let cargo = contents == TargetContents::Cargo
            || (profile && !entry.file_type()?.is_dir() && artifact)
            || name == "doc"
            || name == "package"
            || (profile && ["deps", ".fingerprint", "incremental", "build", "examples"].iter().any(|n| name == *n));
        if entry.file_type()?.is_dir()
            && (cargo || !cargo_locks(&path).is_empty() || path.join(".fingerprint").is_dir())
        {
            let contents = if cargo { TargetContents::Cargo } else { TargetContents::Mixed };
            stray_files(&path, contents, writers, paths)?;
        } else if !cargo {
            paths.push(path);
        }
    }
    Ok(())
}

fn adoption_source(target: &Path, strays: &[PathBuf]) -> Result<u64> {
    let mut paths = vec![target.to_path_buf()];
    let mut entries = BTreeMap::new();
    while let Some(path) = paths.pop() {
        if strays.contains(&path) {
            continue;
        }
        let m = fs::symlink_metadata(&path)?;
        let version = if m.is_dir() {
            paths.extend(fs::read_dir(&path)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?);
            [0; 6]
        } else {
            [m.ino(), m.len(), m.mtime() as u64, m.mtime_nsec() as u64, m.ctime() as u64, m.ctime_nsec() as u64]
        };
        entries.insert(path.strip_prefix(target)?.to_path_buf(), version);
    }
    Ok(entries.into_iter().fold(FNV_OFFSET, |h, (path, version)| {
        let h = fnv1a_update(fnv1a_update(h, path.as_os_str().as_encoded_bytes()), b"\0");
        version.into_iter().fold(h, |h, n| fnv1a_update(h, &n.to_le_bytes()))
    }))
}

/// Lease the best free slot (or a new one) to the worktree and link its `target` to it.
fn lease_slot(repo: &Repo, seed: bool, wait_for_build: bool, rustc_bin: Option<&OsString>) -> Result<()> {
    let started = Instant::now();
    let link = repo.workspace.join("target");
    let slots_dir = repo.slots_dir();
    let max_disk = repo.max_disk_percent();
    let (mut recovery, _recovery_lock) = {
        let _pool = repo.lock()?;
        let slot = load_slots(&slots_dir)?
            .into_iter()
            .find(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.workspace));
        let lock = match &slot {
            Some(s) => Some(
                FileLock::acquire(&s.dir.join("seed.lock"), false)?
                    .ok_or_else(|| anyhow!("slot {} is busy attaching", s.id))?,
            ),
            None => None,
        };
        (slot, lock)
    };
    let mut _build_locks =
        lock_target(&link, wait_for_build).ok_or_else(|| anyhow!("{} is busy building", link.display()))?;
    if let Some(slot) = &recovery {
        let old = slot.saved_files(&repo.workspace).with_extension("attaching");
        if fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir()) {
            _build_locks.extend(
                lock_target(&old, wait_for_build).ok_or_else(|| anyhow!("{} is busy building", old.display()))?,
            );
            merge_target(&old, &link, Conflict::Destination)?;
        }
    }
    let cross_device = fs::metadata(&link)
        .is_ok_and(|m| fs::metadata(repo.pool.parent().unwrap()).is_ok_and(|root| root.dev() != m.dev()));
    let mut strays = vec![];
    stray_files(&link, TargetContents::Mixed, &open_writers(), &mut strays)?;
    let mut ready = false;
    if let Some(slot) = &mut recovery {
        if let Some(Adoption::Ready { source }) = slot.meta.adoption {
            ready = !link.exists() || adoption_source(&link, &strays)? == source;
            if !ready {
                slot.meta.adoption = Some(Adoption::Copying);
                slot.save()?;
            }
        }
        let tmp = slot.dir.join("target.adopting");
        if !ready && tmp.exists() {
            fs::remove_dir_all(tmp)?;
        }
        if slot.meta.adoption == Some(Adoption::Copying) && slot.target().exists() {
            fs::remove_dir_all(slot.target())?;
        }
        if slot.meta.adoption == Some(Adoption::Copying) && !link.exists() {
            slot.meta.adoption = None;
            slot.meta.seed_pending = seed;
            slot.save()?;
        }
    }
    let adopting = recovery.as_ref().is_some_and(|s| s.meta.adoption.is_some()) || cargo_output(&link);
    let extra = if cross_device && adopting && !ready { dir_size([&link], &strays) } else { 0 };
    let device = fs::metadata(repo.pool.parent().unwrap())?.dev();
    for (dev, root) in pool_filesystems(&pool_roots()?) {
        let extra = if dev == device { extra } else { 0 };
        if !free_disk(root, max_disk, extra, &[], Evict::Any)? {
            if extra > 0 {
                bail!("not enough room below {max_disk}% to attach {}", link.display());
            }
            eprintln!(
                "cargo-pool: warning: the disk holding {} is still above {max_disk}% after cleaning up all pools",
                root.display()
            );
        }
    }
    let head = repo.head()?;
    let lease = |pending| Lease { worktree: repo.workspace.clone(), head: head.clone(), acquired_at: now(), pending };

    let lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    reclaim_stale(&mut slots)?;

    // Best free slot: closest commit, then most recently used.
    let pending =
        slots.iter().position(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.workspace));
    let mut pick = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| !adopting && s.state() == State::Free)
        .min_by_key(|(_, s)| {
            (distance(&repo.workspace, s.meta.last_head.as_deref(), &head), u64::MAX - s.meta.last_used)
        })
        .map(|(i, _)| i);

    // If no free slot is available and idle-lease expiry is configured, demote the least recently
    // active eligible idle held slot instead of creating and seeding a redundant slot.
    if !adopting && pending.is_none() && pick.is_none()
        && let Some(days) = repo.idle_lease_days()
    {
        let idle_cutoff = now().saturating_sub(days.saturating_mul(86400));
        let mut idle_indices: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.meta.seed_pending
                    && is_idle_held(s, idle_cutoff)
                    && s.meta.lease.as_ref().is_some_and(|l| l.worktree != repo.workspace)
            })
            .map(|(i, _)| i)
            .collect();
        idle_indices.sort_by_key(|&i| {
            let s = &slots[i];
            let slot_head = s.meta.lease.as_ref().map(|l| l.head.as_str()).or(s.meta.last_head.as_deref());
            (s.last_active(), distance(&repo.workspace, slot_head, &head))
        });
        for i in idle_indices {
            if let Some(worktree) = release_idle_slot(&mut slots[i], idle_cutoff)? {
                eprintln!("cargo-pool: released idle slot {} from {} for reuse", slots[i].id, worktree.display());
                pick = Some(i);
                break;
            }
        }
    }

    let slot_idx = match pending.or(pick) {
        Some(i) => {
            let s = &mut slots[i];
            let d = distance(&repo.workspace, s.meta.last_head.as_deref(), &head);
            s.meta.lease = Some(lease(true));
            if adopting {
                s.meta.seed_pending = false;
                if cross_device && s.meta.adoption.is_none() {
                    s.meta.adoption = Some(Adoption::Copying);
                }
            }
            fs::create_dir_all(s.target())?;
            if pending.is_none() && fs::read_dir(s.target())?.next().is_none() {
                s.meta.seed_pending = seed;
            }
            s.save()?;
            report_pick(s, d);
            drop(lock);
            i
        }
        None => {
            let id = slots.iter().map(|s| s.id + 1).max().unwrap_or(0);
            let dir = slots_dir.join(id.to_string());
            fs::create_dir_all(&dir)?;
            // Don't copy anything yet: many worktrees never build. The slot is seeded by the
            // rustc wrapper on the first cargo command (see `seed_on_first_use`).
            let meta = SlotMeta {
                lease: Some(lease(true)),
                seed_pending: seed && !adopting,
                adoption: (cross_device && adopting).then_some(Adoption::Copying),
                ..Default::default()
            };
            let slot = Slot { id, dir, meta };
            fs::create_dir_all(slot.target())?;
            slot.save()?;
            drop(lock);
            if seed && !adopting {
                eprintln!(
                    "cargo-pool: new slot {id}, filled from the closest warm target dir on the first cargo command"
                );
            }
            slots.push(slot);
            slots.len() - 1
        }
    };

    let slot = &mut slots[slot_idx];
    let _seed_lock = if recovery.is_none() { FileLock::acquire(&slot.dir.join("seed.lock"), true)? } else { None };
    let saved = slot.saved_files(&repo.workspace);
    preserve_link_targets(&link)?;
    exclude_target(repo)?;
    if cross_device {
        // Preserve lock inodes for Cargo commands already waiting, beyond this worktree's lifetime.
        let anchors = repo.common_dir.join("cargo-pool-locks");
        fs::create_dir_all(&anchors)?;
        for path in cargo_locks(&link) {
            let m = fs::metadata(&path)?;
            let anchor = anchors.join(format!("{:x}-{:x}", m.dev(), m.ino()));
            if !anchor.exists() {
                fs::hard_link(fs::canonicalize(&path)?, &anchor)
                    .context("retaining Cargo lock inode requires Git metadata on the target's filesystem")?;
            }
            if fs::read_link(&path).is_ok_and(|p| p == anchor) {
                continue;
            }
            let tmp = path.with_file_name(format!(".cargo-pool-link-{}", now_ns()));
            symlink(&anchor, &tmp)?;
            fs::rename(tmp, path)?;
        }
    }
    for path in &strays {
        let to = saved.join(path.strip_prefix(&link)?);
        if fs::read_link(path).is_ok_and(|p| p == to) {
            continue;
        }
        fs::create_dir_all(to.parent().unwrap())?;
        if fs::symlink_metadata(&to).is_ok() {
            fs::rename(
                &to,
                to.with_file_name(format!(
                    "{}.cargo-pool-saved-{}",
                    to.file_name().unwrap().to_string_lossy(),
                    now_ns()
                )),
            )?;
        }
        preserve_link_targets(path)?;
        fs::rename(path, &to)?;
        symlink(&to, path)?;
    }
    if slot.meta.adoption.is_some() {
        let tmp = slot.dir.join("target.adopting");
        if slot.meta.adoption == Some(Adoption::Copying) {
            let source = adoption_source(&link, &strays)?;
            let copied = Command::new("cp").args(["-a", "--reflink=auto", "--"]).arg(&link).arg(&tmp).status()?;
            if !copied.success() || adoption_source(&link, &strays)? != source {
                slot.discard_partial_copy()?;
                bail!("copying {} failed or source changed; original target kept", link.display());
            }
            slot.meta.adoption = Some(Adoption::Ready { source });
            slot.save()?;
        }
        if tmp.exists() {
            merge_target(&tmp, &slot.target(), Conflict::Source)?;
        }
    } else if !cross_device && fs::symlink_metadata(&link).is_ok_and(|m| m.is_dir()) {
        merge_target(&link, &slot.target(), Conflict::Source)?;
    }
    if saved.exists() {
        let tmp = slot.dir.join("target.strays");
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        let copied = Command::new("cp").args(["-as", "--"]).arg(&saved).arg(&tmp).status()?;
        if !copied.success() {
            bail!("linking stray files from {} failed", saved.display());
        }
        // Relocation carried a link per stray; a link to a saved directory would clash with its tree.
        remove_saved_links(&slot.target())?;
        merge_target(&tmp, &slot.target(), Conflict::Source)?;
    }
    if cross_device && link.exists() {
        let old = saved.with_extension("attaching");
        fs::create_dir_all(old.parent().unwrap())?;
        if fs::symlink_metadata(&old).is_ok_and(|m| m.is_symlink()) {
            fs::remove_file(&old)?;
        }
        // Retire the intact source before publishing the link; retries restore it if publication fails.
        fs::rename(&link, &old)?;
        symlink(slot.target(), &link).with_context(|| format!("creating {}", link.display()))?;
        fs::remove_dir_all(old)?;
    } else {
        symlink(slot.target(), &link).with_context(|| format!("creating {}", link.display()))?;
    }
    if adopting {
        save_snapshot(&slot.dir, &take_snapshot(&repo.workspace, None)?)?;
    }
    warn_path_dependent_config(repo);
    record(&slot.dir, Event::Attach { reused: pick.is_some(), ms: ms_since(started) });
    if !slot.meta.seed_pending {
        prepare_worktree(repo, &slot.dir, rustc_bin)?;
    }
    slot.meta.lease.as_mut().unwrap().pending = false;
    slot.meta.adoption = None;
    slot.save()?;
    if adopting {
        let _ = harvest_shared(&repo.workspace, &repo.common_dir, &slot.dir);
    }
    eprintln!("cargo-pool: {} -> slot {}", link.display(), slot.id);
    Ok(())
}

/// Marks a workspace that should lease a slot on its next cargo command. Lives in the
/// worktree's own git dir, so it disappears with the worktree.
fn attach_marker(workspace: &Path) -> Option<PathBuf> {
    // The workspace is the worktree's top level or a directory one level down.
    let (worktree, name) = if workspace.join(".git").exists() {
        (workspace, "cargo-pool-attach".to_string())
    } else {
        (workspace.parent()?, format!("cargo-pool-attach-{}", workspace.file_name()?.to_string_lossy()))
    };
    let dot_git = worktree.join(".git");
    let git_dir = match fs::read_to_string(&dot_git) {
        Ok(s) => worktree.join(s.strip_prefix("gitdir:")?.trim()),
        Err(_) if dot_git.is_dir() => dot_git,
        Err(_) => return None,
    };
    Some(git_dir.join(name))
}

/// Lease a slot for a worktree whose attach was deferred, when cargo first probes the
/// compiler (`rustc -vV`). Without a target dir cargo has no cached probe, and it probes
/// before touching the target dir, so the link is in place before cargo needs it.
fn attach_on_first_use(rustc_bin: &OsString) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(worktree) = cwd.ancestors().find(|a| a.join(".git").exists()) else { return Ok(()) };
    let Some((workspace, marker)) = cwd
        .ancestors()
        .take_while(|a| a.starts_with(worktree))
        .find_map(|a| Some((a, attach_marker(a).filter(|m| m.exists())?)))
    else {
        return Ok(());
    };
    // Parallel cargo commands wait for one attach.
    let _lock = FileLock::acquire(&marker, true)?;
    if !marker.exists() {
        return Ok(());
    }
    let opts = AttachOpts {
        seed: true,
        force: false,
        from_hook: false,
        wait_for_build: true,
        rustc_bin: Some(rustc_bin.clone()),
    };
    if let Err(e) = attach(workspace, opts) {
        // Cargo caches failed probes too. A directory prevents that write while keeping logs accessible.
        let cache = workspace.join("target/.rustc_info.json");
        let _ = fs::remove_file(&cache);
        fs::create_dir_all(&cache).context("preventing Cargo from caching a failed attach probe")?;
        return Err(e);
    }
    fs::remove_file(&marker)?;
    Ok(())
}

fn wrapper_installed() -> bool {
    cargo_config().is_ok_and(|c| fs::read_to_string(c).is_ok_and(|s| s.lines().any(is_our_wrapper_line)))
}

/// Make a warm slot's build state fit the worktree it was just given to.
fn prepare_worktree(repo: &Repo, slot_dir: &Path, rustc_bin: Option<&OsString>) -> Result<()> {
    let started = Instant::now();
    let (mut matched, mut total) = (0, 0);
    if backdate_enabled(repo)
        && let Some(snap) = valid_snapshot(slot_dir)
    {
        (matched, total) = backdate(&repo.workspace, &snap)?;
        eprintln!(
            "cargo-pool: {matched} of {total} files are unchanged since the slot's last build; only the other {} count as edits",
            total - matched
        );
    }
    // After backdating: this changes the target, which invalidates the snapshot.
    let reset = invalidate_foreign_git_inputs(repo, &slot_dir.join("target"))?;
    if reset > 0 {
        eprintln!("cargo-pool: {reset} build units recorded another checkout's git state (e.g. git_version!); they'll rebuild here");
    }
    prune(&repo.workspace, slot_dir, false)?;
    if let Err(e) = hydrate_slot(repo, slot_dir, rustc_bin) {
        eprintln!("cargo-pool: warning: hydrating shared dependencies failed: {e:#}");
    }
    record(
        slot_dir,
        Event::Prepare { backdated: matched as u64, files: total as u64, reset: reset as u64, ms: ms_since(started) },
    );
    Ok(())
}

/// Seed a pending slot when cargo first probes the compiler (`rustc -vV`) in its worktree.
/// Cargo runs that probe before it creates or locks anything in the target dir, and an
/// empty slot has no cached probe, so the first cargo command always comes through here.
fn seed_on_first_use(rustc_bin: &OsString) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let roots = pool_roots()?;
    let Some(target) = cwd
        .ancestors()
        .filter_map(|a| fs::read_link(a.join("target")).ok())
        .find(|t| containing_pool_root(&roots, t).ok().flatten().is_some())
    else {
        return Ok(());
    };
    let Some(slot_dir) = target.parent() else { return Ok(()) };
    let pending = |dir: &Path| {
        fs::read(dir.join("slot.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<SlotMeta>(&b).ok())
            .is_some_and(|m| m.seed_pending)
    };
    if !pending(slot_dir) {
        return Ok(());
    }
    // Parallel cargo commands (an agent's build and rust-analyzer) wait for one seeding.
    let _seed_lock = FileLock::acquire(&slot_dir.join("seed.lock"), true)?;
    if !pending(slot_dir) {
        return Ok(());
    }
    let id: u32 = slot_dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse().ok()).unwrap_or(0);
    let meta: SlotMeta = serde_json::from_slice(&fs::read(slot_dir.join("slot.json"))?)?;
    let Some(lease) = meta.lease.clone() else { return Ok(()) };
    let repo = Repo::of_workspace(&lease.worktree)?;
    let mut slot = Slot { id, dir: slot_dir.to_path_buf(), meta };
    // Another cargo already started building into it: too late to swap the dir out.
    let seeded_from = if cargo_locks(&slot.target()).is_empty() {
        let sources = {
            let _pool = repo.lock()?;
            let mut others = Vec::new();
            for root in pool_roots()? {
                let pool = root.join(repo.pool.file_name().unwrap());
                others.extend(load_slots(&pool.join("slots"))?.into_iter().filter(|s| s.dir != slot_dir));
            }
            seed_sources(&repo, &others)
        };
        let head = repo.head().unwrap_or(lease.head);
        let active = cargo_invocation_profile();
        seed(&repo, &slot, &head, sources, active.as_deref())?
    } else {
        None
    };
    fs::create_dir_all(slot.target())?;
    {
        let _pool = repo.lock()?;
        slot.meta = serde_json::from_slice(&fs::read(slot_dir.join("slot.json"))?)?;
        slot.meta.seed_pending = false;
        slot.meta.last_head = seeded_from.clone().or(slot.meta.last_head.take());
        slot.save()?;
    }
    if seeded_from.is_some() {
        prepare_worktree(&repo, slot_dir, Some(rustc_bin))?;
    } else if let Err(e) = hydrate_slot(&repo, slot_dir, Some(rustc_bin)) {
        eprintln!("cargo-pool: warning: hydrating shared dependencies failed: {e:#}");
    }
    Ok(())
}

/// `target.<triple>.linker` entries in the worktree's own cargo config that are relative
/// paths. Cargo resolves those inside each checkout and hashes the resolved path into
/// every crate's fingerprint, so a shared target dir gets fully rebuilt per worktree.
fn path_dependent_linkers(worktree: &Path) -> Vec<(String, String)> {
    let mut out = vec![];
    for name in ["config.toml", "config"] {
        let Ok(text) = fs::read_to_string(worktree.join(".cargo").join(name)) else { continue };
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        let Some(targets) = table.get("target").and_then(|t| t.as_table()) else { continue };
        for (triple, t) in targets {
            if let Some(linker) = t.get("linker").and_then(|l| l.as_str())
                && linker.contains('/')
                && !Path::new(linker).is_absolute()
            {
                out.push((triple.clone(), linker.to_string()));
            }
        }
    }
    out
}

/// Other worktree-path-dependent settings in `.cargo/config.toml`: `[env]` entries with
/// `relative = true`, and relative `build.rustc` / `build.rustc-wrapper` / `build.rustc-workspace-wrapper`.
fn path_dependent_settings(worktree: &Path) -> Vec<(String, String)> {
    let mut out = vec![];
    for name in ["config.toml", "config"] {
        let Ok(text) = fs::read_to_string(worktree.join(".cargo").join(name)) else { continue };
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        if let Some(env_tbl) = table.get("env").and_then(|e| e.as_table()) {
            for (k, v) in env_tbl {
                if let Some(t) = v.as_table()
                    && t.get("relative").and_then(|r| r.as_bool()) == Some(true)
                    && let Some(val) = t.get("value").and_then(|v| v.as_str())
                {
                    out.push((format!("env.{k}"), val.to_string()));
                }
            }
        }
        if let Some(build) = table.get("build").and_then(|b| b.as_table()) {
            for key in ["rustc", "rustc-wrapper", "rustc-workspace-wrapper"] {
                if let Some(val) = build.get(key).and_then(|v| v.as_str())
                    && val.contains('/')
                    && !Path::new(val).is_absolute()
                {
                    out.push((format!("build.{key}"), val.to_string()));
                }
            }
        }
    }
    out
}

fn warn_path_dependent_config(repo: &Repo) {
    for (triple, linker) in path_dependent_linkers(&repo.workspace) {
        eprintln!(
            "cargo-pool: warning: .cargo/config.toml sets target.{triple}.linker = \"{linker}\", a relative path.\n\
             cargo-pool:   Cargo hashes the resolved linker path into every crate's fingerprint, so each\n\
             cargo-pool:   worktree recompiles everything even with a shared target dir."
        );
        if let (false, Some(main)) = (triple.starts_with("cfg("), &repo.main_workspace) {
            let var = format!("CARGO_TARGET_{}_LINKER", triple.to_uppercase().replace(['-', '.'], "_"));
            eprintln!(
                "cargo-pool:   Fix: point it at a path that's the same in every checkout, e.g.\n\
                 cargo-pool:     export {var}={}",
                main.join(&linker).display()
            );
        }
    }
    for (setting, value) in path_dependent_settings(&repo.workspace) {
        eprintln!(
            "cargo-pool: warning: .cargo/config.toml sets {setting} = \"{value}\" relative to the worktree.\n\
             cargo-pool:   Cargo resolves this per checkout, which invalidates fingerprints across worktrees."
        );
    }
}

fn report_pick(s: &Slot, d: u64) {
    let built = match (&s.meta.last_head, d) {
        (None, _) => "unknown commit".to_string(),
        (Some(h), u64::MAX) => format!("{} (unrelated)", short(h)),
        (Some(h), d) => format!("{} ({d} commits away)", short(h)),
    };
    eprintln!("cargo-pool: reusing slot {} last built at {built}", s.id);
}

/// Candidate target dirs to copy into a new slot: every slot plus the main checkout's.
fn seed_sources(repo: &Repo, slots: &[Slot]) -> Vec<(PathBuf, Option<String>)> {
    let mut out: Vec<_> = slots
        .iter()
        .filter(|s| !s.meta.lease.as_ref().is_some_and(|l| l.pending))
        .map(|s| {
            let head = match &s.meta.lease {
                Some(l) => git(&l.worktree, &["rev-parse", "HEAD"]).ok().or(Some(l.head.clone())),
                None => s.meta.last_head.clone(),
            };
            (s.target(), head)
        })
        .collect();
    if let Some(main) = &repo.main_workspace {
        let t = main.join("target");
        if fs::symlink_metadata(&t).is_ok_and(|m| m.is_dir()) {
            out.push((t, git(main, &["rev-parse", "HEAD"]).ok()));
        }
    }
    out.retain(|(t, _)| fs::read_dir(t).is_ok_and(|mut d| d.next().is_some()));
    out
}

/// What a seed copies out of a target dir, relative to it: everything (or only the profiles in
/// `only_profiles`) except the `incremental/` caches and rust-analyzer `flycheck*` dirs.
fn seed_parts(target: &Path, only_profiles: Option<&HashSet<String>>, include_top_level: bool) -> Vec<PathBuf> {
    fn walk(
        dir: &Path,
        rel: &Path,
        only_profiles: Option<&HashSet<String>>,
        include_top_level: bool,
        out: &mut Vec<PathBuf>,
    ) {
        let profile = is_profile(dir);
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let (path, rel) = (entry.path(), rel.join(entry.file_name()));
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if entry.file_name() == ".rustc_info.json" && (is_dir || only_profiles.is_some()) {
                continue;
            }
            if !profile && is_dir && entry.file_name().to_str().is_some_and(|n| n.starts_with("flycheck")) {
                continue;
            }
            if profile && is_dir && entry.file_name() == "incremental" {
                continue;
            }
            let holds_profile =
                || is_profile(&path) || fs::read_dir(&path).into_iter().flatten().flatten().any(|e| is_profile(&e.path()));
            if !profile && is_dir && holds_profile() {
                if let Some(only) = only_profiles {
                    let rel_str = rel.to_string_lossy();
                    if is_profile(&path) {
                        if !only.contains(rel_str.as_ref()) {
                            continue;
                        }
                    } else {
                        let prefix = format!("{rel_str}/");
                        if !only.iter().any(|p| p.starts_with(&prefix)) {
                            continue;
                        }
                    }
                }
                walk(&path, &rel, only_profiles, include_top_level, out);
            } else if profile || include_top_level {
                out.push(rel);
            }
        }
    }
    let mut out = vec![];
    walk(target, Path::new(""), only_profiles, include_top_level, &mut out);
    out
}

/// What a seed copies out of `src` for a worktree pinned to `lock` and built by `rustc`:
/// `seed_parts`, with each directory holding units the worktree can't use replaced by its other
/// entries (recursively: `build/` and new-layout `build/<pkg>/`).
struct SeedPlan {
    parts: Vec<PathBuf>,
    /// Directories replaced by their entries, relative to `src`.
    split: Vec<PathBuf>,
    units: usize,
    left_out: Vec<PathBuf>,
}

fn seed_plan_for(
    src: &Path,
    lock: Option<&Lockfile>,
    rustc: Option<u64>,
    only_profiles: Option<&HashSet<String>>,
    include_top_level: bool,
) -> SeedPlan {
    let (units, left_out) = lock.map_or((0, vec![]), |l| unusable_units_for(src, l, rustc, only_profiles));
    let dropped: HashSet<&Path> = left_out.iter().map(PathBuf::as_path).collect();
    let holding: HashSet<&Path> = left_out.iter().flat_map(|p| p.ancestors().skip(1)).collect();
    let (mut parts, mut split) = (vec![], vec![]);
    let mut stack: Vec<PathBuf> = seed_parts(src, only_profiles, include_top_level).into_iter().rev().collect();
    while let Some(part) = stack.pop() {
        let dir = src.join(&part);
        if dropped.contains(dir.as_path()) {
            continue;
        }
        if !holding.contains(dir.as_path()) {
            parts.push(part);
            continue;
        }
        let entries: Vec<_> = fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| part.join(e.file_name())).collect();
        stack.extend(entries.into_iter().rev());
        split.push(part);
    }
    SeedPlan { parts, split, units, left_out }
}

fn copy_seed_parts(src: &Path, tmp: &Path, plan: &SeedPlan) -> bool {
    // As few cp runs as the kernel's argument limit (which counts the environment too)
    // allows: cargo's hard links (`<profile>/<bin>` to `deps/`) only survive within one.
    let env: usize = std::env::vars_os().map(|(k, v)| k.len() + v.len() + 2 + size_of::<usize>()).sum();
    let room = (unsafe { libc::sysconf(libc::_SC_ARG_MAX) } as usize)
        .saturating_sub(env + tmp.as_os_str().len() + (64 << 10));
    let mut runs = vec![];
    let (mut start, mut used) = (0, 0);
    for (i, part) in plan.parts.iter().enumerate() {
        let cost = part.as_os_str().len() + 1 + size_of::<usize>();
        if used + cost > room && i > start {
            runs.push(&plan.parts[start..i]);
            (start, used) = (i, 0);
        }
        used += cost;
    }
    runs.push(&plan.parts[start..]);
    let copied = fs::create_dir(tmp).is_ok()
        && runs.iter().all(|run| {
            Command::new("cp")
                .current_dir(src)
                .args(["-a", "--reflink=auto", "--parents", "--"])
                .args(*run)
                .arg(tmp)
                .status()
                .is_ok_and(|s| s.success())
        });
    if !copied {
        return false;
    }
    // Filling a directory entry by entry gives it a new mtime, which would count as a
    // compile and invalidate the snapshot carried over below.
    for dir in &plan.split {
        if let (Ok(t), Ok(f)) = (fs::metadata(src.join(dir)).and_then(|m| m.modified()), File::open(tmp.join(dir))) {
            let _ = f.set_modified(t);
        }
    }
    true
}

/// Copy the closest non-busy target dir into the slot. Returns the commit it was built for.
fn seed(
    repo: &Repo,
    slot: &Slot,
    head: &str,
    sources: Vec<(PathBuf, Option<String>)>,
    active_profile: Option<&str>,
) -> Result<Option<String>> {
    let mut ranked: Vec<_> = sources
        .into_iter()
        .map(|(t, h)| {
            let missing_active = active_profile.is_some_and(|p| !t.join(p).is_dir());
            (missing_active, distance(&repo.workspace, h.as_deref(), head), t.parent().map_or(0, last_compile_ns), t, h)
        })
        .collect();
    ranked.sort_by_key(|(missing, d, act, ..)| (*missing, *d, u64::MAX - act));
    let begun = Instant::now();

    let max_disk = repo.max_disk_percent();
    // What an interrupted seed left behind would count against the copy's room.
    let tmp = slot.dir.join("target.seeding");
    let _ = fs::remove_dir_all(&tmp);
    let lock = Lockfile::read(&repo.workspace);
    let toolchain = toolchain_hash(&repo.workspace);
    let root = repo.pool.parent().unwrap();
    // What a copy may take below the limit once free_disk has freed all it may for a seed, worked
    // out when the first copy doesn't fit: bigger candidates aren't worth another scan, and none
    // is worth sizing if there's no room at all.
    let mut disk_room: Option<i64> = None;
    let (mut busy, mut no_room, mut failed) = (0, 0, 0);
    for (_, d, _, src, h) in ranked {
        if disk_room.is_some_and(|r| r <= 0) {
            no_room += 1;
            continue;
        }
        // The receiver hasn't built yet: its compiler is the source slot's if their toolchain
        // files match. The main checkout's target has no stamp.
        let from_main = repo.main_workspace.as_ref().is_some_and(|m| src == m.join("target"));
        let rustc = src
            .parent()
            .filter(|_| !from_main)
            .and_then(Stamp::read)
            .filter(|s| s.toolchain == toolchain)
            .and_then(|s| s.rustc);
        let reflink = supports_reflink(&src, &slot.dir);
        let only_profiles = if !reflink
            && let Some(active) = active_profile
            && src.join(active).is_dir()
        {
            Some(active_profile_set(active))
        } else {
            None
        };
        // Make room for the copy first; never let seeding push the disk over the limit.
        let src_compile = src.parent().map_or(0, last_compile_ns);
        let plan = seed_plan_for(&src, lock.as_ref(), rustc, only_profiles.as_ref(), true);
        if plan.parts.is_empty() {
            continue; // evicted since it was listed
        }
        let size = dir_size(plan.parts.iter().map(|p| src.join(p)), &[]);
        let extra = if reflink { 0 } else { size };
        let protect: Vec<&Path> = [src.parent(), Some(slot.dir.as_path())].into_iter().flatten().collect();
        if (extra > 0 && disk_room.is_some_and(|r| extra as i64 > r))
            || !free_disk(root, max_disk, extra, &protect, Evict::Unheld)?
        {
            eprintln!(
                "cargo-pool: copying {} ({}) would push the disk above {max_disk}%, skipping it",
                src.display(),
                human(size)
            );
            if disk_room.is_none() {
                let (used, avail) = disk_usage(root)?;
                let below_limit = ((used + avail) as f64 * max_disk / 100.0) as i64 - used;
                let candidates = reclaim_candidates(root, &[&slot.dir], Evict::Unheld)?;
                let shared = shared_reclaim_candidates(root, &[&slot.dir])?;
                disk_room = Some(below_limit + reclaimable(&candidates, &shared, |_| Ok(false))? as i64);
            }
            no_room += 1;
            continue;
        }
        // Hold cargo's build locks so the source isn't modified mid-copy, and reuse the plan if
        // nothing compiled into the source since we sized it.
        let Some(_locks) = lock_target(&src, false) else {
            eprintln!("cargo-pool: {} is busy building, trying next candidate", src.display());
            busy += 1;
            continue;
        };
        let plan = if src.parent().map_or(0, last_compile_ns) == src_compile {
            plan
        } else {
            seed_plan_for(&src, lock.as_ref(), rustc, only_profiles.as_ref(), true)
        };
        if plan.parts.is_empty() {
            continue; // gone since it was sized
        }
        let dist = if d == u64::MAX { "unrelated".into() } else { format!("{d} commits away") };
        eprintln!("cargo-pool: seeding slot {} from {} ({dist}), this can take a while...", slot.id, src.display());
        let started = SystemTime::now();
        if !copy_seed_parts(&src, &tmp, &plan) {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!("cargo-pool: copy failed, trying next candidate");
            failed += 1;
            continue;
        }
        let left_out = dir_size(&plan.left_out, &[]);
        remove_saved_links(&tmp)?;
        for lock in cargo_locks(&tmp) {
            fs::remove_file(lock)?;
        }
        merge_target(&tmp, &slot.target(), Conflict::Destination)?;
        if only_profiles.is_some() {
            let _ = fs::write(slot.dir.join("partial-profiles"), src.as_os_str().as_encoded_bytes());
        }
        // Carry over the file snapshot that matches this target, so the new worktree can
        // be backdated: the source slot's (if still valid), or one of the main checkout.
        let snap = match (&repo.main_workspace, src.parent()) {
            (Some(main), _) if src == main.join("target") => take_snapshot(main, None).ok(),
            (_, Some(src_slot)) => valid_snapshot(src_slot),
            _ => None,
        };
        if let Some(snap) = snap {
            save_snapshot(&slot.dir, &snap)?;
        }
        if let Some(lock) = &lock {
            Stamp { lock: lock.hash.clone(), toolchain: toolchain.clone(), rustc }.write(&slot.dir)?;
        }
        let secs = started.elapsed().unwrap_or(Duration::ZERO).as_secs();
        eprintln!("cargo-pool: seeded in {secs}s");
        if plan.units > 0 {
            eprintln!(
                "cargo-pool: left out {} build units ({}) this worktree can't use at its Cargo.lock and toolchain",
                plan.units,
                human(left_out)
            );
        }
        record(&slot.dir, Event::Seed { bytes: Some(size), left_out, busy, no_room, failed, ms: ms_since(begun) });
        return Ok(h);
    }
    eprintln!("cargo-pool: no seed available, slot {} starts empty", slot.id);
    record(&slot.dir, Event::Seed { bytes: None, left_out: 0, busy, no_room, failed, ms: ms_since(begun) });
    Ok(None)
}

/// `target/` patterns in .gitignore don't match symlinks, so exclude the link explicitly.
fn exclude_target(repo: &Repo) -> Result<()> {
    let info = repo.common_dir.join("info");
    let exclude = info.join("exclude");
    let current = fs::read_to_string(&exclude).unwrap_or_default();
    let rel = repo.workspace.strip_prefix(&repo.worktree)?;
    let patterns = ["target", "target.cargo-pool-files/"].map(|p| format!("/{}", rel.join(p).display()));
    if patterns.iter().all(|pattern| current.lines().any(|l| l.trim() == pattern)) {
        return Ok(());
    }
    fs::create_dir_all(&info)?;
    let mut f = OpenOptions::new().create(true).append(true).open(&exclude)?;
    if !current.is_empty() && !current.ends_with('\n') {
        writeln!(f)?;
    }
    for pattern in patterns {
        if !current.lines().any(|l| l.trim() == pattern) {
            writeln!(f, "{pattern}")?;
        }
    }
    Ok(())
}

fn detach(path: &Path) -> Result<()> {
    let repos = Repo::discover(path)?;
    if repos.is_empty() {
        bail!("{} is not in a cargo workspace", path.display());
    }
    each_workspace(&repos, detach_workspace)
}

fn detach_workspace(repo: &Repo) -> Result<()> {
    let _attach_lock =
        attach_marker(&repo.workspace).map(|m| FileLock::acquire(&m.with_added_extension("lock"), true)).transpose()?;
    let link = repo.workspace.join("target");
    let _ = fs::remove_dir(link.join(".rustc_info.json"));
    {
        let _pool = repo.lock()?;
        if let Some(mut slot) = load_slots(&repo.slots_dir())?
            .into_iter()
            .find(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.workspace))
        {
            if slot.meta.adoption == Some(Adoption::Copying)
                && !fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink())
            {
                let _seed_lock = FileLock::acquire(&slot.dir.join("seed.lock"), false)?
                    .ok_or_else(|| anyhow!("slot {} is busy attaching", slot.id))?;
                slot.release(None)?;
                slot.save()?;
                if let Some(marker) = attach_marker(&repo.workspace).filter(|m| m.exists()) {
                    fs::remove_file(marker)?;
                }
                println!("cargo-pool: cancelled adoption; original target kept at {}", link.display());
                return Ok(());
            }
            bail!("{} has an unfinished attach; rerun attach before detaching", repo.workspace.display());
        }
    }
    if let Some(marker) = attach_marker(&repo.workspace).filter(|m| m.exists()) {
        fs::remove_file(marker)?;
        if !fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()) {
            let _ = fs::remove_dir(&link);
            println!("cargo-pool: {} won't be attached on its first build anymore", repo.workspace.display());
            return Ok(());
        }
    }
    let dest = fs::read_link(&link).map_err(|_| anyhow!("{} is not attached to a pool", link.display()))?;
    let _lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    let slot = slots
        .iter_mut()
        .find(|s| s.target() == dest)
        .ok_or_else(|| anyhow!("{} points to {}, which is not a slot of this pool", link.display(), dest.display()))?;
    fs::remove_file(&link)?;
    if slot.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.workspace) {
        slot.release(repo.head().ok())?;
        slot.save()?;
    }
    println!("cargo-pool: released slot {}", slot.id);
    Ok(())
}

// ---------------------------------------------------------------------------
// status / gc

fn short(h: &str) -> &str {
    &h[..h.len().min(10)]
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        _ if t == 0 => "never".into(),
        0..120 => format!("{d}s ago"),
        120..7200 => format!("{}m ago", d / 60),
        7200..172800 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn du(p: &Path) -> String {
    Command::new("du")
        .args(["-sh", "--"])
        .arg(p)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().map(String::from))
        .unwrap_or_else(|| "?".into())
}

fn shared_unit_count(root: &Path) -> usize {
    let units = root.join(SHARED_DIR).join("units");
    ["reg", "git"]
        .into_iter()
        .flat_map(|k| fs::read_dir(units.join(k)).into_iter().flatten().flatten())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|e| fs::read_dir(e.path()).into_iter().flatten().flatten())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count()
}

fn status(size: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repos = Repo::discover(&cwd).unwrap_or_default();
    if repos.is_empty() {
        for root in pool_roots()? {
            println!("pools in {}:", root.display());
            for e in fs::read_dir(&root).into_iter().flatten().flatten() {
                if e.file_name() == SHARED_DIR || !e.path().join("slots").is_dir() {
                    continue;
                }
                let repo = fs::read_to_string(e.path().join("repo")).unwrap_or_default();
                let n = fs::read_dir(e.path().join("slots")).map_or(0, |d| d.count());
                println!("  {:<40} {n} slots  {repo}", e.file_name().to_string_lossy());
            }
            let shared_n = shared_unit_count(&root);
            if shared_n > 0 {
                let sz = if size { format!("  {}", du(&root.join(SHARED_DIR))) } else { String::new() };
                println!("  {:<40} {shared_n} units{sz}", "shared dependency cache");
            }
        }
        return Ok(());
    }
    let roots = pool_roots()?;
    for (repo, root) in repos.iter().flat_map(|r| roots.iter().map(move |root| (r, root))) {
        let repo = Repo { pool: fs::canonicalize(root)?.join(repo.pool.file_name().unwrap()), ..repo.clone() };
        let _lock = repo.lock()?;
        let mut slots = load_slots(&repo.slots_dir())?;
        reclaim_stale(&mut slots)?;
        let head = repo.head().ok();
        println!(
            "pool {} (disk at {:.1}%, limit {}%)",
            repo.pool.display(),
            disk_percent(&repo.pool, 0)?,
            repo.max_disk_percent()
        );
        if slots.is_empty() {
            println!("  (no slots yet)");
        }
        for s in &slots {
            let built = s.meta.lease.as_ref().map(|l| l.head.as_str()).or(s.meta.last_head.as_deref());
            let dist = match (&head, built) {
                (Some(h), Some(b)) => match distance(&repo.workspace, Some(b), h) {
                    u64::MAX => "unrelated".into(),
                    d => format!("{d} from HEAD"),
                },
                _ => String::new(),
            };
            let state = match (&s.meta.lease, s.state()) {
                (Some(l), State::Held) if l.pending => format!("seeding for {}", l.worktree.display()),
                (Some(l), State::Held) if s.meta.seed_pending => {
                    format!("held by {} (filled on its first cargo command)", l.worktree.display())
                }
                (Some(l), State::Held) => format!("held by {}", l.worktree.display()),
                _ => "free".into(),
            };
            let size = if size { format!("  {}", du(&s.target())) } else { String::new() };
            println!(
                "  slot {:<3} {state}\n           commit {} {dist}, last build {}{size}",
                s.id,
                built.map_or("?", short),
                ago(s.last_used()),
            );
        }
    }
    for root in &roots {
        let shared_n = shared_unit_count(root);
        if shared_n > 0 {
            let sz = if size { format!("  {}", du(&root.join(SHARED_DIR))) } else { String::new() };
            println!("shared {} ({shared_n} cached external units{sz})", root.join(SHARED_DIR).display());
        }
    }
    Ok(())
}

#[derive(Default)]
struct Totals {
    repo: String,
    slots: usize,
    held: usize,
    attaches: u64,
    reused: u64,
    attach_ms: u64,
    prepares: u64,
    backdated: u64,
    files: u64,
    reset: u64,
    prepare_ms: u64,
    seeds: u64,
    seeded: u64,
    seed_bytes: u64,
    seed_left_out: u64,
    seed_busy: u64,
    seed_no_room: u64,
    seed_failed: u64,
    seed_ms: u64,
    builds: u64,
    units: u64,
    build_ms: u64,
    snapshots: u64,
    snapshot_ms: u64,
    trims: u64,
    trimmed_bytes: u64,
    evictions: u64,
    evicted_bytes: u64,
    prunes: u64,
    pruned_units: u64,
    pruned_bytes: u64,
    prune_ms: u64,
    harvests: u64,
    harvested_units: u64,
    harvested_bytes: u64,
    harvest_ms: u64,
    hydrates: u64,
    hydrated_units: u64,
    hydrated_bytes: u64,
    hydrate_ms: u64,
}

fn stats(days: Option<u64>) -> Result<()> {
    let keys: Vec<OsString> = Repo::discover(&std::env::current_dir()?)
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.pool.file_name().unwrap().to_owned())
        .collect();
    let cutoff = days.map_or(0, |d| now().saturating_sub(d * 86400));
    // Keyed by pool name: a repository's slots can be spread over several pool directories.
    let mut pools: BTreeMap<OsString, Totals> = BTreeMap::new();
    for root in pool_roots()? {
        for pool in fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()) {
            let name = pool.file_name().unwrap().to_owned();
            if (!keys.is_empty() && !keys.contains(&name)) || !pool.join("slots").is_dir() {
                continue;
            }
            let t = pools.entry(name).or_default();
            t.repo = fs::read_to_string(pool.join("repo")).unwrap_or_default();
            for s in load_slots(&pool.join("slots"))? {
                t.slots += 1;
                t.held += (s.state() == State::Held) as usize;
            }
            for line in fs::read_to_string(pool.join("events.jsonl")).unwrap_or_default().lines() {
                let Ok(r) = serde_json::from_str::<Record>(line) else { continue };
                if r.at < cutoff {
                    continue;
                }
                match r.event {
                    Event::Attach { reused, ms } => {
                        t.attaches += 1;
                        t.reused += reused as u64;
                        t.attach_ms += ms;
                    }
                    Event::Prepare { backdated, files, reset, ms } => {
                        t.prepares += 1;
                        t.backdated += backdated;
                        t.files += files;
                        t.reset += reset;
                        t.prepare_ms += ms;
                    }
                    Event::Seed { bytes, left_out, busy, no_room, failed, ms } => {
                        t.seeds += 1;
                        t.seeded += bytes.is_some() as u64;
                        t.seed_bytes += bytes.unwrap_or(0);
                        t.seed_left_out += left_out;
                        t.seed_busy += busy;
                        t.seed_no_room += no_room;
                        t.seed_failed += failed;
                        t.seed_ms += ms;
                    }
                    Event::Build { units, ms } => {
                        t.builds += 1;
                        t.units += units;
                        t.build_ms += ms;
                    }
                    Event::Snapshot { ms, .. } => {
                        t.snapshots += 1;
                        t.snapshot_ms += ms;
                    }
                    Event::Trim { bytes } => {
                        t.trims += 1;
                        t.trimmed_bytes += bytes;
                    }
                    Event::Evict { bytes } => {
                        t.evictions += 1;
                        t.evicted_bytes += bytes;
                    }
                    Event::Prune { units, bytes, ms } => {
                        t.prunes += 1;
                        t.pruned_units += units;
                        t.pruned_bytes += bytes;
                        t.prune_ms += ms;
                    }
                    Event::Harvest { units, bytes, ms } => {
                        t.harvests += 1;
                        t.harvested_units += units;
                        t.harvested_bytes += bytes;
                        t.harvest_ms += ms;
                    }
                    Event::Hydrate { units, bytes, ms } => {
                        t.hydrates += 1;
                        t.hydrated_units += units;
                        t.hydrated_bytes += bytes;
                        t.hydrate_ms += ms;
                    }
                }
            }
        }
    }
    if pools.is_empty() {
        println!("no pools yet");
    }
    let window = days.map_or("all time".to_string(), |d| format!("last {d} days"));
    for (name, t) in &pools {
        println!("{} ({}): {} slots, {} in use, {window}", name.to_string_lossy(), t.repo, t.slots, t.held);
        println!(
            "  attached   {} worktrees: {} to a warm slot, {} to a new one; {}",
            t.attaches,
            t.reused,
            t.attaches - t.reused,
            dur(t.attach_ms)
        );
        println!(
            "  seeded     {} of {} new slots, {} copied, {} left out; {}",
            t.seeded,
            t.seeds,
            human(t.seed_bytes),
            human(t.seed_left_out),
            dur(t.seed_ms)
        );
        println!(
            "  skipped    {} seed sources: {} without room below the disk limit, {} busy building, {} failed to copy",
            t.seed_no_room + t.seed_busy + t.seed_failed,
            t.seed_no_room,
            t.seed_busy,
            t.seed_failed
        );
        println!(
            "  prepared   {} slots: {} of {} files backdated, {} units reset; {}",
            t.prepares,
            t.backdated,
            t.files,
            t.reset,
            dur(t.prepare_ms)
        );
        if t.hydrates > 0 || t.harvests > 0 {
            println!(
                "  shared     {} hydrated ({} in {} passes; {}), {} harvested ({} in {} passes; {})",
                t.hydrated_units,
                human(t.hydrated_bytes),
                t.hydrates,
                dur(t.hydrate_ms),
                t.harvested_units,
                human(t.harvested_bytes),
                t.harvests,
                dur(t.harvest_ms),
            );
        }
        println!("  setup      {} in total (attach + seed + prepare)", dur(t.attach_ms + t.seed_ms + t.prepare_ms));
        println!("  built      {} cargo commands, {} units compiled; {}", t.builds, t.units, dur(t.build_ms));
        println!("  snapshots  {} in the background; {}", t.snapshots, dur(t.snapshot_ms));
        println!(
            "  pruned     {} units in {} passes, {} freed; {}",
            t.pruned_units,
            t.prunes,
            human(t.pruned_bytes),
            dur(t.prune_ms)
        );
        println!("  trimmed    {} incremental caches, {} freed", t.trims, human(t.trimmed_bytes));
        println!("  evicted    {} slots, {} freed", t.evictions, human(t.evicted_bytes));
    }
    Ok(())
}

fn dur(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        60_000..3_600_000 => format!("{}m{:02}s", ms / 60_000, ms / 1000 % 60),
        _ => format!("{}h{:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

fn gc(max_age_days: Option<u64>, detach_idle_days: Option<u64>, dry_run: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let repos = Repo::discover(&cwd).unwrap_or_default();
    let roots = pool_roots()?;
    let cutoff = max_age_days.map_or(0, |d| now().saturating_sub(d.saturating_mul(86400)));
    let pools: Vec<(PathBuf, PathBuf)> = if !repos.is_empty() {
        let mut out = Vec::new();
        for (repo, root) in repos.iter().flat_map(|r| roots.iter().map(move |root| (r, root))) {
            let pool = fs::canonicalize(root).unwrap_or_else(|_| root.clone()).join(repo.pool.file_name().unwrap());
            if pool.join("slots").is_dir() {
                out.push((pool, repo.common_dir.clone()));
            }
        }
        out
    } else {
        let mut out = Vec::new();
        for root in &roots {
            let mut entries: Vec<PathBuf> = fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name() != SHARED_DIR && e.path().join("slots").is_dir())
                .map(|e| e.path())
                .collect();
            entries.sort();
            for pool in entries {
                let git_dir = pool_git_dir(&pool);
                out.push((pool, git_dir));
            }
        }
        out
    };
    let mut total_detached = 0usize;
    let mut total_removed = 0usize;
    for (pool, git_dir) in &pools {
        let lock = FileLock::acquire(&pool.join(".lock"), true)?.ok_or_else(|| anyhow!("could not lock pool"))?;
        let mut slots = load_slots(&pool.join("slots"))?;
        if !dry_run {
            reclaim_stale(&mut slots)?;
        }
        let mut dry_detached = HashSet::new();
        if let Some(d) = detach_idle_days.or_else(|| idle_lease_days(git_dir)) {
            let idle_cutoff = now().saturating_sub(d.saturating_mul(86400));
            for s in &mut slots {
                if !is_idle_held(s, idle_cutoff) {
                    continue;
                }
                if dry_run {
                    if let Some(l) = &s.meta.lease
                        && attach_marker(&l.worktree).is_some()
                    {
                        println!(
                            "would detach idle slot {} from {} ({})",
                            s.id,
                            l.worktree.display(),
                            s.dir.display()
                        );
                        dry_detached.insert(s.id);
                        total_detached += 1;
                    }
                } else if let Some(worktree) = release_idle_slot(s, idle_cutoff)? {
                    println!("detached idle slot {} from {} ({})", s.id, worktree.display(), s.dir.display());
                    total_detached += 1;
                }
            }
        }
        if max_age_days.is_some() {
            let removed: BTreeMap<_, _> = slots
                .iter()
                .filter(|s| {
                    let is_free = s.state() != State::Held || dry_detached.contains(&s.id);
                    let age_ts = if dry_detached.contains(&s.id) { s.last_active() } else { s.last_used() };
                    is_free && age_ts < cutoff
                })
                .map(|s| (s.id, s.dir.clone()))
                .collect();
            for (id, dir) in &removed {
                if !dry_run {
                    let Some(_seed_lock) = FileLock::acquire(&dir.join("seed.lock"), false)? else { continue };
                    let Some(_build_locks) = lock_target(&dir.join("target"), false) else { continue };
                    println!("removing slot {id} ({})", dir.display());
                    let size = dir_size([dir], &[]);
                    fs::remove_dir_all(dir)?;
                    record(dir, Event::Evict { bytes: size });
                    total_removed += 1;
                } else {
                    println!("would remove slot {id} ({})", dir.display());
                    total_removed += 1;
                }
            }
        }
        drop(lock);
    }
    if let (Some(d), 0) = (detach_idle_days, total_detached) {
        println!("no held slots idle for more than {d} days");
    }
    if let (Some(d), 0) = (max_age_days, total_removed) {
        println!("no free slots unused for more than {d} days");
    }
    if max_age_days.is_some() {
        let cutoff_ns = cutoff.saturating_mul(1_000_000_000);
        for (_, root) in pool_filesystems(&roots) {
            let expired: Vec<PathBuf> = shared_reclaim_candidates(root, &[])?
                .into_iter()
                .filter(|u| mtime_ns(u) < cutoff_ns)
                .collect();
            if !expired.is_empty() {
                if dry_run {
                    println!("would remove {} shared dependency units", expired.len());
                } else {
                    let mut removed = 0;
                    for u in &expired {
                        if remove_shared_unit(u).is_ok() {
                            removed += 1;
                        }
                    }
                    println!("removed {removed} shared dependency units");
                }
            }
        }
    }
    let max_disk = setting(&cwd, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    for root in &roots {
        fs::create_dir_all(root)?;
        if dry_run {
            println!("disk at {:.1}% (limit {max_disk}%)", disk_percent(root, 0)?);
        } else if !free_disk(root, max_disk, 0, &[], Evict::Any)? {
            eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hooks

fn hook_path(path: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(git(path, &["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?);
    Ok(p.join("post-checkout"))
}

fn hook_block() -> String {
    format!(
        "{HOOK_BEGIN}\n\
         # Attach new worktrees to a pooled cargo target dir. Never fails the checkout.\n\
         if command -v cargo-pool >/dev/null 2>&1; then\n\
         \x20 cargo-pool post-checkout \"$1\" \"$2\" \"$3\" || true\n\
         fi\n\
         {HOOK_END}\n"
    )
}

fn strip_block(s: &str) -> String {
    match (s.find(HOOK_BEGIN), s.find(HOOK_END)) {
        (Some(a), Some(b)) if b > a => {
            let end = s[b..].find('\n').map_or(s.len(), |n| b + n + 1);
            format!("{}{}", &s[..a], &s[end..])
        }
        _ => s.to_string(),
    }
}

const GLOBAL_HOOK: &str = "hook.cargo-pool";

/// Whether this git runs hooks defined in config (`hook.<name>.command`, git 2.54+).
/// Feature-tested rather than version-parsed: ask git to list a probe hook.
fn git_supports_config_hooks() -> bool {
    let dir = std::env::temp_dir().join(format!("cargo-pool-probe-{}", std::process::id()));
    let supported = fs::create_dir_all(&dir).is_ok()
        && git(&dir, &["init", "-q"]).is_ok()
        && git(
            &dir,
            &["-c", "hook.cargo-pool-probe.event=post-checkout", "-c", "hook.cargo-pool-probe.command=true", "hook", "list", "post-checkout"],
        )
        .is_ok_and(|out| out.contains("cargo-pool-probe"));
    let _ = fs::remove_dir_all(&dir);
    supported
}

/// Global hook when git supports it, otherwise (or when asked) the repository's hook.
fn install_hook_auto(path: Option<PathBuf>, repo_only: bool) -> Result<()> {
    if path.is_some() || repo_only {
        return install_hook(&path_or_cwd(path)?);
    }
    if git_supports_config_hooks() {
        return install_global_hook();
    }
    let cwd = std::env::current_dir()?;
    let version = git(&cwd, &["--version"]).unwrap_or_default();
    eprintln!("cargo-pool: {version} can't define hooks globally (needs git 2.54+)");
    if git(&cwd, &["rev-parse", "--git-dir"]).is_err() {
        eprintln!("cargo-pool: run `cargo pool install-hook` inside each Rust repository you want pooled");
        return Ok(());
    }
    eprintln!("cargo-pool: installing for this repository only; repeat in other Rust repositories");
    install_hook(&cwd)
}

/// Remove the global hook and the current repository's hook block, whichever exist.
fn uninstall_hook_auto(path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = path {
        return uninstall_hook(&path_or_cwd(Some(p))?);
    }
    let cwd = std::env::current_dir()?;
    let mut removed = false;
    if git(&cwd, &["config", "--global", "--get-regexp", r"^hook\.cargo-pool\."]).is_ok() {
        uninstall_global_hook()?;
        removed = true;
    }
    if hook_path(&cwd).is_ok_and(|h| fs::read_to_string(h).is_ok_and(|s| s.contains(HOOK_BEGIN))) {
        uninstall_hook(&cwd)?;
        removed = true;
    }
    if !removed {
        println!("no cargo-pool hook installed (globally or in this repository)");
    }
    Ok(())
}

/// Global config-based hook: git appends the hook arguments to the command.
fn install_global_hook() -> Result<()> {
    let exe = fs::canonicalize(std::env::current_exe()?)?;
    let cwd = std::env::current_dir()?;
    let set = |key: &str, value: &str| git(&cwd, &["config", "--global", "--replace-all", &format!("{GLOBAL_HOOK}.{key}"), value]);
    set("event", "post-checkout")?;
    // A no-op once the binary is gone, so `cargo uninstall` can't break checkouts.
    // Git appends the hook arguments to the command.
    set("command", &format!("f={:?}; [ -x \"$f\" ] || exit 0; exec \"$f\" post-checkout", exe.to_string_lossy()))?;
    println!("installed global post-checkout hook ({GLOBAL_HOOK}) in your global git config");
    // A per-repo hook block would now attach twice (harmlessly); point it out.
    if let Ok(hook) = hook_path(&cwd)
        && fs::read_to_string(&hook).is_ok_and(|s| s.contains(HOOK_BEGIN))
    {
        eprintln!(
            "cargo-pool: note: {} also has a cargo-pool block, so it would attach twice; `cargo pool uninstall-hook .` removes it",
            hook.display()
        );
    }
    Ok(())
}

fn uninstall_global_hook() -> Result<()> {
    git(&std::env::current_dir()?, &["config", "--global", "--remove-section", GLOBAL_HOOK])?;
    println!("removed global post-checkout hook ({GLOBAL_HOOK})");
    Ok(())
}

fn install_hook(path: &Path) -> Result<()> {
    let hook = hook_path(path)?;
    let existing = fs::read_to_string(&hook).unwrap_or_default();
    let mut content = strip_block(&existing);
    if content.trim().is_empty() {
        content = "#!/bin/sh\n".into();
    }
    if content.lines().any(|l| l.trim_start().starts_with("exit")) {
        eprintln!(
            "cargo-pool: warning: {} contains an `exit` statement; the cargo-pool block is appended at the end and may not run",
            hook.display()
        );
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&hook_block());
    fs::create_dir_all(hook.parent().unwrap())?;
    fs::write(&hook, content)?;
    let mut perm = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    fs::set_permissions(&hook, perm)?;
    println!("installed cargo-pool block in {}", hook.display());
    Ok(())
}

fn uninstall_hook(path: &Path) -> Result<()> {
    let hook = hook_path(path)?;
    let existing = fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
    let stripped = strip_block(&existing);
    if stripped.trim() == "#!/bin/sh" || stripped.trim().is_empty() {
        fs::remove_file(&hook)?;
    } else {
        fs::write(&hook, stripped)?;
    }
    println!("removed cargo-pool block from {}", hook.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_relative_linker() {
        let dir = std::env::temp_dir().join(format!("cargo-pool-test-{}", std::process::id()));
        fs::create_dir_all(dir.join(".cargo")).unwrap();
        fs::write(
            dir.join(".cargo/config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nlinker = \"scripts/ld.sh\"\n\n[target.aarch64-unknown-linux-gnu]\nlinker = \"/usr/bin/cc\"\n",
        )
        .unwrap();
        let found = path_dependent_linkers(&dir);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, vec![("x86_64-unknown-linux-gnu".to_string(), "scripts/ld.sh".to_string())]);
    }

    #[test]
    fn backdates_only_unchanged_files() {
        let base = std::env::temp_dir().join(format!("cargo-pool-bd-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        for d in [&a, &b] {
            fs::create_dir_all(d).unwrap();
            git(d, &["init", "-q"]).unwrap();
        }
        fs::write(a.join("same.rs"), "fn same() {}").unwrap();
        fs::write(a.join("edited.rs"), "fn old() {}").unwrap();
        fs::create_dir(a.join("proto")).unwrap();
        fs::write(a.join("proto/x.proto"), "message X {}").unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_000_000);
        for f in ["same.rs", "edited.rs", "proto/x.proto", "proto"] {
            File::open(a.join(f)).unwrap().set_modified(old).unwrap();
        }
        let snap = take_snapshot(&a, None).unwrap();
        assert_eq!(snap.files.len(), 3);

        // A fresh checkout: same content for one file, an edit in the other, new mtimes.
        fs::write(b.join("same.rs"), "fn same() {}").unwrap();
        fs::write(b.join("edited.rs"), "fn new() {}").unwrap();
        fs::write(b.join("added.rs"), "fn added() {}").unwrap();
        fs::create_dir(b.join("proto")).unwrap();
        fs::write(b.join("proto/x.proto"), "message X {}").unwrap();
        assert_eq!(backdate(&b, &snap).unwrap(), (2, 4));

        let mtime = |f: &str| fs::metadata(b.join(f)).unwrap().modified().unwrap();
        assert_eq!(mtime("same.rs"), old);
        assert!(mtime("edited.rs") > old);
        assert!(mtime("added.rs") > old);
        // Unchanged directory (rerun-if-changed=<dir>) gets its old mtime back too.
        assert_eq!(mtime("proto"), old);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn prunes_units_the_lockfile_cannot_use() {
        let base = std::env::temp_dir().join(format!("cargo-pool-prune-{}", std::process::id()));
        let (wt, slot) = (base.join("wt"), base.join("slot"));
        let debug = slot.join("target/debug");
        for d in ["deps", "build", ".fingerprint"] {
            fs::create_dir_all(debug.join(d)).unwrap();
        }
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"gitdep\"\nversion = \"0.1.0\"\nsource = \"git+https://example.com/gitdep?branch=main#abcdef0123456789abcdef0123456789abcdef01\"\n\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(debug.join(".cargo-lock"), "").unwrap();
        // (name, hash, own fingerprint, fingerprints of its deps, first source)
        let units: [(&str, &str, u64, &[u64], &str); 6] = [
            ("serde", "1111111111111111", 1, &[], "/c/registry/src/index.crates.io-0123/serde-1.0.1/src/lib.rs"),
            ("serde", "2222222222222222", 2, &[], "/c/registry/src/index.crates.io-0123/serde-1.0.2/src/lib.rs"),
            ("uses_old", "3333333333333333", 3, &[1], "/work/my\\ app/uses_old/src/lib.rs"),
            ("app", "4444444444444444", 4, &[2, 6, 99], "/work/my\\ app/src/lib.rs"),
            ("gitdep", "5555555555555555", 5, &[], "/c/git/checkouts/gitdep-0123/1234567/src/lib.rs"),
            ("gitdep", "6666666666666666", 6, &[], "/c/git/checkouts/gitdep-0123/abcdef0/src/lib.rs"),
        ];
        for (name, hash, fp, deps, src) in units {
            let dir = debug.join(format!(".fingerprint/{name}-{hash}"));
            fs::create_dir(&dir).unwrap();
            let deps: Vec<String> = deps.iter().map(|d| format!("[1,\"dep\",false,{d}]")).collect();
            fs::write(dir.join(format!("lib-{name}.json")), format!("{{\"rustc\":1,\"deps\":[{}]}}", deps.join(","))).unwrap();
            fs::write(dir.join(format!("lib-{name}")), format!("{:016x}", fp.swap_bytes())).unwrap();
            fs::write(debug.join(format!("deps/lib{name}-{hash}.rlib")), "").unwrap();
            fs::write(debug.join(format!("deps/{name}-{hash}.d")), format!("/t/deps/lib{name}-{hash}.rmeta: {src} /x.rs\n\n{src}:\n")).unwrap();
        }
        // Old serde's build script, an interrupted compile and an uplifted binary.
        fs::create_dir_all(debug.join("build/serde-1111111111111111/out")).unwrap();
        fs::create_dir(debug.join(".fingerprint/broken-7777777777777777")).unwrap();
        fs::write(debug.join(".fingerprint/broken-7777777777777777/invoked.timestamp"), "").unwrap();
        fs::write(debug.join("deps/broken-7777777777777777"), "").unwrap();
        fs::write(debug.join("uses_old"), "").unwrap();

        prune(&wt, &slot, false).unwrap();
        let exists = |p: &str| debug.join(p).exists();
        for dead in ["1111111111111111", "3333333333333333", "5555555555555555", "7777777777777777"] {
            for e in fs::read_dir(debug.join("deps")).unwrap().chain(fs::read_dir(debug.join(".fingerprint")).unwrap()) {
                assert!(!e.unwrap().file_name().to_string_lossy().contains(dead), "unit {dead} must be deleted");
            }
        }
        assert!(!exists("build/serde-1111111111111111"));
        for live in ["serde-2222222222222222", "app-4444444444444444", "gitdep-6666666666666666"] {
            assert!(exists(&format!(".fingerprint/{live}")) && exists(&format!("deps/{live}.d")), "{live} must stay");
        }
        assert!(exists("deps/libserde-2222222222222222.rlib"));
        assert!(exists("uses_old"), "uplifted files without a hash stay");
        assert!(slot.join("pruned-for").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn prunes_new_layout_units_and_other_compilers() {
        let base = std::env::temp_dir().join(format!("cargo-pool-prune-layout-{}", std::process::id()));
        let (wt, slot) = (base.join("wt"), base.join("slot"));
        let debug = slot.join("target/debug");
        fs::create_dir_all(debug.join("deps")).unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"serde_json\"\nversion = \"1.0.149\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"oldtool\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(debug.join(".cargo-lock"), "").unwrap();
        let fingerprint = |dir: &Path, kind: &str, rustc: u64, fp: u64, deps: &[u64]| {
            fs::create_dir_all(dir).unwrap();
            let deps: Vec<String> = deps.iter().map(|d| format!("[1,\"dep\",false,{d}]")).collect();
            fs::write(dir.join(format!("{kind}.json")), format!("{{\"rustc\":{rustc},\"deps\":[{}]}}", deps.join(","))).unwrap();
            fs::write(dir.join(kind), format!("{:016x}", fp.swap_bytes())).unwrap();
        };
        let src = |package: &str| format!("/c/registry/src/index.crates.io-0123/{package}/src/lib.rs");
        // New layout: (pkg, hash, kind, rustc, own fingerprint, fingerprints of its deps, files in `out/`)
        let units: [(&str, &str, &str, u64, u64, &[u64], Vec<(String, String)>); 6] = [
            ("serde_json", "aaaaaaaaaaaaaaaa", "lib-serde_json", 2, 10, &[], vec![
                ("serde_json-aaaaaaaaaaaaaaaa.d".into(), format!("/t/libserde_json-aaaaaaaaaaaaaaaa.rmeta: {}\n", src("serde_json-1.0.149"))),
                ("libserde_json-aaaaaaaaaaaaaaaa.rlib".into(), String::new()),
            ]),
            ("serde_json", "bbbbbbbbbbbbbbbb", "lib-serde_json", 2, 11, &[], vec![
                ("serde_json-bbbbbbbbbbbbbbbb.d".into(), format!("/t/libserde_json-bbbbbbbbbbbbbbbb.rmeta: {}\n", src("serde_json-1.0.100"))),
            ]),
            // A build script build-depending on the old serde_json, and its run with only an OUT_DIR.
            ("oldtool", "cccccccccccccccc", "build-script-build-script-build", 2, 12, &[11], vec![
                ("build_script_build.d".into(), format!("/t/build_script_build: {}\n", src("oldtool-0.1.0"))),
            ]),
            ("oldtool", "dddddddddddddddd", "run-build-script-build-script-build", 2, 13, &[12], vec![("generated.rs".into(), String::new())]),
            // A build script's output that happens to be dep-info of a crate the lock doesn't pin.
            ("livetool", "eeeeeeeeeeeeeeee", "run-build-script-build-script-build", 2, 14, &[], vec![
                ("junk-0123456789abcdef.d".into(), format!("/t/junk: {}\n", src("junk-9.9.9"))),
            ]),
            ("tool", "ffffffffffffffff", "lib-tool", 1, 15, &[], vec![]),
        ];
        for (pkg, hash, kind, rustc, fp, deps, out) in &units {
            let dir = debug.join(format!("build/{pkg}/{hash}"));
            fingerprint(&dir.join("fingerprint"), kind, *rustc, *fp, deps);
            fs::create_dir_all(dir.join("out")).unwrap();
            for (name, content) in out {
                fs::write(dir.join("out").join(name), content).unwrap();
            }
        }
        fs::create_dir(debug.join("build/oldtool/dddddddddddddddd/run")).unwrap();
        // The other compiler's unit was built before the last build.
        File::options()
            .write(true)
            .open(debug.join("build/tool/ffffffffffffffff/fingerprint/lib-tool.json"))
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        // An unfinished new-layout unit.
        fs::create_dir_all(debug.join("build/half/1212121212121212/fingerprint")).unwrap();
        fs::write(debug.join("build/half/1212121212121212/fingerprint/invoked.timestamp"), "").unwrap();
        // Classic units depending on a live and a dead new-layout unit.
        for (name, hash, fp, dep) in [("app", "4444444444444444", 20, 10), ("uses_old", "5555555555555555", 21, 11)] {
            fingerprint(&debug.join(format!(".fingerprint/{name}-{hash}")), &format!("lib-{name}"), 2, fp, &[dep]);
            fs::write(debug.join(format!("deps/lib{name}-{hash}.rlib")), "").unwrap();
        }

        let exists = |p: &str| debug.join(p).exists();
        // The compiler is unknown before the worktree's first build: only the lock rule applies.
        prune(&wt, &slot, false).unwrap();
        for dead in ["build/serde_json/bbbbbbbbbbbbbbbb", "build/oldtool", "build/half", ".fingerprint/uses_old-5555555555555555", "deps/libuses_old-5555555555555555.rlib"] {
            assert!(!exists(dead), "{dead} must be deleted");
        }
        for live in [
            "build/serde_json/aaaaaaaaaaaaaaaa/out/libserde_json-aaaaaaaaaaaaaaaa.rlib",
            "build/livetool/eeeeeeeeeeeeeeee",
            "build/tool/ffffffffffffffff",
            ".fingerprint/app-4444444444444444",
            "deps/libapp-4444444444444444.rlib",
        ] {
            assert!(exists(live), "{live} must stay");
        }
        let stamp = Stamp::read(&slot).unwrap();
        assert_eq!((stamp.lock, stamp.toolchain, stamp.rustc), (Lockfile::read(&wt).unwrap().hash, toolchain_hash(&wt), None));

        // After a build, the compiler that wrote the newest fingerprint is the worktree's.
        prune(&wt, &slot, true).unwrap();
        assert!(!exists("build/tool"), "a unit of another compiler must be deleted with its empty package dir");
        assert!(exists("build/serde_json/aaaaaaaaaaaaaaaa") && exists(".fingerprint/app-4444444444444444"));
        assert_eq!(Stamp::read(&slot).unwrap().rustc, Some(2));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn detects_path_dependent_env_and_wrapper() {
        let dir = std::env::temp_dir().join(format!("cargo-pool-cfg-{}", std::process::id()));
        fs::create_dir_all(dir.join(".cargo")).unwrap();
        fs::write(
            dir.join(".cargo/config.toml"),
            "[target.x86_64-unknown-linux-gnu]\nlinker = \"scripts/ld.sh\"\n\n\
             [env]\nREL_DIR = { value = \"assets\", relative = true }\nABS_VAL = { value = \"assets\", relative = false }\nPLAIN = \"foo\"\n\n\
             [build]\nrustc-wrapper = \"bin/wrap.sh\"\nrustc = \"/usr/bin/rustc\"\n",
        )
        .unwrap();
        let found = path_dependent_settings(&dir);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            found,
            vec![
                ("env.REL_DIR".to_string(), "assets".to_string()),
                ("build.rustc-wrapper".to_string(), "bin/wrap.sh".to_string()),
            ]
        );
    }

    #[test]
    fn parses_cargo_cmdline_profiles_and_targets() {
        let s = |xs: &[&str]| xs.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_cargo_cmdline_profile(&s(&["cargo", "build"]), None), Some("debug".into()));
        assert_eq!(parse_cargo_cmdline_profile(&s(&["/usr/bin/cargo", "build", "--release"]), None), Some("release".into()));
        assert_eq!(parse_cargo_cmdline_profile(&s(&["cargo", "test", "-r"]), None), Some("release".into()));
        assert_eq!(parse_cargo_cmdline_profile(&s(&["cargo", "bench"]), None), Some("release".into()));
        assert_eq!(parse_cargo_cmdline_profile(&s(&["cargo", "build", "--profile", "fast-dev"]), None), Some("fast-dev".into()));
        assert_eq!(parse_cargo_cmdline_profile(&s(&["cargo", "build", "--profile=dev"]), None), Some("debug".into()));
        assert_eq!(
            parse_cargo_cmdline_profile(&s(&["cargo", "build", "--target", "wasm32-unknown-unknown", "--release"]), None),
            Some("wasm32-unknown-unknown/release".into())
        );
        assert_eq!(
            parse_cargo_cmdline_profile(&s(&["cargo", "run", "--", "--release"]), Some("aarch64-unknown-linux-gnu")),
            Some("aarch64-unknown-linux-gnu/debug".into())
        );
        assert_eq!(parse_cargo_cmdline_profile(&s(&["rustc", "-vV"]), None), None);
    }

    #[test]
    fn shared_unit_name_and_profile_roundtrip() {
        let name = format!("{}__0123456789abcdef__99__12345", encode_profile("x86_64-unknown-linux-gnu/custom-fast"));
        assert_eq!(
            parse_shared_unit_name(&name),
            Some(("x86_64-unknown-linux-gnu/custom-fast".into(), "0123456789abcdef".into(), 99, 12345))
        );
        assert_eq!(parse_shared_unit_name("bad-name"), None);
    }

    #[test]
    fn extracts_only_pure_external_units_and_handles_out_dir() {
        let base = std::env::temp_dir().join(format!("cargo-pool-extract-{}", std::process::id()));
        let (wt, common, target) = (base.join("wt"), base.join("common.git"), base.join("wt/target"));
        let debug = target.join("debug");
        for d in [".fingerprint", "deps", "build"] {
            fs::create_dir_all(debug.join(d)).unwrap();
        }
        fs::create_dir_all(&common).unwrap();

        let write_fp = |pkg: &str, hash: &str, kind: &str, fp: u64, deps: &[u64], local_json: &str| {
            let dir = debug.join(format!(".fingerprint/{pkg}-{hash}"));
            fs::create_dir_all(&dir).unwrap();
            let deps_s: Vec<String> = deps.iter().map(|d| format!("[1,\"d\",false,{d}]")).collect();
            fs::write(
                dir.join(format!("{kind}.json")),
                format!("{{\"rustc\":42,\"deps\":[{}],\"local\":{local_json}}}", deps_s.join(",")),
            )
            .unwrap();
            fs::write(dir.join(kind), format!("{:016x}", fp.swap_bytes())).unwrap();
        };

        // 1. Registry crate with build.rs + OUT_DIR generated file in .d (MUST be extracted).
        write_fp("serde_core", "1111111111111111", "build-script-build-script-build", 10, &[], "[]");
        fs::create_dir_all(debug.join("build/serde_core-1111111111111111")).unwrap();
        fs::write(
            debug.join("build/serde_core-1111111111111111/build_script_build-1111111111111111.d"),
            format!(
                "{}/debug/build/serde_core-1111111111111111/build_script_build-1111111111111111: /c/registry/src/idx/serde_core-1.0.229/build.rs\n",
                target.display()
            ),
        )
        .unwrap();

        write_fp(
            "serde_core",
            "2222222222222222",
            "run-build-script-build-script-build",
            11,
            &[10],
            "[{\"RerunIfChanged\":{\"output\":\"debug/build/serde_core-2222222222222222/output\",\"paths\":[\"build.rs\"]}}]",
        );
        fs::create_dir_all(debug.join("build/serde_core-2222222222222222/out")).unwrap();
        fs::write(debug.join("build/serde_core-2222222222222222/out/private.rs"), "// gen").unwrap();

        write_fp("serde_core", "3333333333333333", "lib-serde_core", 12, &[11], "[]");
        fs::write(debug.join("deps/libserde_core-3333333333333333.rlib"), "rlib").unwrap();
        fs::write(
            debug.join("deps/serde_core-3333333333333333.d"),
            format!(
                "{}/debug/deps/serde_core-3333333333333333.d: /c/registry/src/idx/serde_core-1.0.229/src/lib.rs {}/debug/build/serde_core-2222222222222222/out/private.rs\n# env-dep:OUT_DIR={}/debug/build/serde_core-2222222222222222/out\n",
                target.display(),
                target.display(),
                target.display()
            ),
        )
        .unwrap();

        // 2. Registry crate that reads a file inside the worktree (MUST be excluded, along with dependents).
        write_fp("leaky", "4444444444444444", "lib-leaky", 13, &[], "[]");
        fs::write(debug.join("deps/libleaky-4444444444444444.rlib"), "rlib").unwrap();
        fs::write(
            debug.join("deps/leaky-4444444444444444.d"),
            format!("{}/debug/deps/leaky-4444444444444444.d: /c/registry/src/idx/leaky-1.0.0/src/lib.rs {}/config.json\n", target.display(), wt.display()),
        )
        .unwrap();

        write_fp("uses_leaky", "5555555555555555", "lib-uses_leaky", 14, &[13], "[]");
        fs::write(debug.join("deps/libuses_leaky-5555555555555555.rlib"), "rlib").unwrap();
        fs::write(
            debug.join("deps/uses_leaky-5555555555555555.d"),
            format!("{}/debug/deps/uses_leaky-5555555555555555.d: /c/registry/src/idx/uses_leaky-1.0.0/src/lib.rs\n", target.display()),
        )
        .unwrap();

        // 3. Workspace crate (MUST be excluded).
        write_fp("app", "6666666666666666", "bin-app", 15, &[12], "[]");
        fs::write(
            debug.join("deps/app-6666666666666666.d"),
            format!("{}/debug/deps/app-6666666666666666.d: {}/src/main.rs\n", target.display(), wt.display()),
        )
        .unwrap();

        let extracted = extract_external_units(&target, &wt, &common);
        let hashes: HashSet<&str> = extracted.iter().map(|u| u.hash.as_str()).collect();
        assert_eq!(hashes, HashSet::from(["1111111111111111", "2222222222222222", "3333333333333333"]));
        fs::remove_dir_all(&base).unwrap();
    }
}
