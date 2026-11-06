//! Exact knowledge capabilities over persisted graph authority.
//! Raw record reads remain available without a loaded Resolver or Storage handler.

mod builtins;
mod draft;
mod error;
mod registry;
mod storage;
mod types;

pub use builtins::{
    JsonResolver, decode_html_bytes, decode_unicode_bytes, detect_media_type, format_label,
    html_text,
};
pub use draft::{DraftBlock, DraftRelation, GraphDraft, PersistedDraft};
pub use error::{Error, Result};
pub use registry::Registry;
pub use storage::{HttpStorage, HttpStorageConfig, PostgreSqlStorage};
pub use types::*;

use crate::{Block, BlockInput, BlockPatch, Core, HydratedContent, Relation, Storage};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tokio::sync::Mutex;

/// A cheap handle sharing registrations with clones of one Core instance.
#[derive(Clone)]
pub struct Knowledge {
    core: Core,
    registry: Arc<Registry>,
}

impl Knowledge {
    pub fn new(core: Core) -> Self {
        Self {
            registry: core.knowledge_registry.clone(),
            core,
        }
    }
    pub fn core(&self) -> &Core {
        &self.core
    }
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn read_methods(&self, resolver: &str) -> Result<Vec<ReadMethod>> {
        let decoder = self.registry.resolver(resolver)?;
        let mut methods: BTreeMap<String, ReadMethod> = common_read_methods()
            .into_iter()
            .map(|method| (method.name.clone(), method))
            .collect();
        for method in decoder.descriptor().read_methods {
            methods.entry(method.name.clone()).or_insert(method);
        }
        Ok(methods.into_values().collect())
    }

    pub fn match_media_type(&self, media_type: Option<&str>) -> Result<Option<String>> {
        let Some(media_type) = media_type else {
            return Ok(None);
        };
        let media_type = media_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let id = match media_type.as_str() {
            "text/plain" => Some("core.text.v1"),
            "text/markdown" => Some("core.markdown.v1"),
            "text/html" | "application/xhtml+xml" => Some("core.html.v1"),
            "application/pdf" => Some("core.pdf.v1"),
            "application/epub+zip" => Some("core.epub.v1"),
            "application/zip" | "application/x-zip-compressed" => Some("core.zip.v1"),
            other if other.starts_with("image/") => Some("core.image.v1"),
            other if other.starts_with("audio/") => Some("core.audio.v1"),
            other if other.starts_with("video/") => Some("core.video.v1"),
            _ => None,
        };
        match id {
            Some(id) => match self.registry.resolver(id) {
                Ok(_) => Ok(Some(id.into())),
                Err(Error::UnknownResolver(_)) => Ok(None),
                Err(error) => Err(error),
            },
            None => Ok(None),
        }
    }

    pub async fn reader(&self, id: i32) -> Result<ResolverContext> {
        let block = self
            .core
            .block_get(id)
            .await?
            .ok_or(crate::Error::NotFound("block"))?;
        Ok(self.from_block(block))
    }
    pub fn from_block(&self, block: Block) -> ResolverContext {
        ResolverContext {
            block,
            knowledge: self.clone(),
            actual: Mutex::new(None),
            solved: Mutex::new(None),
            relations: Mutex::new(HashMap::new()),
        }
    }
    pub async fn actual_content(&self, block: &Block) -> Result<HydratedContent> {
        let Some(storage_id) = block.storage else {
            return Ok(HydratedContent::Text(block.content.clone()));
        };
        let storage = self
            .core
            .storage_get(storage_id)
            .await?
            .ok_or(crate::Error::NotFound("storage"))?;
        let bytes = self
            .registry
            .storage(&storage.type_)?
            .read(&self.core, &storage, &block.content)
            .await?;
        Ok(HydratedContent::Bytes(bytes))
    }
    pub async fn solved(&self, id: i32, options: ReadOptions) -> Result<Value> {
        self.reader(id).await?.get_solved_content(options).await
    }
    pub async fn text(&self, id: i32, options: TextOptions) -> Result<Option<String>> {
        self.reader(id).await?.get_text(options).await
    }
    pub async fn label(&self, id: i32, refresh: bool) -> Result<String> {
        self.reader(id).await?.get_label(refresh).await
    }
    pub async fn invoke(&self, id: i32, method: &str, arguments: Value) -> Result<Value> {
        self.reader(id).await?.invoke(method, arguments).await
    }

    /// Input admission requires the exact local contract; ordinary `Core::block_get` does not.
    pub async fn create_block(&self, input: BlockInput) -> Result<Block> {
        self.validate_block_input(&input).await?;
        Ok(self.core.block_create(input).await?)
    }
    pub async fn validate_block_input(&self, input: &BlockInput) -> Result<()> {
        let content = if let Some(storage) = input.storage {
            let record = self
                .core
                .storage_get(storage)
                .await?
                .ok_or(crate::Error::NotFound("storage"))?;
            HydratedContent::Bytes(
                self.registry
                    .storage(&record.type_)?
                    .read(&self.core, &record, &input.content)
                    .await?,
            )
        } else {
            HydratedContent::Text(input.content.clone())
        };
        self.registry
            .resolver(&input.resolver)?
            .validate_content(&content)?;
        Ok(())
    }
    /// Validate a new representation inside a caller-owned graph/byte transaction.
    pub async fn validate_block_input_in(
        &self,
        connection: &mut PgConnection,
        input: &BlockInput,
    ) -> Result<()> {
        let actual = if let Some(storage_id) = input.storage {
            let storage = self.storage_record_in(connection, storage_id).await?;
            HydratedContent::Bytes(
                self.registry
                    .storage(&storage.type_)?
                    .read_in(&self.core, connection, &storage, &input.content)
                    .await?,
            )
        } else {
            HydratedContent::Text(input.content.clone())
        };
        self.registry
            .resolver(&input.resolver)?
            .validate_content(&actual)
    }
    /// Lock the row, validate its complete next representation, then update once.
    pub async fn patch_block(&self, id: i32, patch: BlockPatch) -> Result<Block> {
        let mut transaction = self.core.pool().begin().await?;
        let current:Block=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(id).fetch_optional(&mut *transaction).await?.ok_or(crate::Error::NotFound("block"))?;
        let input = BlockInput {
            storage: patch.storage.unwrap_or(current.storage),
            resolver: patch.resolver.unwrap_or(current.resolver),
            content: patch.content.unwrap_or(current.content),
        };
        self.validate_block_input_in(&mut transaction, &input)
            .await?;
        let block=sqlx::query_as("UPDATE inkcre.blocks SET storage=$2,resolver=$3,content=$4 WHERE id=$1 RETURNING id,storage,resolver,content,created_at,updated_at")
            .bind(id).bind(input.storage).bind(input.resolver).bind(input.content).fetch_one(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(block)
    }

    /// Publish runtime capability metadata without replacing deployment-owned instances.
    pub async fn sync_storage_types(&self) -> Result<()> {
        let mut transaction = self.core.pool().begin().await?;
        for descriptor in self.registry.storages()? {
            // Artifact-owned built-in catalog rows were admitted by impl-py.
            // Equivalent Rust schemas must not overwrite their exact projections.
            if matches!(descriptor.id.as_str(), "http" | "postgresql_binary") {
                continue;
            }
            sqlx::query("INSERT INTO inkcre.storage_types(id,description,config_schema,writable) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET description=EXCLUDED.description,config_schema=EXCLUDED.config_schema,writable=EXCLUDED.writable")
                .bind(descriptor.id).bind(descriptor.description).bind(descriptor.config_schema).bind(descriptor.writable).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }
    pub async fn create_storage(
        &self,
        type_id: &str,
        nickname: Option<&str>,
        config: Value,
    ) -> Result<Storage> {
        let descriptor = self.registry.storage(type_id)?.descriptor();
        crate::models::validate_json(&descriptor.config_schema, &config)?;
        Ok(sqlx::query_as("INSERT INTO inkcre.storages(type,nickname,config) VALUES($1,$2,$3) RETURNING id,type,nickname,config")
            .bind(type_id).bind(nickname).bind(config).fetch_one(self.core.pool()).await?)
    }
    pub(crate) async fn storage_record_in(
        &self,
        connection: &mut PgConnection,
        id: i32,
    ) -> Result<Storage> {
        sqlx::query_as("SELECT id,type,nickname,config FROM inkcre.storages WHERE id=$1")
            .bind(id)
            .fetch_optional(connection)
            .await?
            .ok_or_else(|| crate::Error::NotFound("storage").into())
    }
    pub async fn storage_create(
        &self,
        connection: &mut PgConnection,
        storage_id: i32,
        data: &[u8],
    ) -> Result<String> {
        let storage = self.storage_record_in(connection, storage_id).await?;
        let handler = self.registry.storage(&storage.type_)?;
        let writable = handler
            .writable()
            .ok_or_else(|| Error::Unsupported(format!("Storage {} is read-only", storage.type_)))?;
        writable
            .create(&self.core, connection, &storage, data)
            .await
    }
    pub async fn storage_update(
        &self,
        connection: &mut PgConnection,
        storage_id: i32,
        pointer: &str,
        data: &[u8],
    ) -> Result<bool> {
        let storage = self.storage_record_in(connection, storage_id).await?;
        let handler = self.registry.storage(&storage.type_)?;
        let writable = handler
            .writable()
            .ok_or_else(|| Error::Unsupported(format!("Storage {} is read-only", storage.type_)))?;
        writable
            .update(&self.core, connection, &storage, pointer, data)
            .await
    }
    pub async fn storage_delete(
        &self,
        connection: &mut PgConnection,
        storage_id: i32,
        pointer: &str,
    ) -> Result<bool> {
        let storage = self.storage_record_in(connection, storage_id).await?;
        let handler = self.registry.storage(&storage.type_)?;
        let writable = handler
            .writable()
            .ok_or_else(|| Error::Unsupported(format!("Storage {} is read-only", storage.type_)))?;
        writable
            .delete(&self.core, connection, &storage, pointer)
            .await
    }
}

/// One Block's local snapshot and caches. Re-create the reader to observe a changed
/// Block row. `refresh` bypasses its actual-byte/solved/relation snapshots; it grants
/// no recomputation or mutation beyond an explicit `materialize_missing` option.
pub struct ResolverContext {
    pub block: Block,
    pub knowledge: Knowledge,
    actual: Mutex<Option<HydratedContent>>,
    solved: Mutex<Option<Value>>,
    relations: Mutex<HashMap<(bool, bool), Vec<Relation>>>,
}

impl ResolverContext {
    pub async fn actual_content(&self, refresh: bool) -> Result<HydratedContent> {
        let mut cache = self.actual.lock().await;
        if !refresh && let Some(content) = cache.as_ref() {
            return Ok(content.clone());
        }
        let content = self.knowledge.actual_content(&self.block).await?;
        *cache = Some(content.clone());
        Ok(content)
    }
    pub async fn relations(
        &self,
        include_in: bool,
        include_out: bool,
        refresh: bool,
    ) -> Result<Vec<Relation>> {
        if !include_in && !include_out {
            return Ok(vec![]);
        }
        let key = (include_in, include_out);
        let mut cache = self.relations.lock().await;
        if !refresh && let Some(relations) = cache.get(&key) {
            return Ok(relations.clone());
        }
        let relations:Vec<Relation>=sqlx::query_as("SELECT id,from_,to_,content,updated_at FROM inkcre.relations WHERE ($2 AND to_=$1) OR ($3 AND from_=$1) ORDER BY id LIMIT 1001")
            .bind(self.block.id).bind(include_in).bind(include_out).fetch_all(self.knowledge.core.pool()).await?;
        if relations.len() > 1000 {
            return Err(Error::Limit("direct relation read exceeds 1000 entries"));
        }
        cache.insert(key, relations.clone());
        Ok(relations)
    }
    pub async fn get_solved_content(&self, options: ReadOptions) -> Result<Value> {
        let mut cache = self.solved.lock().await;
        if !options.refresh
            && let Some(value) = cache.as_ref()
        {
            return Ok(value.clone());
        }
        let resolver = self.knowledge.registry.resolver(&self.block.resolver)?;
        let value = resolver.solve(self, options).await?;
        *cache = Some(value.clone());
        Ok(value)
    }
    pub async fn get_text(&self, options: TextOptions) -> Result<Option<String>> {
        self.knowledge
            .registry
            .resolver(&self.block.resolver)?
            .text(self, options)
            .await
    }
    pub async fn get_label(&self, refresh: bool) -> Result<String> {
        self.knowledge
            .registry
            .resolver(&self.block.resolver)?
            .label(self, refresh)
            .await
    }
    pub async fn transfer_url(&self) -> Result<Option<String>> {
        let Some(storage_id) = self.block.storage else {
            return Ok(None);
        };
        let storage = self
            .knowledge
            .core
            .storage_get(storage_id)
            .await?
            .ok_or(crate::Error::NotFound("storage"))?;
        self.knowledge
            .registry
            .storage(&storage.type_)?
            .transfer_url(&storage, &self.block.content)
    }
    pub async fn invoke(&self, method: &str, arguments: Value) -> Result<Value> {
        let contract = self
            .knowledge
            .read_methods(&self.block.resolver)?
            .into_iter()
            .find(|contract| contract.name == method)
            .ok_or_else(|| Error::UnknownMethod(method.into()))?;
        crate::models::validate_json(&contract.input_schema, &arguments)?;
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Refresh {
            refresh: bool,
        }
        #[derive(Deserialize)]
        #[serde(default)]
        struct Relations {
            refresh: bool,
            include_in: bool,
            include_out: bool,
        }
        impl Default for Relations {
            fn default() -> Self {
                Self {
                    refresh: false,
                    include_in: true,
                    include_out: true,
                }
            }
        }
        let invalid =
            |_| Error::InvalidInput("read-method arguments do not match their typed contract");
        let to_value = |value| {
            serde_json::to_value(value)
                .map_err(|_| Error::InvalidInput("read result cannot be represented"))
        };
        match method {
            "get_raw_content" => {
                let options: Refresh = serde_json::from_value(arguments).map_err(invalid)?;
                to_value(self.actual_content(options.refresh).await?)
            }
            "get_solved_content" => {
                self.get_solved_content(serde_json::from_value(arguments).map_err(invalid)?)
                    .await
            }
            "get_text" => Ok(self
                .get_text(serde_json::from_value(arguments).map_err(invalid)?)
                .await?
                .map_or(Value::Null, Value::String)),
            "get_label" => {
                let options: Refresh = serde_json::from_value(arguments).map_err(invalid)?;
                Ok(Value::String(self.get_label(options.refresh).await?))
            }
            "get_transfer_url" => Ok(self
                .transfer_url()
                .await?
                .map_or(Value::Null, Value::String)),
            "get_relations" => {
                let options: Relations = serde_json::from_value(arguments).map_err(invalid)?;
                serde_json::to_value(
                    self.relations(options.include_in, options.include_out, options.refresh)
                        .await?,
                )
                .map_err(|_| Error::InvalidInput("Relations cannot be represented"))
            }
            _ => {
                self.knowledge
                    .registry
                    .resolver(&self.block.resolver)?
                    .invoke(self, method, arguments)
                    .await
            }
        }
    }
}
