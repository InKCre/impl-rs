//! 将纯 SDK 协议适配为 Core 本机业务 trait；没有对 ExtensionHost 的反向强引用。

use async_trait::async_trait;
use inkcre_extension_sdk as wire;
use serde_json::Value;
use sqlx::PgConnection;

use super::{
    Arc, CALL_TIMEOUT, Descriptor, Error, ExtensionHost, Manifest, Process, Publication, Result,
    Weak,
};
use crate::{
    Core, HydratedContent, Job, JobType, Storage, knowledge as k,
    source::{Source, SourceCommand, SourceHandler, SourceType},
    worker::{JobContext, JobHandler},
};

#[derive(Clone)]
struct RuntimeLink {
    process: Weak<Process>,
    extension: String,
    version: String,
    peer_id: uuid::Uuid,
}

impl RuntimeLink {
    fn process(&self) -> crate::Result<Arc<Process>> {
        self.process
            .upgrade()
            .filter(|process| process.is_alive())
            .ok_or_else(|| crate::Error::Unsupported("Extension process is unavailable".into()))
    }

    async fn selected(&self, core: &Core) -> crate::Result<bool> {
        Ok(core
            .participation_get(&self.extension, self.peer_id)
            .await?
            .is_some_and(|selected| {
                selected.enabled
                    && selected.distribution.as_deref() == Some(wire::DISTRIBUTION)
                    && selected.version == self.version
            }))
    }

    async fn eligible(&self, core: &Core) -> crate::Result<bool> {
        if !self
            .process
            .upgrade()
            .is_some_and(|process| process.is_idle())
        {
            return Ok(false);
        }
        self.selected(core).await
    }

    async fn available(&self, core: &Core) -> crate::Result<bool> {
        if !self
            .process
            .upgrade()
            .is_some_and(|process| process.is_alive())
        {
            return Ok(false);
        }
        self.selected(core).await
    }

    async fn short(&self, core: &Core, method: &str, params: Value) -> Result<Value> {
        if !self.selected(core).await? {
            return Err(Error::Conflict("Extension 精确选择已撤回".into()));
        }
        self.process()?
            .call(method, params, Some(CALL_TIMEOUT))
            .await
    }

    async fn long(
        &self,
        context: &JobContext,
        method: &str,
        params: Value,
    ) -> crate::Result<Value> {
        if !self.selected(context.core()).await? {
            return Err(crate::Error::Unsupported(
                "Extension selection is unavailable".into(),
            ));
        }
        let process = self.process()?;
        let result = tokio::select! {
            biased;
            _ = context.cancelled() => Err(crate::Error::Conflict("Extension Job execution was cancelled".into())),
            _ = process.stopped() => {
                context.cancel();
                Err(crate::Error::Conflict("Extension execution capability was withdrawn".into()))
            },
            result = process.call(method, params, None) => result.map_err(core_error),
        };
        // select 已丢弃被取消的 RPC future，guard 会发出 kill；回收后才返回 executor。
        if !process.is_alive() {
            process.terminate().await;
        }
        result
    }
}

fn core_error(error: Error) -> crate::Error {
    match error {
        Error::Core(error) => error,
        Error::Remote(_) => {
            crate::Error::InvalidInput("Extension business operation failed".into())
        }
        _ => crate::Error::Unsupported("Extension runtime operation failed".into()),
    }
}

fn knowledge_error(error: Error) -> k::Error {
    match error {
        Error::Remote(code) if code == "unsupported_capability" => {
            k::Error::Unsupported("Extension projection".into())
        }
        error => k::Error::Core(core_error(error)),
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(Error::from)
}

struct ResolverAdapter {
    link: RuntimeLink,
    capability: wire::ResolverCapability,
}

impl ResolverAdapter {
    async fn read(
        &self,
        context: &k::ResolverContext,
        operation: wire::ResolverOperation,
    ) -> k::Result<Value> {
        let refresh = match operation {
            wire::ResolverOperation::Solve { refresh, .. }
            | wire::ResolverOperation::Text { refresh, .. }
            | wire::ResolverOperation::Label { refresh } => refresh,
            wire::ResolverOperation::Read { .. } => false,
        };
        let content = match context.actual_content(refresh).await? {
            HydratedContent::Text(text) => wire::Content::Text(text),
            HydratedContent::Bytes(bytes) => wire::Content::Bytes(bytes),
        };
        let relations =
            if self.capability.relations.include_in || self.capability.relations.include_out {
                context
                    .relations(
                        self.capability.relations.include_in,
                        self.capability.relations.include_out,
                        refresh,
                    )
                    .await?
                    .into_iter()
                    .map(|relation| wire::RelationSnapshot {
                        id: relation.id,
                        from_: relation.from_,
                        to_: relation.to_,
                        content: relation.content,
                    })
                    .collect()
            } else {
                Vec::new()
            };
        let block = &context.block;
        let request = wire::ResolverRequest {
            resolver: self.capability.id.clone(),
            input: wire::ResolverInput {
                block: wire::BlockSnapshot {
                    id: block.id,
                    storage: block.storage,
                    resolver: block.resolver.clone(),
                    content: block.content.clone(),
                },
                content,
                relations,
            },
            operation,
        };
        self.link
            .short(
                context.knowledge.core(),
                "resolver_read",
                encode(request).map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)
    }
}

#[async_trait]
impl k::Resolver for ResolverAdapter {
    fn descriptor(&self) -> k::ResolverDescriptor {
        k::ResolverDescriptor {
            id: self.capability.id.clone(),
            description: self.capability.description.clone(),
            read_methods: self
                .capability
                .read_methods
                .iter()
                .map(|method| k::ReadMethod {
                    name: method.name.clone(),
                    description: method.description.clone(),
                    input_schema: method.input_schema.clone(),
                })
                .collect(),
            draft: self
                .capability
                .draft
                .as_ref()
                .map(|draft| k::DraftCapability {
                    description: draft.description.clone(),
                    input_schema: draft.input_schema.clone(),
                }),
        }
    }

    fn validate_content(&self, content: &HydratedContent) -> k::Result<()> {
        let invalid = || k::Error::InvalidContent {
            resolver: self.capability.id.clone(),
            reason: "content differs from the declared Extension contract",
        };
        let value = match self.capability.content.kind {
            wire::ContentKind::Bytes => match content {
                HydratedContent::Bytes(bytes) => {
                    if self.capability.content.schema.is_none() {
                        return Ok(());
                    }
                    serde_json::to_value(bytes).map_err(|_| invalid())?
                }
                _ => return Err(invalid()),
            },
            wire::ContentKind::Text => Value::String(match content {
                HydratedContent::Text(text) => text.clone(),
                HydratedContent::Bytes(bytes) => {
                    String::from_utf8(bytes.clone()).map_err(|_| invalid())?
                }
            }),
            wire::ContentKind::Json => match content {
                HydratedContent::Text(text) => serde_json::from_str(text).map_err(|_| invalid())?,
                HydratedContent::Bytes(bytes) => {
                    serde_json::from_slice(bytes).map_err(|_| invalid())?
                }
            },
        };
        if let Some(schema) = &self.capability.content.schema {
            crate::models::validate_json(schema, &value).map_err(|_| invalid())?;
        }
        Ok(())
    }

    async fn solve(
        &self,
        context: &k::ResolverContext,
        options: k::ReadOptions,
    ) -> k::Result<Value> {
        self.read(
            context,
            wire::ResolverOperation::Solve {
                refresh: options.refresh,
                materialize_missing: options.materialize_missing,
            },
        )
        .await
    }

    async fn text(
        &self,
        context: &k::ResolverContext,
        options: k::TextOptions,
    ) -> k::Result<Option<String>> {
        let value = self
            .read(
                context,
                wire::ResolverOperation::Text {
                    refresh: options.refresh,
                    materialize_missing: options.materialize_missing,
                    context: match options.context {
                        k::TextProjectionContext::Default => wire::TextContext::Default,
                        k::TextProjectionContext::Lexical => wire::TextContext::Lexical,
                    },
                },
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension text result must be string or null"))
    }

    async fn label(&self, context: &k::ResolverContext, refresh: bool) -> k::Result<String> {
        let value = self
            .read(context, wire::ResolverOperation::Label { refresh })
            .await?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension label result must be a string"))
    }

    async fn invoke(
        &self,
        context: &k::ResolverContext,
        method: &str,
        arguments: Value,
    ) -> k::Result<Value> {
        self.read(
            context,
            wire::ResolverOperation::Read {
                method: method.into(),
                args: arguments,
            },
        )
        .await
    }

    async fn draft(&self, core: &Core, input: Value) -> k::Result<k::GraphDraft> {
        let value = self
            .link
            .short(
                core,
                "resolver_draft",
                encode(wire::ResolverDraftRequest {
                    resolver: self.capability.id.clone(),
                    input,
                })
                .map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension draft result must be a typed graph"))
    }
}

struct StorageAdapter {
    link: RuntimeLink,
    capability: wire::StorageCapability,
}

fn storage_snapshot(storage: &Storage) -> wire::StorageSnapshot {
    wire::StorageSnapshot {
        id: storage.id,
        type_id: storage.type_.clone(),
        config: storage.config.clone(),
    }
}

#[async_trait]
impl k::StorageHandler for StorageAdapter {
    fn descriptor(&self) -> k::StorageDescriptor {
        k::StorageDescriptor {
            id: self.capability.id.clone(),
            description: self.capability.description.clone(),
            config_schema: self.capability.config_schema.clone(),
            writable: self.capability.writable,
        }
    }
    async fn read(&self, core: &Core, storage: &Storage, pointer: &str) -> k::Result<Vec<u8>> {
        let value = self
            .link
            .short(
                core,
                "storage_read",
                encode(wire::StorageReadRequest {
                    storage: storage_snapshot(storage),
                    pointer: pointer.into(),
                })
                .map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension Storage read must return bytes"))
    }
    fn writable(&self) -> Option<&dyn k::WritableStorageHandler> {
        self.capability.writable.then_some(self)
    }
}

#[async_trait]
impl k::WritableStorageHandler for StorageAdapter {
    async fn create(
        &self,
        core: &Core,
        _connection: &mut PgConnection,
        storage: &Storage,
        data: &[u8],
    ) -> k::Result<String> {
        let value = self
            .link
            .short(
                core,
                "storage_create",
                encode(wire::StorageWriteRequest {
                    storage: storage_snapshot(storage),
                    pointer: None,
                    data: data.to_vec(),
                })
                .map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension Storage create must return its pointer"))
    }
    async fn update(
        &self,
        core: &Core,
        _connection: &mut PgConnection,
        storage: &Storage,
        pointer: &str,
        data: &[u8],
    ) -> k::Result<bool> {
        let value = self
            .link
            .short(
                core,
                "storage_update",
                encode(wire::StorageWriteRequest {
                    storage: storage_snapshot(storage),
                    pointer: Some(pointer.into()),
                    data: data.to_vec(),
                })
                .map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension Storage update must return existence"))
    }
    async fn delete(
        &self,
        core: &Core,
        _connection: &mut PgConnection,
        storage: &Storage,
        pointer: &str,
    ) -> k::Result<bool> {
        let value = self
            .link
            .short(
                core,
                "storage_delete",
                encode(wire::StorageReadRequest {
                    storage: storage_snapshot(storage),
                    pointer: pointer.into(),
                })
                .map_err(knowledge_error)?,
            )
            .await
            .map_err(knowledge_error)?;
        serde_json::from_value(value)
            .map_err(|_| k::Error::InvalidInput("Extension Storage delete must return existence"))
    }
}

struct SourceAdapter {
    link: RuntimeLink,
    capability: wire::SourceCapability,
}

impl SourceAdapter {
    async fn execute(
        &self,
        context: JobContext,
        source: Source,
        config: Value,
        backfill: bool,
    ) -> crate::Result<()> {
        if source.type_ != self.capability.id {
            return Err(crate::Error::Unsupported(
                "Source type changed before execution".into(),
            ));
        }
        let request = wire::SourceRequest {
            source: source.id,
            type_id: source.type_,
            job: context.job_id(),
            config,
        };
        let value = self
            .link
            .long(
                &context,
                if backfill {
                    "source_backfill"
                } else {
                    "source_collect"
                },
                encode(request).map_err(core_error)?,
            )
            .await?;
        let outcome: wire::SourceOutcome = serde_json::from_value(value).map_err(|_| {
            crate::Error::InvalidInput("Extension Source outcome is invalid".into())
        })?;
        context.patch_state(outcome.state).await
    }
}

#[async_trait]
impl SourceHandler for SourceAdapter {
    async fn can_handle(
        &self,
        core: &Core,
        source: &Source,
        command: SourceCommand,
    ) -> crate::Result<bool> {
        if source.type_ != self.capability.id
            || (command == SourceCommand::Backfill
                && self.capability.backfill_config_schema.is_none())
        {
            return Ok(false);
        }
        self.link.available(core).await
    }
    async fn collect(
        &self,
        context: JobContext,
        source: Source,
        config: Value,
    ) -> crate::Result<()> {
        self.execute(context, source, config, false).await
    }
    async fn backfill(
        &self,
        context: JobContext,
        source: Source,
        config: Value,
    ) -> crate::Result<()> {
        self.execute(context, source, config, true).await
    }
}

struct JobAdapter {
    link: RuntimeLink,
    capability: wire::JobCapability,
}

fn job_request(job: &Job, peer_id: uuid::Uuid) -> wire::JobRequest {
    wire::JobRequest {
        job: job.id,
        type_id: job.type_.clone(),
        peer_id: peer_id.to_string(),
        parameters: job.parameters.clone(),
        state: job.state.clone(),
    }
}

#[async_trait]
impl JobHandler for JobAdapter {
    fn job_type(&self) -> JobType {
        JobType {
            id: self.capability.id.clone(),
            description: self.capability.description.clone(),
            parameters_schema: self.capability.parameters_schema.clone(),
            default_timeout_seconds: self.capability.default_timeout_seconds,
        }
    }
    async fn can_handle(&self, core: &Core, job: &Job) -> crate::Result<bool> {
        if job.type_ != self.capability.id || !self.link.eligible(core).await? {
            return Ok(false);
        }
        let process = self.link.process()?;
        let Some(result) = process
            .try_call(
                "job_can_handle",
                encode(job_request(job, self.link.peer_id)).map_err(core_error)?,
            )
            .await
            .map_err(core_error)?
        else {
            return Ok(false);
        };
        serde_json::from_value(result).map_err(|_| {
            crate::Error::InvalidInput("Extension Job eligibility must be boolean".into())
        })
    }
    async fn handle(&self, context: JobContext, job: Job) -> crate::Result<()> {
        let value = self
            .link
            .long(
                &context,
                "job_execute",
                encode(job_request(&job, self.link.peer_id)).map_err(core_error)?,
            )
            .await?;
        let outcome: wire::JobOutcome = serde_json::from_value(value)
            .map_err(|_| crate::Error::InvalidInput("Extension Job outcome is invalid".into()))?;
        context.patch_state(outcome.state).await
    }
}

pub(super) async fn publish(
    host: &ExtensionHost,
    manifest: &Manifest,
    descriptor: &Descriptor,
    process: &Arc<Process>,
) -> Result<()> {
    descriptor.capabilities.validate()?;
    for capability in &descriptor.capabilities.resolvers {
        if let Some(schema) = &capability.content.schema {
            jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .build(schema)
                .map_err(|_| Error::Invalid("Resolver canonical content schema 无效".into()))?;
        }
    }
    if !descriptor.capabilities.jobs.is_empty() && host.jobs.is_none() {
        return Err(Error::Conflict("宿主未配置 Job registry".into()));
    }
    if !descriptor.capabilities.sources.is_empty() && host.sources.is_none() {
        return Err(Error::Conflict("宿主未配置 Source runtime".into()));
    }
    let owner = uuid::Uuid::new_v4();
    let registry = host.core.knowledge().registry().clone();
    *process
        .control
        .publication
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Publication {
        owner,
        knowledge: registry.clone(),
        jobs: host.jobs.clone(),
        sources: host
            .sources
            .as_ref()
            .map(|runtime| runtime.registry().clone()),
    });
    let link = RuntimeLink {
        process: Arc::downgrade(process),
        extension: manifest.name.clone(),
        version: manifest.version.to_string(),
        peer_id: host.peer_id,
    };
    for capability in &descriptor.capabilities.resolvers {
        registry
            .register_resolver(
                owner,
                Arc::new(ResolverAdapter {
                    link: link.clone(),
                    capability: capability.clone(),
                }),
            )
            .map_err(crate::Error::from)?;
    }
    for capability in &descriptor.capabilities.storages {
        registry
            .register_storage(
                owner,
                Arc::new(StorageAdapter {
                    link: link.clone(),
                    capability: capability.clone(),
                }),
            )
            .map_err(crate::Error::from)?;
    }
    if let Some(sources) = &host.sources {
        for capability in &descriptor.capabilities.sources {
            let source_type = SourceType {
                id: capability.id.clone(),
                description: capability.description.clone(),
                config_schema: capability.config_schema.clone(),
                collect_config_schema: capability.collect_config_schema.clone(),
                backfill_config_schema: capability.backfill_config_schema.clone(),
            };
            sources.registry().register(
                owner,
                source_type,
                Arc::new(SourceAdapter {
                    link: link.clone(),
                    capability: capability.clone(),
                }),
            )?;
        }
        sources.sync_catalog().await?;
    }
    if let Some(jobs) = &host.jobs {
        for capability in &descriptor.capabilities.jobs {
            jobs.register_owned(
                owner,
                Arc::new(JobAdapter {
                    link: link.clone(),
                    capability: capability.clone(),
                }),
            )?;
        }
        jobs.publish_catalog(&host.core).await?;
    }
    host.core
        .knowledge()
        .sync_storage_types()
        .await
        .map_err(crate::Error::from)?;
    Ok(())
}
