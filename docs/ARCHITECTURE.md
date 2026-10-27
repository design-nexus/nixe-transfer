# Architecture

## Existing suite boundaries

The Nyx umbrella repository is a suite coordinator. `nyx-core` supplies app
identity and XDG paths; `nyx-design` supplies shared GTK4 styling and widgets.
Nyx Files is a separate file manager and is not a dependency. `nyx-cloud`
exposes account metadata and FUSE mounts over `Cloud1`; it has no general
transfer API. Transfer does not modify those repositories or Cloud's rclone
configuration.

## Transfer service

The `nyx-transferd` user service owns a Tokio worker and a bounded job queue.
`transfer-core` defines typed locations, entries, profiles, state, and errors.
`transfer-rclone` translates those operations to managed rclone subprocesses.
`transfer-storage` owns the SQLite and Secret Service access. `transfer-ipc`
defines the public, versioned `Transfer1` session D-Bus contract. The GUI
submits jobs and reads their state through D-Bus; closing it leaves transfers
running. Both browser panes use the same backend interface and can show any
location.

`Transfer1` uses bus/interface `io.github.design_nexus.nyx.Transfer1` and
object `/io/github/design_nexus/nyx/Transfer1`. `Enqueue` accepts source and
destination `Location` JSON and returns a job ID. `ListJobs` returns
`JobSnapshot` JSON with byte counts, speed, and ETA when rclone reports them.
`Pause`, `Resume`, `Cancel`, `Retry`, and `ClearCompleted`
control the persisted queue; `Changed` signals updates. This interface contains
paths and status, never credentials. Breaking changes require `Transfer2`.
`PreviewSync` returns a dry-run change report. `EnqueueSync` accepts its
fingerprint and stores a one-way copy or mirror job. The daemon compares again
at queue time and immediately before execution; a changed plan fails for a
new preview. Mirror jobs also cap deletions at the previewed count. Rclone's
copy and sync semantics perform the actual work, including its default
delete-after behavior for mirrors.
`PreviewWorkflow` and `EnqueueWorkflow` use a saved workflow ID. Workflow
metadata lives in SQLite; each bidirectional workflow has a private rclone
work directory for prior listings and filter state. The first run uses an
explicit resync preference. Later runs use rclone bisync change tracking,
recovery, deletion safeguards, and the selected conflict policy. The default
conflict policy preserves both versions. Changing the workflow plan resets its
baseline and requires a new preview and first-run confirmation.

SQLite stores non-secret connection profiles under the app XDG data directory.
Secret Service stores passwords under stable profile IDs. Rclone credentials
are passed through child environment variables; they are never arguments,
SQLite values, logs, or permanent rclone configurations. SFTP uses an explicit
known-hosts file. Unknown and changed host keys block the connection. FTPS
certificate checking stays enabled. Plain FTP is an explicit connection type.

Imported rclone profiles are a separate boundary: SQLite stores the remote name
and path to the user's rclone config, while rclone reads and may refresh that
file during operations. Transfer never copies those provider credentials or
reads Nyx Cloud's private configuration. The adapter uses one external config
per operation and rejects a pair of imported remotes with different config
paths.

Rclone operations emit progress and errors to the GTK client. Existing target
files are protected by rclone's immutable mode; unsupported resumability is
never advertised. Atomic completion depends on rclone and the destination.

## Restart behavior

State transitions are persisted. Queued jobs restart automatically; jobs active
when the service stopped become failed with an interruption reason. Explicit
retry starts them from the beginning, since backend resume is not assumed safe.
Two jobs may run concurrently, but the same destination is serialized. Transient
network failures receive two bounded retries with exponential delay. Completion
and failure notifications use the desktop notification service.

Future direct Nyx Cloud integration will use public Cloud APIs or mount paths
without reading Cloud's private credentials. A copy into a Cloud FUSE mount is
not yet treated as proof that the cloud upload itself finished.

Saved workflows support include/exclude patterns, a bandwidth limit, bounded
network retries, and checksum comparison where available. A dry-run comparison is a plan, not a
filesystem snapshot: providers can change between the final comparison and
the actual operation. Scheduling remains future work.
