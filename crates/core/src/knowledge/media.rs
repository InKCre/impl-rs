use super::{
    Error, ReadOptions, Resolver, ResolverContext, ResolverDescriptor, Result, TextOptions,
    TextProjectionContext, detect_media_type, format_label,
};
use crate::HydratedContent;
use async_trait::async_trait;
use image::{AnimationDecoder, ImageReader};
use serde_json::{Value, json};
use std::{io::Cursor, sync::Arc};

pub(crate) fn resolvers() -> Vec<Arc<dyn Resolver>> {
    vec![
        Arc::new(MediaResolver { pdf: false }),
        Arc::new(MediaResolver { pdf: true }),
    ]
}

struct MediaResolver {
    pdf: bool,
}

impl MediaResolver {
    fn id(&self) -> &'static str {
        if self.pdf {
            "core.pdf.v1"
        } else {
            "core.image.v1"
        }
    }
    fn invalid(&self) -> Error {
        Error::InvalidContent {
            resolver: self.id().into(),
            reason: "invalid media content",
        }
    }
}

fn inspect_image(bytes: &[u8]) -> Result<Value> {
    let invalid = || Error::InvalidContent {
        resolver: "core.image.v1".into(),
        reason: "invalid or unsupported image",
    };
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| invalid())?;
    let format = reader.format().ok_or_else(invalid)?;
    let decoded = reader.decode().map_err(|_| invalid())?;
    let frames = match format {
        image::ImageFormat::Gif => Some(
            image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|_| invalid())?
                .into_frames(),
        ),
        image::ImageFormat::WebP => Some(
            image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| invalid())?
                .into_frames(),
        ),
        image::ImageFormat::Png => {
            let decoder =
                image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(|_| invalid())?;
            if decoder.is_apng().map_err(|_| invalid())? {
                Some(decoder.apng().map_err(|_| invalid())?.into_frames())
            } else {
                None
            }
        }
        _ => None,
    };
    let frame_count = if let Some(frames) = frames {
        let mut count = 0;
        for frame in frames {
            frame.map_err(|_| invalid())?;
            count += 1;
            if count > 10_000 {
                return Err(Error::Limit("image animation frame inspection"));
            }
        }
        Some(count)
    } else if format == image::ImageFormat::Tiff {
        None
    } else {
        Some(1)
    };
    let format_name = match format {
        image::ImageFormat::Jpeg => "jpeg",
        other => other.extensions_str()[0],
    };
    Ok(
        json!({"content":bytes,"byte_size":bytes.len(),"detected_media_type":detect_media_type(bytes),
        "format":format_name,"width":decoded.width(),"height":decoded.height(),"frame_count":frame_count}),
    )
}

fn pdf_document(bytes: &[u8]) -> Result<lopdf::Document> {
    lopdf::Document::load_mem(bytes).map_err(|_| Error::InvalidContent {
        resolver: "core.pdf.v1".into(),
        reason: "invalid PDF",
    })
}

fn pdf_metadata(doc: &lopdf::Document, key: &[u8]) -> Option<String> {
    let info = doc.trailer.get(b"Info").ok()?.as_reference().ok()?;
    let value = doc.get_dictionary(info).ok()?.get(key).ok()?;
    lopdf::decode_text_string(value).ok()
}

fn inspect_pdf(bytes: &[u8]) -> Result<Value> {
    let doc = pdf_document(bytes)?;
    let encrypted = doc.is_encrypted();
    Ok(
        json!({"content":bytes,"byte_size":bytes.len(),"detected_media_type":detect_media_type(bytes),
        "pdf_version":doc.version,"page_count":if encrypted { None } else { Some(doc.get_pages().len()) },
        "is_encrypted":encrypted,"title":if encrypted {None} else {pdf_metadata(&doc,b"Title")},
        "author":if encrypted {None} else {pdf_metadata(&doc,b"Author")}}),
    )
}

fn pdf_text(bytes: &[u8]) -> Result<Option<String>> {
    let doc = pdf_document(bytes)?;
    if doc.is_encrypted() {
        return Ok(None);
    }
    let mut parts = Vec::new();
    let mut remaining = 1_000_000;
    for page in doc.get_pages().keys().take(2_000) {
        let text = doc
            .extract_text(&[*page])
            .map_err(|_| Error::InvalidContent {
                resolver: "core.pdf.v1".into(),
                reason: "invalid PDF text layer",
            })?;
        if !text.trim().is_empty() {
            let selected: String = text.chars().take(remaining).collect();
            remaining -= selected.chars().count();
            parts.push(selected);
        }
        if remaining == 0 {
            break;
        }
    }
    let text = parts.join("\n\n").trim().to_owned();
    Ok((!text.is_empty()).then_some(text))
}

fn facts(kind: &str, value: &Value, fields: &[(&str, &str)]) -> String {
    let mut output = kind.to_owned();
    for (label, field) in fields {
        if let Some(value) = value.get(field).filter(|value| !value.is_null()) {
            let text = value.as_str().map(str::to_owned).unwrap_or_else(|| {
                if let Some(value) = value.as_bool() {
                    if value { "True".into() } else { "False".into() }
                } else {
                    value.to_string()
                }
            });
            output.push_str(&format!("\n{label}: {text}"));
        }
    }
    output
}

#[async_trait]
impl Resolver for MediaResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: self.id().into(),
            description: if self.pdf {
                "PDF structural and text-layer inspection"
            } else {
                "Image byte inspection"
            }
            .into(),
            read_methods: vec![],
            draft: None,
        }
    }
    fn validate_content(&self, content: &HydratedContent) -> Result<()> {
        if matches!(content, HydratedContent::Bytes(_)) {
            Ok(())
        } else {
            Err(self.invalid())
        }
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value> {
        let HydratedContent::Bytes(bytes) = context.actual_content(options.refresh).await? else {
            return Err(self.invalid());
        };
        let pdf = self.pdf;
        tokio::task::spawn_blocking(move || {
            if pdf {
                inspect_pdf(&bytes)
            } else {
                inspect_image(&bytes)
            }
        })
        .await
        .map_err(|_| Error::InvalidInput("media inspection task failed"))?
    }
    async fn text(
        &self,
        context: &ResolverContext,
        options: TextOptions,
    ) -> Result<Option<String>> {
        if !self.pdf && options.context == TextProjectionContext::Default {
            return Err(Error::Unsupported("core.image.v1 text projection".into()));
        }
        let solved = self.solve(context, options.into()).await?;
        if !self.pdf {
            return Ok(Some(facts(
                "image",
                &solved,
                &[
                    ("media type", "detected_media_type"),
                    ("format", "format"),
                    ("width", "width"),
                    ("height", "height"),
                    ("frames", "frame_count"),
                ],
            )));
        }
        let HydratedContent::Bytes(bytes) = context.actual_content(false).await? else {
            return Err(self.invalid());
        };
        let body = tokio::task::spawn_blocking(move || pdf_text(&bytes))
            .await
            .map_err(|_| Error::InvalidInput("PDF text task failed"))??;
        if options.context == TextProjectionContext::Default {
            return Ok(body);
        }
        let metadata = facts(
            "PDF",
            &solved,
            &[
                ("media type", "detected_media_type"),
                ("title", "title"),
                ("author", "author"),
                ("pages", "page_count"),
                ("PDF version", "pdf_version"),
                ("encrypted", "is_encrypted"),
            ],
        );
        Ok(Some(match body {
            Some(body) => format!("{metadata}\n\n{body}"),
            None => metadata,
        }))
    }
    async fn label(&self, context: &ResolverContext, refresh: bool) -> Result<String> {
        if !self.pdf {
            return Ok("image".into());
        }
        let solved = self
            .solve(
                context,
                ReadOptions {
                    refresh,
                    materialize_missing: false,
                },
            )
            .await?;
        Ok(format_label("PDF", solved["title"].as_str(), false))
    }
}
