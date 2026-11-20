mod schema;

use async_trait::async_trait;
use inkcre_core::{
    Core,
    knowledge::{TextOptions, TextProjectionContext, decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, PROTOCOL_VERSION,
    RelationSelection, ResolverCapability, ResolverOperation, ResolverRequest, RpcError,
    SDK_VERSION, StartContext, TextContext, serve,
};
use schema::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, RpcError>;
fn invalid(message: &str) -> RpcError {
    RpcError::new("graph_integrity", message)
}
fn decode<T: DeserializeOwned>(content: &Content) -> Result<T> {
    let text = match content {
        Content::Text(text) => text.clone(),
        Content::Bytes(bytes) => decode_unicode_bytes(bytes)
            .map_err(|_| invalid("Capture content must be Unicode JSON"))?,
    };
    serde_json::from_str(&text).map_err(|_| invalid("Capture metadata is invalid"))
}
fn metadata(content: &Content) -> Result<Metadata> {
    let value: Metadata = decode(content)?;
    if value.payload_digest.len() != 64
        || !value
            .payload_digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid("Capture digest is invalid"));
    }
    if let Some(origin) = &value.origin {
        for (value, bound) in [
            (&origin.application_id, 1024),
            (&origin.title, 4096),
            (&origin.url, 8192),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| value.chars().count() > bound)
            {
                return Err(invalid("Capture origin exceeds its bound"));
            }
        }
    }
    Ok(value)
}
fn attachment(content: &Content) -> Result<Attachment> {
    let value: Attachment = decode(content)?;
    if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&value.mime_type.as_str())
        || value
            .filename
            .as_ref()
            .is_some_and(|name| name.chars().count() > 512)
    {
        return Err(invalid("Capture attachment metadata is invalid"));
    }
    Ok(value)
}
async fn require_block(core: &Core, id: i32, allowed: &[&str]) -> Result<inkcre_core::Block> {
    let block = core
        .block_get(id)
        .await
        .map_err(|_| invalid("Capture graph could not be read"))?
        .ok_or_else(|| invalid("Capture child is missing"))?;
    if !allowed.contains(&block.resolver.as_str()) {
        return Err(invalid("Capture child resolver is invalid"));
    }
    Ok(block)
}
async fn solve_capture(core: &Core, request: &ResolverRequest) -> Result<SolvedCapture> {
    let metadata = metadata(&request.input.content)?;
    let mut materials = BTreeMap::new();
    let mut annotation = None;
    for edge in &request.input.relations {
        if edge.from_ != Some(request.input.block.id) {
            continue;
        }
        if let Some(index) = edge.content.strip_prefix("material:") {
            if index.is_empty()
                || (index.len() > 1 && index.starts_with('0'))
                || !index.bytes().all(|c| c.is_ascii_digit())
            {
                return Err(invalid("Capture material index is invalid"));
            }
            let index: usize = index
                .parse()
                .map_err(|_| invalid("Capture material index exceeds its bound"))?;
            let id = edge
                .to_
                .ok_or_else(|| invalid("Capture material endpoint is absent"))?;
            if materials.insert(index, id).is_some() {
                return Err(invalid("Capture material index is duplicated"));
            }
        } else if edge.content == "annotation" {
            if annotation.is_some() {
                return Err(invalid("Capture annotation is ambiguous"));
            }
            annotation = Some(
                edge.to_
                    .ok_or_else(|| invalid("Capture annotation endpoint is absent"))?,
            );
        }
    }
    let mut solved = Vec::new();
    for (expected, (index, id)) in materials.into_iter().enumerate() {
        if index != expected {
            return Err(invalid("Capture material sequence has a gap"));
        }
        let block = require_block(core, id, &["core.text.v1", ATTACHMENT]).await?;
        solved.push(Material {
            block_id: id,
            resolver: block.resolver,
        });
    }
    if let Some(id) = annotation {
        require_block(core, id, &["core.text.v1"]).await?;
    }
    Ok(SolvedCapture {
        metadata,
        materials: solved,
        annotation_block_id: annotation,
    })
}
async fn solve_attachment(core: &Core, request: &ResolverRequest) -> Result<SolvedAttachment> {
    let metadata = attachment(&request.input.content)?;
    let links: Vec<_> = request
        .input
        .relations
        .iter()
        .filter(|edge| edge.from_ == Some(request.input.block.id) && edge.content == "content")
        .collect();
    if links.len() != 1 {
        return Err(invalid("Capture attachment must have one content relation"));
    }
    let id = links[0]
        .to_
        .ok_or_else(|| invalid("Capture image endpoint is absent"))?;
    require_block(core, id, &["core.image.v1"]).await?;
    Ok(SolvedAttachment {
        metadata,
        content_block_id: id,
    })
}
fn options(request: &ResolverRequest) -> TextOptions {
    match request.operation {
        ResolverOperation::Text {
            refresh,
            materialize_missing,
            context,
        } => TextOptions {
            refresh,
            materialize_missing,
            context: match context {
                TextContext::Default => TextProjectionContext::Default,
                TextContext::Lexical => TextProjectionContext::Lexical,
            },
        },
        _ => TextOptions::default(),
    }
}
async fn attachment_text(
    core: &Core,
    solved: SolvedAttachment,
    options: TextOptions,
) -> Result<String> {
    let text = core
        .knowledge()
        .text(solved.content_block_id, options)
        .await
        .map_err(|_| invalid("Capture image text projection failed"))?;
    Ok([solved.metadata.filename, text]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n"))
}
async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    let opts = options(&request);
    match request.resolver.as_str() {
        CAPTURE => match request.operation {
            ResolverOperation::Label { .. } => {
                let metadata = metadata(&request.input.content)?;
                let entry = serde_json::to_value(&metadata.entry)
                    .map_err(|_| invalid("Capture entry is invalid"))?;
                Ok(json!(format_label(
                    "随手收集",
                    metadata
                        .origin
                        .as_ref()
                        .and_then(|o| o.title.as_deref())
                        .filter(|s| !s.is_empty())
                        .or(entry.as_str()),
                    false
                )))
            }
            ResolverOperation::Solve { .. } => Ok(json!(solve_capture(core, &request).await?)),
            ResolverOperation::Text { .. } => {
                let solved = solve_capture(core, &request).await?;
                let mut parts = vec![format!(
                    "采集时间：{}",
                    solved
                        .metadata
                        .captured_at
                        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
                )];
                if let Some(origin) = solved.metadata.origin {
                    for (label, value) in [
                        ("来源应用", origin.application_id),
                        ("来源标题", origin.title),
                        ("来源地址", origin.url),
                    ] {
                        if let Some(value) = value.filter(|s| !s.is_empty()) {
                            parts.push(format!("{label}：{value}"));
                        }
                    }
                }
                let mut children: Vec<_> = solved
                    .materials
                    .into_iter()
                    .map(|m| (m.block_id, m.resolver, "材料"))
                    .collect();
                if let Some(id) = solved.annotation_block_id {
                    children.push((id, "core.text.v1".into(), "用户附注"));
                }
                for (id, resolver, label) in children {
                    let text =
                        if resolver == ATTACHMENT {
                            let reader = core
                                .knowledge()
                                .reader(id)
                                .await
                                .map_err(|_| invalid("Capture attachment could not be read"))?;
                            let content =
                                reader.actual_content(opts.refresh).await.map_err(|_| {
                                    invalid("Capture attachment content could not be read")
                                })?;
                            let raw = match content {
                                inkcre_core::HydratedContent::Text(text) => Content::Text(text),
                                inkcre_core::HydratedContent::Bytes(bytes) => Content::Bytes(bytes),
                            };
                            let relations =
                                reader.relations(false, true, opts.refresh).await.map_err(
                                    |_| invalid("Capture attachment relations could not be read"),
                                )?;
                            let nested = ResolverRequest {
                                resolver: ATTACHMENT.into(),
                                input: inkcre_extension_sdk::ResolverInput {
                                    block: inkcre_extension_sdk::BlockSnapshot {
                                        id,
                                        storage: reader.block.storage,
                                        resolver: reader.block.resolver.clone(),
                                        content: reader.block.content.clone(),
                                    },
                                    content: raw,
                                    relations: relations
                                        .into_iter()
                                        .map(|edge| inkcre_extension_sdk::RelationSnapshot {
                                            id: edge.id,
                                            from_: edge.from_,
                                            to_: edge.to_,
                                            content: edge.content,
                                        })
                                        .collect(),
                                },
                                operation: request.operation.clone(),
                            };
                            attachment_text(core, solve_attachment(core, &nested).await?, opts)
                                .await?
                        } else {
                            core.knowledge()
                                .text(id, opts)
                                .await
                                .map_err(|_| invalid("Capture text projection failed"))?
                                .unwrap_or_default()
                        };
                    parts.push(format!("{label}：\n{text}"));
                }
                Ok(json!(parts.join("\n\n")))
            }
            _ => Err(invalid("Capture has no custom read method")),
        },
        ATTACHMENT => {
            let solved = solve_attachment(core, &request).await?;
            match request.operation {
                ResolverOperation::Solve { .. } => Ok(json!(solved)),
                ResolverOperation::Label { .. } => Ok(json!(format_label(
                    "采集图片",
                    solved
                        .metadata
                        .filename
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .or(Some(&solved.metadata.mime_type)),
                    false
                ))),
                ResolverOperation::Text { .. } => {
                    Ok(json!(attachment_text(core, solved, opts).await?))
                }
                _ => Err(invalid("Capture has no custom read method")),
            }
        }
        _ => Err(invalid("Capture resolver is unknown")),
    }
}
#[derive(Default)]
struct Capture {
    core: Option<Core>,
}
#[async_trait]
impl Extension for Capture {
    async fn invoke(&mut self, _method: &str, _params: Value) -> Result<Value> {
        Err(RpcError::new(
            "unknown_method",
            "Capture has no generic command",
        ))
    }
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "inkcre/capture".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","additionalProperties":false,"properties":{}}),
            methods: vec![],
            capabilities: Capabilities {
                passive_knowledge: true,
                resolvers: [
                    (CAPTURE, capture_schema()),
                    (ATTACHMENT, attachment_schema()),
                ]
                .into_iter()
                .map(|(id, schema)| ResolverCapability {
                    id: id.into(),
                    description: "Interpret shared Capture graph".into(),
                    content: ContentContract {
                        kind: ContentKind::Json,
                        schema: Some(schema),
                    },
                    relations: RelationSelection {
                        include_in: false,
                        include_out: true,
                    },
                    read_methods: vec![],
                    draft: None,
                })
                .collect(),
                ..Default::default()
            },
        }
    }
    async fn start_knowledge(
        &mut self,
        context: inkcre_extension_sdk::KnowledgeStartContext,
    ) -> std::result::Result<(), RpcError> {
        self.start(inkcre_extension_sdk::StartContext {
            peer_id: context.peer_id,
            config: json!({}),
            state: json!({}),
        })
        .await
        .map(|_| ())
    }
    async fn stop_knowledge(&mut self) -> std::result::Result<(), RpcError> {
        self.stop().await
    }
    async fn start(&mut self, _context: StartContext) -> Result<Value> {
        let url = std::env::var("INKCRE_DATABASE_URL")
            .map_err(|_| invalid("Capture requires native database capability"))?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|_| invalid("Capture database admission failed"))?,
        );
        Ok(json!({"started":true}))
    }
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        read(
            self.core
                .as_ref()
                .ok_or_else(|| invalid("Capture is not started"))?,
            request,
        )
        .await
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match serve(Capture::default()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("Capture protocol failed");
            std::process::ExitCode::FAILURE
        }
    }
}
