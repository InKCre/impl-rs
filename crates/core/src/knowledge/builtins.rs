use super::{
    DraftCapability, Error, GraphDraft, ReadOptions, Resolver, ResolverContext, ResolverDescriptor,
    Result, TextOptions, single_block_draft,
};
use crate::HydratedContent;
use async_trait::async_trait;
use scraper::{Html, Selector};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn resolvers() -> Vec<Arc<dyn Resolver>> {
    vec![
        Arc::new(TextResolver {
            kind: TextKind::Text,
        }),
        Arc::new(TextResolver {
            kind: TextKind::Markdown,
        }),
        Arc::new(TextResolver {
            kind: TextKind::Html,
        }),
        Arc::new(FileResolver),
        Arc::new(crate::source::SourceAnchorResolver),
    ]
}

struct FileResolver;

#[async_trait]
impl Resolver for FileResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: "core.file.v1".into(),
            description: "Exact generic-file bytes without stronger semantic decoding".into(),
            read_methods: vec![],
            draft: None,
        }
    }
    fn validate_content(&self, content: &HydratedContent) -> Result<()> {
        if !matches!(content, HydratedContent::Bytes(_)) {
            return Err(Error::InvalidContent {
                resolver: "core.file.v1".into(),
                reason: "generic-file content must be storage-backed bytes",
            });
        }
        Ok(())
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value> {
        let HydratedContent::Bytes(content) = context.actual_content(options.refresh).await? else {
            return Err(Error::InvalidContent {
                resolver: "core.file.v1".into(),
                reason: "generic-file content must be storage-backed bytes",
            });
        };
        let byte_size = content.len();
        let detected_media_type = detect_media_type(&content);
        Ok(
            json!({"content":content,"byte_size":byte_size,"detected_media_type":detected_media_type}),
        )
    }
    async fn text(
        &self,
        context: &ResolverContext,
        options: TextOptions,
    ) -> Result<Option<String>> {
        if options.context == super::TextProjectionContext::Default {
            return Err(Error::Unsupported("core.file.v1 text projection".into()));
        }
        let HydratedContent::Bytes(content) = context.actual_content(options.refresh).await? else {
            return Err(Error::InvalidContent {
                resolver: "core.file.v1".into(),
                reason: "generic-file content must be storage-backed bytes",
            });
        };
        let mut text = "file".to_owned();
        if let Some(media) = detect_media_type(&content) {
            text.push_str(&format!("\nmedia type: {media}"));
        }
        text.push_str(&format!("\nbytes: {}", content.len()));
        Ok(Some(text))
    }
    async fn label(&self, _context: &ResolverContext, _refresh: bool) -> Result<String> {
        Ok("file".into())
    }
}

/// Byte-derived detection is evidence, never an implicit choice of Resolver.
pub fn detect_media_type(content: &[u8]) -> Option<&'static str> {
    infer::get(content).map(|kind| kind.mime_type())
}

enum TextKind {
    Text,
    Markdown,
    Html,
}
struct TextResolver {
    kind: TextKind,
}

impl TextResolver {
    fn id(&self) -> &'static str {
        match self.kind {
            TextKind::Text => "core.text.v1",
            TextKind::Markdown => "core.markdown.v1",
            TextKind::Html => "core.html.v1",
        }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            TextKind::Text => "text",
            TextKind::Markdown => "markdown",
            TextKind::Html => "html",
        }
    }
    fn decode(&self, content: &HydratedContent) -> Result<String> {
        match content {
            HydratedContent::Text(text) => Ok(text.clone()),
            HydratedContent::Bytes(bytes) => {
                if matches!(self.kind, TextKind::Html) {
                    decode_html_bytes(bytes)
                } else {
                    decode_unicode_bytes(bytes)
                }
            }
        }
        .map_err(|_| Error::InvalidContent {
            resolver: self.id().into(),
            reason: "invalid text encoding",
        })
    }
}

#[async_trait]
impl Resolver for TextResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: self.id().into(),
            description: format!("Exact {} source interpretation", self.name()),
            read_methods: vec![],
            draft: Some(DraftCapability {
                description: format!("Draft one {} semantic content Block", self.name()),
                input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
            }),
        }
    }
    fn validate_content(&self, content: &HydratedContent) -> Result<()> {
        self.decode(content).map(|_| ())
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value> {
        Ok(Value::String(
            self.decode(&context.actual_content(options.refresh).await?)?,
        ))
    }
    async fn text(
        &self,
        context: &ResolverContext,
        options: TextOptions,
    ) -> Result<Option<String>> {
        let source = self.decode(&context.actual_content(options.refresh).await?)?;
        Ok(Some(if matches!(self.kind, TextKind::Html) {
            tokio::task::spawn_blocking(move || html_text(&source))
                .await
                .map_err(|_| Error::InvalidInput("HTML projection task failed"))??
        } else {
            source
        }))
    }
    async fn label(&self, context: &ResolverContext, refresh: bool) -> Result<String> {
        let source = self.decode(&context.actual_content(refresh).await?)?;
        if matches!(self.kind, TextKind::Html) {
            tokio::task::spawn_blocking(move || {
                let document = Html::parse_document(&source);
                let title = Selector::parse("title")
                    .map_err(|_| Error::InvalidInput("invalid static HTML selector"))?;
                let headings = Selector::parse("h1,h2,h3,h4,h5,h6")
                    .map_err(|_| Error::InvalidInput("invalid static HTML selector"))?;
                let identifier = document
                    .select(&title)
                    .chain(document.select(&headings))
                    .map(|element| element.text().collect::<String>())
                    .find(|text| !text.trim().is_empty());
                Ok(format_label("html", identifier.as_deref(), false))
            })
            .await
            .map_err(|_| Error::InvalidInput("HTML label task failed"))?
        } else {
            Ok(format_label(self.name(), Some(&source), true))
        }
    }
    async fn draft(&self, _core: &crate::Core, input: Value) -> Result<GraphDraft> {
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or(Error::InvalidInput("draft requires text"))?;
        Ok(single_block_draft(self.id(), text.into()))
    }
}

/// A JSON codec for an extension-owned exact Resolver ID. This does not invent
/// `core.json.v1`; JSON encoding alone is not a semantic Resolver contract.
pub struct JsonResolver {
    descriptor: ResolverDescriptor,
    content_schema: Value,
}

impl JsonResolver {
    pub fn new(
        id: impl Into<String>,
        description: impl Into<String>,
        content_schema: Value,
    ) -> Result<Self> {
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&content_schema)
            .map_err(|_| Error::InvalidInput("invalid JSON content schema"))?;
        Ok(Self {
            descriptor: ResolverDescriptor {
                id: id.into(),
                description: description.into(),
                read_methods: vec![],
                draft: Some(DraftCapability {
                    description: "Draft one exact JSON content Block".into(),
                    input_schema: content_schema.clone(),
                }),
            },
            content_schema,
        })
    }
    fn decode(&self, content: &HydratedContent) -> Result<Value> {
        let text = match content {
            HydratedContent::Text(text) => text.clone(),
            HydratedContent::Bytes(bytes) => decode_unicode_bytes(bytes)?,
        };
        serde_json::from_str(&text).map_err(|_| Error::InvalidContent {
            resolver: self.descriptor.id.clone(),
            reason: "invalid JSON encoding",
        })
    }
}

#[async_trait]
impl Resolver for JsonResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        self.descriptor.clone()
    }
    fn validate_content(&self, content: &HydratedContent) -> Result<()> {
        crate::models::validate_json(&self.content_schema, &self.decode(content)?)?;
        Ok(())
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value> {
        // Restore representation only; new-input validation belongs to admission.
        self.decode(&context.actual_content(options.refresh).await?)
    }
    async fn draft(&self, _core: &crate::Core, input: Value) -> Result<GraphDraft> {
        crate::models::validate_json(&self.content_schema, &input)?;
        Ok(single_block_draft(&self.descriptor.id, input.to_string()))
    }
}

/// Unicode BOMs are explicit; unmarked bytes must be strict UTF-8.
pub fn decode_unicode_bytes(bytes: &[u8]) -> Result<String> {
    let invalid = || Error::InvalidInput("invalid Unicode content");
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe, 0, 0]) {
        return decode_utf32(bytes, true);
    }
    if let Some(bytes) = bytes.strip_prefix(&[0, 0, 0xfe, 0xff]) {
        return decode_utf32(bytes, false);
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        return encoding_rs::UTF_16LE
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(|text| text.into_owned())
            .ok_or_else(invalid);
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return encoding_rs::UTF_16BE
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(|text| text.into_owned())
            .ok_or_else(invalid);
    }
    String::from_utf8(
        bytes
            .strip_prefix(&[0xef, 0xbb, 0xbf])
            .unwrap_or(bytes)
            .to_vec(),
    )
    .map_err(|_| invalid())
}

fn decode_utf32(bytes: &[u8], little_endian: bool) -> Result<String> {
    if !bytes.len().is_multiple_of(4) {
        return Err(Error::InvalidInput("invalid UTF-32 content"));
    }
    let mut text = String::new();
    for bytes in bytes.chunks_exact(4) {
        let value = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let value = if little_endian {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        };
        text.push(char::from_u32(value).ok_or(Error::InvalidInput("invalid UTF-32 code point"))?);
    }
    Ok(text)
}

pub fn decode_html_bytes(bytes: &[u8]) -> Result<String> {
    if [
        b"\xff\xfe\0\0".as_slice(),
        b"\0\0\xfe\xff",
        b"\xef\xbb\xbf",
        b"\xff\xfe",
        b"\xfe\xff",
    ]
    .iter()
    .any(|bom| bytes.starts_with(bom))
    {
        return decode_unicode_bytes(bytes);
    }
    // Only the bounded declaration prefix is parsed before charset selection.
    let prefix: String = bytes
        .iter()
        .take(4096)
        .map(|byte| char::from(*byte))
        .collect();
    let document = Html::parse_document(&prefix);
    let selector =
        Selector::parse("meta").map_err(|_| Error::InvalidInput("invalid static HTML selector"))?;
    let charset = document.select(&selector).find_map(|meta| {
        if let Some(charset) = meta.value().attr("charset") {
            return Some(charset.trim().to_owned());
        }
        let value = meta.value().attr("content")?;
        value.split(';').find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['\'', '"']).to_owned())
        })
    });
    let Some(charset) = charset else {
        return decode_unicode_bytes(bytes);
    };
    let encoding = encoding_rs::Encoding::for_label(charset.as_bytes())
        .ok_or(Error::InvalidInput("unknown HTML charset"))?;
    encoding
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|text| text.into_owned())
        .ok_or(Error::InvalidInput("invalid declared HTML encoding"))
}

/// Convert saved HTML to text without fetching resources or executing scripts.
/// This is a structural projection, not article-main-body extraction.
pub fn html_text(source: &str) -> Result<String> {
    let document = Html::parse_document(source);
    let selector = Selector::parse("video,audio,source,track")
        .map_err(|_| Error::InvalidInput("invalid static HTML selector"))?;
    let mut source = source.to_owned();
    for media in document.select(&selector) {
        for attribute in ["src", "poster"] {
            if let Some(address) = media.value().attr(attribute) {
                // Append escaped anchors to retain media targets in text only.
                let escaped = address
                    .replace('&', "&amp;")
                    .replace('"', "&quot;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                source.push_str(&format!("<p><a href=\"{escaped}\">{escaped}</a></p>"));
            }
        }
    }
    html2text::config::plain()
        .no_link_wrapping()
        .string_from_read(source.as_bytes(), 120)
        .map_err(|_| Error::InvalidInput("HTML text projection failed"))
}

pub fn format_label(kind: &str, identifier: Option<&str>, first_line: bool) -> String {
    let identifier = identifier
        .and_then(|value| {
            if first_line {
                value.lines().find(|line| !line.trim().is_empty())
            } else {
                Some(value)
            }
        })
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| !value.is_empty());
    match identifier {
        None => kind.into(),
        Some(value) => {
            let mut identifier: String = value.chars().take(96).collect();
            if value.chars().count() > 96 {
                identifier.push('…');
            }
            format!("{kind} <{identifier}>")
        }
    }
}
