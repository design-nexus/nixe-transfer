use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use transfer_core::{
    JobOperation, JobSnapshot, JobStatus, Location, SyncMode, SyncPreview, SyncWorkflow,
    TransferError, TransferEvent, WorkflowPreview,
};
use transfer_ipc::{BUS, PATH};
use transfer_storage as store;
use zbus::{Connection, interface, object_server::SignalEmitter};

struct State {
    jobs: Mutex<HashMap<String, JobSnapshot>>,
    scheduled: Mutex<HashSet<String>>,
    active: Mutex<HashMap<String, watch::Sender<bool>>>,
    busy_destinations: Mutex<HashSet<String>>,
    permits: Semaphore,
    bus: OnceLock<Connection>,
}
#[derive(Clone)]
struct Service(Arc<State>);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn error(e: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(e.to_string())
}
fn destination_key(location: &Location) -> String {
    match location {
        Location::Local(path) => {
            let normalized = path
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .zip(path.file_name())
                .map(|(parent, name)| parent.join(name))
                .unwrap_or_else(|| path.clone());
            format!("local:{}", normalized.display())
        }
        _ => serde_json::to_string(location).unwrap_or_default(),
    }
}

fn validate_sync(source: &Location, destination: &Location) -> Result<(), TransferError> {
    source.validate()?;
    destination.validate()?;
    if source == destination {
        return Err(TransferError::Conflict(
            "Source and destination are the same".into(),
        ));
    }
    if let Location::Local(path) = source
        && !path.is_dir()
    {
        return Err(TransferError::InvalidPath);
    }
    if let Location::Local(path) = destination
        && !path.is_dir()
    {
        return Err(TransferError::InvalidPath);
    }
    match (source, destination) {
        (Location::Local(a), Location::Local(b)) => {
            let a = a.canonicalize().map_err(|_| TransferError::InvalidPath)?;
            let b = b.canonicalize().map_err(|_| TransferError::InvalidPath)?;
            if a.starts_with(&b) || b.starts_with(&a) {
                return Err(TransferError::Conflict("Sync directories overlap".into()));
            }
        }
        (
            Location::Remote {
                profile_id: a,
                path: ap,
            },
            Location::Remote {
                profile_id: b,
                path: bp,
            },
        ) if a == b
            && (ap == bp
                || ap.starts_with(&format!("{bp}/"))
                || bp.starts_with(&format!("{ap}/"))
                || ap.is_empty()
                || bp.is_empty()) =>
        {
            return Err(TransferError::Conflict("Sync directories overlap".into()));
        }
        _ => {}
    }
    Ok(())
}

async fn preview_request(
    source: &Location,
    destination: &Location,
    mode: SyncMode,
) -> Result<SyncPreview, TransferError> {
    validate_sync(source, destination)?;
    let profiles = store::load().map_err(|e| TransferError::Backend(e.to_string()))?;
    let selected: Vec<_> = profiles
        .into_iter()
        .filter(|p| {
            matches!(source, Location::Remote { profile_id, .. } if profile_id == &p.id)
                || matches!(destination, Location::Remote { profile_id, .. } if profile_id == &p.id)
        })
        .collect();
    let credentials = store::with_credentials(&selected)
        .await
        .map_err(|e| TransferError::Authentication(e.to_string()))?;
    transfer_rclone::preview_sync(source, destination, mode, &credentials).await
}

async fn workflow_preview_request(
    workflow: &SyncWorkflow,
) -> Result<WorkflowPreview, TransferError> {
    workflow.validate()?;
    validate_sync(&workflow.source, &workflow.destination)?;
    let profiles = store::load().map_err(|e| TransferError::Backend(e.to_string()))?;
    let selected: Vec<_> = profiles.into_iter().filter(|p| {
        matches!(&workflow.source, Location::Remote { profile_id, .. } if profile_id == &p.id)
            || matches!(&workflow.destination, Location::Remote { profile_id, .. } if profile_id == &p.id)
    }).collect();
    let credentials = store::with_credentials(&selected)
        .await
        .map_err(|e| TransferError::Authentication(e.to_string()))?;
    transfer_rclone::preview_workflow(workflow, &credentials).await
}

fn parse_sync_mode(mode: &str) -> zbus::fdo::Result<SyncMode> {
    match mode {
        "Copy" => Ok(SyncMode::Copy),
        "Mirror" => Ok(SyncMode::Mirror),
        _ => Err(error("Unknown synchronization mode")),
    }
}

impl State {
    async fn changed(&self) {
        if let Some(bus) = self.bus.get()
            && let Ok(emitter) = SignalEmitter::new(bus, PATH)
        {
            let _ = Service::changed(&emitter).await;
        }
    }

    async fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut JobSnapshot) -> Result<()>,
    ) -> Result<JobSnapshot> {
        let mut jobs = self.jobs.lock().await;
        let job = jobs.get_mut(id).context("Job not found")?;
        change(job)?;
        job.updated_at = now();
        store::save_job(job)?;
        let snapshot = job.clone();
        drop(jobs);
        self.changed().await;
        Ok(snapshot)
    }

    async fn schedule(self: &Arc<Self>, id: String) {
        if !self.scheduled.lock().await.insert(id.clone()) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            state.run(id).await;
        });
    }

    async fn run(self: Arc<Self>, id: String) {
        loop {
            let queued = self
                .jobs
                .lock()
                .await
                .get(&id)
                .is_some_and(|j| j.status == JobStatus::Queued);
            if !queued {
                break;
            }
            let permit = match self.permits.acquire().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let keys = match self.jobs.lock().await.get(&id) {
                Some(job) => {
                    let mut keys = vec![destination_key(&job.destination)];
                    if matches!(&job.operation, JobOperation::Workflow { workflow, .. } if workflow.mode == SyncMode::Bidirectional)
                    {
                        keys.push(destination_key(&job.source));
                    }
                    keys
                }
                None => break,
            };
            let acquired = {
                let mut busy = self.busy_destinations.lock().await;
                if keys.iter().any(|key| busy.contains(key)) {
                    false
                } else {
                    for key in &keys {
                        busy.insert(key.clone());
                    }
                    true
                }
            };
            if !acquired {
                drop(permit);
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let started = self
                .update(&id, |job| {
                    if job.status != JobStatus::Queued {
                        bail!("Job no longer queued")
                    }
                    job.status = JobStatus::Running;
                    job.progress = "Connecting…".into();
                    job.bytes = 0;
                    job.total_bytes = None;
                    job.speed_bytes_per_sec = None;
                    job.eta_seconds = None;
                    Ok(())
                })
                .await;
            let Ok(job) = started else {
                let mut busy = self.busy_destinations.lock().await;
                for key in &keys {
                    busy.remove(key);
                }
                break;
            };
            let (cancel_tx, cancel_rx) = watch::channel(false);
            self.active.lock().await.insert(id.clone(), cancel_tx);
            let result = self.execute(&job, cancel_rx).await;
            self.active.lock().await.remove(&id);
            {
                let mut busy = self.busy_destinations.lock().await;
                for key in &keys {
                    busy.remove(key);
                }
            }
            drop(permit);
            let cancelled = self
                .jobs
                .lock()
                .await
                .get(&id)
                .is_some_and(|j| j.status == JobStatus::Cancelled);
            let result = if cancelled {
                Err(TransferError::Cancelled)
            } else {
                result
            };
            let retryable = matches!(&result, Err(TransferError::Network(_)));
            let retry_limit = match &job.operation {
                JobOperation::Workflow { workflow, .. } => workflow.retry_limit,
                _ => 2,
            };
            let retries = job.retries;
            if retryable && retries < retry_limit {
                let _ = self
                    .update(&id, |job| {
                        job.retries += 1;
                        job.status = JobStatus::Queued;
                        job.progress = format!(
                            "Network interruption; retry {} of {retry_limit}",
                            job.retries
                        );
                        job.bytes = 0;
                        job.total_bytes = None;
                        job.speed_bytes_per_sec = None;
                        job.eta_seconds = None;
                        Ok(())
                    })
                    .await;
                tokio::time::sleep(Duration::from_secs(1 << (retries + 1))).await;
                continue;
            }
            let (status, message) = match result {
                Ok(()) => (JobStatus::Completed, "Transfer completed".to_owned()),
                Err(TransferError::Cancelled) => {
                    (JobStatus::Cancelled, "Transfer cancelled".to_owned())
                }
                Err(e) => {
                    let message = e.to_string();
                    (JobStatus::Failed(message.clone()), message)
                }
            };
            let completed = status == JobStatus::Completed;
            let _ = self
                .update(&id, |job| {
                    job.status = status;
                    job.progress = message.clone();
                    Ok(())
                })
                .await;
            if completed {
                self.notify("Transfer completed", &job.name).await;
            } else if !cancelled {
                self.notify("Transfer failed", &format!("{}: {}", job.name, message))
                    .await;
            }
            break;
        }
        self.scheduled.lock().await.remove(&id);
    }

    async fn execute(
        &self,
        job: &JobSnapshot,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), TransferError> {
        let profiles = store::load().map_err(|e| TransferError::Backend(e.to_string()))?;
        let selected: Vec<_> = profiles.into_iter().filter(|p| {
            matches!(&job.source, Location::Remote { profile_id, .. } if profile_id == &p.id)
                || matches!(&job.destination, Location::Remote { profile_id, .. } if profile_id == &p.id)
        }).collect();
        let credentials = store::with_credentials(&selected)
            .await
            .map_err(|e| TransferError::Authentication(e.to_string()))?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let source = job.source.clone();
        let destination = job.destination.clone();
        let operation = job.operation.clone();
        if let JobOperation::Workflow { workflow, .. } = &operation {
            let current = store::get_workflow(&workflow.id)
                .map_err(|e| TransferError::Backend(e.to_string()))?
                .ok_or_else(|| TransferError::Conflict("Workflow was deleted".into()))?;
            if current != **workflow {
                return Err(TransferError::Conflict(
                    "Workflow changed since queueing; preview again".into(),
                ));
            }
        }
        tokio::spawn(async move {
            match operation {
                JobOperation::Copy => {
                    transfer_rclone::copy(source, destination, credentials, tx, cancel).await
                }
                JobOperation::Sync {
                    mode,
                    preview_fingerprint,
                } => {
                    transfer_rclone::sync(
                        source,
                        destination,
                        mode,
                        preview_fingerprint,
                        credentials,
                        tx,
                        cancel,
                    )
                    .await
                }
                JobOperation::Workflow {
                    workflow,
                    preview_fingerprint,
                } => {
                    transfer_rclone::run_workflow(
                        *workflow,
                        preview_fingerprint,
                        credentials,
                        tx,
                        cancel,
                    )
                    .await
                }
            }
        });
        while let Some(event) = rx.recv().await {
            match event {
                TransferEvent::Progress(text) => {
                    let _ = self
                        .update(&job.id, |job| {
                            job.progress = text.replace('\n', " ");
                            Ok(())
                        })
                        .await;
                }
                TransferEvent::Metrics {
                    bytes,
                    total_bytes,
                    speed_bytes_per_sec,
                    eta_seconds,
                } => {
                    let _ = self
                        .update(&job.id, |job| {
                            job.bytes = bytes;
                            job.total_bytes = total_bytes;
                            job.speed_bytes_per_sec = speed_bytes_per_sec;
                            job.eta_seconds = eta_seconds;
                            Ok(())
                        })
                        .await;
                }
                TransferEvent::Finished(result) => {
                    if result.is_ok()
                        && let JobOperation::Workflow { workflow, .. } = &job.operation
                        && workflow.mode == SyncMode::Bidirectional
                        && !workflow.baseline_ready
                    {
                        store::mark_workflow_initialized(workflow)
                            .map_err(|e| TransferError::Backend(e.to_string()))?;
                    }
                    return result;
                }
            }
        }
        Err(TransferError::Backend(
            "Transfer process stopped unexpectedly".into(),
        ))
    }

    async fn notify(&self, summary: &str, body: &str) {
        let Some(bus) = self.bus.get() else { return };
        let hints: HashMap<String, zbus::zvariant::OwnedValue> = HashMap::new();
        let _ = bus
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "Transfer",
                    0u32,
                    "io.github.design_nexus.nyx.Transfer",
                    summary,
                    body,
                    Vec::<String>::new(),
                    hints,
                    5000i32,
                ),
            )
            .await;
    }
}

#[interface(name = "io.github.design_nexus.nyx.Transfer1")]
impl Service {
    async fn enqueue(&self, source: &str, destination: &str) -> zbus::fdo::Result<String> {
        let source: Location = serde_json::from_str(source).map_err(error)?;
        let destination: Location = serde_json::from_str(destination).map_err(error)?;
        source.validate().map_err(error)?;
        destination.validate().map_err(error)?;
        let same_local_file = match (&source, &destination) {
            (Location::Local(a), Location::Local(b)) => a
                .canonicalize()
                .ok()
                .zip(b.canonicalize().ok())
                .is_some_and(|(a, b)| a == b),
            _ => false,
        };
        if source == destination || same_local_file {
            return Err(error("Source and destination are the same"));
        }
        let profiles = store::load().map_err(error)?;
        for location in [&source, &destination] {
            if let Location::Remote { profile_id, .. } = location
                && !profiles.iter().any(|profile| &profile.id == profile_id)
            {
                return Err(error("Connection profile is unavailable"));
            }
        }
        let name = match &source {
            Location::Local(path) => path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
            Location::Remote { path, .. } => path.rsplit('/').next().unwrap_or(path).to_owned(),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let job = JobSnapshot {
            id: id.clone(),
            source,
            destination,
            name,
            status: JobStatus::Queued,
            retries: 0,
            progress: "Queued".into(),
            created_at: now(),
            updated_at: now(),
            operation: JobOperation::Copy,
            bytes: 0,
            total_bytes: None,
            speed_bytes_per_sec: None,
            eta_seconds: None,
        };
        store::save_job(&job).map_err(error)?;
        self.0.jobs.lock().await.insert(id.clone(), job);
        self.0.changed().await;
        self.0.schedule(id.clone()).await;
        Ok(id)
    }
    async fn preview_sync(
        &self,
        source: &str,
        destination: &str,
        mode: &str,
    ) -> zbus::fdo::Result<String> {
        let source: Location = serde_json::from_str(source).map_err(error)?;
        let destination: Location = serde_json::from_str(destination).map_err(error)?;
        let mode = parse_sync_mode(mode)?;
        let preview = preview_request(&source, &destination, mode)
            .await
            .map_err(error)?;
        serde_json::to_string(&preview).map_err(error)
    }
    async fn enqueue_sync(
        &self,
        source: &str,
        destination: &str,
        mode: &str,
        fingerprint: &str,
    ) -> zbus::fdo::Result<String> {
        let source: Location = serde_json::from_str(source).map_err(error)?;
        let destination: Location = serde_json::from_str(destination).map_err(error)?;
        let mode = parse_sync_mode(mode)?;
        let preview = preview_request(&source, &destination, mode)
            .await
            .map_err(error)?;
        if preview.fingerprint != fingerprint {
            return Err(error(
                "Directory contents changed since preview; preview again",
            ));
        }
        if preview.changes.is_empty() {
            return Err(error("Directories are already in sync"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let job = JobSnapshot {
            id: id.clone(),
            source,
            destination,
            name: match mode {
                SyncMode::Copy => "Copy directory contents",
                SyncMode::Mirror => "Mirror directory contents",
                SyncMode::Bidirectional => {
                    return Err(error("Use a saved workflow for bidirectional sync"));
                }
            }
            .into(),
            status: JobStatus::Queued,
            retries: 0,
            progress: "Queued".into(),
            created_at: now(),
            updated_at: now(),
            operation: JobOperation::Sync {
                mode,
                preview_fingerprint: fingerprint.to_owned(),
            },
            bytes: 0,
            total_bytes: None,
            speed_bytes_per_sec: None,
            eta_seconds: None,
        };
        store::save_job(&job).map_err(error)?;
        self.0.jobs.lock().await.insert(id.clone(), job);
        self.0.changed().await;
        self.0.schedule(id.clone()).await;
        Ok(id)
    }
    async fn preview_workflow(&self, id: &str) -> zbus::fdo::Result<String> {
        let workflow = store::get_workflow(id)
            .map_err(error)?
            .ok_or_else(|| error("Workflow not found"))?;
        let preview = workflow_preview_request(&workflow).await.map_err(error)?;
        serde_json::to_string(&preview).map_err(error)
    }
    async fn enqueue_workflow(&self, id: &str, fingerprint: &str) -> zbus::fdo::Result<String> {
        let workflow = store::get_workflow(id)
            .map_err(error)?
            .ok_or_else(|| error("Workflow not found"))?;
        let preview = workflow_preview_request(&workflow).await.map_err(error)?;
        if preview.fingerprint != fingerprint {
            return Err(error(
                "Workflow contents changed since preview; preview again",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let job = JobSnapshot {
            id: id.clone(),
            source: workflow.source.clone(),
            destination: workflow.destination.clone(),
            name: workflow.name.clone(),
            status: JobStatus::Queued,
            retries: 0,
            progress: "Queued".into(),
            created_at: now(),
            updated_at: now(),
            operation: JobOperation::Workflow {
                workflow: Box::new(workflow),
                preview_fingerprint: fingerprint.to_owned(),
            },
            bytes: 0,
            total_bytes: None,
            speed_bytes_per_sec: None,
            eta_seconds: None,
        };
        store::save_job(&job).map_err(error)?;
        self.0.jobs.lock().await.insert(id.clone(), job);
        self.0.changed().await;
        self.0.schedule(id.clone()).await;
        Ok(id)
    }
    async fn list_jobs(&self) -> zbus::fdo::Result<String> {
        let mut jobs: Vec<_> = self.0.jobs.lock().await.values().cloned().collect();
        jobs.sort_by_key(|j| (j.created_at, j.id.clone()));
        serde_json::to_string(&jobs).map_err(error)
    }
    async fn pause(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0
            .update(id, |job| {
                if job.status != JobStatus::Queued {
                    bail!("Only queued jobs can pause")
                }
                job.status = JobStatus::Paused;
                job.progress = "Paused".into();
                Ok(())
            })
            .await
            .map_err(error)?;
        Ok(())
    }
    async fn resume(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0
            .update(id, |job| {
                if job.status != JobStatus::Paused {
                    bail!("Job is not paused")
                }
                job.status = JobStatus::Queued;
                job.progress = "Queued".into();
                Ok(())
            })
            .await
            .map_err(error)?;
        self.0.schedule(id.to_owned()).await;
        Ok(())
    }
    async fn cancel(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0
            .update(id, |job| {
                if !matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Paused | JobStatus::Running
                ) {
                    bail!("Job cannot be cancelled")
                }
                job.status = JobStatus::Cancelled;
                job.progress = "Cancelled".into();
                Ok(())
            })
            .await
            .map_err(error)?;
        if let Some(sender) = self.0.active.lock().await.get(id) {
            let _ = sender.send(true);
        }
        Ok(())
    }
    async fn retry(&self, id: &str) -> zbus::fdo::Result<()> {
        self.0
            .update(id, |job| {
                if !matches!(job.status, JobStatus::Failed(_)) {
                    bail!("Only failed jobs can retry")
                }
                job.retries += 1;
                job.status = JobStatus::Queued;
                job.progress = "Queued".into();
                job.bytes = 0;
                job.total_bytes = None;
                job.speed_bytes_per_sec = None;
                job.eta_seconds = None;
                Ok(())
            })
            .await
            .map_err(error)?;
        self.0.schedule(id.to_owned()).await;
        Ok(())
    }
    async fn clear_completed(&self) -> zbus::fdo::Result<()> {
        let mut jobs = self.0.jobs.lock().await;
        let done: Vec<_> = jobs
            .iter()
            .filter(|(_, j)| j.status == JobStatus::Completed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            store::delete_job(&id).map_err(error)?;
            jobs.remove(&id);
        }
        drop(jobs);
        self.0.changed().await;
        Ok(())
    }
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok();
    let mut jobs = HashMap::new();
    for mut job in store::load_jobs()? {
        if job.status == JobStatus::Running {
            job.status =
                JobStatus::Failed("Interrupted by service restart; retry to restart safely".into());
            job.progress = "Interrupted".into();
            job.updated_at = now();
            store::save_job(&job)?;
        }
        jobs.insert(job.id.clone(), job);
    }
    let state = Arc::new(State {
        jobs: Mutex::new(jobs),
        scheduled: Mutex::new(HashSet::new()),
        active: Mutex::new(HashMap::new()),
        busy_destinations: Mutex::new(HashSet::new()),
        permits: Semaphore::new(2),
        bus: OnceLock::new(),
    });
    let bus = zbus::connection::Builder::session()?
        .name(BUS)?
        .serve_at(PATH, Service(state.clone()))?
        .build()
        .await?;
    state.bus.set(bus).ok();
    let queued: Vec<_> = state
        .jobs
        .lock()
        .await
        .iter()
        .filter(|(_, j)| j.status == JobStatus::Queued)
        .map(|(id, _)| id.clone())
        .collect();
    for id in queued {
        state.schedule(id).await;
    }
    std::future::pending::<()>().await;
    Ok(())
}
