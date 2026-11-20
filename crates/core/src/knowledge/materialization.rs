//! Faithful media text sidecars. Network execution happens outside graph transactions.
use crate::{
    Core, Error, Result,
    ai::{Ai, ContentPart, Message},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const IMAGE_CONFIG_KEY: &str = "core.resolver.image";
pub const IMAGE_CONFIG_SCHEMA: &str = "core.resolver.image.config.v1";
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub text_model: i64,
}

pub const AUDIO_CONFIG_KEY: &str = "core.resolver.audio";
pub const AUDIO_CONFIG_SCHEMA: &str = "core.resolver.audio.config.v1";
pub const VIDEO_CONFIG_KEY: &str = "core.resolver.video";
pub const VIDEO_CONFIG_SCHEMA: &str = "core.resolver.video.config.v1";
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    pub transcript_model: i64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VideoConfig {
    pub transcript_model: i64,
    pub text_model: i64,
}

pub fn register_configs(
    registry: &mut crate::deployment_config::ConfigRegistry,
) -> crate::deployment_config::Result<()> {
    register_model_config::<ImageConfig>(
        registry,
        IMAGE_CONFIG_KEY,
        IMAGE_CONFIG_SCHEMA,
        "ImageResolverConfig",
        &["text_model"],
    )?;
    register_model_config::<AudioConfig>(
        registry,
        AUDIO_CONFIG_KEY,
        AUDIO_CONFIG_SCHEMA,
        "AudioResolverConfig",
        &["transcript_model"],
    )?;
    register_model_config::<VideoConfig>(
        registry,
        VIDEO_CONFIG_KEY,
        VIDEO_CONFIG_SCHEMA,
        "VideoResolverConfig",
        &["transcript_model", "text_model"],
    )
}
fn register_model_config<T: serde::de::DeserializeOwned + Serialize>(
    registry: &mut crate::deployment_config::ConfigRegistry,
    key: &str,
    schema: &str,
    title: &str,
    fields: &[&str],
) -> crate::deployment_config::Result<()> {
    use crate::deployment_config::{Error as ConfigError, SchemaDescriptor};
    let properties: serde_json::Map<String, Value> = fields
        .iter()
        .map(|field| {
            let title = field
                .split('_')
                .map(|word| {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().to_string() + chars.as_str())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ");
            ((*field).into(), json!({"title":title,"type":"integer"}))
        })
        .collect();
    registry.register(SchemaDescriptor {
        id:schema.into(), keys:vec![key.into()], description:"Model selection for faithful media text derivation.".into(),
        input_schema:Some(json!({"title":title,"type":"object","additionalProperties":false,"required":fields,"properties":properties})),
    }, |input| {
        let value:T = serde_json::from_value(input).map_err(|_|ConfigError::Invalid("invalid media Resolver model configuration"))?;
        serde_json::to_value(value).map_err(|_|ConfigError::Invalid("invalid media Resolver model configuration"))
    })
}
async fn read_config<T: serde::de::DeserializeOwned>(
    core: &Core,
    key: &str,
    schema_id: &str,
) -> Result<Option<T>> {
    let record: Option<(String, Value)> =
        sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key=$1")
            .bind(key)
            .fetch_optional(core.pool())
            .await?;
    let Some((schema, value)) = record else {
        return Ok(None);
    };
    if schema != schema_id {
        return Err(Error::Unsupported(
            "unknown media Resolver config schema".into(),
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| Error::InvalidInput("invalid media Resolver model configuration".into()))
}
pub async fn image_config(core: &Core) -> Result<Option<ImageConfig>> {
    read_config(core, IMAGE_CONFIG_KEY, IMAGE_CONFIG_SCHEMA).await
}
pub async fn audio_config(core: &Core) -> Result<Option<AudioConfig>> {
    read_config(core, AUDIO_CONFIG_KEY, AUDIO_CONFIG_SCHEMA).await
}
pub async fn video_config(core: &Core) -> Result<Option<VideoConfig>> {
    read_config(core, VIDEO_CONFIG_KEY, VIDEO_CONFIG_SCHEMA).await
}

pub async fn related_child(core: &Core, block: i32, role: &str) -> Result<Option<i32>> {
    Ok(sqlx::query_scalar("SELECT b.id FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.to_ WHERE r.from_=$1 AND r.content=$2 ORDER BY r.id LIMIT 1").bind(block).bind(role).fetch_optional(core.pool()).await?)
}

pub async fn materialize_text_child(
    core: &Core,
    block: i32,
    role: &str,
    text: &str,
) -> Result<i32> {
    let mut transaction = core.graph_transaction().await?;
    let parent: Option<i32> =
        sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(block)
            .fetch_optional(transaction.connection())
            .await?;
    if parent.is_none() {
        return Err(Error::NotFound("media Block"));
    }
    let existing:Option<i32>=sqlx::query_scalar("SELECT b.id FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.to_ WHERE r.from_=$1 AND r.content=$2 ORDER BY r.id LIMIT 1").bind(block).bind(role).fetch_optional(transaction.connection()).await?;
    if let Some(id) = existing {
        transaction.commit().await?;
        return Ok(id);
    }
    let child = transaction
        .block_create(crate::BlockInput {
            resolver: "core.text.v1".into(),
            storage: None,
            content: text.into(),
        })
        .await?;
    transaction
        .relation_create(crate::RelationInput {
            from_: block,
            to_: child.id,
            content: role.into(),
        })
        .await?;
    transaction.commit().await?;
    Ok(child.id)
}

pub async fn generate_faithful_text(
    core: &Core,
    model: i64,
    instruction: &str,
    media: ContentPart,
) -> std::result::Result<Option<String>, crate::ai::Error> {
    let response = Ai::new(core.clone())?
        .chat(
            model,
            &[
                Message::System {
                    content: instruction.into(),
                },
                Message::User {
                    content: vec![media],
                },
            ],
            &[],
            None,
        )
        .await?;
    Ok(response
        .content
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty()))
}

pub async fn try_materialize_model_text(
    core: &Core,
    block: i32,
    role: &str,
    model: i64,
    instruction: &str,
    media: ContentPart,
) {
    let effect = async {
        if related_child(core, block, role).await?.is_some() {
            return Ok(());
        }
        let text = generate_faithful_text(core, model, instruction, media)
            .await
            .map_err(|_| Error::Unavailable("faithful media text model"))?;
        if let Some(text) = text {
            materialize_text_child(core, block, role, &text).await?;
        }
        Ok::<_, Error>(())
    }
    .await;
    if effect.is_err() {
        tracing::warn!(
            block,
            role,
            model,
            "Resolver text materialization unavailable"
        );
    }
}
