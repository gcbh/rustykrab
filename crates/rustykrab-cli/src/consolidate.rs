//! Offline staging only. The caller stops and drains every legacy owner first;
//! this command never swaps databases, starts channels or enables schedules.
use anyhow::{bail, Context, Result};
use rustykrab_control::lock::{ControllerLock, LOCK_FILE};
use rustykrab_memory::storage::{MemoryImportReport, SqliteMemoryStorage};
use rustykrab_store::{InstanceImportReport, Store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    /// Required operator acknowledgment: old schedulers may predate the lock.
    offline: bool,
    destination: PathBuf,
    destination_key_env: String,
    sources: Vec<Source>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    name: String,
    data_dir: PathBuf,
    /// Omitted means read the existing OS master key, never generate a key.
    master_key_env: Option<String>,
}
#[derive(Debug, Serialize)]
struct Receipt {
    destination_agent: Uuid,
    database_snapshots: std::collections::BTreeMap<String, String>,
    store: Vec<InstanceImportReport>,
    memory: Vec<MemoryImportReport>,
    imported_schedules_enabled: bool,
}

pub async fn run(args: &[String]) -> Result<()> {
    if args.len() != 5 || args[0] != "stage" || args[1] != "--plan" || args[3] != "--output" {
        bail!("usage: rustykrab-cli consolidate stage --plan PLAN.json --output NEW_DIRECTORY");
    }
    let plan: Plan =
        serde_json::from_slice(&std::fs::read(&args[2])?).context("invalid consolidation plan")?;
    let receipt = stage(plan, Path::new(&args[4])).await?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn key(env: Option<&str>) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = match env {
        Some(name) => {
            if !valid_name(name) {
                bail!("invalid master-key environment name");
            }
            let raw = Zeroizing::new(
                std::env::var(name)
                    .context("required master-key environment variable is absent")?,
            );
            hex::decode(raw.trim()).map_err(|_| anyhow::anyhow!("master key must be hex"))?
        }
        None => rustykrab_store::keychain::get_master_key()?
            .context("existing OS master key is unavailable; specify a key environment variable")?,
    };
    if bytes.len() != 32 {
        bail!("master key must be 32 bytes");
    }
    Ok(Zeroizing::new(bytes))
}
fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)?;
    }
    Ok(())
}
fn copy_asset(source: &Path, target: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(source) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if meta.file_type().is_symlink() {
        bail!("asset symlinks require explicit reconciliation");
    }
    if meta.is_dir() {
        if !target.exists() {
            private_dir(target)?;
        }
        if !target.is_dir() {
            bail!("conflicting asset type");
        }
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_asset(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        if target.exists() {
            if !target.is_file() || std::fs::read(source)? != std::fs::read(target)? {
                bail!("conflicting asset; no destination asset was replaced");
            }
        } else {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options.open(target)?;
            output.write_all(&std::fs::read(source)?)?;
            output.sync_all()?;
        }
    } else {
        bail!("unsupported asset type");
    }
    Ok(())
}
async fn stage(plan: Plan, output: &Path) -> Result<Receipt> {
    if !plan.offline {
        bail!("stop and drain every scheduler and channel owner before offline staging");
    }
    if plan.sources.is_empty() {
        bail!("a consolidation plan must name a source");
    }
    let destination = std::fs::canonicalize(&plan.destination)?;
    let parent = std::fs::canonicalize(output.parent().context("output requires a parent")?)?;
    let output = parent.join(
        output
            .file_name()
            .context("output requires a directory name")?,
    );
    if output.exists() {
        bail!("output directory must be new");
    }
    let mut names = BTreeSet::new();
    let mut roots = BTreeSet::from([destination.clone()]);
    let mut sources = Vec::new();
    for source in plan.sources {
        if source.name == "destination"
            || !valid_name(&source.name)
            || !names.insert(source.name.clone())
        {
            bail!("source names must be unique simple names");
        }
        let root = std::fs::canonicalize(&source.data_dir)?;
        if roots
            .iter()
            .any(|existing| root.starts_with(existing) || existing.starts_with(&root))
        {
            bail!("source and destination directories must be distinct and non-nested");
        }
        roots.insert(root.clone());
        sources.push((source, root));
    }
    if roots
        .iter()
        .any(|root| output.starts_with(root) || root.starts_with(&output))
    {
        bail!("stage directory must be outside all data directories");
    }
    let mut locks = Vec::new();
    for root in &roots {
        locks.push(
            ControllerLock::try_acquire(&root.join(LOCK_FILE))?
                .context("a controller is still running; stop it before staging")?,
        );
    }
    let target_key = key(Some(&plan.destination_key_env))?;
    let agent: Uuid = std::fs::read_to_string(destination.join("agent_id"))?
        .trim()
        .parse()
        .context("destination agent_id must be valid")?;
    private_dir(&output)?;
    private_dir(&output.join("db"))?;
    private_dir(&output.join("snapshots"))?;
    private_dir(&output.join("normalized"))?;
    let original = output.join("snapshots/destination");
    private_dir(&original)?;
    private_dir(&original.join("db"))?;
    let mut receipt = Receipt {
        destination_agent: agent,
        database_snapshots: Default::default(),
        store: vec![],
        memory: vec![],
        imported_schedules_enabled: false,
    };
    receipt.database_snapshots.insert(
        "destination/store".into(),
        Store::snapshot_database(
            &destination.join("db/store.db"),
            &original.join("db/store.db"),
        )?,
    );
    receipt.database_snapshots.insert(
        "destination/memory".into(),
        Store::snapshot_database(&destination.join("memory.db"), &original.join("memory.db"))?,
    );
    Store::snapshot_database(&original.join("db/store.db"), &output.join("db/store.db"))?;
    Store::snapshot_database(&original.join("memory.db"), &output.join("memory.db"))?;
    for name in ["agent_id", "soul.md", "skills", "wiki"] {
        copy_asset(&destination.join(name), &original.join(name))?;
        copy_asset(&original.join(name), &output.join(name))?;
    }
    let target = Store::open(output.join("db"), target_key.to_vec())?;
    let target_memory = SqliteMemoryStorage::open(output.join("memory.db"))?;
    for (source, root) in sources {
        let snapshot = output.join("snapshots").join(&source.name);
        private_dir(&snapshot)?;
        private_dir(&snapshot.join("db"))?;
        receipt.database_snapshots.insert(
            format!("{}/store", source.name),
            Store::snapshot_database(&root.join("db/store.db"), &snapshot.join("db/store.db"))?,
        );
        receipt.database_snapshots.insert(
            format!("{}/memory", source.name),
            Store::snapshot_database(&root.join("memory.db"), &snapshot.join("memory.db"))?,
        );
        for name in ["agent_id", "soul.md", "skills", "wiki"] {
            copy_asset(&root.join(name), &snapshot.join(name))?;
        }
        // Normalize private copies, never the original data directories.
        let source_key = key(source.master_key_env.as_deref())?;
        let normalized_root = output.join("normalized").join(&source.name);
        private_dir(&normalized_root)?;
        private_dir(&normalized_root.join("db"))?;
        Store::snapshot_database(
            &snapshot.join("db/store.db"),
            &normalized_root.join("db/store.db"),
        )?;
        Store::snapshot_database(
            &snapshot.join("memory.db"),
            &normalized_root.join("memory.db"),
        )?;
        let normalized = Store::open(normalized_root.join("db"), source_key.to_vec())?;
        receipt
            .store
            .push(target.import_instance(&normalized, &source.name).await?);
        let normalized_memory = SqliteMemoryStorage::open(normalized_root.join("memory.db"))?;
        receipt.memory.push(
            target_memory
                .import_instance(&normalized_memory, &source.name, agent)
                .await?,
        );
        for name in ["skills", "wiki"] {
            copy_asset(&snapshot.join(name), &output.join(name))?;
        }
    }
    drop(target);
    drop(target_memory);
    let json = serde_json::to_vec_pretty(&receipt)?;
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output.join("receipt.json"))?;
    file.write_all(&json)?;
    file.sync_all()?;
    // Only a stage carrying this receipt is complete. A partial stage after an
    // error is retained for inspection and never becomes active automatically.
    drop(locks);
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stage_merges_private_copies_and_leaves_original_databases_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("target");
        let source = dir.path().join("source");
        for root in [&destination, &source] {
            std::fs::create_dir(root).unwrap();
            let store = Store::open(root.join("db"), vec![1; 32]).unwrap();
            let _memory = SqliteMemoryStorage::open(root.join("memory.db")).unwrap();
            let id = Uuid::new_v4();
            std::fs::write(root.join("agent_id"), id.to_string()).unwrap();
            store.conversations().create().await.unwrap();
        }
        let env = format!("RK_MIGRATION_TEST_{}", Uuid::new_v4().simple());
        std::env::set_var(&env, "01".repeat(32));
        let output = dir.path().join("stage");
        let receipt = stage(
            Plan {
                offline: true,
                destination: destination.clone(),
                destination_key_env: env.clone(),
                sources: vec![Source {
                    name: "main".into(),
                    data_dir: source.clone(),
                    master_key_env: Some(env.clone()),
                }],
            },
            &output,
        )
        .await
        .unwrap();
        std::env::remove_var(env);
        assert_eq!(receipt.store[0].inserted["conversations"], 1);
        assert!(!receipt.imported_schedules_enabled);
        assert!(output.join("receipt.json").is_file());
        assert_eq!(
            Store::open(destination.join("db"), vec![1; 32])
                .unwrap()
                .conversations()
                .list_ids()
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            Store::open(source.join("db"), vec![1; 32])
                .unwrap()
                .conversations()
                .list_ids()
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            Store::open(output.join("db"), vec![1; 32])
                .unwrap()
                .conversations()
                .list_ids()
                .await
                .unwrap()
                .len(),
            2
        );
        // Receipt hashes refer to the immutable pre-normalization snapshots.
        let copy = dir.path().join("hash-check.db");
        assert_eq!(
            Store::snapshot_database(&output.join("snapshots/main/db/store.db"), &copy).unwrap(),
            receipt.database_snapshots["main/store"]
        );
    }
    #[test]
    fn assets_refuse_overwrites_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "old").unwrap();
        std::fs::write(&b, "current").unwrap();
        assert!(copy_asset(&a, &b).is_err());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "current");
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&a, &link).unwrap();
            assert!(copy_asset(&link, &dir.path().join("copy")).is_err());
            std::fs::remove_file(&a).unwrap();
            assert!(copy_asset(&link, &dir.path().join("dangling-copy")).is_err());
        }
    }
    #[tokio::test]
    async fn a_live_controller_blocks_staging_before_creating_output() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let source = dir.path().join("source");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&source).unwrap();
        let _lock = ControllerLock::try_acquire(&target.join(LOCK_FILE))
            .unwrap()
            .unwrap();
        let output = dir.path().join("stage");
        let result = stage(
            Plan {
                offline: true,
                destination: target,
                destination_key_env: "unused".into(),
                sources: vec![Source {
                    name: "main".into(),
                    data_dir: source,
                    master_key_env: None,
                }],
            },
            &output,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("still running"));
        assert!(!output.exists());
    }
}
