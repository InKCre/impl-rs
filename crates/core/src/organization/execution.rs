use super::{Behavior, BehaviorAgentConfig, behaviors};
use crate::{
    Core, Error, Job, JobType, Result,
    agent::{Runtime, Termination},
    ai::{ContentPart, Message},
    deployment_config::{ConfigRegistry, SchemaDescriptor},
    knowledge::{Error as KnowledgeError, TextOptions},
    worker::{JobContext, JobHandler},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub fn register_configs(registry: &mut ConfigRegistry) -> crate::deployment_config::Result<()> {
    for behavior in behaviors() {
        let rumination = behavior.request == "ruminate";
        registry.register(SchemaDescriptor {
            id:behavior.config_schema,
            keys:vec![behavior.config_key],
            description: if rumination {"Deployment selection of one reusable Agent definition."} else {"Deployment selection of one purpose-built Agent definition."}.into(),
            input_schema:Some(json!({"title":if rumination{"RuminationConfig"}else{"BehaviorAgentConfig"},"type":"object","additionalProperties":false,"required":["agent"],"properties":{"agent":{"title":"Agent","type":"integer"}}})),
        },|value| {
            let config:BehaviorAgentConfig=serde_json::from_value(value).map_err(|_|crate::deployment_config::Error::Invalid("invalid Organization Agent configuration"))?;
            Ok(json!(config))
        })?;
    }
    Ok(())
}

pub fn job_handlers(runtime: Runtime) -> Vec<Arc<dyn JobHandler>> {
    let mut handlers: Vec<Arc<dyn JobHandler>> = behaviors()
        .into_iter()
        .map(|behavior| {
            Arc::new(OrganizationJob {
                runtime: runtime.clone(),
                behavior,
                explicit: false,
            }) as Arc<dyn JobHandler>
        })
        .collect();
    handlers.push(Arc::new(OrganizationJob {
        runtime,
        behavior: behaviors()
            .into_iter()
            .find(|b| b.request == "ruminate")
            .expect("canonical rumination behavior"),
        explicit: true,
    }));
    handlers
}
struct OrganizationJob {
    runtime: Runtime,
    behavior: Behavior,
    explicit: bool,
}
#[async_trait]
impl JobHandler for OrganizationJob {
    fn job_type(&self) -> JobType {
        JobType {
            id: if self.explicit {
                "core.organization.rumination.explicit.v1".into()
            } else {
                self.behavior.job_id.clone()
            },
            description: if self.explicit {
                "Reconsider one explicitly selected Block through rumination.".into()
            } else {
                self.behavior.job_description.clone()
            },
            parameters_schema: if self.explicit {
                json!({"title":"RuminationRequest","description":"Explicit request to reconsider one focal Block.","type":"object","additionalProperties":false,"properties":{"block":{"title":"Block","type":"integer"}},"required":["block"]})
            } else {
                serde_json::from_str(include_str!("automatic-job-schema.json"))
                    .expect("canonical automatic Organization schema")
            },
            default_timeout_seconds: 1800,
        }
    }
    async fn can_handle(&self, core: &Core, job: &Job) -> Result<bool> {
        crate::models::validate_json(&self.job_type().parameters_schema, &job.parameters)?;
        let Some(agent) = configured_agent(core, &self.behavior).await? else {
            return Ok(false);
        };
        self.runtime.can_execute(agent, "text").await
    }
    async fn handle(&self, context: JobContext, job: Job) -> Result<()> {
        crate::models::validate_json(&self.job_type().parameters_schema, &job.parameters)?;
        if self.explicit {
            let block = job.parameters["block"]
                .as_i64()
                .and_then(|id| i32::try_from(id).ok())
                .ok_or_else(|| Error::InvalidInput("invalid rumination Block ID".into()))?;
            return context
                .core()
                .organization_ruminate(block, &self.runtime)
                .await;
        }
        let max = job.parameters["max_seeds"].as_u64().unwrap_or(10) as usize;
        context
            .core()
            .organization_automatic(&self.behavior.id, max, &self.runtime)
            .await
    }
}
async fn configured_agent(core: &Core, behavior: &Behavior) -> Result<Option<i64>> {
    let record: Option<(String, Value)> =
        sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key=$1")
            .bind(&behavior.config_key)
            .fetch_optional(core.pool())
            .await?;
    let Some((schema, value)) = record else {
        return Ok(None);
    };
    if schema != behavior.config_schema {
        return Err(Error::InvalidInput(
            "Organization configuration schema mismatch".into(),
        ));
    }
    let config: BehaviorAgentConfig = serde_json::from_value(value)
        .map_err(|_| Error::InvalidInput("invalid Organization Agent configuration".into()))?;
    Ok(Some(config.agent))
}

/// Reserve a position for every nonempty category, then fill each category in
/// priority order. Deduplication retains the first appearance within each category.
pub fn merge_seed_categories(max: usize, categories: &[Vec<i32>]) -> Vec<i32> {
    let normalized: Vec<Vec<i32>> = categories
        .iter()
        .map(|category| {
            let mut seen = BTreeSet::new();
            category
                .iter()
                .copied()
                .filter(|id| seen.insert(*id))
                .collect()
        })
        .collect();
    let mut result = Vec::new();
    for id in normalized.iter().filter_map(|category| category.first()) {
        if !result.contains(id) {
            result.push(*id)
        }
        if result.len() == max {
            return result;
        }
    }
    for category in normalized {
        for id in category.into_iter().skip(1) {
            if !result.contains(&id) {
                result.push(id)
            }
            if result.len() == max {
                return result;
            }
        }
    }
    result
}
impl Core {
    pub async fn organization_seed_ids(&self, behavior_id: &str, max: usize) -> Result<Vec<i32>> {
        if !(3..=100).contains(&max) {
            return Err(Error::InvalidInput(
                "Organization max_seeds must be 3..=100".into(),
            ));
        }
        let behavior = behaviors()
            .into_iter()
            .find(|b| b.id == behavior_id)
            .ok_or_else(|| Error::InvalidInput("unknown Organization behavior".into()))?;
        let excluded: Vec<String> = behaviors().into_iter().map(|b| b.id).collect();
        let mut tx = self.pool().begin().await?;
        let descriptor:Option<i32>=sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE resolver=$1 AND content='' AND storage IS NULL ORDER BY id LIMIT 1").bind(behavior_id).fetch_optional(&mut *tx).await?;
        let descriptor = match descriptor {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO inkcre.blocks(resolver,content) VALUES($1,'') RETURNING id",
                )
                .bind(behavior_id)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        let candidates:Vec<i32>=sqlx::query_scalar("SELECT from_ FROM inkcre.relations WHERE to_=$1 AND content='candidate for' ORDER BY random() LIMIT $2").bind(descriptor).bind(max as i64).fetch_all(&mut *tx).await?;
        let recent:Vec<i32>=sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE NOT(resolver=ANY($1)) ORDER BY updated_at DESC,id DESC LIMIT $2").bind(&excluded).bind(max as i64).fetch_all(&mut *tx).await?;
        let random: Vec<i32> = sqlx::query_scalar(
            "SELECT id FROM inkcre.blocks WHERE NOT(resolver=ANY($1)) ORDER BY random() LIMIT $2",
        )
        .bind(&excluded)
        .bind(max as i64)
        .fetch_all(&mut *tx)
        .await?;
        let strong = if behavior.signals == "none" {
            recent
        } else {
            let rows:Vec<(i32,i32)>=sqlx::query_as("SELECT from_,to_ FROM inkcre.relations WHERE ($1::text IS NULL OR content=$1) ORDER BY updated_at DESC,id DESC LIMIT $2").bind((behavior.signals=="edited").then_some("edited")).bind(max as i64).fetch_all(&mut *tx).await?;
            let mut seen = BTreeSet::new();
            let mut signals: Vec<i32> = rows
                .into_iter()
                .flat_map(|(from, to)| [from, to])
                .filter(|id| seen.insert(*id))
                .collect();
            if behavior.signals == "synthesis" && !signals.is_empty() {
                let affected: Vec<i32> = sqlx::query_scalar(
                    "SELECT to_ FROM inkcre.relations WHERE from_=ANY($1) AND content='synthesis'",
                )
                .bind(&signals)
                .fetch_all(&mut *tx)
                .await?;
                signals.extend(affected.into_iter().filter(|id| seen.insert(*id)));
            }
            merge_seed_categories(max, &[signals, recent])
        };
        tx.commit().await?;
        Ok(merge_seed_categories(max, &[candidates, strong, random]))
    }
    pub async fn organization_automatic(
        &self,
        behavior_id: &str,
        max: usize,
        runtime: &Runtime,
    ) -> Result<()> {
        let behavior = behaviors()
            .into_iter()
            .find(|b| b.id == behavior_id)
            .ok_or_else(|| Error::InvalidInput("unknown Organization behavior".into()))?;
        let seeds = self.organization_seed_ids(behavior_id, max).await?;
        tracing::info!(behavior=behavior_id,max_seeds=max,seed_block_ids=?seeds,"organization.seeds.selected");
        for seed in seeds {
            let result = if behavior.request == "ruminate" {
                self.organization_ruminate(seed, runtime).await
            } else {
                self.organization_judge_seed(&behavior, seed, runtime).await
            };
            match result {
                Ok(()) => {}
                Err(Error::NotFound("organization block")) => tracing::warn!(
                    behavior = behavior_id,
                    seed_block_id = seed,
                    reason = "seed_missing",
                    "organization.seed.considered"
                ),
                Err(Error::Conflict(message))
                    if message == "organization Agent model-call budget exhausted" =>
                {
                    tracing::warn!(
                        behavior = behavior_id,
                        seed_block_id = seed,
                        reason = "model_call_limit",
                        "organization.seed.considered"
                    )
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    async fn organization_judge_seed(
        &self,
        behavior: &Behavior,
        seed: i32,
        runtime: &Runtime,
    ) -> Result<()> {
        let block = self
            .block_get(seed)
            .await?
            .ok_or(Error::NotFound("organization block"))?;
        let knowledge = self.knowledge();
        let text = match knowledge
            .text(
                seed,
                TextOptions {
                    materialize_missing: false,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(Some(text)) if !text.trim().is_empty() => text,
            Ok(_) | Err(KnowledgeError::UnknownResolver(_) | KnowledgeError::Unsupported(_)) => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let label = match knowledge.label(seed, false).await {
            Ok(label) => label,
            Err(KnowledgeError::UnknownResolver(_) | KnowledgeError::Unsupported(_)) => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let mut edges = self.relations_for_block(seed, 20, None).await?.items;
        edges.sort_by_key(|edge| edge.id);
        let mut context = Vec::new();
        for edge in edges {
            let neighbor = if edge.from_ == Some(seed) {
                edge.to_
            } else {
                edge.from_
            };
            let other = match neighbor {
                Some(id) => self.block_get(id).await?,
                None => None,
            };
            let other_label = match neighbor {
                Some(id) => match knowledge.label(id, false).await {
                    Ok(label) => Some(label),
                    Err(KnowledgeError::UnknownResolver(_) | KnowledgeError::Unsupported(_)) => {
                        None
                    }
                    Err(error) => return Err(error.into()),
                },
                None => None,
            };
            context.push(json!({"id":edge.id,"direction":if edge.from_==Some(seed){"outgoing"}else{"incoming"},"content":edge.content,"other_block":{"id":neighbor,"resolver":other.map(|block|block.resolver),"label":other_label}}));
        }
        let agent = configured_agent(self, behavior)
            .await?
            .ok_or(Error::Unavailable("organization agent configuration"))?;
        let prompt = json!({"request":behavior.request,"judgment_contract":behavior.judgment_contract,"seed_block":{"id":seed,"resolver":block.resolver,"label":label,"text":text},"direct_relations":context,"exploration":"The seed and its direct relations are only a starting point. Use the declared retrieval, Resolver, and graph tools when more evidence is needed. Persist only through the exact behavior tool, or cautiously mark a different behavior candidate."});
        let thread = runtime.create_thread(agent).await?;
        if matches!(
            thread
                .turn(Message::User {
                    content: vec![ContentPart::Text {
                        text: prompt.to_string()
                    }]
                })
                .await?,
            Termination::MaxModelCalls
        ) {
            return Err(Error::Conflict(
                "organization Agent model-call budget exhausted".into(),
            ));
        }
        Ok(())
    }
}
