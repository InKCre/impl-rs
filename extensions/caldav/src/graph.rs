//! DAV publications and graph-owned semantic values commit in one native transaction.
use crate::{Error, Result, codec, schema::*};
use chrono::{DateTime, Utc};
use inkcre_core::{Block, BlockInput, Core, GraphTransaction, Relation, RelationInput};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone)]
pub struct Resource {
    pub id: Option<i32>,
    pub resolver: String,
    pub content: ResourceContent,
    pub modified: DateTime<Utc>,
}
#[derive(Clone)]
pub struct Collection {
    pub id: Option<i32>,
    pub content: CollectionContent,
    pub items: BTreeMap<String, Resource>,
    pub modified: DateTime<Utc>,
}
#[derive(Clone)]
struct BoundProperty {
    relation: Relation,
    field: PropertyValue,
}
#[derive(Default)]
pub struct Graph {
    pub collections: BTreeMap<String, Collection>,
    resources: BTreeMap<i32, Resource>,
    original: BTreeMap<i32, (String, String)>,
    collection_content: BTreeMap<i32, CollectionContent>,
    members: BTreeMap<(i32, i32), i32>,
    properties: BTreeMap<i32, Vec<BoundProperty>>,
}
pub async fn transaction(core: &Core, write: bool) -> Result<GraphTransaction<'_>> {
    let mut tx = core.graph_transaction().await?;
    if !write {
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(tx.connection())
            .await?;
    }
    sqlx::query(if write {
        "SELECT pg_advisory_xact_lock(hashtextextended('extensions.caldav.v1',0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('extensions.caldav.v1',0))"
    })
    .execute(tx.connection())
    .await?;
    Ok(tx)
}
async fn semantic(
    block: &Block,
    tx: &mut GraphTransaction<'_>,
    lock: bool,
) -> Result<PropertyData> {
    if [CALENDAR, CONTACT, LEGACY_CALENDAR, LEGACY_CONTACT].contains(&block.resolver.as_str()) {
        let text = if [LEGACY_CALENDAR, LEGACY_CONTACT].contains(&block.resolver.as_str()) {
            serde_json::from_str::<ResourceContent>(&block.content)?.text
        } else {
            serde_json::from_str::<GraphContent>(&block.content)?.remainder
        };
        return Ok(PropertyData::Text(codec::uid(&text)?));
    }
    if let Some(storage) = block.storage {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT type FROM inkcre.storages WHERE id=$1")
                .bind(storage)
                .fetch_optional(tx.connection())
                .await?;
        if kind.as_deref() != Some("postgresql_binary") {
            return Err(Error::Integrity(
                "DAV embedded properties require database-owned bytes",
            ));
        }
        let pointer: serde_json::Value = serde_json::from_str(&block.content)?;
        let id = pointer["blob_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .ok_or(Error::Integrity("invalid DAV blob pointer"))?;
        let data = sqlx::query_scalar(if lock {
            "SELECT data FROM inkcre.storage_blobs WHERE id=$1 FOR UPDATE"
        } else {
            "SELECT data FROM inkcre.storage_blobs WHERE id=$1"
        })
        .bind(id)
        .fetch_optional(tx.connection())
        .await?
        .ok_or(Error::Integrity("DAV property bytes are missing"))?;
        Ok(PropertyData::Bytes(data))
    } else if block.resolver == "core.text.v1" {
        Ok(PropertyData::Text(block.content.clone()))
    } else {
        Err(Error::Integrity(
            "DAV inline property must use the text resolver",
        ))
    }
}
impl Graph {
    pub async fn load(tx: &mut GraphTransaction<'_>, lock: bool) -> Result<Self> {
        let mut graph = Self::default();
        let query = format!(
            "SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE resolver=ANY($1) ORDER BY id{}",
            if lock { " FOR UPDATE" } else { "" }
        );
        let roots: Vec<Block> = sqlx::query_as(&query)
            .bind(RESOLVERS)
            .fetch_all(tx.connection())
            .await?;
        let ids: Vec<_> = roots.iter().map(|b| b.id).collect();
        let query = format!(
            "SELECT id,from_,to_,content,updated_at FROM inkcre.relations WHERE from_=ANY($1) ORDER BY id{}",
            if lock { " FOR UPDATE" } else { "" }
        );
        let relations: Vec<Relation> = sqlx::query_as(&query)
            .bind(&ids)
            .fetch_all(tx.connection())
            .await?;
        let mut dependencies: BTreeMap<i32, Block> =
            roots.iter().map(|b| (b.id, b.clone())).collect();
        let target_ids: BTreeSet<_> = relations
            .iter()
            .filter(|r| r.content.starts_with(PROPERTY_PREFIX))
            .filter_map(|r| r.to_)
            .collect();
        for id in target_ids.iter().filter(|id| !ids.contains(id)) {
            let query = format!(
                "SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=$1{}",
                if lock { " FOR UPDATE" } else { "" }
            );
            let block: Block = sqlx::query_as(&query)
                .bind(id)
                .fetch_optional(tx.connection())
                .await?
                .ok_or(Error::Integrity("DAV property target is missing"))?;
            dependencies.insert(*id, block);
        }
        let mut values = BTreeMap::new();
        for id in target_ids {
            values.insert(
                id,
                semantic(
                    dependencies
                        .get(&id)
                        .ok_or(Error::Integrity("DAV property target is missing"))?,
                    tx,
                    lock,
                )
                .await?,
            );
        }
        for r in relations
            .iter()
            .filter(|r| r.content.starts_with(PROPERTY_PREFIX))
        {
            let (from, to) = (
                r.from_
                    .ok_or(Error::Integrity("DAV property source is missing"))?,
                r.to_
                    .ok_or(Error::Integrity("DAV property target is missing"))?,
            );
            graph
                .properties
                .entry(from)
                .or_default()
                .push(BoundProperty {
                    relation: r.clone(),
                    field: PropertyValue {
                        link: PropertyLink::parse(&r.content)?,
                        value: values[&to].clone(),
                    },
                });
        }
        let mut paths = BTreeMap::new();
        for block in roots {
            if block.storage.is_some() {
                return Err(Error::Integrity(
                    "DAV roots require persisted inline blocks",
                ));
            }
            graph
                .original
                .insert(block.id, (block.resolver.clone(), block.content.clone()));
            if [COLLECTION, LEGACY_COLLECTION].contains(&block.resolver.as_str()) {
                let content: CollectionContent = serde_json::from_str(&block.content)?;
                graph.collection_content.insert(block.id, content.clone());
                if !content.published {
                    continue;
                }
                if paths.insert(block.id, content.path.clone()).is_some()
                    || graph.collections.contains_key(&content.path)
                {
                    return Err(Error::Integrity("duplicate DAV collection path"));
                }
                graph.collections.insert(
                    content.path.clone(),
                    Collection {
                        id: Some(block.id),
                        content,
                        items: BTreeMap::new(),
                        modified: block
                            .updated_at
                            .or(block.created_at)
                            .unwrap_or(DateTime::UNIX_EPOCH),
                    },
                );
            } else {
                let content =
                    if [LEGACY_CALENDAR, LEGACY_CONTACT].contains(&block.resolver.as_str()) {
                        serde_json::from_str::<ResourceContent>(&block.content)?
                    } else {
                        let root: GraphContent = serde_json::from_str(&block.content)?;
                        ResourceContent {
                            href: root.href,
                            text: codec::compose(
                                &root.remainder,
                                &graph
                                    .properties
                                    .get(&block.id)
                                    .into_iter()
                                    .flatten()
                                    .map(|p| p.field.clone())
                                    .collect::<Vec<_>>(),
                            )?,
                        }
                    };
                graph.resources.insert(
                    block.id,
                    Resource {
                        id: Some(block.id),
                        resolver: if [CONTACT, LEGACY_CONTACT].contains(&block.resolver.as_str()) {
                            CONTACT
                        } else {
                            CALENDAR
                        }
                        .into(),
                        content,
                        modified: block
                            .updated_at
                            .or(block.created_at)
                            .unwrap_or(DateTime::UNIX_EPOCH),
                    },
                );
            }
        }
        let mut assigned = BTreeSet::new();
        for r in relations.iter().filter(|r| r.content == "member") {
            let Some(path) = r.from_.and_then(|id| paths.get(&id)) else {
                continue;
            };
            let target = r.to_.ok_or(Error::Integrity("DAV member is missing"))?;
            let resource = graph
                .resources
                .get(&target)
                .ok_or(Error::Integrity("DAV member is not a resource"))?;
            if !assigned.insert(target) {
                return Err(Error::Integrity(
                    "DAV published resource must have one collection owner",
                ));
            }
            let collection = graph
                .collections
                .get_mut(path)
                .expect("published collection");
            let expected = if collection
                .content
                .props
                .get("tag")
                .is_some_and(|v| v == "VADDRESSBOOK")
            {
                CONTACT
            } else {
                CALENDAR
            };
            if resource.resolver != expected {
                return Err(Error::Integrity(
                    "DAV resource type differs from collection",
                ));
            }
            if collection
                .items
                .insert(resource.content.href.clone(), resource.clone())
                .is_some()
            {
                return Err(Error::Integrity("duplicate DAV resource href"));
            }
            collection.modified = collection.modified.max(resource.modified);
            graph
                .members
                .insert((r.from_.expect("collection id"), target), r.id);
        }
        Ok(graph)
    }
    pub async fn save(&mut self, core: &Core, tx: &mut GraphTransaction<'_>) -> Result<()> {
        let mut active = BTreeSet::new();
        let mut memberships = BTreeSet::new();
        let mut resources = self.resources.clone();
        for collection in self.collections.values_mut() {
            let id = save_block(
                tx,
                collection.id,
                COLLECTION,
                serde_json::to_string(&collection.content)?,
                &mut self.original,
            )
            .await?;
            collection.id = Some(id);
            active.insert(id);
            for resource in collection.items.values_mut() {
                let (remainder, _) = codec::extract(&resource.content.text)?;
                let root = GraphContent {
                    href: resource.content.href.clone(),
                    remainder,
                };
                let child = save_block(
                    tx,
                    resource.id,
                    &resource.resolver,
                    serde_json::to_string(&root)?,
                    &mut self.original,
                )
                .await?;
                resource.id = Some(child);
                memberships.insert((id, child));
                resources.insert(child, resource.clone());
            }
        }
        let mut scopes: BTreeMap<i32, BTreeMap<String, Vec<i32>>> = BTreeMap::new();
        for collection in self.collections.values() {
            let mut uids: BTreeMap<String, Vec<i32>> = BTreeMap::new();
            for r in collection.items.values() {
                let uid = codec::uid(&r.content.text)?;
                if !uid.is_empty() {
                    uids.entry(uid)
                        .or_default()
                        .push(r.id.expect("saved resource"));
                }
            }
            for r in collection.items.values() {
                scopes.insert(r.id.expect("saved resource"), uids.clone());
            }
        }
        for (id, resource) in resources {
            let (remainder, properties) = codec::extract(&resource.content.text)?;
            save_block(
                tx,
                Some(id),
                &resource.resolver,
                serde_json::to_string(&GraphContent {
                    href: resource.content.href,
                    remainder,
                })?,
                &mut self.original,
            )
            .await?;
            self.save_properties(core, tx, id, properties, scopes.get(&id))
                .await?;
        }
        for (pair, id) in &self.members {
            if !memberships.contains(pair) {
                sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
                    .bind(id)
                    .execute(tx.connection())
                    .await?;
            }
        }
        for (from_, to_) in memberships {
            if !self.members.contains_key(&(from_, to_)) {
                tx.relation_create(RelationInput {
                    from_,
                    to_,
                    content: "member".into(),
                })
                .await?;
            }
        }
        for (id, content) in &self.collection_content {
            if !active.contains(id) && content.published {
                let mut content = content.clone();
                content.published = false;
                save_block(
                    tx,
                    Some(*id),
                    COLLECTION,
                    serde_json::to_string(&content)?,
                    &mut self.original,
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn save_properties(
        &self,
        core: &Core,
        tx: &mut GraphTransaction<'_>,
        id: i32,
        fields: Vec<PropertyValue>,
        uids: Option<&BTreeMap<String, Vec<i32>>>,
    ) -> Result<()> {
        let mut old = self.properties.get(&id).cloned().unwrap_or_default();
        for field in fields {
            let position = old.iter().position(|p| {
                p.field.link.component == field.link.component
                    && p.field.link.name == field.link.name
                    && p.field.value == field.value
                    && p.field.link.representation == field.link.representation
            });
            let previous = position.map(|index| old.remove(index));
            let mut target = previous.as_ref().and_then(|p| p.relation.to_);
            if field.link.representation == Representation::Reference
                && let PropertyData::Text(uid) = &field.value
                && let Some(targets) = uids.and_then(|uids| uids.get(uid))
                && targets.len() == 1
            {
                target = targets.first().copied()
            }
            let target = match target {
                Some(id) => id,
                None => create_value(core, tx, &field).await?,
            };
            let content = field.link.relation()?;
            if let Some(previous) = previous {
                if previous.relation.content != content || previous.relation.to_ != Some(target) {
                    sqlx::query("UPDATE inkcre.relations SET to_=$2,content=$3 WHERE id=$1")
                        .bind(previous.relation.id)
                        .bind(target)
                        .bind(content)
                        .execute(tx.connection())
                        .await?;
                }
            } else {
                tx.relation_create(RelationInput {
                    from_: id,
                    to_: target,
                    content,
                })
                .await?;
            }
        }
        for old in old {
            sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
                .bind(old.relation.id)
                .execute(tx.connection())
                .await?;
        }
        Ok(())
    }
}
async fn save_block(
    tx: &mut GraphTransaction<'_>,
    id: Option<i32>,
    resolver: &str,
    content: String,
    original: &mut BTreeMap<i32, (String, String)>,
) -> Result<i32> {
    let id = if let Some(id) = id {
        if original.get(&id) != Some(&(resolver.into(), content.clone())) {
            sqlx::query("UPDATE inkcre.blocks SET resolver=$2,content=$3 WHERE id=$1")
                .bind(id)
                .bind(resolver)
                .bind(&content)
                .execute(tx.connection())
                .await?;
        }
        id
    } else {
        tx.block_create(BlockInput {
            storage: None,
            resolver: resolver.into(),
            content: content.clone(),
        })
        .await?
        .id
    };
    original.insert(id, (resolver.into(), content));
    Ok(id)
}
async fn create_value(
    core: &Core,
    tx: &mut GraphTransaction<'_>,
    field: &PropertyValue,
) -> Result<i32> {
    let input = match &field.value {
        PropertyData::Text(value) => BlockInput {
            storage: None,
            resolver: "core.text.v1".into(),
            content: value.clone(),
        },
        PropertyData::Bytes(value) => {
            let kind: Option<String> =
                sqlx::query_scalar("SELECT type FROM inkcre.storages WHERE id=-4")
                    .fetch_optional(tx.connection())
                    .await?;
            if kind.as_deref() != Some("postgresql_binary") {
                return Err(Error::Integrity("database binary storage is unavailable"));
            }
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO inkcre.storage_blobs(data) VALUES($1) RETURNING id",
            )
            .bind(value)
            .fetch_one(tx.connection())
            .await?;
            let knowledge = core.knowledge();
            let resolver = knowledge
                .match_media_type(field.link.media_type.as_deref())
                .map_err(|_| Error::Integrity("DAV media Resolver selection failed"))?
                .or(knowledge
                    .match_media_type(inkcre_core::knowledge::detect_media_type(value))
                    .map_err(|_| Error::Integrity("DAV media Resolver selection failed"))?)
                .unwrap_or_else(|| "core.file.v1".into());
            BlockInput {
                storage: Some(-4),
                resolver,
                content: Core::postgresql_blob_pointer(id),
            }
        }
    };
    Ok(tx.block_create(input).await?.id)
}
pub async fn migrate(core: &Core) -> Result<()> {
    let mut tx = transaction(core, true).await?;
    let mut graph = Graph::load(&mut tx, true).await?;
    graph.save(core, &mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// A resource remains readable when its publication is withdrawn.
pub async fn solve_resource(core: &Core, id: i32) -> Result<ResourceContent> {
    let mut tx = transaction(core, false).await?;
    let block: Block = sqlx::query_as(
        "SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(tx.connection())
    .await?
    .ok_or(Error::Integrity("DAV resource is missing"))?;
    let root: GraphContent = serde_json::from_str(&block.content)?;
    let relations: Vec<Relation> = sqlx::query_as(
        "SELECT id,from_,to_,content,updated_at FROM inkcre.relations WHERE from_=$1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(tx.connection())
    .await?;
    let mut fields = Vec::new();
    for relation in relations
        .iter()
        .filter(|r| r.content.starts_with(PROPERTY_PREFIX))
    {
        let block:Block=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE id=$1").bind(relation.to_).fetch_optional(tx.connection()).await?.ok_or(Error::Integrity("DAV property target is missing"))?;
        fields.push(PropertyValue {
            link: PropertyLink::parse(&relation.content)?,
            value: semantic(&block, &mut tx, false).await?,
        });
    }
    let text = codec::compose(&root.remainder, &fields)?;
    tx.commit().await?;
    Ok(ResourceContent {
        href: root.href,
        text,
    })
}
