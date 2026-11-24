//! Knowledge about Foundry VTT's folders and world files.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Metadata read from a world's `world.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorldMeta {
    pub id: String,
    pub title: Option<String>,
    pub system: Option<String>,
    pub system_version: Option<String>,
    pub core_version: Option<String>,
    pub last_played: Option<String>,
}

/// Facts about a world folder on this computer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalWorld {
    pub meta: WorldMeta,
    pub path: String,
    /// Newest modification time of any file in the world.
    pub modified: Option<DateTime<Utc>>,
    pub size: u64,
    pub file_count: u64,
    pub in_use: bool,
    /// Cheap fingerprint of the folder (paths, sizes, mtimes).
    pub fingerprint: String,
}

/// Foundry's default user-data folder for this OS.
/// macOS: ~/Library/Application Support/FoundryVTT
/// Windows: %LOCALAPPDATA%\FoundryVTT
/// Linux: ~/.local/share/FoundryVTT
pub fn default_user_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("FoundryVTT"))
}

/// Finds the `Data/worlds` folder: reads `Config/options.json` → `dataPath`
/// from the default user-data folder, then falls back to the default layout.
pub fn detect_worlds_dir() -> Option<PathBuf> {
    let user_data = default_user_data_dir()?;
    let mut candidates = Vec::new();
    if let Some(data_path) = read_configured_data_path(&user_data) {
        candidates.push(data_path.join("Data").join("worlds"));
        candidates.push(data_path.join("worlds"));
    }
    candidates.push(user_data.join("Data").join("worlds"));
    candidates.into_iter().find(|p| p.is_dir())
}

fn read_configured_data_path(user_data: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(user_data.join("Config").join("options.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("dataPath")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Parses world.json content. Unknown/missing fields are tolerated.
pub fn parse_world_json(id: &str, text: &str) -> WorldMeta {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    WorldMeta {
        id: id.to_string(),
        title: s("title"),
        system: s("system"),
        system_version: s("systemVersion"),
        core_version: s("coreVersion"),
        last_played: s("lastPlayed"),
    }
}

pub fn read_world_meta(world_dir: &Path, id: &str) -> WorldMeta {
    let text = fs::read_to_string(world_dir.join("world.json")).unwrap_or_default();
    parse_world_json(id, &text)
}

/// Lists world folders (folders with a world.json) in the worlds dir.
pub fn list_local_world_ids(worlds_dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(worlds_dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("world.json").is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| !n.starts_with('.'))
        .collect();
    ids.sort();
    ids
}

pub fn inspect_local_world(worlds_dir: &Path, id: &str) -> LocalWorld {
    let dir = worlds_dir.join(id);
    let stats = scan(&dir);
    LocalWorld {
        meta: read_world_meta(&dir, id),
        path: dir.to_string_lossy().into_owned(),
        modified: stats.modified,
        size: stats.size,
        file_count: stats.files,
        in_use: is_world_in_use(&dir),
        fingerprint: stats.fingerprint,
    }
}

pub struct ScanStats {
    pub files: u64,
    pub size: u64,
    pub modified: Option<DateTime<Utc>>,
    pub fingerprint: String,
}

/// Walks a folder once and returns size, newest mtime and a fingerprint.
/// The fingerprint changes whenever a file is added, removed, resized or
/// modified. It uses FNV-1a so it stays stable across app versions.
pub fn scan(dir: &Path) -> ScanStats {
    scan_filtered(dir, &[])
}

/// Like [`scan`], but leaves out top-level entries named in `skip_top`.
pub fn scan_filtered(dir: &Path, skip_top: &[&str]) -> ScanStats {
    let mut entries: Vec<(String, u64, i128)> = Vec::new();
    let mut newest: Option<std::time::SystemTime> = None;
    for e in WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !is_skipped_top(e, skip_top))
        .filter_map(|e| e.ok())
    {
        if !e.file_type().is_file() {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let mtime = md.modified().ok();
        if let Some(m) = mtime {
            newest = Some(newest.map_or(m, |n| n.max(m)));
        }
        let rel = e
            .path()
            .strip_prefix(dir)
            .unwrap_or(e.path())
            .to_string_lossy()
            .replace('\\', "/");
        let nanos = mtime
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        entries.push((rel, md.len(), nanos));
    }
    entries.sort();
    let mut h = Fnv::new();
    let mut size = 0;
    for (rel, len, nanos) in &entries {
        h.write(rel.as_bytes());
        h.write(&[0]);
        h.write(&len.to_le_bytes());
        h.write(&nanos.to_le_bytes());
        size += len;
    }
    ScanStats {
        files: entries.len() as u64,
        size,
        modified: newest.map(DateTime::<Utc>::from),
        fingerprint: format!("{:016x}-{}", h.finish(), entries.len()),
    }
}

struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// True if Foundry currently has this world open. Foundry (v11+) stores
/// world data in LevelDB databases; LevelDB holds a lock on each database's
/// `LOCK` file while it is open.
pub fn is_world_in_use(world_dir: &Path) -> bool {
    is_dir_in_use(world_dir, 4)
}

/// True if a LevelDB inside `Data` (outside `skip_top`, e.g. a module's
/// compendium pack) is open in Foundry.
pub fn is_data_in_use(data_dir: &Path, skip_top: &[&str]) -> bool {
    let Ok(rd) = fs::read_dir(data_dir) else {
        return false;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            !skip_top.iter().any(|s| n == *s)
        })
        .any(|e| is_dir_in_use(&e.path(), 5))
}

/// True for a depth-1 walkdir entry whose name is in `skip_top`.
pub fn is_skipped_top(e: &walkdir::DirEntry, skip_top: &[&str]) -> bool {
    e.depth() == 1 && skip_top.iter().any(|s| e.file_name() == *s)
}

/// The `Data` folder that holds `worlds_dir` (Foundry's `Data/worlds`).
pub fn data_dir_of(worlds_dir: &Path) -> Option<PathBuf> {
    if worlds_dir.file_name()? != "worlds" {
        return None;
    }
    worlds_dir.parent().filter(|p| p.is_dir()).map(Path::to_path_buf)
}

/// Foundry's user-data folder (the one with `Config`, `Data`, `Logs`) for
/// the configured worlds folder, else the OS default.
pub fn user_data_dir_for(worlds_dir: Option<&Path>) -> Option<PathBuf> {
    worlds_dir
        .and_then(data_dir_of)
        .and_then(|d| d.parent().map(Path::to_path_buf))
        .filter(|p| p.is_dir())
        .or_else(|| default_user_data_dir().filter(|p| p.is_dir()))
}

fn is_dir_in_use(dir: &Path, depth: usize) -> bool {
    WalkDir::new(dir)
        .max_depth(depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.file_name() == "LOCK")
        .any(|e| lock_file_is_held(e.path()))
}

#[cfg(unix)]
fn lock_file_is_held(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    // LevelDB on POSIX uses fcntl() record locks. F_GETLK asks the kernel
    // whether another process holds a conflicting lock.
    // SAFETY: `fl` is a plain C struct; fcntl only writes into it.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as libc::c_short;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = 0;
    fl.l_len = 0;
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut fl) };
    if r == 0 && fl.l_type != libc::F_UNLCK as libc::c_short {
        return true;
    }
    // Also catch flock()-style locks.
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(fs::TryLockError::WouldBlock) => true,
        Err(_) => false,
    }
}

#[cfg(windows)]
fn lock_file_is_held(path: &Path) -> bool {
    // LevelDB on Windows opens LOCK without write sharing and locks it, so
    // opening it for writing fails with a sharing/lock violation while open.
    match fs::OpenOptions::new().read(true).write(true).open(path) {
        Err(e) => matches!(e.raw_os_error(), Some(32) | Some(33)),
        Ok(f) => match f.try_lock() {
            Ok(()) => {
                let _ = f.unlock();
                false
            }
            Err(fs::TryLockError::WouldBlock) => true,
            Err(_) => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_world_json() {
        let m = parse_world_json(
            "prdtr",
            r#"{"title":"PRDTR","system":"CBRPNK","systemVersion":"1.2","coreVersion":"14.370","lastPlayed":"x","playtime":5}"#,
        );
        assert_eq!(m.title.as_deref(), Some("PRDTR"));
        assert_eq!(m.system.as_deref(), Some("CBRPNK"));
        assert_eq!(m.core_version.as_deref(), Some("14.370"));
        let bad = parse_world_json("x", "not json");
        assert_eq!(bad.id, "x");
        assert!(bad.title.is_none());
    }

    #[test]
    fn lists_worlds_and_fingerprint_changes() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("alpha");
        fs::create_dir_all(w.join("data/actors")).unwrap();
        fs::write(w.join("world.json"), r#"{"title":"Alpha"}"#).unwrap();
        fs::create_dir_all(d.path().join("no-world")).unwrap();
        fs::create_dir_all(d.path().join(".alpha.restoring-1")).unwrap();
        fs::write(d.path().join(".alpha.restoring-1/world.json"), "{}").unwrap();
        assert_eq!(list_local_world_ids(d.path()), vec!["alpha"]);

        let a = scan(&w);
        assert_eq!(a.files, 1);
        fs::write(w.join("data/actors/000001.log"), b"hello").unwrap();
        let b = scan(&w);
        assert_eq!(b.files, 2);
        assert_eq!(b.size, a.size + 5);
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_eq!(b.fingerprint, scan(&w).fingerprint);
    }

    #[test]
    fn unlocked_world_is_not_in_use() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("data/actors")).unwrap();
        fs::write(d.path().join("data/actors/LOCK"), b"").unwrap();
        assert!(!is_world_in_use(d.path()));
    }

    /// Holds an fcntl lock from another process (like LevelDB does) and
    /// checks that the world is reported as in use.
    #[cfg(unix)]
    #[test]
    fn locked_world_is_in_use() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("data/actors")).unwrap();
        let lock = d.path().join("data/actors/LOCK");
        fs::write(&lock, b"").unwrap();
        let script = "import fcntl,sys,time\nf=open(sys.argv[1],'r+')\nfcntl.lockf(f,fcntl.LOCK_EX)\nprint('locked',flush=True)\ntime.sleep(30)\n";
        let Ok(mut child) = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&lock)
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("python3 not available, skipping");
            return;
        };
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "locked");
        let in_use = is_world_in_use(d.path());
        let _ = child.kill();
        let _ = child.wait();
        assert!(in_use);
        assert!(!is_world_in_use(d.path()));
    }
}
