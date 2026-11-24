//! Restoring a world, or the modules/systems/assets, from a backup zip.

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
            backup::write_zip_atomic(&world_dir, world_id, &[], None, &dest, &|_, _| {})
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

/// Folder (next to `Data`) that keeps the previous modules/systems/assets
/// after a data restore, so nothing is lost. Replaced on every data restore.
pub const PREVIOUS_DATA_DIR: &str = "Data-before-restore";

/// Replaces the top-level folders of Foundry's `Data` folder (modules,
/// systems, assets, ...) with the ones in a data backup. `Data/worlds` is
/// never touched. The replaced folders are moved to
/// `<user data>/Data-before-restore/`.
pub fn restore_data_backup(
    zip_path: &Path,
    data_dir: &Path,
    progress: backup::Progress,
) -> Result<PathBuf> {
    if foundry::is_data_in_use(data_dir, backup::DATA_SKIP) {
        return Err(AppError::new(
            "A world is open in Foundry. Return to setup in Foundry first.",
        ));
    }
    let file = File::open(zip_path).context(format!("Cannot open {}", zip_path.display()))?;
    let mut ar = ZipArchive::new(file)?;
    let plan = plan_entries(&mut ar, backup::DATA_PREFIX)?;
    if plan.iter().any(|(_, p, _)| top_name(p).is_some_and(|t| backup::DATA_SKIP.contains(&t.as_str()))) {
        return Err(AppError::new("This data backup contains worlds; refusing to restore it."));
    }
    let mut tops: Vec<String> = plan.iter().filter_map(|(_, p, _)| top_name(p)).collect();
    tops.sort();
    tops.dedup();
    if tops.is_empty() {
        return Err(AppError::new("This data backup is empty."));
    }

    fs::create_dir_all(data_dir).context(format!("Cannot create {}", data_dir.display()))?;
    let stamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
    let staging = data_dir.join(format!(".restoring-{stamp}"));
    if let Err(e) = extract_entries(&mut ar, &plan, &staging, progress) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let previous = data_dir
        .parent()
        .unwrap_or(data_dir)
        .join(PREVIOUS_DATA_DIR);
    let _ = fs::remove_dir_all(&previous);
    fs::create_dir_all(&previous)?;

    // Swap each top-level entry; roll everything back on the first failure.
    let mut moved_old: Vec<String> = Vec::new();
    let mut placed: Vec<String> = Vec::new();
    let mut failure = None;
    for name in &tops {
        let target = data_dir.join(name);
        if target.exists() || target.is_symlink() {
            if let Err(e) = fs::rename(&target, previous.join(name)) {
                failure = Some(format!("Could not move away {name} (is Foundry running?): {e}"));
                break;
            }
            moved_old.push(name.clone());
        }
        if let Err(e) = fs::rename(staging.join(name), &target) {
            failure = Some(format!("Could not move restored {name} into place: {e}"));
            break;
        }
        placed.push(name.clone());
    }
    if let Some(msg) = failure {
        for name in &placed {
            let _ = fs::remove_dir_all(data_dir.join(name)).or_else(|_| fs::remove_file(data_dir.join(name)));
        }
        for name in &moved_old {
            let _ = fs::rename(previous.join(name), data_dir.join(name));
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(AppError::new(msg));
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(previous)
}

fn top_name(p: &Path) -> Option<String> {
    match p.components().next()? {
        Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// (zip index, path relative to the world folder, is_dir). The leading
/// `<world_id>/` folder is stripped; the manifest is skipped. Any unsafe
/// path (absolute, `..`) aborts the restore.
fn plan_entries(ar: &mut ZipArchive<File>, prefix: &str) -> Result<Vec<(usize, PathBuf, bool)>> {
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
            .strip_prefix(prefix)
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

    fn make_data(data: &Path) {
        make_world(&data.join("worlds"), "w1");
        fs::create_dir_all(data.join("modules/dice/packs/macros")).unwrap();
        fs::write(data.join("modules/dice/module.json"), b"{}").unwrap();
        fs::write(data.join("modules/dice/packs/macros/LOCK"), b"").unwrap();
        fs::create_dir_all(data.join("systems/dnd5e")).unwrap();
        fs::write(data.join("systems/dnd5e/system.json"), b"{}").unwrap();
        fs::create_dir_all(data.join("assets/maps")).unwrap();
        fs::write(data.join("assets/maps/a.webp"), b"img").unwrap();
    }

    #[test]
    fn data_backup_and_restore_to_new_pc() {
        use crate::backup::{create_data_backup, DATA_ID};
        let d = tempfile::tempdir().unwrap();
        let pc1 = d.path().join("pc1/FoundryVTT/Data");
        let pc2 = d.path().join("pc2/FoundryVTT/Data");
        let cloud = d.path().join("cloud");
        make_data(&pc1);
        let b = create_data_backup(&pc1, &cloud, "pc1", 10, &|_, _| {}).unwrap();
        assert_eq!(b.world_id, DATA_ID);
        // Worlds are not in the data backup.
        let mut ar = ZipArchive::new(File::open(&b.path).unwrap()).unwrap();
        assert!(ar.by_name("Data/modules/dice/module.json").is_ok());
        assert!(ar.by_name("Data/worlds/w1/world.json").is_err());
        drop(ar);

        // PC2 has an old module and its own world; the world must survive.
        make_world(&pc2.join("worlds"), "mine");
        fs::create_dir_all(pc2.join("modules/old")).unwrap();
        fs::write(pc2.join("modules/old/module.json"), b"old").unwrap();
        let prev = restore_data_backup(Path::new(&b.path), &pc2, &|_, _| {}).unwrap();

        let mut expected = snapshot(&pc1);
        expected.retain(|(p, _)| !p.starts_with("worlds/"));
        let mut got = snapshot(&pc2);
        got.retain(|(p, _)| !p.starts_with("worlds/"));
        assert_eq!(got, expected);
        assert!(pc2.join("worlds/mine/world.json").is_file());
        assert!(prev.join("modules/old/module.json").is_file());
        let names: Vec<String> = fs::read_dir(&pc2)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn data_restore_rejects_worlds_in_zip() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("Data");
        make_data(&data);
        let z_path = d.path().join("x.zip");
        let mut z = zip::ZipWriter::new(File::create(&z_path).unwrap());
        z.start_file("Data/worlds/evil/world.json", SimpleFileOptions::default()).unwrap();
        z.write_all(b"{}").unwrap();
        z.finish().unwrap();
        assert!(restore_data_backup(&z_path, &data, &|_, _| {}).is_err());
        assert!(data.join("worlds/w1/world.json").is_file());
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
