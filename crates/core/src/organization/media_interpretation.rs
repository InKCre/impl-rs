//! Organization-owned additive media interpretation through configured Agents.
use crate::{
    Block, Core, Error, Result,
    agent::{Runtime, Termination},
    ai::{ContentPart, Media, Message},
    knowledge::{ReadOptions, ReadValue},
    worker::{JobContext, JobHandler},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const CONFIG_KEY: &str = "core.organization.media_interpretation";
pub const CONFIG_SCHEMA: &str = "core.organization.media_interpretation.config.v1";
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub image_agent: i64,
    pub audio_agent: i64,
    pub video_agent: i64,
}
impl Config {
    fn agent(&self, modality: &str) -> i64 {
        match modality {
            "image" => self.image_agent,
            "audio" => self.audio_agent,
            _ => self.video_agent,
        }
    }
}
#[derive(Default, Serialize)]
pub struct Report {
    pub selected: usize,
    pub interpreted: usize,
    pub unavailable: usize,
    pub failed: usize,
    pub no_output: usize,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Serialize)]
pub struct Diagnostic {
    pub block: i32,
    pub modality: String,
    pub outcome: &'static str,
    pub reason: &'static str,
}
impl Report {
    fn record(&mut self, block: i32, modality: &str, outcome: &'static str, reason: &'static str) {
        match outcome {
            "unavailable" => self.unavailable += 1,
            "failed" => self.failed += 1,
            _ => self.no_output += 1,
        }
        if self.diagnostics.len() < 20 {
            self.diagnostics.push(Diagnostic {
                block,
                modality: modality.into(),
                outcome,
                reason,
            });
        }
    }
}
pub fn register_configs(
    registry: &mut crate::deployment_config::ConfigRegistry,
) -> crate::deployment_config::Result<()> {
    use crate::deployment_config::{Error as ConfigError, SchemaDescriptor};
    registry.register(SchemaDescriptor {id:CONFIG_SCHEMA.into(),keys:vec![CONFIG_KEY.into()],description:"Deployment selection of independent modality-oriented Agents.".into(),input_schema:Some(json!({"title":"MediaInterpretationConfig","type":"object","additionalProperties":false,"required":["image_agent","audio_agent","video_agent"],"properties":{"image_agent":{"type":"integer"},"audio_agent":{"type":"integer"},"video_agent":{"type":"integer"}}}))},|value|{
        let config:Config=serde_json::from_value(value).map_err(|_|ConfigError::Invalid("invalid media interpretation Agent configuration"))?;Ok(json!(config))
    })
}
async fn configuration(core: &Core) -> Result<Option<Config>> {
    let record: Option<(String, Value)> =
        sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key=$1")
            .bind(CONFIG_KEY)
            .fetch_optional(core.pool())
            .await?;
    let Some((schema, value)) = record else {
        return Ok(None);
    };
    if schema != CONFIG_SCHEMA {
        return Err(Error::Unsupported(
            "unknown media interpretation configuration schema".into(),
        ));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|_| Error::InvalidInput("invalid media interpretation Agent configuration".into()))
}
fn modality(resolver: &str) -> Option<&'static str> {
    match resolver {
        "core.image.v1" => Some("image"),
        "core.audio.v1" => Some("audio"),
        "core.video.v1" => Some("video"),
        _ => None,
    }
}
pub fn job_handler(runtime: Runtime) -> Arc<dyn JobHandler> {
    Arc::new(MediaJob { runtime })
}
struct MediaJob {
    runtime: Runtime,
}
#[async_trait::async_trait]
impl JobHandler for MediaJob {
    fn job_type(&self) -> crate::JobType {
        crate::JobType {
            id: "core.organization.media_interpretation.v1".into(),
            description:
                "Interpret missing image, audio, and video Blocks through configured Agents.".into(),
            parameters_schema: json!({"title":"MediaInterpretationJobParameters","type":"object","additionalProperties":false,"properties":{}}),
            default_timeout_seconds: 1800,
        }
    }
    async fn can_handle(&self, core: &Core, job: &crate::Job) -> Result<bool> {
        crate::models::validate_json(&self.job_type().parameters_schema, &job.parameters)?;
        let Some(config) = configuration(core).await? else {
            return Ok(false);
        };
        for modality in ["image", "audio", "video"] {
            if modality != "image" && !crate::knowledge::MediaTools::is_configured(core) {
                continue;
            }
            if self
                .runtime
                .can_execute(config.agent(modality), modality)
                .await?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    async fn handle(&self, context: JobContext, job: crate::Job) -> Result<()> {
        crate::models::validate_json(&self.job_type().parameters_schema, &job.parameters)?;
        let report = context
            .core()
            .organization_interpret_missing_media(&self.runtime)
            .await?;
        context.set_state(json!(report)).await
    }
}
impl Core {
    pub async fn organization_interpret_missing_media(&self, runtime: &Runtime) -> Result<Report> {
        let Some(config) = configuration(self).await? else {
            return Ok(Report::default());
        };
        let candidates:Vec<Block>=sqlx::query_as("SELECT b.id,b.storage,b.resolver,b.content,b.created_at,b.updated_at FROM inkcre.blocks b WHERE b.resolver=ANY($1) AND NOT EXISTS(SELECT 1 FROM inkcre.relations r WHERE r.from_=b.id AND r.content='interpretation') ORDER BY b.id LIMIT 20").bind(["core.image.v1","core.audio.v1","core.video.v1"]).fetch_all(self.pool()).await?;
        let mut report = Report::default();
        for block in candidates {
            report.selected += 1;
            let modality = modality(&block.resolver).expect("selected media Resolver");
            let agent = config.agent(modality);
            if (modality != "image" && !crate::knowledge::MediaTools::is_configured(self))
                || !runtime.can_execute(agent, modality).await?
            {
                report.record(
                    block.id,
                    modality,
                    "unavailable",
                    "agent_not_locally_executable",
                );
                continue;
            }
            let attempted = async {
                let Some(message) = media_message(self, &block, modality).await? else {
                    return Ok(false);
                };
                let thread = runtime.create_thread(agent).await?;
                if matches!(thread.turn(message).await?, Termination::MaxModelCalls) {
                    return Err(Error::Conflict(
                        "media interpretation model-call budget exhausted".into(),
                    ));
                }
                Ok(true)
            }
            .await;
            match attempted {
                Ok(false) => {
                    report.record(block.id, modality, "unavailable", "media_input_unavailable");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(block = block.id, modality, "Media interpretation failed");
                    report.record(block.id, modality, "failed", "RuntimeError");
                    continue;
                }
                Ok(true) => {}
            }
            if crate::knowledge::materialization::related_child(self, block.id, "interpretation")
                .await?
                .is_some()
            {
                report.interpreted += 1
            } else {
                report.record(
                    block.id,
                    modality,
                    "no_output",
                    "missing_interpretation_relation",
                )
            }
        }
        Ok(report)
    }
}
async fn media_message(core: &Core, block: &Block, modality: &str) -> Result<Option<Message>> {
    let reader = core
        .knowledge()
        .reader(block.id)
        .await
        .map_err(Error::from)?;
    let solved = reader
        .get_solved_value(ReadOptions {
            materialize_missing: false,
            ..Default::default()
        })
        .await
        .map_err(Error::from)?;
    let ReadValue::Object(mut fields) = solved else {
        return Ok(None);
    };
    let Some(ReadValue::Bytes(bytes)) = fields.remove("content") else {
        return Ok(None);
    };
    let metadata = ReadValue::Object(fields).into_json();
    let mime = metadata["detected_media_type"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            metadata[if modality == "image" {
                "format"
            } else {
                "container"
            }]
            .as_str()
            .map(|format| format!("{modality}/{format}"))
        });
    let Some(mime) = mime else { return Ok(None) };
    let media = Media {
        data: bytes,
        mime_type: mime,
        transfer_url: reader.transfer_url().await.map_err(Error::from)?,
    };
    let media = match modality {
        "image" => ContentPart::Image(media),
        "audio" => ContentPart::Audio(media),
        _ => ContentPart::Video(media),
    };
    let facts = match modality {
        "image" => {
            json!({"format":metadata["format"],"width":metadata["width"],"height":metadata["height"]})
        }
        "audio" => {
            json!({"container":metadata["container"],"codec":metadata["codec"],"duration_ms":metadata["duration_ms"]})
        }
        _ => {
            json!({"container":metadata["container"],"codec":metadata["video_codec"],"duration_ms":metadata["duration_ms"],"width":metadata["width"],"height":metadata["height"]})
        }
    };
    let mut context = Vec::new();
    for relation in core.relations_for_block(block.id, 20, None).await?.items {
        let other = if relation.from_ == Some(block.id) {
            relation.to_
        } else {
            relation.from_
        };
        let label = match other {
            Some(id) => core.knowledge().label(id, false).await.ok(),
            None => None,
        };
        context.push(json!({"direction":if relation.from_==Some(block.id){"outgoing"}else{"incoming"},"property":relation.content,"other_block":{"id":other,"label":label}}));
    }
    let input = json!({"request":"interpret media and submit only a useful additive graph","focal_block":{"id":block.id,"resolver":block.resolver,"facts":facts},"direct_relations":context});
    Ok(Some(Message::User {
        content: vec![
            ContentPart::Text {
                text: input.to_string(),
            },
            media,
        ],
    }))
}
