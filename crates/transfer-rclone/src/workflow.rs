use super::*;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use transfer_core::{ConflictPolicy, SyncWorkflow, WorkflowPreview};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SnapshotEntry {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: i64,
    #[serde(rename = "ModTime")]
    modified: Option<String>,
}

fn workflow_dir(workflow: &SyncWorkflow) -> Result<PathBuf, TransferError> {
    workflow.validate()?;
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        });
    let dir = base.join("nixe-transfer/workflows").join(&workflow.id);
    std::fs::create_dir_all(&dir).map_err(|e| TransferError::Backend(e.to_string()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    Ok(dir)
}

fn filter_file(workflow: &SyncWorkflow, dir: &Path) -> Result<PathBuf, TransferError> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let mut rules = Vec::new();
    for pattern in &workflow.exclude {
        rules.push(format!("- {pattern}"));
    }
    for pattern in &workflow.include {
        rules.push(format!("+ {pattern}"));
    }
    if !workflow.include.is_empty() {
        rules.push("- **".into());
    }
    let content = if rules.is_empty() {
        String::new()
    } else {
        format!("{}\n", rules.join("\n"))
    };
    let target = dir.join("filters.txt");
    if std::fs::read_to_string(&target).ok().as_deref() == Some(content.as_str()) {
        return Ok(target);
    }
    let temporary = dir.join(format!(
        "filters-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temporary, content).map_err(|e| TransferError::Backend(e.to_string()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    std::fs::rename(&temporary, &target).map_err(|e| TransferError::Backend(e.to_string()))?;
    Ok(target)
}

fn hash<T: Serialize>(value: &T) -> Result<String, TransferError> {
    let bytes = serde_json::to_vec(value).map_err(|e| TransferError::Backend(e.to_string()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn append_bandwidth(cmd: &mut Command, workflow: &SyncWorkflow) {
    if let Some(limit) = workflow.bandwidth_kib {
        cmd.arg("--bwlimit").arg(format!("{limit}k"));
    }
}

async fn listing(
    location: &Location,
    profiles: &HashMap<String, CredentialedProfile>,
    filter: &Path,
) -> Result<Vec<SnapshotEntry>, TransferError> {
    let config = config_for([location], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let path = configure(&mut cmd, location, profiles, "NIXE_A").await?;
    cmd.arg("lsjson")
        .args(["--recursive", "--files-only", "--filter-from"])
        .arg(filter)
        .arg("--")
        .arg(path)
        .kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    let mut values: Vec<SnapshotEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|e| TransferError::Backend(format!("Cannot compare directories: {e}")))?;
    values.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(values)
}

fn initial_resolution(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::KeepBoth | ConflictPolicy::PreferLeft => "path1",
        ConflictPolicy::PreferRight => "path2",
        ConflictPolicy::PreferNewer => "newer",
    }
}

fn conflict_resolution(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::KeepBoth => "none",
        ConflictPolicy::PreferLeft => "path1",
        ConflictPolicy::PreferRight => "path2",
        ConflictPolicy::PreferNewer => "newer",
    }
}

fn append_bisync_args(
    cmd: &mut Command,
    workflow: &SyncWorkflow,
    dir: &Path,
    filter: &Path,
    dry_run: bool,
) {
    cmd.arg("bisync")
        .arg("--workdir")
        .arg(dir.join("bisync"))
        .arg("--filters-file")
        .arg(filter)
        .arg("--conflict-resolve")
        .arg(conflict_resolution(workflow.conflict))
        .args([
            "--conflict-loser",
            "num",
            "--use-json-log",
            "--color",
            "NEVER",
            "--resilient",
            "--recover",
            "--retries",
            "1",
        ]);
    if !workflow.baseline_ready {
        cmd.arg("--resync-mode")
            .arg(initial_resolution(workflow.conflict));
    }
    if workflow.checksum {
        cmd.arg("--compare").arg("size,modtime,checksum");
    }
    append_bandwidth(cmd, workflow);
    if dry_run {
        cmd.arg("--dry-run");
    } else {
        cmd.args([
            "--stats",
            "1s",
            "--stats-one-line",
            "--stats-log-level",
            "NOTICE",
        ]);
    }
}

pub async fn preview_workflow(
    workflow: &SyncWorkflow,
    profiles: &HashMap<String, CredentialedProfile>,
) -> Result<WorkflowPreview, TransferError> {
    workflow.validate()?;
    let dir = workflow_dir(workflow)?;
    let filter = filter_file(workflow, &dir)?;
    if workflow.mode != SyncMode::Bidirectional {
        let preview = preview_sync_with_options(
            &workflow.source,
            &workflow.destination,
            workflow.mode,
            profiles,
            Some(&filter),
            workflow.checksum,
        )
        .await?;
        let deletions = preview
            .changes
            .iter()
            .filter(|c| c.action == SyncAction::Delete)
            .count();
        let lines = preview
            .changes
            .iter()
            .map(|c| format!("{:?}  {}", c.action, c.path))
            .collect();
        return Ok(WorkflowPreview {
            lines,
            fingerprint: hash(&(workflow, &preview.fingerprint))?,
            deletions,
            initial: false,
        });
    }
    let left = listing(&workflow.source, profiles, &filter).await?;
    let right = listing(&workflow.destination, profiles, &filter).await?;
    if !workflow.baseline_ready && workflow.conflict == ConflictPolicy::KeepBoth {
        let left_paths: std::collections::HashSet<_> = left.iter().map(|e| &e.path).collect();
        if right.iter().any(|entry| left_paths.contains(&entry.path)) {
            return Err(TransferError::Conflict("Initial bidirectional run has matching file paths; choose an initial conflict preference".into()));
        }
    }
    let fingerprint = hash(&(workflow, &left, &right))?;
    let workdir = dir.join("bisync");
    std::fs::create_dir_all(&workdir).map_err(|e| TransferError::Backend(e.to_string()))?;
    std::fs::set_permissions(&workdir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    let config = config_for([&workflow.source, &workflow.destination], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let src = configure(&mut cmd, &workflow.source, profiles, "NIXE_A").await?;
    let dst = configure(&mut cmd, &workflow.destination, profiles, "NIXE_B").await?;
    append_bisync_args(&mut cmd, workflow, &dir, &filter, true);
    cmd.arg("--").arg(src).arg(dst).kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| TransferError::Backend(e.to_string()))?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    let mut lines = Vec::new();
    for raw in String::from_utf8_lossy(&output.stderr).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Some(msg) = value.get("msg").and_then(|v| v.as_str()) else {
            continue;
        };
        if [
            "File is",
            "Queue copy",
            "Queue delete",
            "Resync is copying",
            "Skipped copy",
            "Not deleting",
            "changes:",
            "conflict",
        ]
        .iter()
        .any(|part| msg.contains(part))
        {
            let object = value.get("object").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!(
                "{}{}",
                msg.replace('\n', " "),
                if object.is_empty() {
                    String::new()
                } else {
                    format!(" · {object}")
                }
            ));
        }
    }
    let deletions = lines
        .iter()
        .filter(|line| line.contains("Queue delete") || line.contains("Not deleting"))
        .count();
    if lines.is_empty() {
        lines.push("No changes reported by rclone.".into());
    }
    Ok(WorkflowPreview {
        lines,
        fingerprint,
        deletions,
        initial: !workflow.baseline_ready,
    })
}

pub async fn run_workflow(
    workflow: SyncWorkflow,
    expected_fingerprint: String,
    profiles: HashMap<String, CredentialedProfile>,
    events: mpsc::UnboundedSender<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    let result = run_workflow_inner(
        &workflow,
        &expected_fingerprint,
        &profiles,
        &events,
        &mut cancel,
    )
    .await;
    let _ = events.send(Event::Finished(result));
}

async fn run_workflow_inner(
    workflow: &SyncWorkflow,
    expected_fingerprint: &str,
    profiles: &HashMap<String, CredentialedProfile>,
    events: &mpsc::UnboundedSender<Event>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), TransferError> {
    if *cancel.borrow() {
        return Err(TransferError::Cancelled);
    }
    let preview = preview_workflow(workflow, profiles).await?;
    if preview.fingerprint != expected_fingerprint {
        return Err(TransferError::Conflict(
            "Workflow contents changed since preview; preview again".into(),
        ));
    }
    let dir = workflow_dir(workflow)?;
    let filter = filter_file(workflow, &dir)?;
    let config = config_for([&workflow.source, &workflow.destination], profiles)?;
    let mut cmd = base_command(config.as_deref());
    let src = configure(&mut cmd, &workflow.source, profiles, "NIXE_A").await?;
    let dst = configure(&mut cmd, &workflow.destination, profiles, "NIXE_B").await?;
    if workflow.mode == SyncMode::Bidirectional {
        append_bisync_args(&mut cmd, workflow, &dir, &filter, false);
    } else {
        cmd.arg(if workflow.mode == SyncMode::Mirror {
            "sync"
        } else {
            "copy"
        })
        .arg("--filter-from")
        .arg(&filter)
        .args([
            "--stats",
            "1s",
            "--stats-one-line",
            "--use-json-log",
            "--stats-log-level",
            "NOTICE",
            "--retries",
            "1",
            "--low-level-retries",
            "1",
        ]);
        if workflow.checksum {
            cmd.arg("--checksum");
        }
        if workflow.mode == SyncMode::Mirror {
            cmd.arg("--max-delete").arg(preview.deletions.to_string());
        }
        append_bandwidth(&mut cmd, workflow);
    }
    cmd.arg("--").arg(src).arg(dst);
    run_command(cmd, events, cancel).await
}
