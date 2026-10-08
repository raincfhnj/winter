use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::{AppError, AppResult};

use super::manifest::BackupManifest;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct FileSnapshot {
    pub bytes: Vec<u8>,
    pub sha256: String,
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for &byte in digest.as_slice() {
        hex.push(HEX[usize::from(byte >> 4)] as char);
        hex.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    hex
}

/// Reads the file's bytes and SHA-256, rejecting symbolic links with a `Settings` error.
pub(crate) fn read_snapshot(path: &Path) -> AppResult<FileSnapshot> {
    reject_symlink(path)?;
    read_bytes(path).map(snapshot_from_bytes)
}

/// Reads a snapshot when the file exists, or `Ok(None)` when it does not.
///
/// Performs exactly one `symlink_metadata` probe: the symlink rejection and the byte read
/// share it, so symbolic links are still rejected with a `Settings` error while regular files
/// are inspected only once per read.
pub(crate) fn read_optional_snapshot(path: &Path) -> AppResult<Option<FileSnapshot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io("inspect integration file", path, error)),
    };
    reject_symlink_type(path, metadata.file_type())?;
    read_bytes(path).map(|bytes| Some(snapshot_from_bytes(bytes)))
}

fn read_bytes(path: &Path) -> AppResult<Vec<u8>> {
    fs::read(path).map_err(|error| AppError::io("read integration file", path, error))
}

fn snapshot_from_bytes(bytes: Vec<u8>) -> FileSnapshot {
    let sha256 = sha256_hex(&bytes);
    FileSnapshot { bytes, sha256 }
}

pub(crate) fn create_backup(
    state_dir: &Path,
    label: &str,
    source_path: &Path,
    snapshot: &FileSnapshot,
) -> AppResult<BackupManifest> {
    let backup_dir = state_dir.join("backups");
    fs::create_dir_all(&backup_dir)
        .map_err(|error| AppError::io("create integration backup directory", &backup_dir, error))?;
    let safe_label: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let path = unique_path(&backup_dir, &format!("{safe_label}.settings"), "json");
    if let Err(error) = write_verified_backup(&path, snapshot) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    prune_backup_directory(&backup_dir);

    Ok(BackupManifest {
        source_path: source_path.to_path_buf(),
        backup_path: path,
        sha256: snapshot.sha256.clone(),
        byte_len: snapshot.bytes.len() as u64,
    })
}

/// Writes the backup file and verifies it against the source snapshot.
///
/// Any error after the file is created — including a write interrupted by a full disk or a
/// lock — is surfaced to the caller, which deletes the file so no mismatching backup remains.
fn write_verified_backup(path: &Path, snapshot: &FileSnapshot) -> AppResult<()> {
    write_new_synced(path, &snapshot.bytes, "write integration backup")?;
    let verified = read_snapshot(path)?;
    if verified.sha256 != snapshot.sha256 {
        return Err(AppError::Settings {
            path: path.to_path_buf(),
            message: "backup verification checksum did not match source bytes".to_owned(),
        });
    }
    Ok(())
}

/// Maximum number of backup files kept per label by [`prune_backup_directory`].
///
/// Verified against every `create_backup` call site: `fragment` (fragment.rs:128) and the
/// per-channel labels `stable`/`preview`/`canary`/`unpackaged`/`portable`
/// (targets.rs:148) are the only manifest-referenced labels, each manifest record is written
/// in the same install round as the backup it points at, and each label has at most one
/// active record plus the just-created backup during a prune. `fragment-uninstall`
/// (fragment.rs:226) and `<channel>-uninstall` (targets.rs:233) discard their paths, and
/// `shell` (shell.rs:166/181/250) only reports its path without persisting a manifest, so
/// those labels have zero active records. Ten comfortably exceeds the maximum simultaneous
/// active count per label.
const KEEP_BACKUPS_PER_LABEL: usize = 10;

/// Housekeeping: keeps only the newest [`KEEP_BACKUPS_PER_LABEL`] backup files per label in
/// `backup_dir` and deletes the rest so `state_dir/backups` cannot grow without bound.
///
/// Files created by [`create_backup`] are named `{label}.settings-{pid}-{nanos}-{seq}.json`
/// through `unique_path`; sanitized labels never contain `.`, so grouping on the prefix
/// before the first `.settings-` recovers the label exactly, and files that do not match
/// that shape are never touched. Files are ranked by `symlink_metadata().modified()`,
/// breaking ties by descending filename; the just-created backup was written last, so it
/// always ranks inside the kept window and is never pruned. Deletions target only entries
/// directly inside `backup_dir` and use `remove_file`, which can never remove a directory
/// or anything outside the backups directory.
///
/// Housekeeping: errors are ignored so a locked old backup can never fail a fresh install —
/// a locked, permission-denied, or otherwise unprunable entry simply stays on disk until a
/// later prune succeeds.
fn prune_backup_directory(backup_dir: &Path) {
    let Ok(entries) = fs::read_dir(backup_dir) else {
        return;
    };
    let mut groups: HashMap<String, Vec<(SystemTime, String, PathBuf)>> = HashMap::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Some((label, _)) = name.split_once(".settings-") else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        let path = entry.path();
        let modified = fs::symlink_metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        groups
            .entry(label.to_owned())
            .or_default()
            .push((modified, name.to_owned(), path));
    }
    for (_, mut group) in groups {
        group.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        for (_, _, path) in group.into_iter().skip(KEEP_BACKUPS_PER_LABEL) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Replaces a file only if its current bytes still match the snapshot used to plan the edit.
///
/// The replacement is staged in a `.winterminalp-tmp-*` file inside the destination directory
/// and moved into place only after a second compare-and-swap check closes the long window spent
/// writing and syncing that staging file. Every error path — including a failed staging write —
/// deletes the staging file before the error is returned. Known residual race: a concurrent
/// writer can modify the destination between the second check and the move, and the move then
/// overwrites that writer's bytes with the already-verified replacement.
pub(crate) fn atomic_replace(
    path: &Path,
    expected_sha256: Option<&str>,
    replacement: &[u8],
) -> AppResult<String> {
    atomic_replace_with_writer(path, expected_sha256, replacement, |temp_path, bytes| {
        write_new_synced(temp_path, bytes, "write integration temporary file")
    })
}

/// Runs the `atomic_replace` sequence with an injectable staging writer so tests can simulate
/// a failing write (disk full, antivirus lock) and observe the staging-file cleanup.
fn atomic_replace_with_writer<W>(
    path: &Path,
    expected_sha256: Option<&str>,
    replacement: &[u8],
    write_temp: W,
) -> AppResult<String>
where
    W: FnOnce(&Path, &[u8]) -> AppResult<()>,
{
    assert_compare_and_swap(path, expected_sha256)?;
    let parent = path.parent().ok_or_else(|| AppError::Settings {
        path: path.to_path_buf(),
        message: "target file has no parent directory".to_owned(),
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| AppError::io("create integration target directory", parent, error))?;
    let temp_path = unique_path(parent, ".winterminalp-tmp", "json");
    let outcome = stage_replace(&temp_path, path, expected_sha256, replacement, write_temp);
    if outcome.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    outcome
}

/// Writes the staging file, re-checks the destination hash, moves the staging file into place,
/// and verifies the bytes read back from the destination.
fn stage_replace<W>(
    temp_path: &Path,
    path: &Path,
    expected_sha256: Option<&str>,
    replacement: &[u8],
    write_temp: W,
) -> AppResult<String>
where
    W: FnOnce(&Path, &[u8]) -> AppResult<()>,
{
    write_temp(temp_path, replacement)?;
    assert_compare_and_swap(path, expected_sha256)?;
    replace_with_native_atomic_move(temp_path, path)?;
    let expected_replacement_hash = sha256_hex(replacement);
    let verified = read_snapshot(path)?;
    if verified.sha256 != expected_replacement_hash || verified.bytes != replacement {
        return Err(AppError::Settings {
            path: path.to_path_buf(),
            message: "read-back verification failed after atomic replacement".to_owned(),
        });
    }
    Ok(expected_replacement_hash)
}

/// Removes `path` only when its current content still hashes to `expected_sha256`.
///
/// Verifies the content hash immediately before removal with no intervening fallible
/// operations; a concurrent swap between verification and removal is a known residual race,
/// because Windows provides no atomic compare-and-delete.
pub(crate) fn remove_if_hash(path: &Path, expected_sha256: &str) -> AppResult<bool> {
    let Some(snapshot) = read_optional_snapshot(path)? else {
        return Ok(false);
    };
    if snapshot.sha256 != expected_sha256 {
        return Ok(false);
    }
    fs::remove_file(path)
        .map_err(|error| AppError::io("remove managed integration file", path, error))?;
    Ok(true)
}

fn assert_compare_and_swap(path: &Path, expected_sha256: Option<&str>) -> AppResult<()> {
    let actual = read_optional_snapshot(path)?;
    let matches = match (expected_sha256, actual.as_ref()) {
        (None, None) => true,
        (Some(expected), Some(actual)) => expected == actual.sha256,
        _ => false,
    };
    if matches {
        return Ok(());
    }

    Err(AppError::SettingsConflict(format!(
        "{} changed after it was read; no bytes were replaced",
        path.display()
    )))
}

fn reject_symlink(path: &Path) -> AppResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::io("inspect integration file", path, error))?;
    reject_symlink_type(path, metadata.file_type())
}

fn reject_symlink_type(path: &Path, file_type: fs::FileType) -> AppResult<()> {
    if file_type.is_symlink() {
        return Err(AppError::Settings {
            path: path.to_path_buf(),
            message: "refusing to modify a symbolic link".to_owned(),
        });
    }
    Ok(())
}

fn write_new_synced(path: &Path, bytes: &[u8], operation: &'static str) -> AppResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| AppError::io(operation, path, error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AppError::io(operation, path, error))
}

fn unique_path(directory: &Path, stem: &str, extension: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    directory.join(format!(
        "{stem}-{}-{now}-{sequence}.{extension}",
        std::process::id()
    ))
}

#[cfg(windows)]
fn replace_with_native_atomic_move(source: &Path, destination: &Path) -> AppResult<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both buffers are valid, immutable, NUL-terminated UTF-16 paths for this call.
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| {
        AppError::Native(format!(
            "MoveFileExW failed from {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })
}

#[cfg(not(windows))]
fn replace_with_native_atomic_move(source: &Path, destination: &Path) -> AppResult<()> {
    fs::rename(source, destination)
        .map_err(|error| AppError::io("atomically replace integration file", destination, error))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn compare_and_swap_rejects_stale_snapshot() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let original = read_snapshot(&path).expect("fixture should be readable");
        fs::write(&path, b"{\"value\":2}").expect("fixture should be changed");

        let result = atomic_replace(&path, Some(&original.sha256), b"{\"value\":3}");

        assert!(matches!(result, Err(AppError::SettingsConflict(_))));
        assert_eq!(
            fs::read(&path).expect("fixture should remain readable"),
            b"{\"value\":2}"
        );
    }

    #[test]
    fn backup_preserves_the_exact_source_bytes() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        let bytes = b"\xef\xbb\xbf{\r\n  // user comment\r\n}\r\n";
        fs::write(&path, bytes).expect("fixture should be written");
        let snapshot = read_snapshot(&path).expect("fixture should be readable");

        let backup = create_backup(temp.path(), "stable", &path, &snapshot)
            .expect("backup should be created");

        assert_eq!(
            fs::read(&backup.backup_path).expect("backup should be readable"),
            bytes
        );
        assert_eq!(backup.sha256, sha256_hex(bytes));
    }

    fn staging_files(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".winterminalp-tmp-"))
            })
            .collect()
    }

    #[test]
    fn failed_staging_write_leaves_no_temp_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let original = read_snapshot(&path).expect("fixture should be readable");

        let result = atomic_replace_with_writer(
            &path,
            Some(&original.sha256),
            b"{\"value\":2}",
            |temp_path, _bytes| -> AppResult<()> {
                fs::write(temp_path, b"{\"partial\":").expect("staging file should be creatable");
                Err(AppError::InvalidConfiguration(
                    "injected disk-full failure".to_owned(),
                ))
            },
        );

        assert!(matches!(result, Err(AppError::InvalidConfiguration(_))));
        assert!(
            staging_files(temp.path()).is_empty(),
            "failed staging write must not leave a .winterminalp-tmp file"
        );
        assert_eq!(
            fs::read(&path).expect("fixture should remain readable"),
            b"{\"value\":1}"
        );
    }

    #[test]
    fn failed_second_check_deletes_the_staging_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let original = read_snapshot(&path).expect("fixture should be readable");

        let result = atomic_replace_with_writer(
            &path,
            Some(&original.sha256),
            b"{\"value\":3}",
            |temp_path, bytes| -> AppResult<()> {
                fs::write(temp_path, bytes).expect("staging file should be written");
                fs::write(&path, b"{\"value\":2}").expect("concurrent writer should win");
                Ok(())
            },
        );

        assert!(matches!(result, Err(AppError::SettingsConflict(_))));
        assert!(
            staging_files(temp.path()).is_empty(),
            "second-check failure must not leave a .winterminalp-tmp file"
        );
        assert_eq!(
            fs::read(&path).expect("fixture should remain readable"),
            b"{\"value\":2}"
        );
    }

    #[test]
    fn successful_replace_leaves_no_temp_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let original = read_snapshot(&path).expect("fixture should be readable");

        let hash = atomic_replace(&path, Some(&original.sha256), b"{\"value\":2}")
            .expect("replace should succeed");

        assert_eq!(hash, sha256_hex(b"{\"value\":2}"));
        assert_eq!(
            fs::read(&path).expect("fixture should remain readable"),
            b"{\"value\":2}"
        );
        assert!(staging_files(temp.path()).is_empty());
    }

    #[test]
    fn backup_verification_failure_deletes_the_mismatching_backup() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let mismatched = FileSnapshot {
            bytes: b"{\"value\":1}".to_vec(),
            sha256: "0".repeat(64),
        };

        let result = create_backup(temp.path(), "mismatched", &path, &mismatched);

        assert!(
            matches!(result, Err(AppError::Settings { .. })),
            "verification mismatch must fail the backup"
        );
        let backups = fs::read_dir(temp.path().join("backups"))
            .expect("backup directory should be readable")
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert!(
            backups.is_empty(),
            "mismatching backup file must be deleted, found {:?}",
            backups.iter().map(|entry| entry.path()).collect::<Vec<_>>()
        );
    }

    fn seed_backup(directory: &Path, label: &str, index: u32) -> PathBuf {
        fs::create_dir_all(directory).expect("backup directory should be created");
        let path = directory.join(format!("{label}.settings-1-{index:04}-0.json"));
        fs::write(&path, b"{\"seed\":true}").expect("backup fixture should be written");
        path
    }

    fn set_backup_mtime(path: &Path, time: SystemTime) {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("backup fixture should be openable to set its mtime");
        file.set_modified(time)
            .expect("backup fixture mtime should be settable");
    }

    fn backup_entries(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("backup directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect()
    }

    fn count_with_prefix(entries: &[PathBuf], prefix: &str) -> usize {
        entries
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .count()
    }

    #[test]
    fn prune_keeps_newest_ten_per_label() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let backups = temp.path().join("backups");
        let base = SystemTime::now() - Duration::from_secs(3_600);
        let seeded = (0..15u32)
            .map(|index| {
                let path = seed_backup(&backups, "alpha", index);
                set_backup_mtime(&path, base + Duration::from_secs(u64::from(index)));
                path
            })
            .collect::<Vec<_>>();

        prune_backup_directory(&backups);

        assert_eq!(
            backup_entries(&backups).len(),
            10,
            "only the newest ten alpha backups must remain"
        );
        for (index, path) in seeded.iter().enumerate() {
            assert_eq!(
                path.exists(),
                index >= 5,
                "alpha backup {index} retention is wrong"
            );
        }
    }

    #[test]
    fn prune_treats_labels_independently() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let backups = temp.path().join("backups");
        let base = SystemTime::now() - Duration::from_secs(3_600);
        for index in 0..15u32 {
            let path = seed_backup(&backups, "alpha", index);
            set_backup_mtime(&path, base + Duration::from_secs(u64::from(index)));
        }
        let beta = (0..3u32)
            .map(|index| seed_backup(&backups, "beta", index))
            .collect::<Vec<_>>();

        prune_backup_directory(&backups);

        let entries = backup_entries(&backups);
        assert_eq!(
            count_with_prefix(&entries, "alpha."),
            10,
            "alpha must be trimmed to the newest ten"
        );
        assert_eq!(
            count_with_prefix(&entries, "beta."),
            3,
            "beta must keep all three backups"
        );
        for path in &beta {
            assert!(path.exists(), "beta backups must survive an alpha prune");
        }
    }

    #[test]
    fn create_backup_never_prunes_its_own_fresh_file() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let source = temp.path().join("settings.json");
        fs::write(&source, b"{\"value\":1}").expect("fixture should be written");
        let snapshot = read_snapshot(&source).expect("fixture should be readable");
        let base = SystemTime::now() - Duration::from_secs(3_600);

        let mut created = Vec::new();
        for index in 0..11u32 {
            let backup = create_backup(temp.path(), "steady", &source, &snapshot)
                .expect("backup should be created");
            set_backup_mtime(
                &backup.backup_path,
                base + Duration::from_secs(u64::from(index)),
            );
            created.push(backup);
        }

        assert!(
            !created[0].backup_path.exists(),
            "the oldest backup must be pruned on the eleventh create"
        );
        for backup in &created[1..] {
            assert!(
                backup.backup_path.exists(),
                "every surviving backup including the fresh one must remain"
            );
        }
        assert_eq!(
            backup_entries(&temp.path().join("backups")).len(),
            10,
            "prune after each create must settle at the newest ten"
        );
    }

    #[test]
    fn create_backup_survives_an_unprunable_stale_entry() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let backups = temp.path().join("backups");
        fs::create_dir_all(&backups).expect("backup directory should be created");
        // A directory that ranks as the oldest alpha backup: remove_file fails on it.
        let blocked = backups.join("alpha.settings-0-0-0.json");
        fs::create_dir(&blocked).expect("blocked directory fixture should be created");
        fs::write(blocked.join("child.json"), b"[]").expect("fixture child should be written");
        for index in 0..10u32 {
            seed_backup(&backups, "alpha", index);
        }
        let source = temp.path().join("settings.json");
        fs::write(&source, b"{\"value\":1}").expect("fixture should be written");
        let snapshot = read_snapshot(&source).expect("fixture should be readable");

        let backup = create_backup(temp.path(), "alpha", &source, &snapshot)
            .expect("an unprunable stale entry must not fail a fresh backup");

        assert!(
            backup.backup_path.exists(),
            "the fresh backup must survive the prune"
        );
        assert!(
            blocked.is_dir(),
            "the undeletable directory must be left in place"
        );
    }

    #[test]
    fn optional_snapshot_reads_existing_and_missing_files() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("settings.json");

        assert!(
            read_optional_snapshot(&path)
                .expect("missing file should not error")
                .is_none()
        );

        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let snapshot = read_optional_snapshot(&path)
            .expect("existing file should be readable")
            .expect("existing file should produce a snapshot");
        assert_eq!(snapshot.bytes, b"{\"value\":1}");
        assert_eq!(snapshot.sha256, sha256_hex(b"{\"value\":1}"));
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_reads_reject_symbolic_links() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let target = temp.path().join("target.json");
        fs::write(&target, b"{\"value\":1}").expect("fixture should be written");
        let link = temp.path().join("link.json");
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &link) {
            eprintln!("skipping symlink rejection test: {error}");
            return;
        }

        let optional = read_optional_snapshot(&link).expect_err("symlink must be rejected");
        assert!(matches!(optional, AppError::Settings { .. }));
        let direct = read_snapshot(&link).expect_err("symlink must be rejected");
        assert!(matches!(direct, AppError::Settings { .. }));
    }

    #[test]
    fn remove_if_hash_deletes_only_matching_content() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let path = temp.path().join("fragment.json");
        fs::write(&path, b"{\"value\":1}").expect("fixture should be written");
        let snapshot = read_snapshot(&path).expect("fixture should be readable");

        assert!(!remove_if_hash(&path, "0").expect("stale hash should be rejected"));
        assert!(path.exists());

        assert!(remove_if_hash(&path, &snapshot.sha256).expect("matching hash should delete"));
        assert!(!path.exists());
        assert!(!remove_if_hash(&path, &snapshot.sha256).expect("missing file should be a no-op"));
    }
}
