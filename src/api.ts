import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  backupDir: string | null;
  worldsDir: string | null;
  keepLast: number;
}

export interface BackupInfo {
  fileName: string;
  path: string;
  worldId: string;
  created: string;
  host: string | null;
  size: number;
}

export interface LocalInfo {
  path: string;
  modified: string | null;
  size: number;
  fileCount: number;
  inUse: boolean;
}

export type Status =
  | "synced"
  | "localNewer"
  | "backupNewer"
  | "conflict"
  | "onlyInBackup"
  | "noBackup";

export interface WorldEntry {
  /** "data" = modules, systems & assets; "world" = a single world. */
  kind: "world" | "data";
  id: string;
  title: string;
  system: string | null;
  systemVersion: string | null;
  coreVersion: string | null;
  lastPlayed: string | null;
  local: LocalInfo | null;
  backups: BackupInfo[];
  status: Status;
  statusGuessed: boolean;
}

export interface Overview {
  host: string;
  worldsDirOk: boolean;
  backupDirOk: boolean;
  worlds: WorldEntry[];
}

export interface ProgressEvent {
  worldId: string;
  op: "backup" | "restore";
  done: number;
  total: number;
}

export const api = {
  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<Settings>("save_settings", { settings }),
  detectWorldsDir: () => invoke<string | null>("detect_worlds_dir"),
  listWorlds: () => invoke<Overview>("list_worlds"),
  backupWorld: (worldId: string) => invoke<BackupInfo>("backup_world", { worldId }),
  restoreWorld: (worldId: string, fileName: string) =>
    invoke<void>("restore_world", { worldId, fileName }),
  openFolder: (path: string) => invoke<void>("open_folder", { path }),
  userDataDir: () => invoke<string | null>("user_data_dir"),
  safetyDir: () => invoke<string>("safety_dir"),
};
