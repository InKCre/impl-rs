//! Exact Organization graph mutation and behavior metadata.
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

#[derive(Clone, Deserialize, Serialize)]
pub struct Behavior {
    pub id: String,
    pub description: String,
    pub label: String,
    pub judgment_contract: Vec<String>,
    pub config_key: String,
    pub config_schema: String,
    pub job_id: String,
    pub job_description: String,
    pub request: String,
    pub signals: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorAgentConfig {
    pub agent: i64,
}

pub fn behaviors() -> Vec<Behavior> {
    serde_json::from_str(include_str!("behaviors.json"))
        .expect("embedded canonical behavior metadata")
}
#[derive(Serialize)]
pub struct RelationWrite {
    pub relation_id: i32,
    pub created: bool,
}
#[derive(Serialize)]
pub struct CandidateWrite {
    pub descriptor_block_id: i32,
    pub relation_id: i32,
    pub created: bool,
}
async fn require_blocks(c: &mut PgConnection, ids: &[i32]) -> Result<()> {
    let found: Vec<i32> =
        sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE id=ANY($1) FOR KEY SHARE")
            .bind(ids)
            .fetch_all(c)
            .await?;
    if ids.iter().any(|id| !found.contains(id)) {
        return Err(Error::NotFound("organization block"));
    }
    Ok(())
}
async fn relation(c: &mut PgConnection, from: i32, to: i32, kind: &str) -> Result<RelationWrite> {
    let existing:Option<i32>=sqlx::query_scalar("SELECT id FROM inkcre.relations WHERE from_=$1 AND to_=$2 AND content=$3 ORDER BY id LIMIT 1").bind(from).bind(to).bind(kind).fetch_optional(&mut *c).await?;
    if let Some(id) = existing {
        return Ok(RelationWrite {
            relation_id: id,
            created: false,
        });
    }
    let id = sqlx::query_scalar(
        "INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(from)
    .bind(to)
    .bind(kind)
    .fetch_one(c)
    .await?;
    Ok(RelationWrite {
        relation_id: id,
        created: true,
    })
}
impl Core {
    pub async fn organization_candidate(
        &self,
        block: i32,
        behavior: &str,
    ) -> Result<CandidateWrite> {
        if !behaviors().iter().any(|b| b.id == behavior) {
            return Err(Error::InvalidInput("unknown organization behavior".into()));
        }
        let mut tx = self.pool().begin().await?;
        require_blocks(&mut tx, &[block]).await?;
        let descriptor:Option<i32>=sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE resolver=$1 AND content='' AND storage IS NULL ORDER BY id LIMIT 1").bind(behavior).fetch_optional(&mut *tx).await?;
        let id = match descriptor {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO inkcre.blocks(resolver,content) VALUES($1,'') RETURNING id",
                )
                .bind(behavior)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        if block == id {
            return Err(Error::InvalidInput(
                "behavior cannot be its own candidate".into(),
            ));
        }
        let result = relation(&mut tx, block, id, "candidate for").await?;
        tx.commit().await?;
        Ok(CandidateWrite {
            descriptor_block_id: id,
            relation_id: result.relation_id,
            created: result.created,
        })
    }
    pub async fn organization_relation(
        &self,
        mut from: i32,
        mut to: i32,
        kind: &str,
    ) -> Result<RelationWrite> {
        if ![
            "supersedes",
            "refines",
            "supports",
            "challenges",
            "duplicates assertion",
        ]
        .contains(&kind)
            || from == to
        {
            return Err(Error::InvalidInput("invalid organization relation".into()));
        }
        if kind == "duplicates assertion" && from > to {
            std::mem::swap(&mut from, &mut to)
        }
        let mut tx = self.pool().begin().await?;
        require_blocks(&mut tx, &[from, to]).await?;
        if matches!(kind, "supports" | "challenges") {
            let opposite = if kind == "supports" {
                "challenges"
            } else {
                "supports"
            };
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM inkcre.relations WHERE from_=$1 AND to_=$2 AND content=$3)",
            ).bind(from).bind(to).bind(opposite).fetch_one(&mut *tx).await?;
            if exists {
                return Err(Error::Conflict(
                    "evidence pair already has opposite stance".into(),
                ));
            }
        }
        if ["supersedes", "refines"].contains(&kind) {
            let cycle:bool=sqlx::query_scalar("WITH RECURSIVE reachable(id) AS (SELECT $1::integer UNION SELECT r.to_ FROM inkcre.relations r JOIN reachable p ON r.from_=p.id WHERE r.content=$3) SELECT EXISTS(SELECT 1 FROM reachable WHERE id=$2)").bind(to).bind(from).bind(kind).fetch_one(&mut *tx).await?;
            if cycle {
                return Err(Error::Conflict(
                    "organization relation would create a cycle".into(),
                ));
            }
        }
        let saved = relation(&mut tx, from, to, kind).await?;
        tx.commit().await?;
        Ok(saved)
    }
    pub async fn organization_synthesis(
        &self,
        text: &str,
        sources: &[i32],
        previous: Option<i32>,
    ) -> Result<Value> {
        let source_count = sources.len();
        let sources: std::collections::BTreeSet<i32> = sources.iter().copied().collect();
        if text.trim().is_empty() || sources.len() < 2 || sources.len() != source_count {
            return Err(Error::InvalidInput(
                "synthesis requires text and distinct multiple sources".into(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        let sources: Vec<_> = sources.into_iter().collect();
        require_blocks(&mut tx, &sources).await?;
        if let Some(id) = previous {
            require_blocks(&mut tx, &[id]).await?;
            let basis:i64=sqlx::query_scalar("SELECT count(DISTINCT from_) FROM inkcre.relations WHERE to_=$1 AND content='synthesis'").bind(id).fetch_one(&mut *tx).await?;
            if basis < 2 {
                return Err(Error::InvalidInput(
                    "previous synthesis lacks multi-source basis".into(),
                ));
            }
        }
        let existing:Option<i32>=sqlx::query_scalar("SELECT b.id FROM inkcre.blocks b WHERE b.resolver='core.text.v1' AND b.storage IS NULL AND b.content=$1 AND ARRAY(SELECT DISTINCT r.from_ FROM inkcre.relations r WHERE r.to_=b.id AND r.content='synthesis' ORDER BY r.from_)=$2::integer[] ORDER BY b.id LIMIT 1").bind(text).bind(&sources).fetch_optional(&mut *tx).await?;
        let id=match existing{Some(id)=>id,None=>sqlx::query_scalar("INSERT INTO inkcre.blocks(resolver,content) VALUES('core.text.v1',$1) RETURNING id").bind(text).fetch_one(&mut *tx).await?};
        let mut basis = Vec::new();
        for source in sources {
            basis.push(relation(&mut tx, source, id, "synthesis").await?)
        }
        let edited = match previous.filter(|p| *p != id) {
            Some(p) => Some(relation(&mut tx, p, id, "edited").await?),
            None => None,
        };
        tx.commit().await?;
        Ok(
            json!({"synthesis_block_id":id,"synthesis_created":existing.is_none(),"basis":basis,"edited":edited}),
        )
    }
}

impl Core {
    pub async fn organization_anchor(
        &self,
        source: i32,
        text: &str,
        referent: i32,
    ) -> Result<Value> {
        if text.trim().is_empty() || source == referent {
            return Err(Error::InvalidInput("invalid referent anchor".into()));
        }
        let mut tx = self.pool().begin().await?;
        require_blocks(&mut tx, &[source, referent]).await?;
        let source_fragment: bool = sqlx::query_scalar(
            "SELECT resolver='core.text.v1' AND content=$2 FROM inkcre.blocks WHERE id=$1",
        )
        .bind(source)
        .bind(text)
        .fetch_one(&mut *tx)
        .await?;
        let existing:Option<(i32,Option<i32>,i32)>=sqlx::query_as("SELECT b.id,m.id,r.id FROM inkcre.blocks b JOIN inkcre.relations r ON r.from_=b.id AND r.to_=$3 AND r.content='refers to' LEFT JOIN inkcre.relations m ON m.from_=$1 AND m.to_=b.id AND m.content='has mention' WHERE b.resolver='core.text.v1' AND b.content=$2 AND (b.id=$1 OR m.id IS NOT NULL) ORDER BY b.id LIMIT 1").bind(source).bind(text).bind(referent).fetch_optional(&mut *tx).await?;
        if let Some((fragment, mention, refers)) = existing {
            tx.commit().await?;
            return Ok(
                json!({"fragment_block_id":fragment,"fragment_created":false,"has_mention":mention.map(|id|RelationWrite{relation_id:id,created:false}),"refers_to":RelationWrite{relation_id:refers,created:false}}),
            );
        }
        let fragment = if source_fragment {
            source
        } else {
            sqlx::query_scalar("INSERT INTO inkcre.blocks(resolver,content) VALUES('core.text.v1',$1) RETURNING id").bind(text).fetch_one(&mut *tx).await?
        };
        let mention = if source_fragment {
            None
        } else {
            Some(relation(&mut tx, source, fragment, "has mention").await?)
        };
        let refers = relation(&mut tx, fragment, referent, "refers to").await?;
        tx.commit().await?;
        Ok(
            json!({"fragment_block_id":fragment,"fragment_created":!source_fragment,"has_mention":mention,"refers_to":refers}),
        )
    }
}

use crate::knowledge::{ReadOptions, Resolver, ResolverContext, ResolverDescriptor, TextOptions};
use async_trait::async_trait;
use std::sync::Arc;
pub fn resolvers() -> Vec<Arc<dyn Resolver>> {
    behaviors()
        .into_iter()
        .map(|behavior| Arc::new(BehaviorResolver(behavior)) as Arc<dyn Resolver>)
        .collect()
}
struct BehaviorResolver(Behavior);
#[async_trait]
impl Resolver for BehaviorResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: self.0.id.clone(),
            description: self.0.description.clone(),
            read_methods: if self.0.id == "core.organization.behavior.supersession.v1" {
                vec![crate::knowledge::ReadMethod {
                    name: "read_lineage".into(),
                    description: "Read bounded supersession history and current frontier".into(),
                    input_schema: json!({"type":"object","additionalProperties":false,"required":["focal_block_id"],"properties":{"focal_block_id":{"type":"integer"},"max_explored_blocks":{"type":"integer","minimum":1,"default":1000},"max_explored_relations":{"type":"integer","minimum":1,"default":10000}}}),
                }]
            } else {
                vec![]
            },
            draft: None,
        }
    }
    async fn solve(
        &self,
        context: &ResolverContext,
        options: ReadOptions,
    ) -> crate::knowledge::Result<Value> {
        match context.actual_content(options.refresh).await? {
            crate::HydratedContent::Text(text) => Ok(json!(text)),
            _ => Err(crate::knowledge::Error::InvalidInput(
                "behavior content must be text",
            )),
        }
    }
    async fn invoke(
        &self,
        context: &ResolverContext,
        method: &str,
        arguments: Value,
    ) -> crate::knowledge::Result<Value> {
        if self.0.id != "core.organization.behavior.supersession.v1" || method != "read_lineage" {
            return Err(crate::knowledge::Error::UnknownMethod(method.into()));
        }
        let focal = arguments["focal_block_id"]
            .as_i64()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or(crate::knowledge::Error::InvalidInput(
                "invalid focal block ID",
            ))?;
        context
            .knowledge
            .core()
            .organization_lineage(
                focal,
                arguments["max_explored_blocks"].as_u64().unwrap_or(1000) as usize,
                arguments["max_explored_relations"]
                    .as_u64()
                    .unwrap_or(10000) as usize,
            )
            .await
            .map_err(Into::into)
    }
    async fn text(
        &self,
        _: &ResolverContext,
        _: TextOptions,
    ) -> crate::knowledge::Result<Option<String>> {
        Ok(Some(self.0.description.clone()))
    }
    async fn label(&self, _: &ResolverContext, _: bool) -> crate::knowledge::Result<String> {
        Ok(self.0.label.clone())
    }
}

pub fn register_tools(runtime: &mut crate::agent::Runtime, core: &Core) -> Result<()> {
    for id in [
        "record_supersession",
        "record_refinement",
        "record_evidence_stance",
        "create_synthesis",
        "anchor_existing_referent",
        "record_duplicate_assertion",
        "record_organization_candidate",
    ] {
        runtime.register(Arc::new(OrganizationTool {
            core: core.clone(),
            id,
        }))?;
    }
    Ok(())
}
struct OrganizationTool {
    core: Core,
    id: &'static str,
}
#[async_trait]
impl crate::agent::Tool for OrganizationTool {
    fn definition(&self) -> crate::ai::FunctionTool {
        let fields: &[(&str, &str)] = match self.id {
            "record_supersession" => &[
                ("successor_block_id", "integer"),
                ("predecessor_block_id", "integer"),
            ],
            "record_refinement" => &[
                ("refinement_block_id", "integer"),
                ("predecessor_block_id", "integer"),
            ],
            "record_evidence_stance" => &[
                ("evidence_block_id", "integer"),
                ("assertion_block_id", "integer"),
                ("stance", "string"),
            ],
            "create_synthesis" => &[("text", "string"), ("source_block_ids", "array")],
            "anchor_existing_referent" => &[
                ("source_block_id", "integer"),
                ("selected_text", "string"),
                ("referent_block_id", "integer"),
            ],
            "record_duplicate_assertion" => {
                &[("left_block_id", "integer"), ("right_block_id", "integer")]
            }
            _ => &[("block_id", "integer"), ("behavior", "string")],
        };
        let mut properties = serde_json::Map::new();
        for (name, kind) in fields {
            properties.insert((*name).into(),if *kind=="array"{json!({"type":"array","items":{"type":"integer"},"minItems":2,"uniqueItems":true})}else{json!({"type":kind})});
        }
        if self.id == "record_evidence_stance" {
            properties.insert("stance".into(), json!({"enum":["supports","challenges"]}));
        }
        if self.id == "create_synthesis" {
            properties.insert(
                "previous_synthesis_block_id".into(),
                json!({"type":["integer","null"]}),
            );
        }
        let schema = json!({"type":"object","additionalProperties":false,"properties":properties,"required":fields.iter().map(|f|f.0).collect::<Vec<_>>()});
        crate::ai::FunctionTool {
            id: self.id.into(),
            description: serde_json::from_str::<std::collections::BTreeMap<String, String>>(
                include_str!("tool-descriptions.json"),
            )
            .expect("embedded Organization tool descriptions")
            .remove(self.id)
            .expect("known Organization tool ID"),
            input_schema: schema.as_object().unwrap().clone(),
        }
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, crate::agent::ToolFailure> {
        self.execute(input).await.map_err(|error| match error {
            Error::Database(_) => crate::agent::ToolFailure::Failed,
            _ => crate::agent::ToolFailure::Actionable(
                json!({"error":"organization_error","message":error.to_string()}),
            ),
        })
    }
}
impl OrganizationTool {
    async fn execute(&self, v: Value) -> Result<Value> {
        let id = |name: &str| {
            v[name]
                .as_i64()
                .and_then(|id| i32::try_from(id).ok())
                .ok_or_else(|| Error::InvalidInput("invalid block ID".into()))
        };
        let text = |name: &str| {
            v[name]
                .as_str()
                .ok_or_else(|| Error::InvalidInput("invalid text argument".into()))
        };
        match self.id {
            "record_organization_candidate" => Ok(json!(
                self.core
                    .organization_candidate(id("block_id")?, text("behavior")?)
                    .await?
            )),
            "create_synthesis" => {
                let sources: Vec<i32> = serde_json::from_value(v["source_block_ids"].clone())
                    .map_err(|_| Error::InvalidInput("invalid synthesis sources".into()))?;
                self.core
                    .organization_synthesis(
                        text("text")?,
                        &sources,
                        v["previous_synthesis_block_id"]
                            .as_i64()
                            .and_then(|v| i32::try_from(v).ok()),
                    )
                    .await
            }
            "anchor_existing_referent" => {
                self.core
                    .organization_anchor(
                        id("source_block_id")?,
                        text("selected_text")?,
                        id("referent_block_id")?,
                    )
                    .await
            }
            _ => {
                let (from, to, kind) = match self.id {
                    "record_supersession" => (
                        id("successor_block_id")?,
                        id("predecessor_block_id")?,
                        "supersedes",
                    ),
                    "record_refinement" => (
                        id("refinement_block_id")?,
                        id("predecessor_block_id")?,
                        "refines",
                    ),
                    "record_evidence_stance" => (
                        id("evidence_block_id")?,
                        id("assertion_block_id")?,
                        text("stance")?,
                    ),
                    _ => (
                        id("left_block_id")?,
                        id("right_block_id")?,
                        "duplicates assertion",
                    ),
                };
                Ok(json!(
                    self.core.organization_relation(from, to, kind).await?
                ))
            }
        }
    }
}

impl Core {
    pub async fn organization_ruminate(
        &self,
        block: i32,
        runtime: &crate::agent::Runtime,
    ) -> Result<()> {
        let row = self
            .block_get(block)
            .await?
            .ok_or(Error::NotFound("organization block"))?;
        let text = match self.knowledge().text(block, TextOptions::default()).await {
            Ok(Some(text)) if !text.trim().is_empty() => text,
            Ok(_)
            | Err(
                crate::knowledge::Error::UnknownResolver(_)
                | crate::knowledge::Error::Unsupported(_),
            ) => return Ok(()),
            Err(_) => return Err(Error::Unavailable("organization focal text")),
        };
        let config: Option<(String, Value)> = sqlx::query_as(
            "SELECT schema,value FROM inkcre.configs WHERE key='core.organization.rumination'",
        )
        .fetch_optional(self.pool())
        .await?;
        let Some((schema, value)) = config else {
            return Err(Error::Unavailable("organization agent configuration"));
        };
        if schema != "core.organization.rumination.config.v1" {
            return Err(Error::InvalidInput(
                "invalid rumination config schema".into(),
            ));
        }
        let config: BehaviorAgentConfig = serde_json::from_value(value)
            .map_err(|_| Error::InvalidInput("invalid rumination Agent configuration".into()))?;
        let agent = config.agent;
        let mut relations = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.relations_for_block(block, 1000, cursor).await?;
            relations.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        relations.sort_by_key(|relation| relation.id);
        let mut neighbors = Vec::new();
        for edge in relations {
            let other = if edge.from_ == Some(block) {
                edge.to_
            } else {
                edge.from_
            };
            let label = match other {
                Some(id) => self.knowledge().label(id, false).await.ok(),
                None => None,
            };
            let other_resolver = match other {
                Some(id) => self.block_get(id).await?.map(|block| block.resolver),
                None => None,
            };
            neighbors.push(json!({"id":edge.id,"direction":if edge.from_==Some(block){"outgoing"}else{"incoming"},"property":edge.content,"other_block":{"id":other,"resolver":other_resolver,"label":label}}));
        }
        let drafting = self
            .knowledge()
            .registry()
            .resolvers()
            .map_err(|_| Error::Unavailable("Resolver registry"))?
            .into_iter()
            .filter_map(|resolver| {
                resolver
                    .draft
                    .map(|draft| json!({"resolver":resolver.id,"description":draft.description}))
            })
            .collect::<Vec<_>>();
        let context = json!({"request":"ruminate","focal_block":{"id":block,"resolver":row.resolver,"text":text},"direct_relations":neighbors,"available_draft_resolvers":drafting});
        let message = crate::ai::Message::User {
            content: vec![crate::ai::ContentPart::Text {
                text: context.to_string(),
            }],
        };
        let thread = runtime.create_thread(agent).await?;
        if matches!(
            thread.turn(message).await?,
            crate::agent::Termination::MaxModelCalls
        ) {
            return Err(Error::Conflict(
                "organization Agent model-call budget exhausted".into(),
            ));
        }
        Ok(())
    }
}

impl Core {
    pub async fn organization_lineage(
        &self,
        focal: i32,
        max_blocks: usize,
        max_relations: usize,
    ) -> Result<Value> {
        use std::collections::{BTreeMap, BTreeSet, VecDeque};
        if max_blocks == 0 || max_relations == 0 {
            return Err(Error::InvalidInput(
                "lineage exploration bounds must be positive".into(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        require_blocks(&mut tx, &[focal]).await?;
        let mut visited = BTreeSet::from([focal]);
        let mut frontier = VecDeque::from([focal]);
        let mut edges = BTreeMap::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        while let Some(current) = frontier.pop_front() {
            for outgoing in [true, false] {
                let remaining = max_relations.saturating_sub(scanned);
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let rows:Vec<crate::Relation>=sqlx::query_as("SELECT id,from_,to_,content,updated_at FROM inkcre.relations WHERE content='supersedes' AND (($2 AND from_=$1) OR (NOT $2 AND to_=$1)) ORDER BY id DESC LIMIT $3").bind(current).bind(outgoing).bind(i64::try_from(remaining.saturating_add(1)).unwrap_or(i64::MAX)).fetch_all(&mut *tx).await?;
                if rows.len() > remaining {
                    truncated = true
                }
                for edge in rows.into_iter().take(remaining) {
                    scanned += 1;
                    let neighbor = if edge.from_ == Some(current) {
                        edge.to_
                    } else {
                        edge.from_
                    };
                    edges.insert(edge.id, edge);
                    if let Some(neighbor) = neighbor.filter(|n| !visited.contains(n)) {
                        if visited.len() >= max_blocks {
                            truncated = true;
                            break;
                        }
                        visited.insert(neighbor);
                        frontier.push_back(neighbor);
                    }
                }
                if truncated {
                    break;
                }
            }
            if truncated {
                break;
            }
        }
        let blocks:Vec<crate::Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=ANY($1) ORDER BY id").bind(visited.iter().copied().collect::<Vec<_>>()).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let incoming: BTreeSet<_> = edges.values().filter_map(|edge| edge.to_).collect();
        let mut indegree: BTreeMap<i32, usize> = visited.iter().map(|id| (*id, 0)).collect();
        for edge in edges.values() {
            if let Some(to) = edge.to_.and_then(|to| indegree.get_mut(&to)) {
                *to += 1
            }
        }
        let mut queue: VecDeque<_> = indegree
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(*id))
            .collect();
        let mut removed = 0;
        while let Some(id) = queue.pop_front() {
            removed += 1;
            for edge in edges.values().filter(|edge| edge.from_ == Some(id)) {
                if let Some(to) = edge.to_
                    && let Some(count) = indegree.get_mut(&to)
                {
                    *count -= 1;
                    if *count == 0 {
                        queue.push_back(to)
                    }
                }
            }
        }
        let cycle = removed != visited.len();
        let current = if truncated || cycle {
            vec![]
        } else {
            visited.difference(&incoming).copied().collect::<Vec<_>>()
        };
        let existing: BTreeSet<_> = blocks.iter().map(|block| block.id).collect();
        let relations = edges
            .into_values()
            .filter(|edge| {
                edge.from_.is_some_and(|id| existing.contains(&id))
                    && edge.to_.is_some_and(|id| existing.contains(&id))
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"graph":{"blocks":blocks,"relations":relations},"current_block_ids":current,"truncated":truncated,"cycle_detected":cycle}),
        )
    }
}

pub const RUMINATION_CAPABILITY: &str = "core.organization.rumination.v1";
mod execution;
pub use execution::{job_handlers, register_configs};

impl Core {
    pub async fn organization_ruminate_via(
        &self,
        self_id: uuid::Uuid,
        block: i32,
        target: Option<uuid::Uuid>,
        runtime: &crate::agent::Runtime,
        options: &crate::peer::PeerHttpOptions,
    ) -> std::result::Result<(), crate::peer::DelegationError> {
        let Some(target) = target.filter(|target| *target != self_id) else {
            return self
                .organization_ruminate(block, runtime)
                .await
                .map_err(Into::into);
        };
        let response = self
            .peer_delegate(
                self_id,
                RUMINATION_CAPABILITY,
                json!({"body":{"block":block}}),
                Some(target),
                options,
            )
            .await?;
        if response["status"] != 204
            || response
                .as_object()
                .is_none_or(|value| value.contains_key("body"))
        {
            return Err(crate::peer::DelegationError::Protocol);
        }
        Ok(())
    }
}
