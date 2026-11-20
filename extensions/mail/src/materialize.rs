use crate::{
    Result,
    adapter::{Adapter, decode_bytes, decode_text},
    fail, reconcile,
    schema::*,
};
use inkcre_core::{Block, Core};
use serde_json::{Value, json};
fn db<E>(_: E) -> inkcre_extension_sdk::RpcError {
    fail("Mail materialization database operation failed")
}
async fn existing(core: &Core, id: i32) -> Result<Option<Block>> {
    let children:Vec<Block>=sqlx::query_as("SELECT b.id,b.storage,b.resolver,b.content,b.created_at,b.updated_at FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.to_ WHERE r.from_=$1 AND r.content='content' ORDER BY r.id").bind(id).fetch_all(core.pool()).await.map_err(db)?;
    if children.len() > 1 {
        return Err(fail("Mail MIME content child is ambiguous"));
    }
    Ok(children.into_iter().next())
}
pub async fn run(core: &Core, id: i32) -> Result<Block> {
    if let Some(child) = existing(core, id).await? {
        return Ok(child);
    }
    let metadata = core
        .block_get(id)
        .await
        .map_err(db)?
        .ok_or_else(|| fail("MIME part Block not found"))?;
    if metadata.resolver != PART {
        return Err(fail("Block is not a Mail MIME part"));
    }
    let part: MimePart =
        serde_json::from_str(&metadata.content).map_err(|_| fail("Invalid MIME part metadata"))?;
    let owners:Vec<(i32,String)>=sqlx::query_as("SELECT r.from_,r.content FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.from_ WHERE r.to_=$1 AND b.resolver=$2 ORDER BY r.id").bind(id).bind(EMAIL).fetch_all(core.pool()).await.map_err(db)?;
    let owners: Vec<_> = owners
        .into_iter()
        .filter_map(|(email, raw)| {
            let value: Value = serde_json::from_str(&raw).ok()?;
            if ["attachment", "inline"]
                .iter()
                .any(|role| value["role"] == *role)
            {
                Some((email, value["part_id"].as_str()?.to_owned()))
            } else {
                None
            }
        })
        .collect();
    if owners.len() != 1 {
        return Err(fail("MIME part has no unique owning Email"));
    }
    let (email, path) = &owners[0];
    let occurrences:Vec<(i32,String,String,i32,Value,Value)>=sqlx::query_as("SELECT mb.id,mb.content,occ.content,s.id,s.config,s.state FROM inkcre.relations occ JOIN inkcre.blocks mb ON mb.id=occ.from_ JOIN inkcre.relations manages ON manages.to_=mb.id AND manages.content='manages' JOIN inkcre.sources s ON s.block=manages.from_ WHERE occ.to_=$1 AND mb.resolver=$2 AND s.type=$3 ORDER BY occ.id,manages.id").bind(email).bind(MAILBOX).bind(SOURCE).fetch_all(core.pool()).await.map_err(db)?;
    let mut available = None;
    for (mailbox_id, mailbox, locator, source_id, setup, state) in occurrences {
        let Ok(locator) = serde_json::from_str::<Value>(&locator) else {
            continue;
        };
        if locator["type"] != "contains" {
            continue;
        }
        let Some(uid) = locator["uid"].as_u64().and_then(|v| u32::try_from(v).ok()) else {
            continue;
        };
        let Some(epoch) = locator["uid_validity"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
        else {
            continue;
        };
        let mailbox: Mailbox = serde_json::from_str(&mailbox).map_err(db)?;
        let config = Config::parse(setup)?;
        let state: State = serde_json::from_value(state).map_err(db)?;
        if state.binding.as_ref() != Some(&config.binding()) {
            continue;
        }
        let parameters = config.parameters.clone();
        let target = mailbox.name.clone();
        let path = path.clone();
        let raw = tokio::task::spawn_blocking(move || {
            let mut adapter = Adapter::connect(&parameters)?;
            if adapter.select_epoch(&target)? != epoch {
                return Err(fail("Mail UIDVALIDITY no longer matches exact occurrence"));
            }
            adapter.part(uid, &path)
        })
        .await
        .map_err(|_| fail("Mail adapter task failed"))?;
        if let Ok(raw) = raw {
            available = Some((mailbox_id, source_id, config, locator, raw));
            break;
        }
    }
    let (mailbox_id, source_id, config, locator, raw) =
        available.ok_or_else(|| fail("No live exact Mail occurrence is available"))?;
    let mut bytes = decode_bytes(&part, &raw)?;
    let knowledge = core.knowledge();
    let resolver = knowledge
        .match_media_type(Some(&part.media_type))
        .map_err(db)?
        .or(knowledge
            .match_media_type(inkcre_core::knowledge::detect_media_type(&bytes))
            .map_err(db)?)
        .unwrap_or_else(|| "core.file.v1".into());
    if ["core.text.v1", "core.html.v1"].contains(&resolver.as_str()) {
        bytes = decode_text(&part, &raw)?.into_bytes();
    }
    let mut tx = reconcile::begin(core).await?;
    let current: Option<(String, String)> =
        sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
    if current != Some((PART.into(), metadata.content)) {
        return Err(fail("MIME metadata changed during materialization"));
    }
    if let Some(child) = existing(core, id).await? {
        tx.rollback().await.map_err(db)?;
        return Ok(child);
    }
    let (stored, state, storage): (Value, Value, Option<i32>) =
        sqlx::query_as("SELECT config,state,storage FROM inkcre.sources WHERE id=$1 FOR SHARE")
            .bind(source_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| fail("Mail Source no longer exists"))?;
    let state: State = serde_json::from_value(state).map_err(db)?;
    if stored != json!(config) || state.binding.as_ref() != Some(&config.binding()) {
        return Err(fail("Mail Source access changed during materialization"));
    }
    let current_owners:Vec<(i32,String)>=sqlx::query_as("SELECT r.from_,r.content FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.from_ WHERE r.to_=$1 AND b.resolver=$2 ORDER BY r.id").bind(id).bind(EMAIL).fetch_all(&mut *tx).await.map_err(db)?;
    let current_owners: Vec<_> = current_owners
        .into_iter()
        .filter_map(|(owner, raw)| {
            let value: Value = serde_json::from_str(&raw).ok()?;
            ["attachment", "inline"]
                .iter()
                .any(|role| value["role"] == *role)
                .then(|| {
                    value["part_id"]
                        .as_str()
                        .map(|path| (owner, path.to_owned()))
                })
                .flatten()
        })
        .collect();
    if current_owners != owners {
        return Err(fail("MIME owning Email changed during materialization"));
    }
    let current_occurrences = reconcile::relations(&mut tx, mailbox_id).await?;
    if !current_occurrences.iter().any(|(_, child, raw)| {
        *child == *email && serde_json::from_str::<Value>(raw).ok().as_ref() == Some(&locator)
    }) {
        return Err(fail("Mail occurrence disappeared during materialization"));
    }
    let storage = if let Some(storage) = storage {
        storage
    } else {
        let policy: Option<(String, Value)> =
            sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key='core.source'")
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        if let Some((schema, value)) = policy {
            if schema != "core.source.config.v1" {
                return Err(fail("Unknown Source storage policy schema"));
            }
            value["default_storage"]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| fail("Invalid Source storage policy"))?
        } else {
            -4
        }
    };
    let pointer = knowledge
        .storage_create(&mut tx, storage, &bytes)
        .await
        .map_err(db)?;
    let child: i32 = sqlx::query_scalar(
        "INSERT INTO inkcre.blocks(storage,resolver,content) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(storage)
    .bind(resolver)
    .bind(pointer)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    reconcile::edge(&mut tx, id, child, json!("content")).await?;
    tx.commit().await.map_err(db)?;
    core.block_get(child)
        .await
        .map_err(db)?
        .ok_or_else(|| fail("Materialized MIME content missing"))
}
pub async fn child(core: &Core, id: i32, materialize: bool) -> Result<Option<Block>> {
    if let Some(child) = existing(core, id).await? {
        Ok(Some(child))
    } else if materialize {
        run(core, id).await.map(Some)
    } else {
        Ok(None)
    }
}
