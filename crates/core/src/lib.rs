//! An embeddable native Peer over InKCre's admitted PostgreSQL protocol.
//! The SDK never runs migrations or reads process environment configuration.

mod admission;
mod error;
mod extension_store;
mod graph;
mod jobs;
pub mod knowledge;
pub mod models;
mod peer;
mod storage;

pub mod cron;
pub mod extension;
pub mod source;
pub mod worker;

pub use error::{Error, Result};
pub use graph::GraphTransaction;
pub use models::*;
pub use sqlx::PgPool;

use sqlx::postgres::PgPoolOptions;
use std::{sync::Arc, time::Duration};

/// A cloneable, bounded connection pool. Identity belongs to the embedding Peer.
#[derive(Clone)]
pub struct Core {
    pool: PgPool,
    pub(crate) knowledge_registry: Arc<knowledge::Registry>,
}

impl Core {
    /// Connect with a deployment-admitted native login, then verify admission.
    /// Connection errors intentionally do not expose the supplied URL.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self> {
        if max_connections == 0 || max_connections > 100 {
            return Err(Error::InvalidInput(
                "max_connections must be 1..=100".into(),
            ));
        }
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(15))
            .connect(database_url)
            .await?;
        let core = Self::from_pool(pool);
        core.admission().await?;
        Ok(core)
    }

    /// Embed an existing pool. Call `admission` before serving requests.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            knowledge_registry: Arc::new(knowledge::Registry::default()),
        }
    }

    /// 当前嵌入实例的知识能力；Core clones 共享注册，独立 Core 不共享运行状态。
    pub fn knowledge(&self) -> knowledge::Knowledge {
        knowledge::Knowledge::new(self.clone())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
