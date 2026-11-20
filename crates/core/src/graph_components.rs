//! Bounded undirected seed partition with an endpoint-closed spanning proof.
use crate::{Block, Core, Error, GraphModel, Relation, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentsRequest {
    pub seed_block_ids: Vec<i32>,
    pub contents: Vec<String>,
    #[serde(default = "block_budget")]
    pub max_explored_blocks: usize,
    #[serde(default = "relation_budget")]
    pub max_explored_relations: usize,
}
fn block_budget() -> usize {
    1000
}
fn relation_budget() -> usize {
    10000
}
#[derive(Serialize)]
pub struct ConnectedSeedComponent {
    pub seed_block_ids: Vec<i32>,
    pub member_block_ids: Vec<i32>,
}
#[derive(Serialize)]
pub struct ConnectedComponents {
    pub components: Vec<ConnectedSeedComponent>,
    pub proof_graph: GraphModel,
    pub missing_seed_block_ids: Vec<i32>,
    pub truncated: bool,
}
impl Core {
    pub async fn connected_components(
        &self,
        request: ComponentsRequest,
    ) -> Result<ConnectedComponents> {
        let mut seeds = request.seed_block_ids;
        let mut unique = BTreeSet::new();
        seeds.retain(|id| unique.insert(*id));
        if request.contents.is_empty()
            || request.max_explored_blocks == 0
            || request.max_explored_relations == 0
            || seeds.len() > request.max_explored_blocks
        {
            return Err(Error::InvalidInput("components require exact contents and positive exploration bounds accommodating seeds".into()));
        }
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let present: Vec<i32> = sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE id=ANY($1)")
            .bind(&seeds)
            .fetch_all(&mut *tx)
            .await?;
        let present = present.into_iter().collect::<BTreeSet<_>>();
        let missing_seed_block_ids = seeds
            .iter()
            .filter(|id| !present.contains(id))
            .copied()
            .collect();
        let mut explored = present.clone();
        let mut assigned = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut proof = BTreeMap::new();
        let mut examined = 0usize;
        let mut components = Vec::new();
        let mut truncated = false;
        for seed in &seeds {
            if !present.contains(seed) || assigned.contains(seed) {
                continue;
            }
            if truncated {
                components.push(ConnectedSeedComponent {
                    seed_block_ids: vec![*seed],
                    member_block_ids: vec![*seed],
                });
                assigned.insert(*seed);
                continue;
            }
            let mut members = BTreeSet::from([*seed]);
            let mut frontier = VecDeque::from([*seed]);
            while !frontier.is_empty() && !truncated {
                let current = frontier.pop_front().expect("nonempty component frontier");
                for outgoing in [true, false] {
                    let mut cursor: Option<i32> = None;
                    while !truncated {
                        let remaining = request.max_explored_relations - examined;
                        if remaining == 0 {
                            truncated = true;
                            break;
                        }
                        let requested = remaining.saturating_add(1).min(100);
                        let endpoint = if outgoing { "r.from_" } else { "r.to_" };
                        let mut page:Vec<Relation>=sqlx::query_as(&format!("SELECT r.id,r.from_,r.to_,r.content,r.updated_at FROM inkcre.relations r JOIN inkcre.blocks f ON f.id=r.from_ JOIN inkcre.blocks t ON t.id=r.to_ WHERE {endpoint}=$1 AND r.content=ANY($2) AND ($3::integer IS NULL OR r.id<$3) ORDER BY r.id DESC LIMIT $4"))
       .bind(current).bind(&request.contents).bind(cursor).bind(requested as i64).fetch_all(&mut *tx).await?;
                        if page.len() > remaining {
                            page.truncate(remaining);
                            truncated = true
                        }
                        examined += page.len();
                        let page_len = page.len();
                        let next_cursor = page.last().map(|r| r.id);
                        for relation in page {
                            if !seen.insert(relation.id) {
                                continue;
                            }
                            let neighbor = if relation.from_ == Some(current) {
                                relation.to_
                            } else {
                                relation.from_
                            };
                            let Some(neighbor) = neighbor else { continue };
                            if members.contains(&neighbor) {
                                continue;
                            }
                            if !explored.contains(&neighbor)
                                && explored.len() >= request.max_explored_blocks
                            {
                                truncated = true;
                                break;
                            }
                            members.insert(neighbor);
                            explored.insert(neighbor);
                            frontier.push_back(neighbor);
                            proof.insert(relation.id, relation);
                        }
                        if truncated || page_len < requested {
                            break;
                        }
                        cursor = next_cursor;
                    }
                }
            }
            let component_seeds = seeds
                .iter()
                .filter(|id| members.contains(id))
                .copied()
                .collect::<Vec<_>>();
            assigned.extend(component_seeds.iter().copied());
            components.push(ConnectedSeedComponent {
                seed_block_ids: component_seeds,
                member_block_ids: members.into_iter().collect(),
            });
        }
        let blocks:Vec<Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=ANY($1) ORDER BY id").bind(explored.into_iter().collect::<Vec<_>>()).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(ConnectedComponents {
            components,
            proof_graph: GraphModel {
                blocks,
                relations: proof.into_values().collect(),
            },
            missing_seed_block_ids,
            truncated,
        })
    }
}
