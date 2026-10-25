use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use secret_service::{EncryptionType, SecretService};
use std::os::unix::fs::PermissionsExt;
use std::{collections::HashMap, path::PathBuf};
use transfer_core::{Auth, ConnectionProfile, JobSnapshot, SyncWorkflow};
use transfer_rclone::CredentialedProfile;

fn db_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join("nixe-transfer/connections.db")
}

fn connection() -> Result<Connection> {
    let path = db_path();
    let dir = path.parent().context("Invalid database path")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let conn = Connection::open(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS profiles (id TEXT PRIMARY KEY, metadata TEXT NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS jobs (id TEXT PRIMARY KEY, snapshot TEXT NOT NULL, updated_at INTEGER NOT NULL)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS workflows (id TEXT PRIMARY KEY, metadata TEXT NOT NULL)",
        [],
    )?;
    Ok(conn)
}

pub fn load() -> Result<Vec<ConnectionProfile>> {
    let conn = connection()?;
    let mut stmt = conn.prepare("SELECT metadata FROM profiles ORDER BY id")?;
    let values = stmt.query_map([], |row| row.get::<_, String>(0))?;
    values.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
}

pub fn save(profile: &ConnectionProfile) -> Result<()> {
    connection()?.execute(
        "INSERT OR REPLACE INTO profiles (id, metadata) VALUES (?1, ?2)",
        params![profile.id, serde_json::to_string(profile)?],
    )?;
    Ok(())
}

pub fn load_jobs() -> Result<Vec<JobSnapshot>> {
    let conn = connection()?;
    let mut stmt = conn.prepare("SELECT snapshot FROM jobs ORDER BY updated_at, id")?;
    let values = stmt.query_map([], |row| row.get::<_, String>(0))?;
    values.map(|v| Ok(serde_json::from_str(&v?)?)).collect()
}

pub fn save_job(job: &JobSnapshot) -> Result<()> {
    connection()?.execute(
        "INSERT INTO jobs (id, snapshot, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET snapshot=excluded.snapshot, updated_at=excluded.updated_at",
        params![job.id, serde_json::to_string(job)?, job.updated_at],
    )?;
    Ok(())
}

pub fn delete_job(id: &str) -> Result<()> {
    connection()?.execute("DELETE FROM jobs WHERE id=?1", params![id])?;
    Ok(())
}

pub fn load_workflows() -> Result<Vec<SyncWorkflow>> {
    let conn = connection()?;
    let mut stmt = conn.prepare("SELECT metadata FROM workflows ORDER BY id")?;
    let values = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut workflows: Vec<SyncWorkflow> = values
        .map(|v| Ok(serde_json::from_str(&v?)?))
        .collect::<Result<_>>()?;
    workflows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(workflows)
}

pub fn get_workflow(id: &str) -> Result<Option<SyncWorkflow>> {
    use rusqlite::OptionalExtension;
    let value: Option<String> = connection()?
        .query_row(
            "SELECT metadata FROM workflows WHERE id=?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|json| Ok(serde_json::from_str(&json)?))
        .transpose()
}

pub fn save_workflow(workflow: &SyncWorkflow) -> Result<SyncWorkflow> {
    workflow.validate()?;
    let mut saved = workflow.clone();
    saved.baseline_ready = get_workflow(&workflow.id)?
        .is_some_and(|old| old.baseline_ready && old.same_baseline(workflow));
    connection()?.execute(
        "INSERT INTO workflows (id, metadata) VALUES (?1, ?2) ON CONFLICT(id) DO UPDATE SET metadata=excluded.metadata",
        params![saved.id, serde_json::to_string(&saved)?],
    )?;
    Ok(saved)
}

pub fn mark_workflow_initialized(expected: &SyncWorkflow) -> Result<()> {
    let mut workflow = get_workflow(&expected.id)?.context("Workflow not found")?;
    if !workflow.same_baseline(expected) {
        return Ok(());
    }
    workflow.baseline_ready = true;
    connection()?.execute(
        "UPDATE workflows SET metadata=?2 WHERE id=?1",
        params![workflow.id, serde_json::to_string(&workflow)?],
    )?;
    Ok(())
}

pub fn delete_workflow(id: &str) -> Result<()> {
    connection()?.execute("DELETE FROM workflows WHERE id=?1", params![id])?;
    Ok(())
}

pub async fn save_password(profile_id: &str, password: &str) -> Result<()> {
    let service = SecretService::connect(EncryptionType::Dh).await?;
    let collection = service.get_default_collection().await?;
    if collection.is_locked().await? {
        collection.unlock().await?;
    }
    let mut attrs = HashMap::new();
    attrs.insert("application", "nixe-transfer");
    attrs.insert("profile", profile_id);
    collection
        .create_item(
            "Transfer connection password",
            attrs,
            password.as_bytes(),
            true,
            "text/plain",
        )
        .await?;
    Ok(())
}

pub async fn with_credentials(
    profiles: &[ConnectionProfile],
) -> Result<HashMap<String, CredentialedProfile>> {
    let needs_secret = profiles.iter().any(|p| p.auth == Auth::Password);
    let service = if needs_secret {
        Some(SecretService::connect(EncryptionType::Dh).await?)
    } else {
        None
    };
    let mut out = HashMap::new();
    for profile in profiles {
        let password = if profile.auth == Auth::Password {
            let service = service.as_ref().context("Secret Service unavailable")?;
            let mut attrs = HashMap::new();
            attrs.insert("application", "nixe-transfer");
            attrs.insert("profile", profile.id.as_str());
            let found = service.search_items(attrs).await?;
            let item = if let Some(item) = found.unlocked.into_iter().next() {
                item
            } else if let Some(item) = found.locked.into_iter().next() {
                item.unlock().await?;
                item
            } else {
                anyhow::bail!("Save the connection password again")
            };
            Some(String::from_utf8(item.get_secret().await?).context("Invalid saved password")?)
        } else {
            None
        };
        out.insert(
            profile.id.clone(),
            CredentialedProfile {
                profile: profile.clone(),
                password,
            },
        );
    }
    Ok(out)
}
