use crate::{
    Block, BlockInput, BlockPatch, Core, Error, GraphPath, Neighborhood, Page, Relation,
    RelationInput, RelationPatch, Result,
};
use crate::{Direction, GraphModel, PathOptions, PathOutcome};
use sqlx::{PgConnection, Postgres, Transaction};
use std::collections::{BTreeSet, HashMap, VecDeque};

const BLOCK_COLUMNS: &str = "id,storage,resolver,content,created_at,updated_at";
const RELATION_COLUMNS: &str = "id,from_,to_,content,updated_at";

pub(crate) fn page_limit(limit: i64) -> Result<()> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::InvalidInput("limit must be 1..=1000".into()));
    }
    Ok(())
}

fn validate_block(input: &BlockInput) -> Result<()> {
    if input.resolver.trim().is_empty() {
        return Err(Error::InvalidInput("resolver must not be empty".into()));
    }
    Ok(())
}

fn validate_relation(input: &RelationInput) -> Result<()> {
    if input.from_ <= 0 || input.to_ <= 0 {
        return Err(Error::InvalidInput(
            "relation endpoints must be positive".into(),
        ));
    }
    Ok(())
}

async fn create_block(connection: &mut PgConnection, input: BlockInput) -> Result<Block> {
    validate_block(&input)?;
    Ok(sqlx::query_as(&format!("INSERT INTO inkcre.blocks(storage,resolver,content) VALUES($1,$2,$3) RETURNING {BLOCK_COLUMNS}"))
        .bind(input.storage).bind(input.resolver).bind(input.content).fetch_one(connection).await?)
}

async fn create_relation(connection: &mut PgConnection, input: RelationInput) -> Result<Relation> {
    validate_relation(&input)?;
    Ok(sqlx::query_as(&format!("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3) RETURNING {RELATION_COLUMNS}"))
        .bind(input.from_).bind(input.to_).bind(input.content).fetch_one(connection).await?)
}

/// Explicit graph transaction for compound operations. Drop rolls back.
pub struct GraphTransaction<'a> {
    transaction: Transaction<'a, Postgres>,
}

impl GraphTransaction<'_> {
    pub async fn block_create(&mut self, input: BlockInput) -> Result<Block> {
        create_block(&mut self.transaction, input).await
    }
    pub async fn relation_create(&mut self, input: RelationInput) -> Result<Relation> {
        create_relation(&mut self.transaction, input).await
    }
    pub async fn commit(self) -> Result<()> {
        Ok(self.transaction.commit().await?)
    }
    pub async fn rollback(self) -> Result<()> {
        Ok(self.transaction.rollback().await?)
    }
    /// Compose another protocol operation without opening a second transaction.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.transaction
    }
}

impl Core {
    pub async fn graph_transaction(&self) -> Result<GraphTransaction<'_>> {
        Ok(GraphTransaction {
            transaction: self.pool().begin().await?,
        })
    }
    pub async fn block_create(&self, input: BlockInput) -> Result<Block> {
        create_block(&mut *self.pool().acquire().await?, input).await
    }
    pub async fn block_get(&self, id: i32) -> Result<Option<Block>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {BLOCK_COLUMNS} FROM inkcre.blocks WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn blocks_recent(&self, limit: i64, cursor: Option<i32>) -> Result<Page<Block, i32>> {
        page_limit(limit)?;
        let mut items:Vec<Block> = sqlx::query_as(&format!("SELECT {BLOCK_COLUMNS} FROM inkcre.blocks WHERE ($1::integer IS NULL OR id<$1) ORDER BY id DESC LIMIT $2"))
            .bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    pub async fn block_patch(&self, id: i32, patch: BlockPatch) -> Result<Block> {
        if patch.resolver.as_ref().is_some_and(|x| x.trim().is_empty()) {
            return Err(Error::InvalidInput("resolver must not be empty".into()));
        }
        sqlx::query_as(&format!("UPDATE inkcre.blocks SET storage=CASE WHEN $2 THEN $3 ELSE storage END,resolver=COALESCE($4,resolver),content=COALESCE($5,content) WHERE id=$1 RETURNING {BLOCK_COLUMNS}"))
            .bind(id).bind(patch.storage.is_some()).bind(patch.storage.flatten()).bind(patch.resolver).bind(patch.content).fetch_optional(self.pool()).await?.ok_or(Error::NotFound("block"))
    }
    pub async fn block_delete(&self, id: i32) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.blocks WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
    pub async fn relation_create(&self, input: RelationInput) -> Result<Relation> {
        create_relation(&mut *self.pool().acquire().await?, input).await
    }
    pub async fn relation_get(&self, id: i32) -> Result<Option<Relation>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {RELATION_COLUMNS} FROM inkcre.relations WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn relation_patch(&self, id: i32, patch: RelationPatch) -> Result<Relation> {
        if patch.from_.is_some_and(|x| x <= 0) || patch.to_.is_some_and(|x| x <= 0) {
            return Err(Error::InvalidInput(
                "relation endpoints must be positive".into(),
            ));
        }
        sqlx::query_as(&format!("UPDATE inkcre.relations SET from_=COALESCE($2,from_),to_=COALESCE($3,to_),content=COALESCE($4,content) WHERE id=$1 RETURNING {RELATION_COLUMNS}"))
            .bind(id).bind(patch.from_).bind(patch.to_).bind(patch.content).fetch_optional(self.pool()).await?.ok_or(Error::NotFound("relation"))
    }
    pub async fn relation_delete(&self, id: i32) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
    pub async fn relations_for_block(
        &self,
        id: i32,
        limit: i64,
        cursor: Option<i32>,
    ) -> Result<Page<Relation, i32>> {
        page_limit(limit)?;
        let mut items:Vec<Relation>=sqlx::query_as(&format!("SELECT {RELATION_COLUMNS} FROM inkcre.relations WHERE (from_=$1 OR to_=$1) AND ($2::integer IS NULL OR id<$2) ORDER BY id DESC LIMIT $3"))
            .bind(id).bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    /// A single incident page preserves persisted direction and exact content.
    pub async fn block_neighborhood(
        &self,
        id: i32,
        direction: Direction,
        contents: &[String],
        limit: i64,
        cursor: Option<i32>,
    ) -> Result<Option<Neighborhood>> {
        page_limit(limit)?;
        if self.block_get(id).await?.is_none() {
            return Ok(None);
        }
        let mut relations:Vec<Relation>=sqlx::query_as("SELECT r.id,r.from_,r.to_,r.content,r.updated_at FROM inkcre.relations r JOIN inkcre.blocks f ON f.id=r.from_ JOIN inkcre.blocks t ON t.id=r.to_ WHERE (($2='out' AND r.from_=$1) OR ($2='in' AND r.to_=$1) OR ($2='both' AND (r.from_=$1 OR r.to_=$1))) AND (cardinality($3::text[])=0 OR r.content=ANY($3)) AND ($4::integer IS NULL OR r.id<$4) ORDER BY r.id DESC LIMIT $5")
            .bind(id).bind(direction.as_str()).bind(contents).bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let truncated = relations.len() > limit as usize;
        relations.truncate(limit as usize);
        let next_cursor = if truncated {
            relations.last().map(|r| r.id)
        } else {
            None
        };
        let graph = self.assemble_graph(&[id], &relations).await?;
        if !graph.blocks.iter().any(|b| b.id == id) {
            return Ok(None);
        }
        Ok(Some(Neighborhood {
            blocks: graph.blocks,
            relations: graph.relations,
            truncated,
            next_cursor,
        }))
    }

    pub async fn relation_neighborhood(&self, id: i32) -> Result<Option<GraphModel>> {
        let Some(relation) = self.relation_get(id).await? else {
            return Ok(None);
        };
        let graph = self.assemble_graph(&[], &[relation]).await?;
        if graph.relations.is_empty() {
            return Ok(None);
        }
        Ok(Some(graph))
    }

    pub async fn random_block(&self) -> Result<Option<Block>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {BLOCK_COLUMNS} FROM inkcre.blocks ORDER BY random() LIMIT 1"
        ))
        .fetch_optional(self.pool())
        .await?)
    }

    async fn assemble_graph(&self, initial: &[i32], relations: &[Relation]) -> Result<GraphModel> {
        let mut ids: BTreeSet<i32> = initial.iter().copied().collect();
        for r in relations {
            ids.extend(r.from_);
            ids.extend(r.to_);
        }
        let ids: Vec<_> = ids.into_iter().collect();
        let blocks: Vec<Block> = sqlx::query_as(&format!(
            "SELECT {BLOCK_COLUMNS} FROM inkcre.blocks WHERE id=ANY($1) ORDER BY id"
        ))
        .bind(ids)
        .fetch_all(self.pool())
        .await?;
        let existing: BTreeSet<_> = blocks.iter().map(|b| b.id).collect();
        let relations = relations
            .iter()
            .filter(|r| {
                r.from_.is_some_and(|id| existing.contains(&id))
                    && r.to_.is_some_and(|id| existing.contains(&id))
            })
            .cloned()
            .collect();
        Ok(GraphModel { blocks, relations })
    }

    /// Shortest path with explicit direction, exact filters, hop and Block budgets.
    pub async fn find_path(
        &self,
        source: i32,
        target: i32,
        options: PathOptions,
    ) -> Result<GraphPath> {
        if options.max_hops > 32
            || !(1..=10000).contains(&options.max_explored_blocks)
            || options.contents.len() > 100
        {
            return Err(Error::InvalidInput(
                "path requires at most 32 hops, 1..=10000 explored Blocks and at most 100 contents"
                    .into(),
            ));
        }
        if self.block_get(source).await?.is_none() || self.block_get(target).await?.is_none() {
            return Err(Error::NotFound("path endpoint"));
        }
        if source == target {
            return self.assemble_path(vec![source], vec![]).await;
        }
        let mut queue = VecDeque::from([(source, 0u8)]);
        let mut previous: HashMap<i32, (i32, Relation)> = HashMap::new();
        let mut visited = BTreeSet::from([source]);
        let mut limited = false;
        while let Some((current, depth)) = queue.pop_front() {
            let remaining = options.max_explored_blocks as usize - visited.len();
            let visited_ids: Vec<_> = visited.iter().copied().collect();
            // DISTINCT ON prevents parallel edges consuming the explored-Block budget.
            let edges:Vec<Relation>=sqlx::query_as("SELECT DISTINCT ON (other.id) r.id,r.from_,r.to_,r.content,r.updated_at FROM inkcre.relations r JOIN inkcre.blocks f ON f.id=r.from_ JOIN inkcre.blocks t ON t.id=r.to_ JOIN inkcre.blocks other ON other.id=CASE WHEN r.from_=$1 THEN r.to_ ELSE r.from_ END WHERE (($2='out' AND r.from_=$1) OR ($2='in' AND r.to_=$1) OR ($2='both' AND (r.from_=$1 OR r.to_=$1))) AND (cardinality($3::text[])=0 OR r.content=ANY($3)) AND NOT(other.id=ANY($4::integer[])) ORDER BY other.id,r.id LIMIT $5")
                .bind(current).bind(options.direction.as_str()).bind(&options.contents).bind(visited_ids).bind((remaining+1) as i64).fetch_all(self.pool()).await?;
            if depth >= options.max_hops {
                limited |= !edges.is_empty();
                continue;
            }
            for edge in edges {
                let other = if edge.from_ == Some(current) {
                    edge.to_
                } else {
                    edge.from_
                };
                let Some(other) = other else {
                    continue;
                };
                if visited.len() >= options.max_explored_blocks as usize {
                    limited = true;
                    break;
                }
                if !visited.insert(other) {
                    continue;
                }
                previous.insert(other, (current, edge));
                if other == target {
                    let mut block_ids = vec![target];
                    let mut relations = vec![];
                    let mut at = target;
                    while let Some((parent, edge)) = previous.get(&at) {
                        block_ids.push(*parent);
                        relations.push(edge.clone());
                        at = *parent;
                    }
                    block_ids.reverse();
                    relations.reverse();
                    return self.assemble_path(block_ids, relations).await;
                }
                queue.push_back((other, depth + 1));
            }
        }
        Ok(GraphPath {
            outcome: if limited {
                PathOutcome::LimitReached
            } else {
                PathOutcome::NotFound
            },
            graph: GraphModel::default(),
            block_ids: vec![],
            relation_ids: vec![],
        })
    }

    async fn assemble_path(
        &self,
        block_ids: Vec<i32>,
        expected: Vec<Relation>,
    ) -> Result<GraphPath> {
        let relation_ids: Vec<_> = expected.iter().map(|r| r.id).collect();
        let relations: Vec<Relation> = sqlx::query_as(&format!(
            "SELECT {RELATION_COLUMNS} FROM inkcre.relations WHERE id=ANY($1)"
        ))
        .bind(&relation_ids)
        .fetch_all(self.pool())
        .await?;
        // Re-read authority: concurrent edits fail visibly instead of fabricating a path.
        for old in &expected {
            if !relations.iter().any(|r| {
                r.id == old.id
                    && r.from_ == old.from_
                    && r.to_ == old.to_
                    && r.content == old.content
            }) {
                return Err(Error::Conflict(
                    "path authority changed during retrieval".into(),
                ));
            }
        }
        let graph = self.assemble_graph(&block_ids, &expected).await?;
        if graph.blocks.len() != block_ids.len() || graph.relations.len() != expected.len() {
            return Err(Error::Conflict(
                "path authority changed during retrieval".into(),
            ));
        }
        Ok(GraphPath {
            outcome: PathOutcome::Found,
            graph,
            block_ids,
            relation_ids,
        })
    }
}
