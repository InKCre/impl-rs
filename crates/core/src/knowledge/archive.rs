use super::{
    Error, ReadOptions, ReadValue, Resolver, ResolverContext, ResolverDescriptor, Result,
    TextOptions, TextProjectionContext, decode_unicode_bytes, format_label, media::facts,
};
use crate::HydratedContent;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read},
    sync::Arc,
};
use zip::{ZipArchive, unstable::read::ZipIterable};

const PACKAGE_LIMIT: usize = 2 * 1024 * 1024;

pub(super) fn resolvers() -> [Arc<dyn Resolver>; 2] {
    [Arc::new(Archive::Zip), Arc::new(Archive::Epub)]
}

#[derive(Clone, Copy)]
enum Archive {
    Zip,
    Epub,
}

impl Archive {
    fn id(self) -> &'static str {
        match self {
            Self::Zip => "core.zip.v1",
            Self::Epub => "core.epub.v1",
        }
    }

    fn invalid(self) -> Error {
        Error::InvalidContent {
            resolver: self.id().into(),
            reason: "invalid archive",
        }
    }

    async fn inspect(self, context: &ResolverContext, refresh: bool) -> Result<(Vec<u8>, Value)> {
        let HydratedContent::Bytes(bytes) = context.actual_content(refresh).await? else {
            return Err(self.invalid());
        };
        tokio::task::spawn_blocking(move || {
            let metadata = match self {
                Self::Zip => inspect_zip(&bytes),
                Self::Epub => inspect_epub(&bytes),
            }?;
            Ok((bytes, metadata))
        })
        .await
        .map_err(|_| Error::InvalidInput("archive inspection failed"))?
    }
}

fn inspect_zip(bytes: &[u8]) -> Result<Value> {
    let invalid = || Archive::Zip.invalid();
    let mut archive =
        ZipIterable::try_new(Cursor::new(bytes), Default::default()).map_err(|_| invalid())?;
    let mut compressed = 0u64;
    let mut uncompressed = 0u64;
    let mut encrypted = 0usize;
    let mut methods = BTreeSet::new();
    for entry in archive.files().map_err(|_| invalid())? {
        let file = entry.map_err(|_| invalid())?;
        compressed = compressed
            .checked_add(file.compressed_size())
            .ok_or_else(invalid)?;
        uncompressed = uncompressed.checked_add(file.size()).ok_or_else(invalid)?;
        encrypted += usize::from(file.encrypted());
        methods.insert(file.compression().into_compression().as_u16());
    }
    Ok(
        json!({"byte_size":bytes.len(),"detected_media_type":"application/zip",
        "member_count":archive.len(),"total_compressed_bytes":compressed,
        "total_uncompressed_bytes":uncompressed,"compression_methods":methods,
        "encrypted_member_count":encrypted}),
    )
}

fn read_member(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    let invalid = || Archive::Epub.invalid();
    let member = archive.by_name(name).map_err(|_| invalid())?;
    if member.size() > limit as u64 {
        return Err(Error::Limit("EPUB metadata exceeds the inspection limit"));
    }
    let mut data = Vec::new();
    member
        .take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|_| invalid())?;
    if data.len() > limit {
        return Err(Error::Limit("EPUB metadata exceeds the inspection limit"));
    }
    Ok(data)
}

fn inspect_epub(bytes: &[u8]) -> Result<Value> {
    let invalid = || Archive::Epub.invalid();
    let reader = Cursor::new(bytes);
    if ZipIterable::try_new(reader.clone(), Default::default())
        .map_err(|_| invalid())?
        .len()
        > 10_000
    {
        return Err(Error::Limit("EPUB contains more than 10000 members"));
    }
    let mut archive = ZipArchive::new(reader).map_err(|_| invalid())?;
    if read_member(&mut archive, "mimetype", 128)? != b"application/epub+zip" {
        return Err(invalid());
    }
    let container = decode_unicode_bytes(&read_member(
        &mut archive,
        "META-INF/container.xml",
        PACKAGE_LIMIT,
    )?)?;
    let container = roxmltree::Document::parse(&container).map_err(|_| invalid())?;
    let package_path = container
        .descendants()
        .filter(|node| node.has_tag_name("rootfile"))
        .filter_map(|node| node.attribute("full-path"))
        .find(|path| !path.is_empty())
        .ok_or_else(invalid)?;
    let package = decode_unicode_bytes(&read_member(&mut archive, package_path, PACKAGE_LIMIT)?)?;
    let package = roxmltree::Document::parse(&package).map_err(|_| invalid())?;
    let named = |local| {
        package
            .descendants()
            .filter(move |node| node.has_tag_name(local))
    };
    fn text<'doc>(node: roxmltree::Node<'doc, '_>) -> Option<&'doc str> {
        node.text().map(str::trim).filter(|value| !value.is_empty())
    }
    let modified = named("meta")
        .filter(|node| node.attribute("property") == Some("dcterms:modified"))
        .find_map(text)
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|_| invalid())?;
    Ok(
        json!({"byte_size":bytes.len(),"detected_media_type":"application/epub+zip",
        "epub_version":package.root_element().attribute("version"),"title":named("title").find_map(text),
        "creators":named("creator").filter_map(text).collect::<Vec<_>>(),
        "languages":named("language").filter_map(text).collect::<Vec<_>>(),"modified_at":modified,
        "manifest_count":named("item").count(),"spine_count":named("itemref").count(),
        "has_navigation":named("item").any(|item| item.attribute("properties")
            .is_some_and(|properties| properties.split_whitespace().any(|part| part == "nav")))}),
    )
}

#[async_trait]
impl Resolver for Archive {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: self.id().into(),
            description: match self {
                Self::Zip => "ZIP central-directory metadata",
                Self::Epub => "EPUB package metadata",
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
        Ok(self.solve_value(context, options).await?.into_json())
    }

    async fn solve_value(
        &self,
        context: &ResolverContext,
        options: ReadOptions,
    ) -> Result<ReadValue> {
        let (bytes, metadata) = self.inspect(context, options.refresh).await?;
        Ok(ReadValue::media_content(bytes, metadata))
    }

    async fn text(
        &self,
        context: &ResolverContext,
        options: TextOptions,
    ) -> Result<Option<String>> {
        if options.context == TextProjectionContext::Default {
            return Err(Error::Unsupported(format!("{} text projection", self.id())));
        }
        let (_, mut value) = self.inspect(context, options.refresh).await?;
        let (title, fields): (&str, &[(&str, &str)]) = match self {
            Self::Zip => (
                "ZIP",
                &[
                    ("media type", "detected_media_type"),
                    ("members", "member_count"),
                    ("compressed bytes", "total_compressed_bytes"),
                    ("uncompressed bytes", "total_uncompressed_bytes"),
                    ("encrypted members", "encrypted_member_count"),
                ],
            ),
            Self::Epub => {
                for key in ["creators", "languages"] {
                    let joined = value[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    value[key] = if joined.is_empty() {
                        Value::Null
                    } else {
                        Value::String(joined)
                    };
                }
                (
                    "EPUB",
                    &[
                        ("media type", "detected_media_type"),
                        ("title", "title"),
                        ("creators", "creators"),
                        ("languages", "languages"),
                        ("EPUB version", "epub_version"),
                        ("manifest items", "manifest_count"),
                        ("spine items", "spine_count"),
                    ],
                )
            }
        };
        Ok(Some(facts(title, &value, fields)))
    }

    async fn label(&self, context: &ResolverContext, refresh: bool) -> Result<String> {
        match self {
            Self::Zip => Ok("ZIP".into()),
            Self::Epub => {
                let (_, value) = self.inspect(context, refresh).await?;
                Ok(format_label("EPUB", value["title"].as_str(), false))
            }
        }
    }
}
