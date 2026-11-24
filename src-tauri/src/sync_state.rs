//! Per-computer memory of which backup each local world was last synced
//! with. This lets the UI tell "local changes" apart from "newer backup
//! from another PC" without comparing file dates across machines.

use crate::backup::BackupInfo;
use crate::error::Result;
use crate::settings::write_json_atomic;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncRecord {
    /// Backup file the local world matched at the last backup/restore.
    pub backup_file: String,
    pub backup_created: DateTime<Utc>,
    /// Fingerprint of the local world right after that backup/restore.
    pub fingerprint: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncState(pub BTreeMap<String, SyncRecord>);

impl SyncState {
    pub fn load(path: &Path) -> SyncState {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Local world matches the latest backup.
    Synced,
    /// Local world changed since the last backup.
    LocalNewer,
    /// A newer backup exists (e.g. made on another PC).
    BackupNewer,
    /// Local changes AND a newer backup from elsewhere.
    Conflict,
    /// World exists only in the backup folder.
    OnlyInBackup,
    /// World was never backed up.
    NoBackup,
}

pub struct LocalFacts<'a> {
    pub fingerprint: &'a str,
    pub modified: Option<DateTime<Utc>>,
}

/// Returns (status, guessed). `guessed` is true when there is no sync record
/// and the status is based on file dates only.
pub fn compute_status(
    local: Option<LocalFacts>,
    latest: Option<&BackupInfo>,
    record: Option<&SyncRecord>,
    this_host: &str,
) -> (Status, bool) {
    let (local, latest) = match (local, latest) {
        (None, _) => return (Status::OnlyInBackup, false),
        (Some(_), None) => return (Status::NoBackup, false),
        (Some(l), Some(b)) => (l, b),
    };
    if let Some(rec) = record {
        let local_changed = rec.fingerprint != local.fingerprint;
        let backup_changed =
            latest.file_name != rec.backup_file && latest.created > rec.backup_created;
        let s = match (local_changed, backup_changed) {
            (false, false) => Status::Synced,
            (true, false) => Status::LocalNewer,
            (false, true) => Status::BackupNewer,
            (true, true) => Status::Conflict,
        };
        return (s, false);
    }
    // No record yet: compare dates, with a minute of tolerance.
    let modified = local.modified.unwrap_or(DateTime::<Utc>::MIN_UTC);
    let s = if modified > latest.created + Duration::seconds(60) {
        Status::LocalNewer
    } else if latest.host.as_deref() == Some(this_host) {
        Status::Synced
    } else {
        Status::BackupNewer
    };
    (s, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn b(name: &str, day: u32) -> BackupInfo {
        BackupInfo {
            file_name: name.into(),
            path: name.into(),
            world_id: "w".into(),
            created: Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap(),
            host: Some("pc2".into()),
            size: 1,
        }
    }

    #[test]
    fn statuses() {
        let rec = SyncRecord {
            backup_file: "a".into(),
            backup_created: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            fingerprint: "f1".into(),
        };
        let l = |fp| {
            Some(LocalFacts {
                fingerprint: fp,
                modified: None,
            })
        };
        let a = b("a", 1);
        let newer = b("b", 2);
        assert_eq!(
            compute_status(l("f1"), Some(&a), Some(&rec), "pc1"),
            (Status::Synced, false)
        );
        assert_eq!(
            compute_status(l("f2"), Some(&a), Some(&rec), "pc1"),
            (Status::LocalNewer, false)
        );
        assert_eq!(
            compute_status(l("f1"), Some(&newer), Some(&rec), "pc1"),
            (Status::BackupNewer, false)
        );
        assert_eq!(
            compute_status(l("f2"), Some(&newer), Some(&rec), "pc1"),
            (Status::Conflict, false)
        );
        assert_eq!(
            compute_status(None, Some(&a), None, "pc1").0,
            Status::OnlyInBackup
        );
        assert_eq!(
            compute_status(l("f1"), None, None, "pc1").0,
            Status::NoBackup
        );
        // Guesses without a record.
        let old_local = Some(LocalFacts {
            fingerprint: "x",
            modified: Some(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap()),
        });
        assert_eq!(
            compute_status(old_local, Some(&a), None, "pc1"),
            (Status::BackupNewer, true)
        );
        let new_local = Some(LocalFacts {
            fingerprint: "x",
            modified: Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()),
        });
        assert_eq!(
            compute_status(new_local, Some(&a), None, "pc1"),
            (Status::LocalNewer, true)
        );
    }
}
