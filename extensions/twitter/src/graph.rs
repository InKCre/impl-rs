//! The shared native graph transaction, used identically by providers and archive HTTP.
use crate::{Result, db, invalid, merge, schema::*};
use inkcre_core::{Block, Core};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
pub type Identity = (String, String);
#[derive(Default, Serialize)]
pub struct WriteResult {
    pub changed: BTreeSet<Identity>,
    pub block_ids: BTreeMap<String, BTreeMap<String, i32>>,
}
fn identity(kind: &str, id: &str) -> Identity {
    (kind.into(), id.into())
}
fn id(value: &Value) -> Result<&str> {
    value["id"]
        .as_str()
        .ok_or_else(|| invalid("Twitter native ID missing"))
}
fn references(post: &Value) -> Vec<(String, Identity, Option<usize>)> {
    let mut result = Vec::new();
    if let Some(author) = post["authorId"].as_str() {
        result.push(("author".into(), identity("users", author), None))
    }
    let mut media = Vec::new();
    if let Some(ids) = post["mediaIds"].as_array() {
        media.extend(ids.iter().filter_map(Value::as_str));
    }
    if let Some(id) = post["article"]["coverMediaId"].as_str() {
        media.push(id)
    }
    if let Some(ids) = post["article"]["mediaIds"].as_array() {
        media.extend(ids.iter().filter_map(Value::as_str));
    }
    let mut seen = BTreeSet::new();
    for id in media {
        if seen.insert(id) {
            let index = seen.len() - 1;
            result.push(("media".into(), identity("media", id), Some(index)));
        }
    }
    for (field, kind) in [("replyTo", "reply"), ("quote", "quote")] {
        if let Some(id) = post[field]["postId"].as_str() {
            result.push((kind.into(), identity("posts", id), None));
        }
    }
    if let Some(id) = post["repostOf"].as_str() {
        result.push(("repost".into(), identity("posts", id), None));
    }
    result
}
fn reference(kind: &str, index: Option<usize>) -> String {
    let mut value = json!({"schema":REFERENCE,"type":kind});
    if let Some(index) = index {
        value["position"] = json!(index)
    }
    value.to_string()
}
fn managed(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if value["schema"] != REFERENCE {
        return false;
    }
    if value["type"] == "media" {
        object.len() == 3 && value["position"].as_u64().is_some()
    } else {
        object.len() == 2
            && matches!(
                value["type"].as_str(),
                Some("author" | "reply" | "quote" | "repost")
            )
    }
}
async fn roots(
    connection: &mut PgConnection,
    identities: &BTreeSet<Identity>,
    legacy: bool,
) -> Result<BTreeMap<Identity, Block>> {
    let mut result = BTreeMap::new();
    for kind in KINDS {
        let ids: Vec<&str> = identities
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, id)| id.as_str())
            .collect();
        if ids.is_empty() {
            continue;
        }
        let mut resolvers = vec![resolver(kind)];
        if legacy && kind == "posts" {
            resolvers.push(LEGACY)
        }
        let blocks:Vec<Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE resolver=ANY($1) AND storage IS NULL AND CASE WHEN pg_input_is_valid(content,'jsonb') THEN content::jsonb->>'id' END=ANY($2) ORDER BY (resolver='extensions.twitter.tweet.v1'),id")
   .bind(resolvers).bind(ids).fetch_all(&mut *connection).await.map_err(db)?;
        for block in blocks {
            let value: Value = serde_json::from_str(&block.content)
                .map_err(|_| invalid("Invalid stored Twitter JSON"))?;
            let native_id = value["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| value["id"].as_i64().map(|n| n.to_string()))
                .ok_or_else(|| invalid("Stored Twitter ID missing"))?;
            result.entry(identity(kind, &native_id)).or_insert(block);
        }
    }
    Ok(result)
}
fn decode_roots(blocks: Vec<Block>) -> Result<BTreeMap<Identity, Block>> {
    let mut roots = BTreeMap::new();
    for block in blocks {
        let kind = match block.resolver.as_str() {
            USER => "users",
            POST => "posts",
            MEDIA => "media",
            _ => return Err(invalid("Unexpected Twitter root Resolver")),
        };
        let value: Value = serde_json::from_str(&block.content)
            .map_err(|_| invalid("Invalid persisted Twitter JSON"))?;
        roots.insert(identity(kind, id(&value)?), block);
    }
    Ok(roots)
}
async fn create_many(
    connection: &mut PgConnection,
    records: &[(Identity, Value)],
) -> Result<BTreeMap<Identity, Block>> {
    if records.is_empty() {
        return Ok(BTreeMap::new());
    }
    let resolvers: Vec<_> = records
        .iter()
        .map(|(key, _)| resolver(&key.0).to_owned())
        .collect();
    let contents: Vec<_> = records.iter().map(|(_, value)| value.to_string()).collect();
    let blocks=sqlx::query_as("INSERT INTO inkcre.blocks(resolver,content) SELECT * FROM unnest($1::text[],$2::text[]) RETURNING id,storage,resolver,content,created_at,updated_at").bind(resolvers).bind(contents).fetch_all(connection).await.map_err(db)?;
    decode_roots(blocks)
}
async fn update_many(
    connection: &mut PgConnection,
    records: &[(i32, Identity, Value)],
) -> Result<BTreeMap<Identity, Block>> {
    if records.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ids: Vec<_> = records.iter().map(|(id, _, _)| *id).collect();
    let resolvers: Vec<_> = records
        .iter()
        .map(|(_, key, _)| resolver(&key.0).to_owned())
        .collect();
    let contents: Vec<_> = records
        .iter()
        .map(|(_, _, value)| value.to_string())
        .collect();
    let blocks:Vec<Block>=sqlx::query_as("UPDATE inkcre.blocks b SET resolver=v.resolver,content=v.content,storage=NULL FROM unnest($1::integer[],$2::text[],$3::text[]) AS v(id,resolver,content) WHERE b.id=v.id RETURNING b.id,b.storage,b.resolver,b.content,b.created_at,b.updated_at").bind(ids).bind(resolvers).bind(contents).fetch_all(connection).await.map_err(db)?;
    if blocks.len() != records.len() {
        return Err(invalid("Twitter root disappeared during capture"));
    }
    decode_roots(blocks)
}
pub async fn persist(
    connection: &mut PgConnection,
    batch: &Batch,
    supplement: bool,
) -> Result<WriteResult> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('extensions.twitter.reconcile',0))")
        .execute(&mut *connection)
        .await
        .map_err(db)?;
    let mut incoming = BTreeMap::<Identity, Value>::new();
    for kind in KINDS {
        for value in batch.items(kind) {
            let key = identity(kind, id(value)?);
            let merged = merge::record(incoming.get(&key).unwrap_or(&json!({})), value);
            incoming.insert(key, merged);
        }
    }
    let mut identities: BTreeSet<Identity> = incoming.keys().cloned().collect();
    for ((kind, _), post) in &incoming {
        if kind == "posts" {
            identities.extend(references(post).into_iter().map(|(_, target, _)| target));
        }
    }
    let mut blocks = roots(connection, &identities, true).await?;
    let mut records = BTreeMap::new();
    let mut result = WriteResult::default();
    let mut new_records = Vec::new();
    let mut updated_records = Vec::new();
    for key in &identities {
        let root = blocks.get(key);
        let existing = if let Some(root) = root.filter(|b| b.resolver != LEGACY) {
            serde_json::from_str(&root.content)
                .map_err(|_| invalid("Stored Twitter JSON invalid"))?
        } else {
            json!({"id":key.1})
        };
        let mut value = incoming
            .get(key)
            .cloned()
            .unwrap_or_else(|| json!({"id":key.1}));
        if supplement
            && existing.get("text").is_some()
            && let Some(value) = value.as_object_mut()
        {
            value.remove("text");
        }
        let merged = merge::record(&existing, &value);
        if incoming.contains_key(key) {
            validate(model(&key.0), &merged)?;
        }
        if root.is_none() {
            new_records.push((key.clone(), merged.clone()));
            result.changed.insert(key.clone());
        } else if incoming.contains_key(key)
            && (merged != existing || root.is_some_and(|b| b.resolver != resolver(&key.0)))
        {
            updated_records.push((
                root.ok_or_else(|| invalid("Twitter root disappeared"))?.id,
                key.clone(),
                merged.clone(),
            ));
            result.changed.insert(key.clone());
        }
        records.insert(key.clone(), merged);
    }
    blocks.extend(create_many(connection, &new_records).await?);
    blocks.extend(update_many(connection, &updated_records).await?);
    let retained: BTreeSet<_> = records
        .iter()
        .filter(|((kind, _), _)| kind == "posts")
        .flat_map(|(_, post)| references(post).into_iter().map(|(_, target, _)| target))
        .filter(|target| !blocks.contains_key(target))
        .collect();
    if !retained.is_empty() {
        blocks.extend(roots(connection, &retained, true).await?);
        let placeholders: Vec<_> = retained
            .into_iter()
            .filter(|key| !blocks.contains_key(key))
            .map(|key| {
                let value = json!({"id":key.1});
                (key, value)
            })
            .collect();
        blocks.extend(create_many(connection, &placeholders).await?);
    }
    let mut desired = BTreeSet::new();
    let mut post_ids = Vec::new();
    for key in incoming.keys() {
        if key.0 != "posts" {
            continue;
        }
        let root = blocks[key].id;
        post_ids.push(root);
        for (kind, target, index) in references(&records[key]) {
            desired.insert((root, blocks[&target].id, reference(&kind, index)));
        }
    }
    if !post_ids.is_empty() {
        let edges: Vec<(i32, Option<i32>, Option<i32>, String)> = sqlx::query_as(
            "SELECT id,from_,to_,content FROM inkcre.relations WHERE from_=ANY($1) ORDER BY id",
        )
        .bind(post_ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(db)?;
        let mut stale = Vec::new();
        for (edge, from, to, content) in edges {
            if !managed(&content) {
                continue;
            }
            let keep = if let (Some(from), Some(to)) = (from, to) {
                desired.remove(&(from, to, content))
            } else {
                false
            };
            if !keep {
                stale.push(edge);
            }
        }
        if !stale.is_empty() {
            sqlx::query("DELETE FROM inkcre.relations WHERE id=ANY($1)")
                .bind(stale)
                .execute(&mut *connection)
                .await
                .map_err(db)?;
        }
        if !desired.is_empty() {
            let from: Vec<_> = desired.iter().map(|(from, _, _)| *from).collect();
            let to: Vec<_> = desired.iter().map(|(_, to, _)| *to).collect();
            let contents: Vec<_> = desired.into_iter().map(|(_, _, content)| content).collect();
            sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) SELECT * FROM unnest($1::integer[],$2::integer[],$3::text[])").bind(from).bind(to).bind(contents).execute(&mut *connection).await.map_err(db)?;
        }
    }
    for (key, block) in blocks {
        result
            .block_ids
            .entry(key.0)
            .or_default()
            .insert(key.1, block.id);
    }
    Ok(result)
}
pub async fn write(core: &Core, batch: &Batch, supplement: bool) -> Result<WriteResult> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    let result = persist(&mut tx, batch, supplement).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}
pub async fn read(core: &Core, query: &Value) -> Result<Value> {
    if query
        .as_object()
        .is_none_or(|object| object.keys().any(|key| !KINDS.contains(&key.as_str())))
    {
        return Err(invalid("Invalid Twitter query"));
    }
    let mut identities = BTreeSet::new();
    for kind in KINDS {
        let items = match query.get(kind) {
            None => None,
            Some(value) => Some(
                value
                    .as_array()
                    .ok_or_else(|| invalid("Invalid Twitter query"))?,
            ),
        };
        if let Some(items) = items {
            if items.len() > 1000 {
                return Err(invalid("Twitter query exceeds limit"));
            }
            for id in items {
                let value = json!({"id":id});
                validate("TwitterIdentity", &value)?;
                identities.insert(identity(
                    kind,
                    id.as_str()
                        .ok_or_else(|| invalid("Invalid Twitter identity"))?,
                ));
            }
        }
    }
    let mut tx = core.pool().begin().await.map_err(db)?;
    let blocks = roots(&mut tx, &identities, false).await?;
    let mut response = json!({"users":[],"posts":[],"media":[]});
    for kind in KINDS {
        if let Some(ids) = query[kind].as_array() {
            for id in ids {
                let key = identity(kind, id.as_str().unwrap_or_default());
                let mut item = json!({"id":id,"status":"missing"});
                if let Some(block) = blocks.get(&key) {
                    let value: Value = serde_json::from_str(&block.content)
                        .map_err(|_| invalid("Stored Twitter JSON invalid"))?;
                    if value != json!({"id":id}) {
                        if validate(model(kind), &value).is_ok() {
                            item["status"] = json!("found");
                            item["data"] = value;
                        } else {
                            item["status"] = json!("failed");
                            item["error"] = json!("Stored archive record is invalid");
                        }
                    }
                }
                response[kind]
                    .as_array_mut()
                    .ok_or_else(|| invalid("Invalid Twitter projection"))?
                    .push(item);
            }
        }
    }
    Ok(response)
}
