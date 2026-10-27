//! Versioned session-bus contract. Paths and job metadata only; no credentials.
pub const BUS: &str = "io.github.design_nexus.nyx.Transfer1";
pub const PATH: &str = "/io/github/design_nexus/nyx/Transfer1";

#[zbus::proxy(
    interface = "io.github.design_nexus.nyx.Transfer1",
    default_service = "io.github.design_nexus.nyx.Transfer1",
    default_path = "/io/github/design_nexus/nyx/Transfer1"
)]
pub trait Transfer {
    fn enqueue(&self, source: &str, destination: &str) -> zbus::Result<String>;
    fn preview_sync(&self, source: &str, destination: &str, mode: &str) -> zbus::Result<String>;
    fn enqueue_sync(
        &self,
        source: &str,
        destination: &str,
        mode: &str,
        fingerprint: &str,
    ) -> zbus::Result<String>;
    fn preview_workflow(&self, id: &str) -> zbus::Result<String>;
    fn enqueue_workflow(&self, id: &str, fingerprint: &str) -> zbus::Result<String>;
    fn list_jobs(&self) -> zbus::Result<String>;
    fn pause(&self, id: &str) -> zbus::Result<()>;
    fn resume(&self, id: &str) -> zbus::Result<()>;
    fn cancel(&self, id: &str) -> zbus::Result<()>;
    fn retry(&self, id: &str) -> zbus::Result<()>;
    fn clear_completed(&self) -> zbus::Result<()>;
    #[zbus(signal)]
    fn changed(&self) -> zbus::Result<()>;
}

pub struct Client {
    connection: zbus::Connection,
}

impl Client {
    pub async fn connect() -> zbus::Result<Self> {
        Ok(Self {
            connection: zbus::Connection::session().await?,
        })
    }
    async fn proxy(&self) -> zbus::Result<TransferProxy<'_>> {
        TransferProxy::new(&self.connection).await
    }
    pub async fn enqueue(
        &self,
        source: &transfer_core::Location,
        destination: &transfer_core::Location,
    ) -> anyhow::Result<String> {
        let source = serde_json::to_string(source)?;
        let destination = serde_json::to_string(destination)?;
        Ok(self.proxy().await?.enqueue(&source, &destination).await?)
    }
    pub async fn jobs(&self) -> anyhow::Result<Vec<transfer_core::JobSnapshot>> {
        Ok(serde_json::from_str(
            &self.proxy().await?.list_jobs().await?,
        )?)
    }
    pub async fn preview_sync(
        &self,
        source: &transfer_core::Location,
        destination: &transfer_core::Location,
        mode: transfer_core::SyncMode,
    ) -> anyhow::Result<transfer_core::SyncPreview> {
        let source = serde_json::to_string(source)?;
        let destination = serde_json::to_string(destination)?;
        let mode = match mode {
            transfer_core::SyncMode::Copy => "Copy",
            transfer_core::SyncMode::Mirror => "Mirror",
            transfer_core::SyncMode::Bidirectional => "Bidirectional",
        };
        Ok(serde_json::from_str(
            &self
                .proxy()
                .await?
                .preview_sync(&source, &destination, mode)
                .await?,
        )?)
    }
    pub async fn enqueue_sync(
        &self,
        source: &transfer_core::Location,
        destination: &transfer_core::Location,
        mode: transfer_core::SyncMode,
        fingerprint: &str,
    ) -> anyhow::Result<String> {
        let source = serde_json::to_string(source)?;
        let destination = serde_json::to_string(destination)?;
        let mode = match mode {
            transfer_core::SyncMode::Copy => "Copy",
            transfer_core::SyncMode::Mirror => "Mirror",
            transfer_core::SyncMode::Bidirectional => "Bidirectional",
        };
        Ok(self
            .proxy()
            .await?
            .enqueue_sync(&source, &destination, mode, fingerprint)
            .await?)
    }
    pub async fn pause(&self, id: &str) -> zbus::Result<()> {
        self.proxy().await?.pause(id).await
    }
    pub async fn resume(&self, id: &str) -> zbus::Result<()> {
        self.proxy().await?.resume(id).await
    }
    pub async fn cancel(&self, id: &str) -> zbus::Result<()> {
        self.proxy().await?.cancel(id).await
    }
    pub async fn retry(&self, id: &str) -> zbus::Result<()> {
        self.proxy().await?.retry(id).await
    }
    pub async fn clear_completed(&self) -> zbus::Result<()> {
        self.proxy().await?.clear_completed().await
    }
    pub async fn preview_workflow(
        &self,
        id: &str,
    ) -> anyhow::Result<transfer_core::WorkflowPreview> {
        Ok(serde_json::from_str(
            &self.proxy().await?.preview_workflow(id).await?,
        )?)
    }
    pub async fn enqueue_workflow(&self, id: &str, fingerprint: &str) -> anyhow::Result<String> {
        Ok(self
            .proxy()
            .await?
            .enqueue_workflow(id, fingerprint)
            .await?)
    }
}
