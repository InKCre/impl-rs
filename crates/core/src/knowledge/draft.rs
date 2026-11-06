use super::{Error, Knowledge, Result};
use crate::BlockInput;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize)]
pub struct DraftBlock {
    pub id: i32,
    #[serde(flatten)]
    pub block: BlockInput,
}

impl<'de> Deserialize<'de> for DraftBlock {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Serde does not support deny_unknown_fields together with flatten.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            id: i32,
            #[serde(default)]
            storage: Option<i32>,
            resolver: String,
            content: String,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            id: input.id,
            block: BlockInput {
                storage: input.storage,
                resolver: input.resolver,
                content: input.content,
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftRelation {
    pub from_: i32,
    pub to_: i32,
    pub content: String,
}

/// A rooted authoring command, distinct from persisted GraphModel reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphDraft {
    pub root: i32,
    #[serde(default)]
    pub blocks: Vec<DraftBlock>,
    #[serde(default)]
    pub relations: Vec<DraftRelation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersistedDraft {
    pub root: i32,
    pub blocks: BTreeMap<i32, i32>,
    pub relations: Vec<i32>,
}

impl GraphDraft {
    pub fn validate(&self) -> Result<()> {
        if self.blocks.len() > 1000 || self.relations.len() > 5000 || self.root == 0 {
            return Err(Error::InvalidInput(
                "draft requires a nonzero root, at most 1000 Blocks and 5000 Relations",
            ));
        }
        let mut ids = BTreeSet::new();
        for block in &self.blocks {
            if block.id >= 0 || !ids.insert(block.id) || block.block.resolver.trim().is_empty() {
                return Err(Error::InvalidInput(
                    "draft Block identities must be unique negative integers with exact Resolvers",
                ));
            }
        }
        for endpoint in
            std::iter::once(self.root).chain(self.relations.iter().flat_map(|r| [r.from_, r.to_]))
        {
            if endpoint == 0 || (endpoint < 0 && !ids.contains(&endpoint)) {
                return Err(Error::InvalidInput(
                    "draft has an unresolved local Block identity",
                ));
            }
        }
        Ok(())
    }
}

impl Knowledge {
    /// The exact Resolver owns input grammar; drafting itself has no persistence effect.
    pub async fn draft(&self, resolver: &str, input: serde_json::Value) -> Result<GraphDraft> {
        let decoder = self.registry().resolver(resolver)?;
        let contract = decoder
            .descriptor()
            .draft
            .ok_or_else(|| Error::Unsupported(format!("Resolver {resolver} graph drafting")))?;
        crate::models::validate_json(&contract.input_schema, &input)?;
        let draft = decoder.draft(self.core(), input).await?;
        draft.validate()?;
        Ok(draft)
    }

    pub async fn submit_draft(&self, draft: GraphDraft) -> Result<PersistedDraft> {
        let mut transaction = self.core().pool().begin().await?;
        let result = self.persist_draft(&mut transaction, &draft).await?;
        transaction.commit().await?;
        Ok(result)
    }

    /// Compose graph and byte writes in the caller's transaction. On an error the
    /// caller must roll back its unit of work, including any already-created blobs.
    pub async fn persist_draft(
        &self,
        connection: &mut PgConnection,
        draft: &GraphDraft,
    ) -> Result<PersistedDraft> {
        draft.validate()?;
        // Validate every new representation before writing graph rows.
        for block in &draft.blocks {
            self.validate_block_input_in(connection, &block.block)
                .await?;
        }
        let positive: BTreeSet<_> = std::iter::once(draft.root)
            .chain(draft.relations.iter().flat_map(|r| [r.from_, r.to_]))
            .filter(|id| *id > 0)
            .collect();
        if !positive.is_empty() {
            let existing: Vec<i32> =
                sqlx::query_scalar("SELECT id FROM inkcre.blocks WHERE id=ANY($1) FOR KEY SHARE")
                    .bind(positive.iter().copied().collect::<Vec<_>>())
                    .fetch_all(&mut *connection)
                    .await?;
            if existing.len() != positive.len() {
                return Err(crate::Error::NotFound("draft endpoint").into());
            }
        }
        let mut blocks = BTreeMap::new();
        for draft_block in &draft.blocks {
            let block = &draft_block.block;
            let id: i32 = sqlx::query_scalar(
                "INSERT INTO inkcre.blocks(storage,resolver,content) VALUES($1,$2,$3) RETURNING id",
            )
            .bind(block.storage)
            .bind(&block.resolver)
            .bind(&block.content)
            .fetch_one(&mut *connection)
            .await?;
            blocks.insert(draft_block.id, id);
        }
        let resolve = |id| if id < 0 { blocks[&id] } else { id };
        let mut relations = Vec::new();
        for relation in &draft.relations {
            relations.push(
                sqlx::query_scalar(
                    "INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3) RETURNING id",
                )
                .bind(resolve(relation.from_))
                .bind(resolve(relation.to_))
                .bind(&relation.content)
                .fetch_one(&mut *connection)
                .await?,
            );
        }
        Ok(PersistedDraft {
            root: resolve(draft.root),
            blocks,
            relations,
        })
    }
}
