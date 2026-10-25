use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Local(PathBuf),
    Remote { profile_id: String, path: String },
}

impl Location {
    pub fn validate(&self) -> Result<(), TransferError> {
        match self {
            Self::Local(path) => {
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(TransferError::InvalidPath);
                }
            }
            Self::Remote { profile_id, path } => {
                if profile_id.is_empty()
                    || path.starts_with('/')
                    || path.split('/').any(|part| {
                        !path.is_empty() && (part.is_empty() || part == "." || part == "..")
                    })
                    || path.contains('\0')
                {
                    return Err(TransferError::InvalidPath);
                }
            }
        }
        Ok(())
    }

    pub fn child(&self, name: &str) -> Result<Self, TransferError> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\0')
        {
            return Err(TransferError::InvalidPath);
        }
        Ok(match self {
            Self::Local(path) => Self::Local(path.join(name)),
            Self::Remote { profile_id, path } => Self::Remote {
                profile_id: profile_id.clone(),
                path: if path.is_empty() {
                    name.to_owned()
                } else {
                    format!("{path}/{name}")
                },
            },
        })
    }

    pub fn parent(&self) -> Self {
        match self {
            Self::Local(path) => Self::Local(path.parent().unwrap_or(path).to_path_buf()),
            Self::Remote { profile_id, path } => Self::Remote {
                profile_id: profile_id.clone(),
                path: path
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
                    .unwrap_or("")
                    .to_owned(),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Sftp,
    Ftp,
    FtpsExplicit,
    FtpsImplicit,
    Webdav,
    Rclone,
}

impl Protocol {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Sftp => "SFTP",
            Self::Ftp => "FTP",
            Self::FtpsExplicit => "FTPS (explicit)",
            Self::FtpsImplicit => "FTPS (implicit)",
            Self::Webdav => "WebDAV",
            Self::Rclone => "rclone",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: Auth,
    pub default_dir: String,
    pub timeout_secs: u64,
    pub ssh_key_file: Option<PathBuf>,
    pub known_hosts_file: Option<PathBuf>,
    #[serde(default)]
    pub ca_cert: Option<PathBuf>,
    pub allow_plaintext: bool,
    #[serde(default)]
    pub rclone_remote: Option<String>,
    #[serde(default)]
    pub rclone_config: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Auth {
    Agent,
    KeyFile,
    Password,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    InvalidPath,
    MissingProfile,
    Authentication(String),
    HostKey(String),
    Certificate(String),
    Permission(String),
    Network(String),
    Conflict(String),
    Cancelled,
    Backend(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath => write!(f, "Invalid file path"),
            Self::MissingProfile => write!(f, "Connection profile is unavailable"),
            Self::Authentication(s) => write!(f, "Authentication failed: {s}"),
            Self::HostKey(s) => write!(f, "SSH host key verification failed: {s}"),
            Self::Certificate(s) => write!(f, "TLS certificate verification failed: {s}"),
            Self::Permission(s) => write!(f, "Permission denied: {s}"),
            Self::Network(s) => write!(f, "Network error: {s}"),
            Self::Conflict(s) => write!(f, "Destination conflict: {s}"),
            Self::Cancelled => write!(f, "Transfer cancelled"),
            Self::Backend(s) => write!(f, "Transfer failed: {s}"),
        }
    }
}
impl std::error::Error for TransferError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Queued,
    Paused,
    Running,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: String,
    pub source: Location,
    pub destination: Location,
    pub name: String,
    pub status: JobStatus,
    pub retries: u32,
    pub progress: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub operation: JobOperation,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobOperation {
    #[default]
    Copy,
    Sync {
        mode: SyncMode,
        preview_fingerprint: String,
    },
    Workflow {
        workflow: SyncWorkflow,
        preview_fingerprint: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    Copy,
    Mirror,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    KeepBoth,
    PreferLeft,
    PreferRight,
    PreferNewer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncWorkflow {
    pub id: String,
    pub name: String,
    pub source: Location,
    pub destination: Location,
    pub mode: SyncMode,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub conflict: ConflictPolicy,
    pub checksum: bool,
    pub bandwidth_kib: Option<u64>,
    #[serde(default = "default_retry_limit")]
    pub retry_limit: u32,
    #[serde(default)]
    pub baseline_ready: bool,
}

fn default_retry_limit() -> u32 {
    2
}

impl SyncWorkflow {
    pub fn validate(&self) -> Result<(), TransferError> {
        self.source.validate()?;
        self.destination.validate()?;
        if self.id.is_empty()
            || self.id.len() > 64
            || !self.id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            || self.name.trim().is_empty()
            || self.name.len() > 160
            || self.include.len() + self.exclude.len() > 100
            || self.include.iter().chain(&self.exclude).any(|pattern| {
                pattern.trim().is_empty()
                    || pattern.contains(['\n', '\r', '\0'])
                    || pattern.len() > 512
            })
            || self.bandwidth_kib == Some(0)
            || self.retry_limit > 10
        {
            return Err(TransferError::Backend("Invalid workflow settings".into()));
        }
        Ok(())
    }
    pub fn same_plan(&self, other: &Self) -> bool {
        self.same_baseline(other)
            && self.bandwidth_kib == other.bandwidth_kib
            && self.retry_limit == other.retry_limit
    }
    pub fn same_baseline(&self, other: &Self) -> bool {
        self.source == other.source
            && self.destination == other.destination
            && self.mode == other.mode
            && self.include == other.include
            && self.exclude == other.exclude
            && self.conflict == other.conflict
            && self.checksum == other.checksum
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPreview {
    pub lines: Vec<String>,
    pub fingerprint: String,
    pub deletions: usize,
    pub initial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SyncAction {
    Copy,
    Update,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncChange {
    pub action: SyncAction,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPreview {
    pub changes: Vec<SyncChange>,
    pub fingerprint: String,
}

impl JobStatus {
    pub fn allows(&self, next: &Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Paused | Self::Running | Self::Cancelled)
                | (Self::Paused, Self::Queued | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Completed | Self::Failed(_) | Self::Cancelled
                )
                | (Self::Failed(_), Self::Queued)
        )
    }
}

#[derive(Clone, Debug)]
pub enum TransferEvent {
    Progress(String),
    Finished(Result<(), TransferError>),
}

#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub copy: bool,
    pub rename: bool,
    pub checksum: bool,
    pub resume: bool,
}

#[async_trait::async_trait]
pub trait StorageBackend: Send + Sync {
    fn capabilities(&self, location: &Location) -> Capabilities;
    async fn list(&self, location: &Location) -> Result<Vec<FileEntry>, TransferError>;
    async fn copy(
        &self,
        source: Location,
        destination: Location,
        events: mpsc::UnboundedSender<TransferEvent>,
        cancel: watch::Receiver<bool>,
    );
}

#[derive(Clone, Debug)]
pub struct TransferJob {
    pub id: String,
    pub source: Location,
    pub destination: Location,
    pub name: String,
    pub status: JobStatus,
    pub retries: u32,
    pub bytes: u64,
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_children_reject_traversal() {
        let root = Location::Remote {
            profile_id: "p".into(),
            path: "a".into(),
        };
        assert!(root.child("..").is_err());
        assert!(root.child("a/b").is_err());
        assert_eq!(root.child("☃ file").unwrap().parent(), root);
    }

    #[test]
    fn job_transitions_reject_completed_restart() {
        assert!(JobStatus::Queued.allows(&JobStatus::Running));
        assert!(JobStatus::Running.allows(&JobStatus::Cancelled));
        assert!(!JobStatus::Completed.allows(&JobStatus::Running));
    }

    #[test]
    fn locations_reject_relative_and_remote_traversal() {
        assert!(Location::Local("relative/file".into()).validate().is_err());
        assert!(
            Location::Remote {
                profile_id: "p".into(),
                path: "../file".into()
            }
            .validate()
            .is_err()
        );
        assert!(Location::Local("/tmp/file".into()).validate().is_ok());
    }
}
