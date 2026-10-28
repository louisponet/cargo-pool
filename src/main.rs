//! cargo-pool: share warm cargo `target/` directories between git worktrees.
//!
//! Each repository (identified by its git common dir) gets a pool of slots under
//! the configured pool directories (default `~/.cache/cargo-pool`). A worktree attaches to a slot
//! by having its `target` replaced with a symlink into the slot. A slot is leased
//! to at most one worktree at a time, so concurrent agents never fight over the
//! same cargo build lock. Leases are released explicitly with `detach`, or
//! implicitly when the worktree disappears (e.g. `git worktree remove`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
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
            attach(&path_or_cwd(path)?, AttachOpts { seed: !no_seed, force, from_hook, wait_for_build: false })
        }
        Cmd::Detach { path } => detach(&path_or_cwd(path)?),
        Cmd::Status { size } => status(size),
        Cmd::Stats { days } => stats(days),
        Cmd::Gc { max_age_days, dry_run } => gc(max_age_days, dry_run),
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
            let opts = AttachOpts { seed: true, force: false, from_hook: true, wait_for_build: false };
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

#[derive(Clone)]
struct Repo {
    /// Top level of the worktree we're operating on.
    worktree: PathBuf,
    common_dir: PathBuf,
    /// Main worktree, if the repository isn't bare.
    main_worktree: Option<PathBuf>,
    /// Pool directory for this repository.
    pool: PathBuf,
}

impl Repo {
    fn discover(path: &Path) -> Result<Repo> {
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
        let key = format!("{name}-{:016x}", fnv1a(common_dir.as_os_str().as_encoded_bytes()));
        let roots = pool_roots()?;
        let pool = select_pool(&roots, &key, &worktree, &common_dir)?;
        Ok(Repo { worktree, common_dir, main_worktree, pool })
    }

    fn head(&self) -> Result<String> {
        git(&self.worktree, &["rev-parse", "HEAD"])
    }

    fn max_disk_percent(&self) -> f64 {
        setting(&self.common_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0)
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

/// Env var, else git config of the repository at `git_dir` (any dir inside it), else default.
fn setting<T: std::str::FromStr>(git_dir: &Path, env: &str, git_key: &str, default: T) -> T {
    std::env::var(env)
        .ok()
        .or_else(|| git(git_dir, &["config", "--get", git_key]).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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
    let mut warm = Vec::new();
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
                }
            }
        }
    }
    if let Some((_, _, pool)) = warm.into_iter().min() {
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

fn containing_pool_root(path: &Path) -> Result<Option<PathBuf>> {
    let path = fs::canonicalize(path)?;
    Ok(pool_roots()?
        .into_iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count()))
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
            if name == "deps" || name == ".fingerprint" || name == "incremental" {
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
        if l.pending && (l.worktree.join(".git").exists() || now().saturating_sub(l.acquired_at) < PENDING_TIMEOUT_SECS) {
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
            self.meta.last_head = head.or(Some(l.head));
            self.meta.last_used = now().max(last_activity(&self.target()));
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
    deletable: bool,
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

/// What's in the `incremental_caches` of a target dir, one entry per crate.
fn incremental_entries(target: &Path) -> Vec<PathBuf> {
    incremental_caches(target).iter().flat_map(fs::read_dir).flatten().flatten().map(|e| e.path()).collect()
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
                let (used, deletable) = match (s.state(), &s.meta.lease) {
                    (State::Held, Some(l)) if l.pending => continue,
                    (State::Held, Some(l)) => (used.max(l.acquired_at), evict == Evict::Any),
                    _ => (used, true),
                };
                candidates.push((used, Reclaim { pool: pool.clone(), slot: s, deletable }));
            }
        }
    }
    candidates.sort_by_key(|(used, _)| *used);
    Ok(candidates.into_iter().map(|(_, c)| c).collect())
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

/// Bytes `free_disk` could take from `candidates` right now, in its order, counted until `enough`
/// says so: every incremental cache, then every deletable slot.
fn reclaimable(candidates: &[Reclaim], mut enough: impl FnMut(u64) -> Result<bool>) -> Result<u64> {
    let mut freed = 0;
    let mut caches = vec![0; candidates.len()];
    // The probe lets its locks go before the walk, which would otherwise stall a build starting
    // in the slot.
    for (c, cache) in candidates.iter().zip(&mut caches) {
        if enough(freed)? {
            return Ok(freed);
        }
        if lock_candidate(c)?.is_some() {
            *cache = dir_size(incremental_entries(&c.slot.target()), &[]);
            freed += *cache;
        }
    }
    for (c, cache) in candidates.iter().zip(caches).filter(|(c, _)| c.deletable) {
        if enough(freed)? {
            return Ok(freed);
        }
        if lock_candidate(c)?.is_some() {
            freed += dir_size([&c.slot.dir], &[]).saturating_sub(cache);
        }
    }
    Ok(freed)
}

/// Free space on `root`'s filesystem, across configured directories, until disk usage (plus
/// `extra` bytes about to be written) is below `max_percent`.
///
/// The incremental caches of the least recently used slots go first: losing one costs a worktree
/// a slower rebuild of the next crate it edits, while losing a slot costs a reseed or a full
/// build. Then whole slots go, least recently used first; `evict` says whether held ones may go
/// too. A deleted held slot's worktree is re-attached on its next build. Slots in `protect`,
/// slots being attached, seeded or built and pools locked by another cargo-pool process are
/// never touched. Must be called without holding any pool lock. Returns whether the goal was met.
///
/// With `extra` bytes to make room for, nothing is deleted unless they fit once everything
/// eligible is gone: a copy that can't fit anyway isn't worth a single warm slot.
fn free_disk(root: &Path, max_percent: f64, extra: u64, protect: &[&Path], evict: Evict) -> Result<bool> {
    fs::create_dir_all(root)?;
    let fits = |freed: u64| disk_percent(root, extra as i64 - freed as i64).map(|p| p < max_percent);
    if fits(0)? {
        return Ok(true);
    }
    let candidates = reclaim_candidates(root, protect, evict)?;
    if fits(0)? {
        return Ok(true);
    }
    if extra > 0 && !fits(reclaimable(&candidates, &fits)?)? {
        return Ok(false);
    }

    for c in &candidates {
        let Some((s, _locks)) = lock_candidate(c)? else { continue };
        let size = dir_size(incremental_entries(&s.target()), &[]);
        if size == 0 {
            continue;
        }
        // Empty each cache but keep it with its mtime: it dates the slot's last compile, and
        // cargo would recreate a missing one on its next command, compiling or not.
        for cache in incremental_caches(&s.target()) {
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
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted the incremental caches of slot {} of {} ({})",
            s.id,
            c.pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
        if fits(0)? {
            return Ok(true);
        }
    }

    for c in candidates.iter().filter(|c| c.deletable) {
        let Some((s, _locks)) = lock_candidate(c)? else { continue };
        if s.state() == State::Held && evict == Evict::Unheld {
            continue;
        }
        let mut from = String::new();
        if let (State::Held, Some(l)) = (s.state(), &s.meta.lease) {
            let link = l.worktree.join("target");
            if fs::read_link(&link).is_ok_and(|t| t == s.target()) {
                if let Some(marker) = attach_marker(&l.worktree) {
                    fs::write(marker, "")?;
                }
                fs::remove_file(&link)?;
                fs::create_dir_all(&link)?;
            }
            from = format!(", taken from worktree {}", l.worktree.display());
        }
        let size = dir_size([&s.dir], &[]);
        fs::remove_dir_all(&s.dir).with_context(|| format!("removing {}", s.dir.display()))?;
        record(&s.dir, Event::Evict { bytes: size });
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted slot {} of {} ({}{from})",
            s.id,
            c.pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
        if fits(0)? {
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
    let name = Path::new(first).file_name().unwrap_or_default().to_string_lossy();
    let is_subcommand = <Cli as clap::CommandFactory>::command()
        .get_subcommands()
        .any(|c| c.get_name() == first.to_string_lossy());
    !is_subcommand && (Path::new(first).is_file() || name.starts_with("rustc") || name.starts_with("clippy-driver"))
}

/// Check disk usage if this compile writes into a pooled target dir, then exec the
/// compiler (through `CARGO_POOL_RUSTC_WRAPPER`, e.g. sccache, if set).
fn rustc_wrapper(args: Vec<OsString>) -> ! {
    if args[2..].iter().any(|a| a == "-vV") {
        if let Err(e) = attach_on_first_use() {
            eprintln!("cargo-pool: attaching to a pooled target dir failed: {e:#}");
            std::process::exit(1);
        }
        if let Err(e) = seed_on_first_use() {
            eprintln!("cargo-pool: warning: seeding the slot failed, building from scratch: {e:#}");
        }
    }
    if let Err(e) = check_disk_during_build(&args[2..]) {
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
    // Waiters poll; opening the slots once keeps a poll to one fcntl per slot.
    let first = std::process::id() as usize;
    let slots = (0..jobs)
        .map(|i| OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(((first + i) % jobs).to_string())))
        .collect::<std::io::Result<Vec<_>>>()?;
    let since = Instant::now();
    let mut notified = false;
    loop {
        for slot in &slots {
            let lock = libc::flock { l_type: libc::F_WRLCK as _, l_whence: libc::SEEK_SET as _, l_start: 0, l_len: 0, l_pid: 0 };
            if unsafe { libc::fcntl(slot.as_raw_fd(), libc::F_SETLK, &lock) } != 0 {
                let err = std::io::Error::last_os_error();
                if matches!(err.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) {
                    continue;
                }
                return Err(err).context("lock a job slot");
            }
            // Closing any fd to the slot would drop the lock, so the compiler inherits this one.
            if unsafe { libc::fcntl(slot.as_raw_fd(), libc::F_SETFD, 0) } != 0 {
                return Err(std::io::Error::last_os_error()).context("clear FD_CLOEXEC on the job slot");
            }
            std::mem::forget(slots);
            return Ok(true);
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn check_disk_during_build(rustc_args: &[OsString]) -> Result<()> {
    let Some(out_dir) = rustc_args.iter().position(|a| a == "--out-dir").and_then(|i| rustc_args.get(i + 1)) else {
        return Ok(());
    };
    let Ok(out_dir) = fs::canonicalize(out_dir) else { return Ok(()) };
    let Some(root) = containing_pool_root(&out_dir)? else { return Ok(()) };
    let Ok(rel) = out_dir.strip_prefix(&root) else { return Ok(()) };
    let mut parts = rel.components().map(|c| c.as_os_str());
    let Some(pool) = parts.next() else { return Ok(()) };
    let mut slot_dir = None;
    if let (Some(slots), Some(id)) = (parts.next(), parts.next())
        && slots == "slots"
    {
        let dir = root.join(pool).join(slots).join(id);
        if let Err(e) = note_pooled_compile(&dir) {
            eprintln!("cargo-pool: warning: can't track build for mtime snapshots: {e:#}");
        }
        slot_dir = Some(dir);
    }
    let git_dir = PathBuf::from(fs::read_to_string(root.join(pool).join("repo")).unwrap_or_default());
    let max_disk = setting(&git_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    if disk_percent(&root, 0)? < max_disk {
        return Ok(());
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
        return Ok(());
    }
    let Some(_lock) = FileLock::acquire(&marker, false)? else { return Ok(()) };
    if recent() {
        return Ok(());
    }
    let ok = free_disk(&root, max_disk, 0, slot_dir.as_deref().as_slice(), Evict::Any);
    fs::write(&marker, now().to_string())?;
    if !ok? {
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
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

fn hash_file(p: &Path) -> Option<u64> {
    let mut f = File::open(p).ok()?;
    let mut buf = vec![0; 1 << 16];
    let mut h = FNV_OFFSET;
    loop {
        match f.read(&mut buf).ok()? {
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
                let Some(h) = hash_file(&p) else { continue };
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

/// Give every file in `wt` whose content matches the snapshot its recorded mtime.
/// Returns (files matched, files in the worktree).
fn backdate(wt: &Path, snap: &Snapshot) -> Result<(usize, usize)> {
    let files = worktree_files(wt)?;
    let mut matched = 0;
    for rel in &files {
        let Some(&(size, mtime, hash)) = snap.files.get(rel) else { continue };
        let p = wt.join(rel);
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if !m.is_file() || m.len() != size {
            continue;
        }
        if modified_ns(&m) != mtime {
            if hash_file(&p) != Some(hash) {
                continue;
            }
            if let Err(e) = File::open(&p).and_then(|f| f.set_modified(UNIX_EPOCH + Duration::from_nanos(mtime))) {
                eprintln!("cargo-pool: warning: cannot backdate {}: {e}", p.display());
                continue;
            }
        }
        matched += 1;
    }
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
    let own_gitdir = git(&repo.worktree, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
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
    let mut reset = 0;
    for (_, fingerprint, _) in profile_dirs(target).iter().flat_map(|d| profile_units(d)) {
        let foreign = fs::read_dir(&fingerprint)
            .into_iter()
            .flatten()
            .flatten()
            .any(|f| fs::read(f.path()).is_ok_and(|b| refers_elsewhere(&b)));
        if foreign {
            fs::remove_dir_all(&fingerprint)?;
            reset += 1;
        }
    }
    Ok(reset)
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

/// What pruning reads from a unit's fingerprint JSON (`<kind>-<target>.json`).
#[derive(Deserialize)]
struct Fingerprint {
    /// Hash of the compiler that built the unit.
    rustc: Option<u64>,
    /// (package id, name, public, fingerprint)
    deps: Vec<(u64, String, bool, u64)>,
}

/// Build units in `target` that a worktree at `lock` can't use: built from a crate version or git
/// revision it doesn't pin, by another compiler than `rustc` (if known), never finished (no
/// fingerprint JSON), or depending on such a unit. Returns their number and the paths to delete:
/// classic units' entries in `.fingerprint/`, `deps/`, `build/` and `examples/`, and new-layout
/// units' `build/<pkg>/<h>/` dirs, or `build/<pkg>/` if none of its units stay.
fn unusable_units(target: &Path, lock: &Lockfile, rustc: Option<u64>) -> (usize, Vec<PathBuf>) {
    struct Unit {
        profile: usize,
        hash: String,
        dir: Option<PathBuf>,
        deps: Vec<u64>,
        dead: bool,
    }
    let profiles = profile_dirs(target);
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

fn backdate_enabled(repo: &Repo) -> bool {
    setting(&repo.common_dir, "CARGO_POOL_BACKDATE", "cargo-pool.backdate", true)
}

/// Called by the rustc wrapper for every compile into a slot: invalidate the slot's
/// snapshot and make sure a waiter will snapshot the worktree once the build ends.
fn note_pooled_compile(slot_dir: &Path) -> Result<()> {
    let marker = OpenOptions::new().create(true).truncate(false).write(true).open(slot_dir.join("last-compile"))?;
    marker.set_modified(SystemTime::now())?;
    // One line per compile; the snapshot waiter turns them into a `Build` event.
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(slot_dir.join("compiles"))
        .and_then(|mut f| f.write_all(format!("{}\n", now_ns()).as_bytes()));
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

/// Wait until no build is running in the slot, then snapshot its worktree. Retries if a
/// build started meanwhile, so the snapshot always reflects the state after the last one.
fn snapshot_after_build(slot_dir: &Path) -> Result<()> {
    let target = slot_dir.join("target");
    loop {
        let Some(_lock) = FileLock::acquire(&slot_dir.join("snapshot.lock"), false)? else { return Ok(()) };
        loop {
            for l in cargo_locks(&target) {
                drop(FileLock::acquire(&l, true)?);
            }
            record_build(slot_dir);
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
                break;
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
fn record_build(slot_dir: &Path) {
    let taken = slot_dir.join("compiles.taken");
    if fs::rename(slot_dir.join("compiles"), &taken).is_err() {
        return;
    }
    let starts: Vec<u64> =
        fs::read_to_string(&taken).unwrap_or_default().lines().filter_map(|l| l.parse().ok()).collect();
    let _ = fs::remove_file(&taken);
    if let Some(first) = starts.iter().min() {
        let ms = now_ns().saturating_sub(*first) / 1_000_000;
        record(slot_dir, Event::Build { units: starts.len() as u64, ms });
    }
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
}

fn attach(path: &Path, opts: AttachOpts) -> Result<()> {
    let repo = Repo::discover(path)?;
    let _attach_lock =
        attach_marker(&repo.worktree).map(|m| FileLock::acquire(&m.with_extension("lock"), true)).transpose()?;
    if !repo.worktree.join("Cargo.toml").exists() {
        if !opts.from_hook {
            println!("{} is not a cargo workspace, nothing to do", repo.worktree.display());
        }
        return Ok(());
    }
    let link = repo.worktree.join("target");
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
                    .find(|s| s.target() == dest && s.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree));
                if let Some(mut slot) = held {
                    if slot.meta.lease.as_ref().is_some_and(|l| l.pending) {
                        drop(_lock);
                        let _build_locks = lock_target(&slot.target(), opts.wait_for_build)
                            .ok_or_else(|| anyhow!("{} is busy building", link.display()))?;
                        let _lock = repo.lock()?;
                        let old = slot.saved_files(&repo.worktree).with_extension("attaching");
                        if old.exists() {
                            fs::remove_dir_all(old)?;
                        }
                        if slot.meta.adoption.is_some() {
                            save_snapshot(&slot.dir, &take_snapshot(&repo.worktree, None)?)?;
                        }
                        if !slot.meta.seed_pending {
                            prepare_worktree(&repo, &slot.dir)?;
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
                        && s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.worktree)
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
        && let Some(marker) = attach_marker(&repo.worktree)
    {
        fs::write(marker, "")?;
        // An empty dir rather than nothing: other post-checkout hooks may fill a missing
        // `target` (e.g. by copying the main checkout's).
        fs::create_dir_all(&link)?;
        exclude_target(&repo)?;
        warn_path_dependent_config(&repo);
        eprintln!("cargo-pool: {} gets a pooled target dir on the first cargo command", link.display());
        return Ok(());
    }
    lease_slot(&repo, opts.seed, opts.wait_for_build)
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
fn lease_slot(repo: &Repo, seed: bool, wait_for_build: bool) -> Result<()> {
    let started = Instant::now();
    let link = repo.worktree.join("target");
    let slots_dir = repo.slots_dir();
    let max_disk = repo.max_disk_percent();
    let (mut recovery, _recovery_lock) = {
        let _pool = repo.lock()?;
        let slot = load_slots(&slots_dir)?
            .into_iter()
            .find(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.worktree));
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
        let old = slot.saved_files(&repo.worktree).with_extension("attaching");
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
    if !free_disk(repo.pool.parent().unwrap(), max_disk, extra, &[], Evict::Any)? {
        if extra > 0 {
            bail!("not enough room below {max_disk}% to attach {}", link.display());
        }
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
    }
    let head = repo.head()?;
    let lease = |pending| Lease { worktree: repo.worktree.clone(), head: head.clone(), acquired_at: now(), pending };

    let lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    reclaim_stale(&mut slots)?;

    // Best free slot: closest commit, then most recently used.
    let pending =
        slots.iter().position(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.worktree));
    let pick = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| !adopting && s.state() == State::Free)
        .min_by_key(|(_, s)| {
            (distance(&repo.worktree, s.meta.last_head.as_deref(), &head), u64::MAX - s.meta.last_used)
        })
        .map(|(i, _)| i);

    let slot_idx = match pending.or(pick) {
        Some(i) => {
            let s = &mut slots[i];
            let d = distance(&repo.worktree, s.meta.last_head.as_deref(), &head);
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
    let saved = slot.saved_files(&repo.worktree);
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
        save_snapshot(&slot.dir, &take_snapshot(&repo.worktree, None)?)?;
    }
    warn_path_dependent_config(repo);
    record(&slot.dir, Event::Attach { reused: pick.is_some(), ms: ms_since(started) });
    if !slot.meta.seed_pending {
        prepare_worktree(repo, &slot.dir)?;
    }
    slot.meta.lease.as_mut().unwrap().pending = false;
    slot.meta.adoption = None;
    slot.save()?;
    eprintln!("cargo-pool: {} -> slot {}", link.display(), slot.id);
    Ok(())
}

/// Marks a worktree that should lease a slot on its next cargo command. Lives in the
/// worktree's own git dir, so it disappears with the worktree.
fn attach_marker(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let git_dir = match fs::read_to_string(&dot_git) {
        Ok(s) => worktree.join(s.strip_prefix("gitdir:")?.trim()),
        Err(_) if dot_git.is_dir() => dot_git,
        Err(_) => return None,
    };
    Some(git_dir.join("cargo-pool-attach"))
}

/// Lease a slot for a worktree whose attach was deferred, when cargo first probes the
/// compiler (`rustc -vV`). Without a target dir cargo has no cached probe, and it probes
/// before touching the target dir, so the link is in place before cargo needs it.
fn attach_on_first_use() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(worktree) = cwd.ancestors().find(|a| a.join(".git").exists()) else { return Ok(()) };
    let Some(marker) = attach_marker(worktree).filter(|m| m.exists()) else { return Ok(()) };
    // Parallel cargo commands wait for one attach.
    let _lock = FileLock::acquire(&marker, true)?;
    if !marker.exists() {
        return Ok(());
    }
    if let Err(e) = attach(worktree, AttachOpts { seed: true, force: false, from_hook: false, wait_for_build: true }) {
        // Cargo caches failed probes too. A directory prevents that write while keeping logs accessible.
        let cache = worktree.join("target/.rustc_info.json");
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
fn prepare_worktree(repo: &Repo, slot_dir: &Path) -> Result<()> {
    let started = Instant::now();
    let (mut matched, mut total) = (0, 0);
    if backdate_enabled(repo)
        && let Some(snap) = valid_snapshot(slot_dir)
    {
        (matched, total) = backdate(&repo.worktree, &snap)?;
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
    prune(&repo.worktree, slot_dir, false)?;
    record(
        slot_dir,
        Event::Prepare { backdated: matched as u64, files: total as u64, reset: reset as u64, ms: ms_since(started) },
    );
    Ok(())
}

/// Seed a pending slot when cargo first probes the compiler (`rustc -vV`) in its worktree.
/// Cargo runs that probe before it creates or locks anything in the target dir, and an
/// empty slot has no cached probe, so the first cargo command always comes through here.
fn seed_on_first_use() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Some(target) = cwd
        .ancestors()
        .filter_map(|a| fs::read_link(a.join("target")).ok())
        .find(|t| containing_pool_root(t).ok().flatten().is_some())
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
    let repo = Repo::discover(&lease.worktree)?;
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
        seed(&repo, &slot, &head, sources)?
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
        prepare_worktree(&repo, slot_dir)?;
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

fn warn_path_dependent_config(repo: &Repo) {
    for (triple, linker) in path_dependent_linkers(&repo.worktree) {
        eprintln!(
            "cargo-pool: warning: .cargo/config.toml sets target.{triple}.linker = \"{linker}\", a relative path.\n\
             cargo-pool:   Cargo hashes the resolved linker path into every crate's fingerprint, so each\n\
             cargo-pool:   worktree recompiles everything even with a shared target dir."
        );
        if let (false, Some(main)) = (triple.starts_with("cfg("), &repo.main_worktree) {
            let var = format!("CARGO_TARGET_{}_LINKER", triple.to_uppercase().replace(['-', '.'], "_"));
            eprintln!(
                "cargo-pool:   Fix: point it at a path that's the same in every checkout, e.g.\n\
                 cargo-pool:     export {var}={}",
                main.join(&linker).display()
            );
        }
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
    if let Some(main) = &repo.main_worktree {
        let t = main.join("target");
        if fs::symlink_metadata(&t).is_ok_and(|m| m.is_dir()) {
            out.push((t, git(main, &["rev-parse", "HEAD"]).ok()));
        }
    }
    out.retain(|(t, _)| fs::read_dir(t).is_ok_and(|mut d| d.next().is_some()));
    out
}

/// What a seed copies out of a target dir, relative to it: everything but the `incremental/`
/// caches of its profile dirs (`target/<profile>` and `target/<triple>/<profile>`). Those are
/// often most of the bytes and only speed up the first rebuild of a crate edited after seeding.
fn seed_parts(target: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
        let profile = is_profile(dir);
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let (path, rel) = (entry.path(), rel.join(entry.file_name()));
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && entry.file_name() == ".rustc_info.json" {
                continue;
            }
            if profile && is_dir && entry.file_name() == "incremental" {
                continue;
            }
            let holds_profile =
                || is_profile(&path) || fs::read_dir(&path).into_iter().flatten().flatten().any(|e| is_profile(&e.path()));
            if !profile && is_dir && holds_profile() {
                walk(&path, &rel, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = vec![];
    walk(target, Path::new(""), &mut out);
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

fn seed_plan(src: &Path, lock: Option<&Lockfile>, rustc: Option<u64>) -> SeedPlan {
    let (units, left_out) = lock.map_or((0, vec![]), |l| unusable_units(src, l, rustc));
    let dropped: HashSet<&Path> = left_out.iter().map(PathBuf::as_path).collect();
    let holding: HashSet<&Path> = left_out.iter().flat_map(|p| p.ancestors().skip(1)).collect();
    let (mut parts, mut split) = (vec![], vec![]);
    let mut stack: Vec<PathBuf> = seed_parts(src).into_iter().rev().collect();
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

/// Copy the closest non-busy target dir into the slot. Returns the commit it was built for.
fn seed(repo: &Repo, slot: &Slot, head: &str, sources: Vec<(PathBuf, Option<String>)>) -> Result<Option<String>> {
    let mut ranked: Vec<_> = sources
        .into_iter()
        .map(|(t, h)| (distance(&repo.worktree, h.as_deref(), head), t.parent().map_or(0, last_compile_ns), t, h))
        .collect();
    ranked.sort_by_key(|(d, act, ..)| (*d, u64::MAX - act));
    let begun = Instant::now();

    let max_disk = repo.max_disk_percent();
    // What an interrupted seed left behind would count against the copy's room.
    let tmp = slot.dir.join("target.seeding");
    let _ = fs::remove_dir_all(&tmp);
    let lock = Lockfile::read(&repo.worktree);
    let toolchain = toolchain_hash(&repo.worktree);
    let root = repo.pool.parent().unwrap();
    // What a copy may take below the limit once free_disk has freed all it may for a seed, worked
    // out when the first copy doesn't fit: bigger candidates aren't worth another scan, and none
    // is worth sizing if there's no room at all.
    let mut disk_room: Option<i64> = None;
    let (mut busy, mut no_room, mut failed) = (0, 0, 0);
    for (d, _, src, h) in ranked {
        if disk_room.is_some_and(|r| r <= 0) {
            no_room += 1;
            continue;
        }
        // The receiver hasn't built yet: its compiler is the source slot's if their toolchain
        // files match. The main checkout's target has no stamp.
        let from_main = repo.main_worktree.as_ref().is_some_and(|m| src == m.join("target"));
        let rustc = src
            .parent()
            .filter(|_| !from_main)
            .and_then(Stamp::read)
            .filter(|s| s.toolchain == toolchain)
            .and_then(|s| s.rustc);
        // Make room for the copy first; never let seeding push the disk over the limit.
        let plan = seed_plan(&src, lock.as_ref(), rustc);
        if plan.parts.is_empty() {
            continue; // evicted since it was listed
        }
        let size = dir_size(plan.parts.iter().map(|p| src.join(p)), &[]);
        let protect: Vec<&Path> = [src.parent(), Some(slot.dir.as_path())].into_iter().flatten().collect();
        if disk_room.is_some_and(|r| size as i64 > r) || !free_disk(root, max_disk, size, &protect, Evict::Unheld)? {
            eprintln!(
                "cargo-pool: copying {} ({}) would push the disk above {max_disk}%, skipping it",
                src.display(),
                human(size)
            );
            if disk_room.is_none() {
                let (used, avail) = disk_usage(root)?;
                let below_limit = ((used + avail) as f64 * max_disk / 100.0) as i64 - used;
                let candidates = reclaim_candidates(root, &[&slot.dir], Evict::Unheld)?;
                disk_room = Some(below_limit + reclaimable(&candidates, |_| Ok(false))? as i64);
            }
            no_room += 1;
            continue;
        }
        // Hold cargo's build locks so the source isn't modified mid-copy, and copy what's
        // there now: a build may have run since it was sized.
        let Some(_locks) = lock_target(&src, false) else {
            eprintln!("cargo-pool: {} is busy building, trying next candidate", src.display());
            busy += 1;
            continue;
        };
        let plan = seed_plan(&src, lock.as_ref(), rustc);
        if plan.parts.is_empty() {
            continue; // gone since it was sized
        }
        let dist = if d == u64::MAX { "unrelated".into() } else { format!("{d} commits away") };
        eprintln!("cargo-pool: seeding slot {} from {} ({dist}), this can take a while...", slot.id, src.display());
        let started = SystemTime::now();
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
        let copied = fs::create_dir(&tmp).is_ok()
            && runs.iter().all(|run| {
                Command::new("cp")
                    .current_dir(&src)
                    .args(["-a", "--reflink=auto", "--parents", "--"])
                    .args(*run)
                    .arg(&tmp)
                    .status()
                    .is_ok_and(|s| s.success())
            });
        if !copied {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!("cargo-pool: copy failed, trying next candidate");
            failed += 1;
            continue;
        }
        // Filling a directory entry by entry gives it a new mtime, which would count as a
        // compile and invalidate the snapshot carried over below.
        for dir in &plan.split {
            if let (Ok(t), Ok(f)) = (fs::metadata(src.join(dir)).and_then(|m| m.modified()), File::open(tmp.join(dir))) {
                f.set_modified(t)?;
            }
        }
        let left_out = dir_size(&plan.left_out, &[]);
        remove_saved_links(&tmp)?;
        for lock in cargo_locks(&tmp) {
            fs::remove_file(lock)?;
        }
        merge_target(&tmp, &slot.target(), Conflict::Destination)?;
        // Carry over the file snapshot that matches this target, so the new worktree can
        // be backdated: the source slot's (if still valid), or one of the main checkout.
        let snap = match (&repo.main_worktree, src.parent()) {
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
    let patterns = ["/target", "/target.cargo-pool-files/"];
    if patterns.iter().all(|pattern| current.lines().any(|l| l.trim() == *pattern)) {
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
    let repo = Repo::discover(path)?;
    let _attach_lock =
        attach_marker(&repo.worktree).map(|m| FileLock::acquire(&m.with_extension("lock"), true)).transpose()?;
    let link = repo.worktree.join("target");
    let _ = fs::remove_dir(link.join(".rustc_info.json"));
    {
        let _pool = repo.lock()?;
        if let Some(mut slot) = load_slots(&repo.slots_dir())?
            .into_iter()
            .find(|s| s.meta.lease.as_ref().is_some_and(|l| l.pending && l.worktree == repo.worktree))
        {
            if slot.meta.adoption == Some(Adoption::Copying)
                && !fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink())
            {
                let _seed_lock = FileLock::acquire(&slot.dir.join("seed.lock"), false)?
                    .ok_or_else(|| anyhow!("slot {} is busy attaching", slot.id))?;
                slot.release(None)?;
                slot.save()?;
                if let Some(marker) = attach_marker(&repo.worktree).filter(|m| m.exists()) {
                    fs::remove_file(marker)?;
                }
                println!("cargo-pool: cancelled adoption; original target kept at {}", link.display());
                return Ok(());
            }
            bail!("{} has an unfinished attach; rerun attach before detaching", repo.worktree.display());
        }
    }
    if let Some(marker) = attach_marker(&repo.worktree).filter(|m| m.exists()) {
        fs::remove_file(marker)?;
        if !fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()) {
            let _ = fs::remove_dir(&link);
            println!("cargo-pool: {} won't be attached on its first build anymore", repo.worktree.display());
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
    if slot.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree) {
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

fn status(size: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Ok(repo) = Repo::discover(&cwd) else {
        for root in pool_roots()? {
            println!("pools in {}:", root.display());
            for e in fs::read_dir(&root).into_iter().flatten().flatten() {
                let repo = fs::read_to_string(e.path().join("repo")).unwrap_or_default();
                let n = fs::read_dir(e.path().join("slots")).map_or(0, |d| d.count());
                println!("  {:<40} {n} slots  {repo}", e.file_name().to_string_lossy());
            }
        }
        return Ok(());
    };
    for root in pool_roots()? {
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
                (Some(h), Some(b)) => match distance(&repo.worktree, Some(b), h) {
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
}

fn stats(days: Option<u64>) -> Result<()> {
    let key = Repo::discover(&std::env::current_dir()?).ok().map(|r| r.pool.file_name().unwrap().to_owned());
    let cutoff = days.map_or(0, |d| now().saturating_sub(d * 86400));
    // Keyed by pool name: a repository's slots can be spread over several pool directories.
    let mut pools: BTreeMap<OsString, Totals> = BTreeMap::new();
    for root in pool_roots()? {
        for pool in fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()) {
            let name = pool.file_name().unwrap().to_owned();
            if key.as_ref().is_some_and(|k| *k != name) || !pool.join("slots").is_dir() {
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

fn gc(max_age_days: Option<u64>, dry_run: bool) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    for root in pool_roots()? {
        let repo = Repo { pool: fs::canonicalize(root)?.join(repo.pool.file_name().unwrap()), ..repo.clone() };
        let lock = repo.lock()?;
        let mut slots = load_slots(&repo.slots_dir())?;
        if !dry_run {
            reclaim_stale(&mut slots)?;
        }
        let cutoff = max_age_days.map_or(0, |d| now().saturating_sub(d * 86400));
        let removed: BTreeMap<_, _> = slots
            .iter()
            .filter(|s| s.state() != State::Held && s.last_used() < cutoff)
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
            } else {
                println!("would remove slot {id} ({})", dir.display());
            }
        }
        if let (Some(d), true) = (max_age_days, removed.is_empty()) {
            println!("no free slots unused for more than {d} days");
        }
        drop(lock);

        let max_disk = repo.max_disk_percent();
        let pct = disk_percent(&repo.pool, 0)?;
        if dry_run {
            println!("disk at {pct:.1}% (limit {max_disk}%)");
        } else if !free_disk(repo.pool.parent().unwrap(), max_disk, 0, &[], Evict::Any)? {
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
}
