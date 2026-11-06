use super::{Error, GraphDraft, ResolverContext, Result};
use crate::{BlockInput, HydratedContent, Storage};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadMethod {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftCapability {
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverDescriptor {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub read_methods: Vec<ReadMethod>,
    pub draft: Option<DraftCapability>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadOptions {
    pub refresh: bool,
    pub materialize_missing: bool,
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            refresh: false,
            materialize_missing: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextProjectionContext {
    #[default]
    Default,
    Lexical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TextOptions {
    pub refresh: bool,
    pub materialize_missing: bool,
    pub context: TextProjectionContext,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            refresh: false,
            materialize_missing: true,
            context: TextProjectionContext::Default,
        }
    }
}

impl From<TextOptions> for ReadOptions {
    fn from(value: TextOptions) -> Self {
        Self {
            refresh: value.refresh,
            materialize_missing: value.materialize_missing,
        }
    }
}

/// A decoder receives an instance-local content snapshot and explicit effect options.
/// Ordinary persisted-record reads do not call `validate_content` again.
#[async_trait]
pub trait Resolver: Send + Sync {
    fn descriptor(&self) -> ResolverDescriptor;
    fn validate_content(&self, _content: &HydratedContent) -> Result<()> {
        Ok(())
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value>;
    async fn text(
        &self,
        _context: &ResolverContext,
        _options: TextOptions,
    ) -> Result<Option<String>> {
        Err(Error::Unsupported("Resolver text projection".into()))
    }
    async fn label(&self, _context: &ResolverContext, _refresh: bool) -> Result<String> {
        Err(Error::Unsupported("Resolver label projection".into()))
    }
    async fn invoke(
        &self,
        _context: &ResolverContext,
        method: &str,
        _arguments: Value,
    ) -> Result<Value> {
        Err(Error::UnknownMethod(method.into()))
    }
    async fn draft(&self, _core: &crate::Core, _input: Value) -> Result<GraphDraft> {
        Err(Error::Unsupported("Resolver graph drafting".into()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageDescriptor {
    pub id: String,
    pub description: String,
    pub config_schema: Value,
    pub writable: bool,
}

/// Storage owns opaque pointer mechanics and bytes, never MIME or information kind.
#[async_trait]
pub trait StorageHandler: Send + Sync {
    fn descriptor(&self) -> StorageDescriptor;
    async fn read(&self, core: &crate::Core, storage: &Storage, pointer: &str) -> Result<Vec<u8>>;
    /// PostgreSQL handlers override this to observe uncommitted writes in a graph UoW.
    async fn read_in(
        &self,
        core: &crate::Core,
        _connection: &mut PgConnection,
        storage: &Storage,
        pointer: &str,
    ) -> Result<Vec<u8>> {
        self.read(core, storage, pointer).await
    }
    fn transfer_url(&self, _storage: &Storage, _pointer: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn writable(&self) -> Option<&dyn WritableStorageHandler> {
        None
    }
}

/// Native PostgreSQL writes share the supplied graph transaction. An external
/// handler owns its external effects; a SQL rollback cannot reverse those effects.
#[async_trait]
pub trait WritableStorageHandler: Send + Sync {
    async fn create(
        &self,
        core: &crate::Core,
        connection: &mut PgConnection,
        storage: &Storage,
        data: &[u8],
    ) -> Result<String>;
    async fn update(
        &self,
        core: &crate::Core,
        connection: &mut PgConnection,
        storage: &Storage,
        pointer: &str,
        data: &[u8],
    ) -> Result<bool>;
    async fn delete(
        &self,
        core: &crate::Core,
        connection: &mut PgConnection,
        storage: &Storage,
        pointer: &str,
    ) -> Result<bool>;
}

pub fn common_read_methods() -> Vec<ReadMethod> {
    let refresh = json!({"type":"boolean","default":false});
    let mut methods = vec![];
    for (name, description, properties) in [
        (
            "get_raw_content",
            "Read actual inline text or Storage bytes.",
            json!({"refresh":refresh}),
        ),
        (
            "get_solved_content",
            "Read the exact Resolver's use-facing value.",
            json!({"refresh":refresh,"materialize_missing":{"type":"boolean","default":true}}),
        ),
        (
            "get_text",
            "Read a text projection; null and authored empty text differ.",
            json!({"refresh":refresh,"materialize_missing":{"type":"boolean","default":true},"context":{"enum":["default","lexical"],"default":"default"}}),
        ),
        (
            "get_label",
            "Read a concise Resolver-owned label.",
            json!({"refresh":refresh}),
        ),
        (
            "get_transfer_url",
            "Read an optional Storage transfer hint.",
            json!({}),
        ),
        (
            "get_relations",
            "Read direct Relations relative to this Block.",
            json!({"refresh":refresh,"include_in":{"type":"boolean","default":true},"include_out":{"type":"boolean","default":true}}),
        ),
    ] {
        methods.push(ReadMethod{name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"additionalProperties":false})});
    }
    methods
}

pub fn single_block_draft(resolver: &str, content: String) -> GraphDraft {
    GraphDraft {
        root: -1,
        blocks: vec![super::DraftBlock {
            id: -1,
            block: BlockInput {
                storage: None,
                resolver: resolver.into(),
                content,
            },
        }],
        relations: vec![],
    }
}
