//! Capture transport selection; all providers share the native archive transaction.
use crate::{
    Result, conflict,
    official::Official,
    schema::{Batch, EXTENSION},
    twikit::Twikit,
};
use inkcre_core::Core;
use serde_json::Value;
pub struct Page {
    pub batch: Batch,
    pub primary: Vec<String>,
    pub next: Option<String>,
}
pub enum Provider {
    Official(Official),
    Twikit(Box<Twikit>),
}
impl Provider {
    pub async fn new(core: &Core) -> Result<Self> {
        let extension = core
            .extension_get(EXTENSION)
            .await
            .map_err(|_| conflict("Twitter config read failed"))?
            .ok_or_else(|| conflict("Twitter extension missing"))?;
        match extension.config["backend"].as_str().unwrap_or("official") {
            "official" => Official::new(core).await.map(Self::Official),
            "twikit" => Twikit::new(core, &extension.config)
                .await
                .map(|api| Self::Twikit(Box::new(api))),
            _ => Err(conflict("Unknown Twitter capture backend")),
        }
    }
    pub async fn bookmarks(&self, page: Option<&str>, limit: usize) -> Result<Page> {
        match self {
            Self::Official(api) => api.bookmarks(page, limit).await,
            Self::Twikit(api) => api.bookmarks(page, limit).await,
        }
    }
    pub async fn guard_in(&self, connection: &mut sqlx::PgConnection) -> Result<()> {
        let _: (Value, Value) = match self {
            Self::Official(api) => api.guard_in(connection).await?,
            Self::Twikit(api) => api.guard_in(connection).await?,
        };
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        match self {
            Self::Official(_) => Ok(()),
            Self::Twikit(api) => api.close().await,
        }
    }
}
