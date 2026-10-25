# Roadmap

1. **Foundation:** independent buildable workspace, core contracts, GTK shell,
   architecture and backend documentation.
2. **Interactive transfers:** local, SFTP, FTP, FTPS, WebDAV browsing and
   copy in either direction, saved connections, queue, progress, cancellation,
   retries, clear errors, and installation.
3. **Reliability:** `nixe-transferd`, SQLite job state, versioned D-Bus IPC,
   restart recovery, notifications, and history are implemented. Safe active
   transfer resume remains backend-specific future work.
4. **Universal locations:** mounted SMB/NFS via local paths and imports from
   the user's existing rclone config are implemented. Nixe Cloud public API
   integration and finer backend capability discovery remain.
5. **Synchronization:** dry-run comparison, queued copy/mirror, rclone bisync,
   conflict handling, filters, and saved workflows are implemented. Scheduling
   and richer comparison/verification controls remain later work.
6. **Integration:** Nixe Files/Drive requests, versioned shared contracts,
   systemd user timers for scheduling, and adapters only where needed.
