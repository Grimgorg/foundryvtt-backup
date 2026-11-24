use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const DEFAULT_KEEP_LAST: u32 = 10;

/// User settings, stored as JSON in the OS app-config folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Cloud-synced folder that holds the backups.
    pub backup_dir: Option<String>,
    /// Foundry `Data/worlds` folder on this computer.
    pub worlds_dir: Option<String>,
    /// How many backups to keep per world (0 = keep all).
    pub keep_last: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            backup_dir: None,
            worlds_dir: None,
            keep_last: DEFAULT_KEEP_LAST,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }
}

/// Writes JSON to `path` via a temp file + rename, so a crash never leaves a
/// half-written file behind.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/settings.json");
        assert_eq!(Settings::load(&p), Settings::default());
        let s = Settings {
            backup_dir: Some("/cloud/fvtt".into()),
            worlds_dir: Some("/data/worlds".into()),
            keep_last: 3,
        };
        s.save(&p).unwrap();
        assert_eq!(Settings::load(&p), s);
        // Missing fields fall back to defaults.
        fs::write(&p, r#"{"backupDir":"/x"}"#).unwrap();
        let l = Settings::load(&p);
        assert_eq!(l.backup_dir.as_deref(), Some("/x"));
        assert_eq!(l.keep_last, DEFAULT_KEEP_LAST);
    }
}
