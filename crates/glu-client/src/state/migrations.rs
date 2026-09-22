//! One-time, prefix-local state migrations. A completed ID is recorded only
//! after its work is durable. Rerunning a pending migration must be safe after
//! a crash at any point before that final record write.

use crate::state::{atomic_write, op_lock::OperationLock, store::InstalledStateStore};
use anyhow::{bail, Context, Result};
use glu_core::{Prefix, Target};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    path::PathBuf,
};

const SCHEMA: &str = "glu.migrations.v1";
const LEDGER_NAME: &str = "migrations.json";
const MAX_LEDGER_BYTES: u64 = 1_000_000;

struct Migration {
    id: &'static str,
    applies: fn(&Target) -> bool,
    run: fn(&Prefix) -> Result<()>,
}

const MIGRATIONS: &[Migration] = &[Migration {
    id: "golden-gate-receipt-tag-v1",
    applies: |target| target.0 == "arm64_golden_gate",
    run: |prefix| {
        InstalledStateStore::new(prefix.clone()).migrate_golden_gate_bottle_tags()?;
        Ok(())
    },
}];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema: String,
    completed: BTreeSet<String>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema: SCHEMA.to_string(),
            completed: BTreeSet::new(),
        }
    }
}

fn ledger_path(prefix: &Prefix) -> PathBuf {
    prefix.0.join("var/glu").join(LEDGER_NAME)
}

fn load_ledger(prefix: &Prefix) -> Result<Ledger> {
    let path = ledger_path(prefix);
    let parent_path = path.parent().expect("migration ledger has a parent");
    match fs::symlink_metadata(parent_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", parent_path.display()))
        }
        Ok(_) => {}
    }
    let parent = atomic_write::open_directory(parent_path)?;
    // Open relative to the verified directory and reject a ledger symlink.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c"migrations.json".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(Ledger::default());
        }
        return Err(error).with_context(|| format!("opening {}", path.display()));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        bail!("migration ledger is too large: {}", path.display());
    }
    let ledger: Ledger = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing migration ledger {}", path.display()))?;
    if ledger.schema != SCHEMA {
        bail!("unsupported migration ledger schema in {}", path.display());
    }
    Ok(ledger)
}

/// Cheap startup check. Once complete, it reads only the small ledger and
/// does not scan receipts or take the prefix lock.
pub fn has_pending(prefix: &Prefix, target: &Target) -> Result<bool> {
    if !MIGRATIONS
        .iter()
        .any(|migration| (migration.applies)(target))
    {
        return Ok(false);
    }
    let ledger = load_ledger(prefix)?;
    Ok(MIGRATIONS
        .iter()
        .any(|migration| (migration.applies)(target) && !ledger.completed.contains(migration.id)))
}

/// Caller must hold the prefix operation lock and run interrupted-install
/// recovery first. Rechecks the ledger after lock acquisition to handle a
/// concurrent process that finished while this one was starting.
pub fn run_pending(prefix: &Prefix, target: &Target, _lock: &OperationLock) -> Result<usize> {
    if !MIGRATIONS
        .iter()
        .any(|migration| (migration.applies)(target))
    {
        return Ok(0);
    }
    let mut ledger = load_ledger(prefix)?;
    let mut completed = 0;
    for migration in MIGRATIONS {
        if !(migration.applies)(target) || ledger.completed.contains(migration.id) {
            continue;
        }
        (migration.run)(prefix).with_context(|| format!("running migration {}", migration.id))?;
        ledger.completed.insert(migration.id.to_string());
        let bytes = serde_json::to_vec_pretty(&ledger).context("encoding migration ledger")?;
        atomic_write::replace_file_at_path(&ledger_path(prefix), &bytes)?;
        completed += 1;
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::op_lock;
    use serde_json::{json, Value};

    fn target(name: &str) -> Target {
        Target(name.to_string())
    }

    fn receipt(prefix: &Prefix, name: &str, tag: &str) -> PathBuf {
        let keg = prefix.0.join("Cellar").join(name).join("1.0");
        let path = keg.join(".glu/receipt.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = json!({
            "schema": "glu.install-receipt.v1",
            "status": "complete",
            "package": {
                "id": format!("pkg:test/{name}@1.0"),
                "package_key": format!("package:{name}"),
                "name": name,
                "aliases": [],
                "oldnames": [],
                "version": "1.0",
                "revision": 0,
                "keg_version": "1.0"
            },
            "artifact": {
                "id": format!("art:test/{name}@1.0"),
                "sha256": "a".repeat(64),
                "bottle_tag": tag,
                "cellar": ":any"
            },
            "paths": {
                "keg": keg,
                "opt": prefix.0.join("opt").join(name)
            },
            "links": { "opt_names": [] },
            "install": {
                "exposure": { "mode": "global" },
                "linked": true,
                "link_overwrite": [],
                "deps": [],
                "dependency_requirements": {}
            }
        });
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        path
    }

    fn read(path: &PathBuf) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn migrates_only_legacy_tag_and_records_completion() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let legacy = receipt(&prefix, "legacy", "aarch64_macos");
        let unrelated = receipt(&prefix, "unrelated", "arm64_sonoma");
        let before = read(&legacy);
        let unrelated_before = fs::read(&unrelated).unwrap();
        let golden_gate = target("arm64_golden_gate");

        assert!(has_pending(&prefix, &golden_gate).unwrap());
        let lock = op_lock::acquire(&prefix).unwrap();
        assert_eq!(run_pending(&prefix, &golden_gate, &lock).unwrap(), 1);
        let mut expected = before;
        expected["artifact"]["bottle_tag"] = json!("arm64_golden_gate");
        assert_eq!(read(&legacy), expected);
        assert_eq!(fs::read(&unrelated).unwrap(), unrelated_before);
        assert!(!has_pending(&prefix, &golden_gate).unwrap());
        assert_eq!(run_pending(&prefix, &golden_gate, &lock).unwrap(), 0);
    }

    #[test]
    fn resumes_a_partly_rewritten_set_when_marker_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let already_rewritten = receipt(&prefix, "first", "arm64_golden_gate");
        let pending = receipt(&prefix, "second", "aarch64_macos");
        let first_before = fs::read(&already_rewritten).unwrap();
        let golden_gate = target("arm64_golden_gate");
        let lock = op_lock::acquire(&prefix).unwrap();

        assert_eq!(run_pending(&prefix, &golden_gate, &lock).unwrap(), 1);
        assert_eq!(fs::read(&already_rewritten).unwrap(), first_before);
        assert_eq!(
            read(&pending)["artifact"]["bottle_tag"],
            "arm64_golden_gate"
        );
        assert!(!has_pending(&prefix, &golden_gate).unwrap());
    }

    #[test]
    fn invalid_receipt_blocks_all_writes_and_keeps_migration_pending() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let good = receipt(&prefix, "good", "aarch64_macos");
        let bad = receipt(&prefix, "bad", "aarch64_macos");
        fs::write(&bad, b"not JSON").unwrap();
        let original = fs::read(&good).unwrap();
        let golden_gate = target("arm64_golden_gate");
        let lock = op_lock::acquire(&prefix).unwrap();

        assert!(run_pending(&prefix, &golden_gate, &lock).is_err());
        assert_eq!(fs::read(&good).unwrap(), original);
        assert!(has_pending(&prefix, &golden_gate).unwrap());
        assert!(!ledger_path(&prefix).exists());
    }

    #[test]
    fn other_targets_do_not_rewrite_or_complete_golden_gate_migration() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = Prefix(temp.path().join("prefix"));
        let path = receipt(&prefix, "test", "aarch64_macos");
        let original = fs::read(&path).unwrap();
        let other = target("arm64_tahoe");
        let lock = op_lock::acquire(&prefix).unwrap();

        assert!(!has_pending(&prefix, &other).unwrap());
        assert_eq!(run_pending(&prefix, &other, &lock).unwrap(), 0);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!ledger_path(&prefix).exists());
        assert!(has_pending(&prefix, &target("arm64_golden_gate")).unwrap());
    }
}
