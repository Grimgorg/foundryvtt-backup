mod backup;
mod error;
mod foundry;
mod restore;
mod settings;
mod sync_state;

use backup::BackupInfo;
use chrono::{DateTime, Utc};
use error::{AppError, Result};
use serde::Serialize;
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use sync_state::{compute_status, LocalFacts, Status, SyncRecord, SyncState};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    sync_state_path: PathBuf,
    safety_dir: PathBuf,
    busy: Arc<AtomicBool>,
}

/// Makes sure only one backup/restore runs at a time.
struct BusyGuard(Arc<AtomicBool>);
impl BusyGuard {
    fn take(flag: &Arc<AtomicBool>) -> Result<BusyGuard> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err(AppError::new("Another backup or restore is still running."));
        }
        Ok(BusyGuard(flag.clone()))
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProgressEvent {
    world_id: String,
    op: &'static str,
    done: u64,
    total: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalInfo {
    path: String,
    modified: Option<DateTime<Utc>>,
    size: u64,
    file_count: u64,
    in_use: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorldEntry {
    id: String,
    title: String,
    system: Option<String>,
    system_version: Option<String>,
    core_version: Option<String>,
    last_played: Option<String>,
    local: Option<LocalInfo>,
    backups: Vec<BackupInfo>,
    status: Status,
    status_guessed: bool,
    last_sync: Option<SyncRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Overview {
    host: String,
    worlds_dir_ok: bool,
    backup_dir_ok: bool,
    worlds: Vec<WorldEntry>,
}

fn dir_of(opt: &Option<String>) -> Option<PathBuf> {
    opt.as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
}

fn require_dirs(s: &Settings) -> Result<(PathBuf, PathBuf)> {
    let worlds = dir_of(&s.worlds_dir)
        .filter(|p| p.is_dir())
        .ok_or_else(|| AppError::new("Choose your Foundry worlds folder first."))?;
    let backups = dir_of(&s.backup_dir)
        .filter(|p| p.is_dir())
        .ok_or_else(|| AppError::new("Choose your backup (cloud) folder first."))?;
    Ok((worlds, backups))
}

fn progress_emitter(app: AppHandle, world_id: String, op: &'static str) -> impl Fn(u64, u64) {
    move |done, total| {
        let step = (total / 200).max(1);
        if done == 0 || done == total || done % step == 0 {
            let _ = app.emit(
                "progress",
                ProgressEvent {
                    world_id: world_id.clone(),
                    op,
                    done,
                    total,
                },
            );
        }
    }
}

fn record_sync(state_path: &Path, worlds_dir: &Path, world_id: &str, b: &BackupInfo) {
    let fp = foundry::scan(&worlds_dir.join(world_id)).fingerprint;
    let mut st = SyncState::load(state_path);
    st.0.insert(
        world_id.to_string(),
        SyncRecord {
            backup_file: b.file_name.clone(),
            backup_created: b.created,
            fingerprint: fp,
        },
    );
    let _ = st.save(state_path);
}

fn build_overview(s: &Settings, sync_state_path: &Path) -> Overview {
    let host = backup::this_host();
    let worlds_dir = dir_of(&s.worlds_dir).filter(|p| p.is_dir());
    let backup_dir = dir_of(&s.backup_dir).filter(|p| p.is_dir());
    let state = SyncState::load(sync_state_path);

    let mut ids: Vec<String> = Vec::new();
    if let Some(w) = &worlds_dir {
        ids.extend(foundry::list_local_world_ids(w));
    }
    if let Some(b) = &backup_dir {
        ids.extend(backup::list_backup_world_ids(b));
    }
    ids.sort();
    ids.dedup();

    let mut worlds: Vec<WorldEntry> = ids
        .into_iter()
        .map(|id| {
            let local = worlds_dir
                .as_ref()
                .filter(|w| w.join(&id).join("world.json").is_file())
                .map(|w| foundry::inspect_local_world(w, &id));
            let backups = backup_dir
                .as_ref()
                .map(|b| backup::list_backups(b, &id))
                .unwrap_or_default();
            let record = state.0.get(&id);
            let (status, guessed) = compute_status(
                local.as_ref().map(|l| LocalFacts {
                    fingerprint: &l.fingerprint,
                    modified: l.modified,
                }),
                backups.first(),
                record,
                &host,
            );
            let meta = match &local {
                Some(l) => l.meta.clone(),
                None => backups
                    .first()
                    .and_then(|b| backup::read_manifest(Path::new(&b.path)))
                    .map(|m| m.world)
                    .unwrap_or_else(|| foundry::WorldMeta {
                        id: id.clone(),
                        ..Default::default()
                    }),
            };
            WorldEntry {
                title: meta.title.clone().unwrap_or_else(|| id.clone()),
                id: id.clone(),
                system: meta.system,
                system_version: meta.system_version,
                core_version: meta.core_version,
                last_played: meta.last_played,
                local: local.map(|l| LocalInfo {
                    path: l.path,
                    modified: l.modified,
                    size: l.size,
                    file_count: l.file_count,
                    in_use: l.in_use,
                }),
                backups,
                status,
                status_guessed: guessed,
                last_sync: record.cloned(),
            }
        })
        .collect();
    worlds.sort_by_key(|w| w.title.to_lowercase());

    Overview {
        host,
        worlds_dir_ok: worlds_dir.is_some(),
        backup_dir_ok: backup_dir.is_some(),
        worlds,
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::new(format!("Background task failed: {e}")))?
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(state: State<AppState>, settings: Settings) -> Result<Settings> {
    settings.save(&state.settings_path)?;
    *state.settings.lock().unwrap() = settings.clone();
    Ok(settings)
}

#[tauri::command]
fn detect_worlds_dir() -> Option<String> {
    foundry::detect_worlds_dir().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
async fn list_worlds(state: State<'_, AppState>) -> Result<Overview> {
    let s = state.settings.lock().unwrap().clone();
    let path = state.sync_state_path.clone();
    blocking(move || Ok(build_overview(&s, &path))).await
}

#[tauri::command]
async fn backup_world(
    app: AppHandle,
    state: State<'_, AppState>,
    world_id: String,
) -> Result<BackupInfo> {
    let guard = BusyGuard::take(&state.busy)?;
    let s = state.settings.lock().unwrap().clone();
    let state_path = state.sync_state_path.clone();
    blocking(move || {
        let _guard = guard;
        let (worlds, backups) = require_dirs(&s)?;
        let progress = progress_emitter(app, world_id.clone(), "backup");
        let host = backup::this_host();
        let b = backup::create_backup(&worlds, &backups, &world_id, &host, s.keep_last, &progress)?;
        record_sync(&state_path, &worlds, &world_id, &b);
        Ok(b)
    })
    .await
}

#[tauri::command]
async fn restore_world(
    app: AppHandle,
    state: State<'_, AppState>,
    world_id: String,
    file_name: String,
) -> Result<()> {
    if file_name.contains(['/', '\\'])
        || world_id.contains(['/', '\\'])
        || world_id.starts_with('.')
    {
        return Err(AppError::new("Invalid backup name."));
    }
    let guard = BusyGuard::take(&state.busy)?;
    let s = state.settings.lock().unwrap().clone();
    let state_path = state.sync_state_path.clone();
    let safety = state.safety_dir.clone();
    blocking(move || {
        let _guard = guard;
        let (worlds, backups) = require_dirs(&s)?;
        let info = backup::list_backups(&backups, &world_id)
            .into_iter()
            .find(|b| b.file_name == file_name)
            .ok_or_else(|| AppError::new("That backup no longer exists."))?;
        let progress = progress_emitter(app, world_id.clone(), "restore");
        restore::restore_backup(
            Path::new(&info.path),
            &worlds,
            &world_id,
            Some(&safety),
            &progress,
        )?;
        record_sync(&state_path, &worlds, &world_id, &info);
        Ok(())
    })
    .await
}

#[tauri::command]
fn open_folder(app: AppHandle, path: String) -> Result<()> {
    if !Path::new(&path).exists() {
        return Err(AppError::new(format!("Folder not found: {path}")));
    }
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| AppError::new(e.to_string()))
}

#[tauri::command]
fn safety_dir(state: State<AppState>) -> String {
    state.safety_dir.to_string_lossy().into_owned()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_local_data_dir()?;
            let settings_path = config_dir.join("settings.json");
            let mut settings = Settings::load(&settings_path);
            // First start: try to find Foundry's worlds folder automatically.
            if settings.worlds_dir.is_none() {
                if let Some(p) = foundry::detect_worlds_dir() {
                    settings.worlds_dir = Some(p.to_string_lossy().into_owned());
                    let _ = settings.save(&settings_path);
                }
            }
            app.manage(AppState {
                settings: Mutex::new(settings),
                settings_path,
                sync_state_path: data_dir.join("sync-state.json"),
                safety_dir: data_dir.join("pre-restore"),
                busy: Arc::new(AtomicBool::new(false)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            detect_worlds_dir,
            list_worlds,
            backup_world,
            restore_world,
            open_folder,
            safety_dir
        ])
        .run(tauri::generate_context!())
        .expect("error while running FoundryVTT Backup");
}
