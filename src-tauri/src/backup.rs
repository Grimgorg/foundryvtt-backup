//! Creating, listing and pruning world backups in the backup (cloud) folder.
//!
//! Layout: `<backup_dir>/<world_id>/<world_id>__<UTC time>__<host>.zip`
//! Each zip holds the world folder under `<world_id>/` plus a small
//! `fvtt-backup.json` manifest at the root.

use crate::error::{AppError, Context, Result};
use crate::foundry::{self, WorldMeta};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const MANIFEST_NAME: &str = "fvtt-backup.json";
/// Backup id for everything in Foundry's `Data` folder except `worlds`
/// (modules, systems, assets, ...). Stored like a world in the backup dir.
pub const DATA_ID: &str = "_foundry-data";
/// Name of the folder inside a data backup zip.
pub const DATA_PREFIX: &str = "Data";
/// Top-level folders of `Data` that the data backup leaves out.
pub const DATA_SKIP: &[&str] = &["worlds"];
const TS_FORMAT: &str = "%Y-%m-%dT%H-%M-%SZ";
const PARTIAL_SUFFIX: &str = ".partial";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub file_name: String,
    pub path: String,
    pub world_id: String,
    pub created: DateTime<Utc>,
    pub host: Option<String>,
    pub size: u64,
}

/// Stored inside every backup zip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: u32,
    pub app_version: String,
    pub created: DateTime<Utc>,
    pub host: String,
    pub world: WorldMeta,
    pub file_count: u64,
    pub size: u64,
}

/// Progress callback: (files done, files total).
pub type Progress<'a> = &'a dyn Fn(u64, u64);

/// Hostname reduced to characters that are safe in file names everywhere.
pub fn this_host() -> String {
    sanitize_host(&gethostname::gethostname().to_string_lossy())
}

pub fn sanitize_host(raw: &str) -> String {
    let short = raw.split('.').next().unwrap_or(raw);
    let s: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "pc".into()
    } else {
        s
    }
}

pub fn backup_file_name(world_id: &str, created: DateTime<Utc>, host: &str) -> String {
    format!("{world_id}__{}__{host}.zip", created.format(TS_FORMAT))
}

/// Parses `<id>__<ts>__<host>.zip` → (id, created, host).
pub fn parse_backup_file_name(name: &str) -> Option<(String, DateTime<Utc>, String)> {
    let stem = name.strip_suffix(".zip")?;
    let mut parts = stem.rsplitn(3, "__");
    let host = parts.next()?;
    let ts = parts.next()?;
    let id = parts.next()?;
    if id.is_empty() {
        return None;
    }
    let naive = NaiveDateTime::parse_from_str(ts, TS_FORMAT).ok()?;
    Some((
        id.to_string(),
        Utc.from_utc_datetime(&naive),
        host.to_string(),
    ))
}

/// World ids that have a folder in the backup dir with at least one zip.
pub fn list_backup_world_ids(backup_dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(backup_dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| !n.starts_with('.'))
        .filter(|id| !list_backups(backup_dir, id).is_empty())
        .collect();
    ids.sort();
    ids
}

/// All finished backups of a world, newest first. Unfinished `.partial`
/// files and other files are ignored.
pub fn list_backups(backup_dir: &Path, world_id: &str) -> Vec<BackupInfo> {
    let dir = backup_dir.join(world_id);
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<BackupInfo> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !name.to_ascii_lowercase().ends_with(".zip") {
                return None;
            }
            let md = e.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            let (created, host) = match parse_backup_file_name(&name) {
                Some((_, c, h)) => (c, Some(h)),
                None => (md.modified().ok().map(DateTime::<Utc>::from)?, None),
            };
            Some(BackupInfo {
                path: e.path().to_string_lossy().into_owned(),
                file_name: name,
                world_id: world_id.to_string(),
                created,
                host,
                size: md.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.created
            .cmp(&a.created)
            .then(b.file_name.cmp(&a.file_name))
    });
    out
}

/// Reads the manifest from a backup zip (if it has one).
pub fn read_manifest(zip_path: &Path) -> Option<Manifest> {
    let mut ar = ZipArchive::new(File::open(zip_path).ok()?).ok()?;
    let mut f = ar.by_name(MANIFEST_NAME).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

/// Backs up `<worlds_dir>/<world_id>` into the backup folder, then applies
/// the retention rule. Fails if Foundry has the world open.
pub fn create_backup(
    worlds_dir: &Path,
    backup_dir: &Path,
    world_id: &str,
    host: &str,
    keep_last: u32,
    progress: Progress,
) -> Result<BackupInfo> {
    let world_dir = worlds_dir.join(world_id);
    if !world_dir.join("world.json").is_file() {
        return Err(AppError::new(format!(
            "World folder not found: {}",
            world_dir.display()
        )));
    }
    if foundry::is_world_in_use(&world_dir) {
        return Err(AppError::new(format!(
            "“{world_id}” is open in Foundry. Close the world (return to setup) first."
        )));
    }
    let (created, file_name, final_path) = new_backup_path(backup_dir, world_id, host)?;
    let manifest = Manifest {
        format: 1,
        app_version: env!("CARGO_PKG_VERSION").into(),
        created,
        host: host.into(),
        world: foundry::read_world_meta(&world_dir, world_id),
        file_count: 0,
        size: 0,
    };
    write_zip_atomic(
        &world_dir,
        world_id,
        &[],
        Some(manifest),
        &final_path,
        progress,
    )?;
    finish_backup(
        backup_dir, world_id, host, keep_last, created, file_name, final_path,
    )
}

/// Backs up everything in Foundry's `Data` folder except `worlds`: modules,
/// systems, assets and any other folders. Fails if Foundry has a world open
/// (modules' compendium packs are locked then).
pub fn create_data_backup(
    data_dir: &Path,
    backup_dir: &Path,
    host: &str,
    keep_last: u32,
    progress: Progress,
) -> Result<BackupInfo> {
    if !data_dir.is_dir() {
        return Err(AppError::new(format!(
            "Foundry Data folder not found: {}",
            data_dir.display()
        )));
    }
    if foundry::is_data_in_use(data_dir, DATA_SKIP) {
        return Err(AppError::new(
            "A world is open in Foundry. Return to setup in Foundry first.",
        ));
    }
    let (created, file_name, final_path) = new_backup_path(backup_dir, DATA_ID, host)?;
    let manifest = Manifest {
        format: 1,
        app_version: env!("CARGO_PKG_VERSION").into(),
        created,
        host: host.into(),
        world: WorldMeta {
            id: DATA_ID.into(),
            title: Some(DATA_TITLE.into()),
            ..Default::default()
        },
        file_count: 0,
        size: 0,
    };
    write_zip_atomic(
        data_dir,
        DATA_PREFIX,
        DATA_SKIP,
        Some(manifest),
        &final_path,
        progress,
    )?;
    finish_backup(
        backup_dir, DATA_ID, host, keep_last, created, file_name, final_path,
    )
}

pub const DATA_TITLE: &str = "Modules, systems & assets";

/// Picks a unique file name for a new backup (+1s if the name is taken).
fn new_backup_path(
    backup_dir: &Path,
    id: &str,
    host: &str,
) -> Result<(DateTime<Utc>, String, PathBuf)> {
    let target_dir = backup_dir.join(id);
    fs::create_dir_all(&target_dir).context(format!("Cannot create {}", target_dir.display()))?;
    let mut created = Utc::now().with_nanosecond(0).unwrap_or_else(Utc::now);
    let mut file_name = backup_file_name(id, created, host);
    while target_dir.join(&file_name).exists() {
        created += chrono::Duration::seconds(1);
        file_name = backup_file_name(id, created, host);
    }
    let path = target_dir.join(&file_name);
    Ok((created, file_name, path))
}

fn finish_backup(
    backup_dir: &Path,
    id: &str,
    host: &str,
    keep_last: u32,
    created: DateTime<Utc>,
    file_name: String,
    path: PathBuf,
) -> Result<BackupInfo> {
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    apply_retention(backup_dir, id, keep_last)?;
    Ok(BackupInfo {
        file_name,
        path: path.to_string_lossy().into_owned(),
        world_id: id.into(),
        created,
        host: Some(host.into()),
        size,
    })
}

/// Zips `src_dir` (entries under `<prefix>/`) to `dest` via `dest.partial`,
/// renaming only after the zip is complete and flushed. Top-level entries
/// named in `skip_top` are left out.
pub fn write_zip_atomic(
    src_dir: &Path,
    prefix: &str,
    skip_top: &[&str],
    manifest: Option<Manifest>,
    dest: &Path,
    progress: Progress,
) -> Result<()> {
    let partial = PathBuf::from(format!("{}{PARTIAL_SUFFIX}", dest.display()));
    let result = write_zip(src_dir, prefix, skip_top, manifest, &partial, progress)
        .and_then(|_| fs::rename(&partial, dest).map_err(AppError::from));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_zip(
    src_dir: &Path,
    prefix: &str,
    skip_top: &[&str],
    manifest: Option<Manifest>,
    dest: &Path,
    progress: Progress,
) -> Result<()> {
    let files: Vec<walkdir::DirEntry> = WalkDir::new(src_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !foundry::is_skipped_top(e, skip_top))
        .collect::<std::result::Result<_, _>>()?;
    let total = files.iter().filter(|e| e.file_type().is_file()).count() as u64;
    let out = File::create(dest).context(format!("Cannot write {}", dest.display()))?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1 << 20, out));

    let mut done = 0u64;
    let mut bytes = 0u64;
    progress(0, total);
    for e in &files {
        let rel = e.path().strip_prefix(src_dir).unwrap_or(e.path());
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel = rel.to_string_lossy().replace('\\', "/");
        let name = format!("{prefix}/{rel}");
        let md = e.metadata()?;
        let opts = SimpleFileOptions::default()
            .last_modified_time(zip_time(md.modified().ok()))
            .large_file(md.len() >= u32::MAX as u64);
        if e.file_type().is_dir() {
            zip.add_directory(name, opts)?;
        } else if e.file_type().is_file() {
            let method = if is_precompressed(&rel) {
                CompressionMethod::Stored
            } else {
                CompressionMethod::Deflated
            };
            zip.start_file(name, opts.compression_method(method))?;
            let mut f =
                File::open(e.path()).context(format!("Cannot read {}", e.path().display()))?;
            bytes += io::copy(&mut f, &mut zip)?;
            done += 1;
            progress(done, total);
        }
    }
    if let Some(mut m) = manifest {
        m.file_count = done;
        m.size = bytes;
        zip.start_file(MANIFEST_NAME, SimpleFileOptions::default())?;
        serde_json::to_writer_pretty(&mut zip, &m)?;
    }
    let buf = zip.finish()?;
    let file = buf.into_inner().map_err(|e| AppError::new(e.to_string()))?;
    file.sync_all()?;
    Ok(())
}

fn is_precompressed(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "avif"
            | "mp3"
            | "ogg"
            | "oga"
            | "m4a"
            | "aac"
            | "flac"
            | "opus"
            | "mp4"
            | "m4v"
            | "webm"
            | "mkv"
            | "zip"
            | "7z"
            | "gz"
            | "woff"
            | "woff2"
    )
}

fn zip_time(t: Option<std::time::SystemTime>) -> zip::DateTime {
    t.map(DateTime::<Local>::from)
        .and_then(|d| {
            zip::DateTime::from_date_and_time(
                d.year().clamp(1980, 2107) as u16,
                d.month() as u8,
                d.day() as u8,
                d.hour() as u8,
                d.minute() as u8,
                d.second() as u8,
            )
            .ok()
        })
        .unwrap_or_default()
}

/// Deletes the oldest backups so at most `keep_last` remain (0 = keep all).
/// Returns the deleted files.
pub fn apply_retention(backup_dir: &Path, world_id: &str, keep_last: u32) -> Result<Vec<PathBuf>> {
    if keep_last == 0 {
        return Ok(Vec::new());
    }
    let mut deleted = Vec::new();
    for b in list_backups(backup_dir, world_id)
        .into_iter()
        .skip(keep_last as usize)
    {
        let p = PathBuf::from(&b.path);
        fs::remove_file(&p).context(format!("Cannot delete old backup {}", b.file_name))?;
        deleted.push(p);
    }
    Ok(deleted)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn make_world(worlds: &Path, id: &str) -> PathBuf {
        let w = worlds.join(id);
        fs::create_dir_all(w.join("data/actors")).unwrap();
        fs::create_dir_all(w.join("scenes")).unwrap();
        fs::create_dir_all(w.join("assets/empty")).unwrap();
        fs::write(
            w.join("world.json"),
            format!(r#"{{"title":"T {id}","system":"dnd5e"}}"#),
        )
        .unwrap();
        fs::write(w.join("data/actors/000003.log"), vec![7u8; 10_000]).unwrap();
        fs::write(w.join("data/actors/LOCK"), b"").unwrap();
        fs::write(w.join("scenes/bg.webp"), b"RIFF....WEBP").unwrap();
        w
    }

    #[test]
    fn file_names_roundtrip() {
        let t = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 5).unwrap();
        let n = backup_file_name("my-world_2", t, "Gaming-PC");
        assert_eq!(n, "my-world_2__2026-11-24T09-30-05Z__Gaming-PC.zip");
        let (id, c, h) = parse_backup_file_name(&n).unwrap();
        assert_eq!((id.as_str(), c, h.as_str()), ("my-world_2", t, "Gaming-PC"));
        assert!(parse_backup_file_name("random.zip").is_none());
        assert_eq!(sanitize_host("Fabians-MacBook.local"), "Fabians-MacBook");
        assert_eq!(sanitize_host("my_pc__x"), "my-pc--x");
        assert_eq!(sanitize_host("..."), "pc");
    }

    #[test]
    fn backup_writes_zip_with_manifest_and_ignores_partials() {
        let d = tempfile::tempdir().unwrap();
        let worlds = d.path().join("worlds");
        let cloud = d.path().join("cloud");
        make_world(&worlds, "alpha");
        let calls = std::cell::Cell::new(0u64);
        let b = create_backup(&worlds, &cloud, "alpha", "pc1", 10, &|_, _| {
            calls.set(calls.get() + 1)
        })
        .unwrap();
        assert!(calls.get() >= 4);
        assert!(Path::new(&b.path).is_file());
        let m = read_manifest(Path::new(&b.path)).unwrap();
        assert_eq!(m.world.title.as_deref(), Some("T alpha"));
        assert_eq!(m.file_count, 4);
        assert_eq!(m.host, "pc1");

        let mut ar = ZipArchive::new(File::open(&b.path).unwrap()).unwrap();
        assert!(ar.by_name("alpha/world.json").is_ok());
        assert!(ar.by_name("alpha/assets/empty/").is_ok());

        fs::write(
            cloud
                .join("alpha")
                .join("alpha__2030-01-01T00-00-00Z__x.zip.partial"),
            b"x",
        )
        .unwrap();
        fs::write(cloud.join("alpha").join("notes.txt"), b"x").unwrap();
        let list = list_backups(&cloud, "alpha");
        assert_eq!(list.len(), 1);
        assert_eq!(list_backups(&cloud, "alpha")[0].file_name, b.file_name);
        assert_eq!(list_backup_world_ids(&cloud), vec!["alpha"]);
    }

    #[test]
    fn retention_keeps_newest_n() {
        let d = tempfile::tempdir().unwrap();
        let cloud = d.path();
        fs::create_dir_all(cloud.join("w")).unwrap();
        for day in 1..=5 {
            let t = Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap();
            fs::write(cloud.join("w").join(backup_file_name("w", t, "pc")), b"z").unwrap();
        }
        let deleted = apply_retention(cloud, "w", 2).unwrap();
        assert_eq!(deleted.len(), 3);
        let left = list_backups(cloud, "w");
        assert_eq!(left.len(), 2);
        assert_eq!(
            left[0].created,
            Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap()
        );
        assert_eq!(
            left[1].created,
            Utc.with_ymd_and_hms(2026, 1, 4, 0, 0, 0).unwrap()
        );
        assert!(apply_retention(cloud, "w", 0).unwrap().is_empty());
    }
}
