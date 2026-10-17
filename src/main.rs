//! cargo-pool: share warm cargo `target/` directories between git worktrees.
//!
//! Each repository (identified by its git common dir) gets a pool of slots under
//! the configured pool directories (default `~/.cache/cargo-pool`). A worktree attaches to a slot
//! by having its `target` replaced with a symlink into the slot. A slot is leased
//! to at most one worktree at a time, so concurrent agents never fight over the
//! same cargo build lock. Leases are released explicitly with `detach`, or
//! implicitly when the worktree disappears (e.g. `git worktree remove`).

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const HOOK_BEGIN: &str = "# >>> cargo-pool >>>";
const HOOK_END: &str = "# <<< cargo-pool <<<";
const NULL_SHA: &str = "0000000000000000000000000000000000000000";
/// A lease still being set up (slot seeding in progress) is honoured for this long.
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
        /// Also pool the main worktree (by default only linked worktrees are pooled).
        #[arg(long)]
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
    /// `$CARGO_HOME/config.toml`), so disk usage is checked during every pooled build.
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
        Cmd::Attach { path, no_seed, force, allow_main, from_hook } => {
            attach(&path_or_cwd(path)?, AttachOpts { seed: !no_seed, force, allow_main, from_hook })
        }
        Cmd::Detach { path } => detach(&path_or_cwd(path)?),
        Cmd::Status { size } => status(size),
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
            if fs::read_to_string(cargo_config()?).is_ok_and(|s| s.lines().any(is_our_wrapper_line)) {
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
            let opts = AttachOpts { seed: true, force: false, allow_main: false, from_hook: true };
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

    fn is_main(&self) -> bool {
        self.main_worktree.as_deref() == Some(&self.worktree)
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

fn idle_hours(git_dir: &Path) -> u64 {
    setting(git_dir, "CARGO_POOL_IDLE_HOURS", "cargo-pool.idleHours", 48)
}

fn pool_roots() -> Result<Vec<PathBuf>> {
    let configured = if let Some(dirs) = std::env::var_os("CARGO_POOL_DIRS") {
        std::env::split_paths(&dirs).collect::<Vec<_>>()
    } else if let Some(dir) = std::env::var_os("CARGO_POOL_DIR") {
        vec![PathBuf::from(dir)]
    } else {
        let output = Command::new("git")
            .args(["config", "--global", "--path", "--get-all", "cargo-pool.directory"])
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
fn lock_target(target: &Path) -> Option<Vec<FileLock>> {
    cargo_locks(target)
        .iter()
        .map(|p| FileLock::acquire(p, false).ok().flatten())
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
    /// Slot is still being seeded; the symlink doesn't exist yet.
    #[serde(default)]
    pending: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct SlotMeta {
    lease: Option<Lease>,
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

    fn save(&self) -> Result<()> {
        let tmp = self.dir.join("slot.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.meta)?)?;
        fs::rename(tmp, self.dir.join("slot.json"))?;
        Ok(())
    }

    fn state(&self) -> State {
        let Some(l) = &self.meta.lease else { return State::Free };
        if l.pending && now().saturating_sub(l.acquired_at) < PENDING_TIMEOUT_SECS {
            return State::Held;
        }
        if fs::read_link(l.worktree.join("target")).is_ok_and(|t| t == self.target()) {
            State::Held
        } else {
            State::Stale
        }
    }

    /// Turn a lease into a free slot, remembering what it was built for.
    fn release(&mut self, head: Option<String>) {
        if let Some(l) = self.meta.lease.take() {
            self.meta.last_head = head.or(Some(l.head));
            self.meta.last_used = now().max(last_activity(&self.target()));
        }
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
        s.release(None);
        s.save()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// disk pressure

/// Disk usage (in percent, as `df` reports it) of the filesystem holding `path`,
/// pretending `extra` more bytes had already been written.
fn disk_percent(path: &Path, extra: u64) -> Result<f64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    let bs = st.f_frsize as u64;
    let used = (st.f_blocks - st.f_bfree) as u64 * bs + extra;
    let avail = (st.f_bavail as u64 * bs).saturating_sub(extra);
    Ok(100.0 * used as f64 / (used + avail).max(1) as f64)
}

/// Bytes allocated on disk by everything under `p` (symlinks not followed).
fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
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

/// Delete pooled target dirs on `root`'s filesystem, across configured directories, until disk usage
/// (plus `extra` bytes about to be written) is below `max_percent`.
///
/// Free slots go first, then slots of worktrees idle for longer than their repo's
/// `idleHours`, least recently used first. Slots in `protect`, slots with a cargo
/// build running and pools locked by another cargo-pool process are never touched.
/// Must be called without holding any pool lock. Returns whether the goal was met.
fn free_disk(root: &Path, max_percent: f64, extra: u64, protect: &[&Path]) -> Result<bool> {
    fs::create_dir_all(root)?;
    if disk_percent(root, extra)? < max_percent {
        return Ok(true);
    }

    // (idle-held?, last used, pool, slot id)
    let mut candidates = vec![];
    let device = fs::metadata(root)?.dev();
    for directory in pool_roots()? {
        if !fs::metadata(&directory).is_ok_and(|m| m.dev() == device) {
            continue;
        }
        for pool in fs::read_dir(directory)?.flatten().map(|e| e.path()) {
            let git_dir = PathBuf::from(fs::read_to_string(pool.join("repo")).unwrap_or_default());
            let cutoff = now().saturating_sub(idle_hours(&git_dir) * 3600);
            for s in load_slots(&pool.join("slots"))? {
                if protect.contains(&s.dir.as_path()) {
                    continue;
                }
                let used = s.meta.last_used.max(last_activity(&s.target()));
                match (s.state(), &s.meta.lease) {
                    (State::Free | State::Stale, _) => candidates.push((false, used, pool.clone(), s.id)),
                    (State::Held, Some(l)) if !l.pending && used.max(l.acquired_at) < cutoff => {
                        candidates.push((true, used.max(l.acquired_at), pool.clone(), s.id))
                    }
                    _ => {}
                }
            }
        }
    }
    candidates.sort();

    for (idle_held, _, pool, id) in candidates {
        let Some(_pool_lock) = FileLock::acquire(&pool.join(".lock"), false)? else { continue };
        // Re-check under the lock, things may have changed since the scan.
        let Some(s) = load_slots(&pool.join("slots"))?.into_iter().find(|s| s.id == id) else { continue };
        let state = s.state();
        if state == State::Held && !idle_held {
            continue;
        }
        let Some(_build_locks) = lock_target(&s.target()) else { continue };
        let mut from = String::new();
        if let (State::Held, Some(l)) = (state, &s.meta.lease) {
            let link = l.worktree.join("target");
            if fs::read_link(&link).is_ok_and(|t| t == s.target()) {
                fs::remove_file(&link)?;
            }
            from = format!(", taken from idle worktree {}", l.worktree.display());
        }
        let size = dir_size(&s.dir);
        fs::remove_dir_all(&s.dir).with_context(|| format!("removing {}", s.dir.display()))?;
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted slot {id} of {} ({}{from})",
            pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
        if disk_percent(root, extra)? < max_percent {
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
    if args[2..].iter().any(|a| a == "-vV")
        && let Err(e) = seed_on_first_use()
    {
        eprintln!("cargo-pool: warning: seeding the slot failed, building from scratch: {e:#}");
    }
    if let Err(e) = check_disk_during_build(&args[2..]) {
        eprintln!("cargo-pool: warning: disk check failed: {e:#}");
    }
    let (program, rest) = match std::env::var_os("CARGO_POOL_RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
        Some(inner) => (inner, &args[1..]),
        None => (args[1].clone(), &args[2..]),
    };
    let err = Command::new(&program).args(rest).exec();
    eprintln!("cargo-pool: failed to run {}: {err}", program.to_string_lossy());
    std::process::exit(1);
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
    if let (Some(slots), Some(id)) = (parts.next(), parts.next())
        && slots == "slots"
    {
        let slot_dir = root.join(pool).join(slots).join(id);
        if let Err(e) = note_pooled_compile(&slot_dir) {
            eprintln!("cargo-pool: warning: can't track build for mtime snapshots: {e:#}");
        }
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
    let ok = free_disk(&root, max_disk, 0, &[]);
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
            File::open(&p)?.set_modified(UNIX_EPOCH + Duration::from_nanos(mtime))?;
        }
        matched += 1;
    }
    // Same for directories whose entries are unchanged (setting a child's mtime doesn't
    // touch its parent's, so the order doesn't matter).
    for (rel, &(mtime, listing)) in &snap.dirs {
        let p = wt.join(rel);
        if mtime_ns(&p) != mtime && dir_listing_hash(&p) == Some(listing) {
            File::open(&p)?.set_modified(UNIX_EPOCH + Duration::from_nanos(mtime))?;
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
    let refers_elsewhere = |bytes: &[u8]| {
        bytes
            .windows(common.len())
            .enumerate()
            .filter(|(_, w)| *w == common)
            .any(|(i, _)| !bytes[i..].starts_with(own_gitdir.as_bytes()))
    };
    let mut fingerprint_dirs = vec![];
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if e.file_name() == ".fingerprint" {
                fingerprint_dirs.push(e.path());
            } else if depth < 2 {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    let mut reset = 0;
    for unit in fingerprint_dirs.iter().flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten()) {
        let foreign = fs::read_dir(unit.path())
            .into_iter()
            .flatten()
            .flatten()
            .any(|f| fs::read(f.path()).is_ok_and(|b| refers_elsewhere(&b)));
        if foreign {
            fs::remove_dir_all(unit.path())?;
            reset += 1;
        }
    }
    Ok(reset)
}

fn backdate_enabled(repo: &Repo) -> bool {
    setting(&repo.common_dir, "CARGO_POOL_BACKDATE", "cargo-pool.backdate", true)
}

/// Called by the rustc wrapper for every compile into a slot: invalidate the slot's
/// snapshot and make sure a waiter will snapshot the worktree once the build ends.
fn note_pooled_compile(slot_dir: &Path) -> Result<()> {
    let marker = OpenOptions::new().create(true).truncate(false).write(true).open(slot_dir.join("last-compile"))?;
    marker.set_modified(SystemTime::now())?;
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
            let meta: SlotMeta = serde_json::from_slice(&fs::read(slot_dir.join("slot.json"))?)?;
            let Some(lease) = meta.lease.filter(|l| !l.pending) else { return Ok(()) };
            if fs::read_link(lease.worktree.join("target")).ok().as_deref() != Some(target.as_path()) {
                return Ok(());
            }
            let snap = take_snapshot(&lease.worktree, load_snapshot(slot_dir).as_ref())?;
            if last_compile_ns(slot_dir) < snap.taken_at {
                save_snapshot(slot_dir, &snap)?;
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
// attach / detach

struct AttachOpts {
    seed: bool,
    force: bool,
    allow_main: bool,
    from_hook: bool,
}

fn attach(path: &Path, opts: AttachOpts) -> Result<()> {
    let repo = Repo::discover(path)?;
    if !repo.worktree.join("Cargo.toml").exists() {
        if !opts.from_hook {
            println!("{} is not a cargo workspace, nothing to do", repo.worktree.display());
        }
        return Ok(());
    }
    if repo.is_main() && !opts.allow_main {
        if !opts.from_hook {
            println!("skipping main worktree (pass --allow-main to pool it too)");
        }
        return Ok(());
    }

    let link = repo.worktree.join("target");
    let slots_dir = repo.slots_dir();
    match fs::symlink_metadata(&link) {
        Err(_) => {}
        Ok(m) if m.file_type().is_symlink() => {
            let dest = fs::read_link(&link)?;
            if dest.starts_with(&slots_dir) {
                let _lock = repo.lock()?;
                let held = load_slots(&repo.slots_dir())?.into_iter().any(|s| {
                    s.target() == dest && s.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree)
                });
                if held {
                    if !opts.from_hook {
                        println!("already attached: {} -> {}", link.display(), dest.display());
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
            if fs::read_dir(&link)?.next().is_none() {
                fs::remove_dir(&link)?;
            } else if opts.force {
                fs::remove_dir_all(&link)?;
            } else {
                bail!("{} already exists and is not empty; use --force to discard it", link.display());
            }
        }
        Ok(_) => bail!("{} exists and is not a directory", link.display()),
    }

    let max_disk = repo.max_disk_percent();
    if !free_disk(repo.pool.parent().unwrap(), max_disk, 0, &[])? {
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
    }

    let head = repo.head()?;
    let lease = |pending| Lease { worktree: repo.worktree.clone(), head: head.clone(), acquired_at: now(), pending };

    let lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    reclaim_stale(&mut slots)?;

    // Best free slot: closest commit, then most recently used.
    let pick = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| s.state() == State::Free)
        .min_by_key(|(_, s)| (distance(&repo.worktree, s.meta.last_head.as_deref(), &head), u64::MAX - s.meta.last_used))
        .map(|(i, _)| i);

    let slot_idx = match pick {
        Some(i) => {
            let s = &mut slots[i];
            let d = distance(&repo.worktree, s.meta.last_head.as_deref(), &head);
            s.meta.lease = Some(lease(false));
            fs::create_dir_all(s.target())?;
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
            let meta = SlotMeta { lease: Some(lease(false)), seed_pending: opts.seed, ..Default::default() };
            let slot = Slot { id, dir, meta };
            fs::create_dir_all(slot.target())?;
            slot.save()?;
            drop(lock);
            if opts.seed {
                println!("cargo-pool: new slot {id}, filled from the closest warm target dir on the first cargo command");
            }
            slots.push(slot);
            slots.len() - 1
        }
    };

    let slot = &slots[slot_idx];
    symlink(slot.target(), &link).with_context(|| format!("creating {}", link.display()))?;
    exclude_target(&repo)?;
    warn_path_dependent_config(&repo);
    if !slot.meta.seed_pending {
        prepare_worktree(&repo, &slot.dir)?;
    }
    println!("cargo-pool: {} -> slot {}", link.display(), slot.id);
    Ok(())
}

/// Make a warm slot's build state fit the worktree it was just given to.
fn prepare_worktree(repo: &Repo, slot_dir: &Path) -> Result<()> {
    if backdate_enabled(repo)
        && let Some(snap) = valid_snapshot(slot_dir)
    {
        let (matched, total) = backdate(&repo.worktree, &snap)?;
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
    println!("cargo-pool: reusing slot {} last built at {built}", s.id);
}

/// Candidate target dirs to copy into a new slot: every slot plus the main checkout's.
fn seed_sources(repo: &Repo, slots: &[Slot]) -> Vec<(PathBuf, Option<String>)> {
    let mut out: Vec<_> = slots
        .iter()
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

/// Copy the closest non-busy target dir into the slot. Returns the commit it was built for.
fn seed(repo: &Repo, slot: &Slot, head: &str, sources: Vec<(PathBuf, Option<String>)>) -> Result<Option<String>> {
    let mut ranked: Vec<_> = sources
        .into_iter()
        .map(|(t, h)| (distance(&repo.worktree, h.as_deref(), head), last_activity(&t), t, h))
        .collect();
    ranked.sort_by_key(|(d, act, ..)| (*d, u64::MAX - act));

    let max_disk = repo.max_disk_percent();
    for (d, _, src, h) in ranked {
        // Make room for the copy first; never let seeding push the disk over the limit.
        let size = dir_size(&src);
        let protect: Vec<&Path> = [src.parent(), Some(slot.dir.as_path())].into_iter().flatten().collect();
        if !free_disk(repo.pool.parent().unwrap(), max_disk, size, &protect)? {
            eprintln!(
                "cargo-pool: copying {} ({}) would push the disk above {max_disk}%, skipping it",
                src.display(),
                human(size)
            );
            continue;
        }
        // Hold cargo's build locks so the source isn't modified mid-copy.
        let Some(_locks) = lock_target(&src) else {
            eprintln!("cargo-pool: {} is busy building, trying next candidate", src.display());
            continue;
        };
        let dist = if d == u64::MAX { "unrelated".into() } else { format!("{d} commits away") };
        eprintln!("cargo-pool: seeding slot {} from {} ({dist}), this can take a while...", slot.id, src.display());
        let tmp = slot.dir.join("target.seeding");
        let _ = fs::remove_dir_all(&tmp);
        let started = SystemTime::now();
        let status = Command::new("cp").arg("-a").arg("--reflink=auto").arg(&src).arg(&tmp).status()?;
        if !status.success() {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!("cargo-pool: copy failed, trying next candidate");
            continue;
        }
        let _ = fs::remove_dir_all(slot.target());
        fs::rename(&tmp, slot.target())?;
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
        let secs = started.elapsed().unwrap_or(Duration::ZERO).as_secs();
        eprintln!("cargo-pool: seeded in {secs}s");
        return Ok(h);
    }
    eprintln!("cargo-pool: no seed available, slot {} starts empty", slot.id);
    Ok(None)
}

/// `target/` patterns in .gitignore don't match symlinks, so exclude the link explicitly.
fn exclude_target(repo: &Repo) -> Result<()> {
    let info = repo.common_dir.join("info");
    let exclude = info.join("exclude");
    let current = fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == "/target") {
        return Ok(());
    }
    fs::create_dir_all(&info)?;
    let mut f = OpenOptions::new().create(true).append(true).open(&exclude)?;
    if !current.is_empty() && !current.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "# cargo-pool: worktree target symlinks\n/target")?;
    Ok(())
}

fn detach(path: &Path) -> Result<()> {
    let repo = Repo::discover(path)?;
    let link = repo.worktree.join("target");
    let dest = fs::read_link(&link).map_err(|_| anyhow!("{} is not attached to a pool", link.display()))?;
    let _lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    let slot = slots
        .iter_mut()
        .find(|s| s.target() == dest)
        .ok_or_else(|| anyhow!("{} points to {}, which is not a slot of this pool", link.display(), dest.display()))?;
    fs::remove_file(&link)?;
    if slot.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree) {
        slot.release(repo.head().ok());
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
                ago(last_activity(&s.target()).max(s.meta.last_used)),
            );
        }
    }
    Ok(())
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
            .filter(|s| s.state() != State::Held && s.meta.last_used.max(last_activity(&s.target())) < cutoff)
            .map(|s| (s.id, s.dir.clone()))
            .collect();
        for (id, dir) in &removed {
            println!("{} slot {id} ({})", if dry_run { "would remove" } else { "removing" }, dir.display());
            if !dry_run {
                fs::remove_dir_all(dir)?;
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
        } else if !free_disk(repo.pool.parent().unwrap(), max_disk, 0, &[])? {
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
}
