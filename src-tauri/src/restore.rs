//! Restoring a world from a backup zip.

use crate::backup::{self, MANIFEST_NAME};
use crate::error::{AppError, Context, Result};
use crate::foundry;
use chrono::Utc;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;

/// Number of local safety copies kept per world.
pub const KEEP_SAFETY_COPIES: u32 = 5;

/// Replaces `<worlds_dir>/<world_id>` with the content of `zip_path`.
///
/// 1. Refuses if Foundry has the world open.
/// 2. Checks every entry of the zip before touching anything.
/// 3. Zips the current local world into `safety_dir` (if it exists).
/// 4. Extracts to a hidden staging folder next to the world.
/// 5. Swaps the folders; rolls back if the swap fails.
pub fn restore_backup(
    zip_path: &Path,
    worlds_dir: &Path,
    world_id: &str,
    safety_dir: Option<&Path>,
    progress: backup::Progress,
) -> Result<()> {
    let world_dir = worlds_dir.join(world_id);
    if world_dir.exists() && foundry::is_world_in_use(&world_dir) {
        return Err(AppError::new(format!(
            "“{world_id}” is open in Foundry. Close the world (return to setup) first."
        )));
    }

    let file = File::open(zip_path).context(format!("Cannot open {}", zip_path.display()))?;
    let mut ar = ZipArchive::new(file)?;
    let plan = plan_entries(&mut ar, world_id)?;
    if !plan
        .iter()
        .any(|(_, p, is_dir)| !is_dir && p == Path::new("world.json"))
    {
        return Err(AppError::new("This backup does not contain a world.json."));
    }

    fs::create_dir_all(worlds_dir).context(format!("Cannot create {}", worlds_dir.display()))?;
    let stamp = Utc::now().format("%Y%m%d%H%M%S").to_string();

    // Safety copy of the current local world.
    if world_dir.exists() {
        if let Some(safety_dir) = safety_dir {
            fs::create_dir_all(safety_dir)?;
            let dest = safety_dir.join(format!("{world_id}__{stamp}.zip"));
            backup::write_zip_atomic(&world_dir, world_id, None, &dest, &|_, _| {})
                .context("Could not make a safety copy of the current world")?;
            prune_safety_copies(safety_dir, world_id, KEEP_SAFETY_COPIES);
        }
    }

    // Extract into a staging folder.
    let staging = worlds_dir.join(format!(".{world_id}.restoring-{stamp}"));
    let extract = extract_entries(&mut ar, &plan, &staging, progress);
    if let Err(e) = extract {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Swap folders.
    let old = worlds_dir.join(format!(".{world_id}.old-{stamp}"));
    let had_old = world_dir.exists();
    if had_old {
        if let Err(e) = fs::rename(&world_dir, &old) {
            let _ = fs::remove_dir_all(&staging);
            return Err(AppError::new(format!(
                "Could not replace the world folder (is Foundry or another program using it?): {e}"
            )));
        }
    }
    if let Err(e) = fs::rename(&staging, &world_dir) {
        if had_old {
            let _ = fs::rename(&old, &world_dir);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(AppError::new(format!(
            "Could not move the restored world into place: {e}"
        )));
    }
    if had_old {
        let _ = fs::remove_dir_all(&old);
    }
    Ok(())
}

/// (zip index, path relative to the world folder, is_dir). The leading
/// `<world_id>/` folder is stripped; the manifest is skipped. Any unsafe
/// path (absolute, `..`) aborts the restore.
fn plan_entries(ar: &mut ZipArchive<File>, world_id: &str) -> Result<Vec<(usize, PathBuf, bool)>> {
    let mut out = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let f = ar.by_index(i)?;
        let raw = f.name().map(|n| n.to_string()).unwrap_or_default();
        let Some(path) = f.enclosed_name() else {
            return Err(AppError::new(format!(
                "Unsafe path in backup, refusing to restore: {raw}"
            )));
        };
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(AppError::new(format!(
                "Unsafe path in backup, refusing to restore: {raw}"
            )));
        }
        if path == Path::new(MANIFEST_NAME) {
            continue;
        }
        let rel = path
            .strip_prefix(world_id)
            .map(Path::to_path_buf)
            .unwrap_or(path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        out.push((i, rel, f.is_dir()));
    }
    Ok(out)
}

fn extract_entries(
    ar: &mut ZipArchive<File>,
    plan: &[(usize, PathBuf, bool)],
    dest: &Path,
    progress: backup::Progress,
) -> Result<()> {
    fs::create_dir_all(dest)?;
    let total = plan.iter().filter(|(_, _, d)| !d).count() as u64;
    let mut done = 0;
    progress(0, total);
    for (i, rel, is_dir) in plan {
        let out = dest.join(rel);
        if *is_dir {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = ar.by_index(*i)?;
        let mut w = File::create(&out).context(format!("Cannot write {}", out.display()))?;
        io::copy(&mut f, &mut w).context(format!("Cannot extract {}", rel.display()))?;
        done += 1;
        progress(done, total);
    }
    Ok(())
}

fn prune_safety_copies(dir: &Path, world_id: &str, keep: u32) {
    let prefix = format!("{world_id}__");
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".zip"))
        })
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep as usize);
    for p in files.into_iter().take(excess) {
        let _ = fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{create_backup, tests::make_world};
    use std::io::Write;
    use walkdir::WalkDir;
    use zip::write::SimpleFileOptions;

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<(String, Vec<u8>)> = WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let rel = e
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                (rel, fs::read(e.path()).unwrap())
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn backup_then_restore_roundtrip_with_safety_copy() {
        let d = tempfile::tempdir().unwrap();
        let worlds = d.path().join("worlds");
        let cloud = d.path().join("cloud");
        let safety = d.path().join("safety");
        let w = make_world(&worlds, "alpha");
        let before = snapshot(&w);
        let b = create_backup(&worlds, &cloud, "alpha", "pc1", 10, &|_, _| {}).unwrap();

        // Change the world locally, then restore.
        fs::write(w.join("data/actors/000003.log"), b"changed").unwrap();
        fs::write(w.join("extra.txt"), b"new").unwrap();
        restore_backup(
            Path::new(&b.path),
            &worlds,
            "alpha",
            Some(&safety),
            &|_, _| {},
        )
        .unwrap();

        assert_eq!(snapshot(&w), before);
        assert!(w.join("assets/empty").is_dir());
        // Safety copy holds the changed version.
        let copies: Vec<_> = fs::read_dir(&safety).unwrap().collect();
        assert_eq!(copies.len(), 1);
        // No staging or old folders left behind.
        let names: Vec<String> = fs::read_dir(&worlds)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn restore_to_new_pc_without_local_world() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("pc1");
        let dst = d.path().join("pc2");
        let cloud = d.path().join("cloud");
        let w = make_world(&src, "beta");
        let b = create_backup(&src, &cloud, "beta", "pc1", 10, &|_, _| {}).unwrap();
        restore_backup(
            Path::new(&b.path),
            &dst,
            "beta",
            Some(&d.path().join("s")),
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(snapshot(&dst.join("beta")), snapshot(&w));
        assert!(!d.path().join("s").exists());
    }

    #[test]
    fn rejects_zip_slip_and_leaves_world_untouched() {
        let d = tempfile::tempdir().unwrap();
        let worlds = d.path().join("worlds");
        let w = make_world(&worlds, "gamma");
        let before = snapshot(&w);
        let evil = d.path().join("evil.zip");
        let mut z = zip::ZipWriter::new(File::create(&evil).unwrap());
        z.start_file("gamma/world.json", SimpleFileOptions::default())
            .unwrap();
        z.write_all(b"{}").unwrap();
        z.start_file("gamma/../../escaped.txt", SimpleFileOptions::default())
            .unwrap();
        z.write_all(b"pwned").unwrap();
        z.finish().unwrap();

        let err = restore_backup(&evil, &worlds, "gamma", None, &|_, _| {}).unwrap_err();
        assert!(err.0.contains("Unsafe path"), "{err}");
        assert!(!d.path().join("escaped.txt").exists());
        assert_eq!(snapshot(&w), before);
    }

    #[test]
    fn rejects_zip_without_world_json() {
        let d = tempfile::tempdir().unwrap();
        let worlds = d.path().join("worlds");
        let w = make_world(&worlds, "delta");
        let before = snapshot(&w);
        let z_path = d.path().join("x.zip");
        let mut z = zip::ZipWriter::new(File::create(&z_path).unwrap());
        z.start_file("delta/readme.txt", SimpleFileOptions::default())
            .unwrap();
        z.write_all(b"hi").unwrap();
        z.finish().unwrap();
        assert!(restore_backup(&z_path, &worlds, "delta", None, &|_, _| {}).is_err());
        assert_eq!(snapshot(&w), before);
    }

    #[test]
    fn safety_copies_are_pruned() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..8 {
            fs::write(d.path().join(format!("w__2026010100000{i}.zip")), b"").unwrap();
        }
        fs::write(d.path().join("other__1.zip"), b"").unwrap();
        prune_safety_copies(d.path(), "w", 5);
        let n = fs::read_dir(d.path()).unwrap().count();
        assert_eq!(n, 6);
        assert!(d.path().join("w__20260101000007.zip").exists());
        assert!(!d.path().join("w__20260101000002.zip").exists());
    }
}
