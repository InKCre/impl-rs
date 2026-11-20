use crate::{Result, db, remote::Bot, schema::*};
use inkcre_core::{Block, Core, source::Source};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

fn fail(message: &str) -> inkcre_extension_sdk::RpcError {
    inkcre_extension_sdk::RpcError::new("materialization_unavailable", message)
}

const BLOCK_COLUMNS: &str = "id,storage,resolver,content,created_at,updated_at";
async fn content(tx: &mut Transaction<'_, Postgres>, id: i32) -> Result<Option<Block>> {
    let children: Vec<Option<i32>> = sqlx::query_scalar(
        "SELECT to_ FROM inkcre.relations WHERE from_=$1 AND content='content' ORDER BY id",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await
    .map_err(db)?;
    if children.len() > 1 {
        return Err(fail("Telegram attachment has multiple content children"));
    }
    let Some(child) = children.first() else {
        return Ok(None);
    };
    let child = child.ok_or_else(|| fail("Telegram attachment content endpoint missing"))?;
    let block = sqlx::query_as(&format!(
        "SELECT {BLOCK_COLUMNS} FROM inkcre.blocks WHERE id=$1"
    ))
    .bind(child)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| fail("Telegram attachment content child missing"))?;
    Ok(Some(block))
}
async fn owner(tx: &mut Transaction<'_, Postgres>, id: i32) -> Result<Source> {
    let sources:Vec<Source>=sqlx::query_as("SELECT s.id,s.type,s.nickname,s.config,s.storage,s.block,s.state,s.created_at,s.updated_at FROM inkcre.sources s JOIN inkcre.relations r ON r.from_=s.block WHERE r.to_=$1 AND r.content='collects' ORDER BY s.id FOR SHARE OF s")
        .bind(id).fetch_all(&mut **tx).await.map_err(db)?;
    if sources.len() != 1 {
        return Err(fail("Telegram attachment has no unique live owning Source"));
    }
    let source = sources
        .into_iter()
        .next()
        .ok_or_else(|| fail("Telegram attachment Source missing"))?;
    if source.type_ != SOURCE {
        return Err(fail("Telegram attachment Source type changed"));
    }
    Ok(source)
}
async fn target(tx: &mut Transaction<'_, Postgres>, source: &Source) -> Result<i32> {
    if let Some(id) = source.storage {
        return Ok(id);
    }
    let config: Option<(String, Value)> =
        sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key='core.source' FOR SHARE")
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    let Some((schema, value)) = config else {
        return Ok(-4);
    };
    if schema != "core.source.config.v1"
        || value
            .as_object()
            .is_none_or(|o| o.keys().any(|key| key != "default_storage"))
    {
        return Err(fail("Invalid Source storage policy"));
    }
    value.get("default_storage").map_or(Ok(-4), |v| {
        v.as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| fail("Invalid Source default Storage"))
    })
}
pub async fn run(core: &Core, id: i32) -> Result<Block> {
    let metadata = core
        .block_get(id)
        .await
        .map_err(|_| fail("Telegram attachment read failed"))?
        .ok_or_else(|| {
            inkcre_extension_sdk::RpcError::new("not_found", "Telegram attachment not found")
        })?;
    if metadata.resolver != ATTACHMENT {
        return Err(inkcre_extension_sdk::RpcError::new(
            "invalid_input",
            "Block is not a Telegram attachment",
        ));
    }
    let root = Attachment::parse(&metadata.content)?;
    let mut tx = core.pool().begin().await.map_err(db)?;
    if let Some(existing) = content(&mut tx, id).await? {
        return Ok(existing);
    }
    let source = owner(&mut tx, id).await?;
    let setup = Config::parse(source.config.clone())?;
    let state = State::parse(source.state.clone())?;
    let pinned = state
        .bot_id
        .ok_or_else(|| fail("Telegram Source has no pinned bot identity"))?;
    tx.rollback().await.map_err(db)?;
    let bot = Bot::new(setup.bot_token)?;
    if bot.identity().await? != pinned {
        return Err(fail("Telegram Source bot identity changed"));
    }
    let body = bot.download(&root.file_id).await?;
    let knowledge = core.knowledge();
    let guessed = root
        .filename
        .as_ref()
        .and_then(|name| mime_guess::from_path(name).first_raw());
    let mut resolver = None;
    for mime in [
        inkcre_core::knowledge::detect_media_type(&body),
        root.mime_type.as_deref(),
        guessed,
    ] {
        if let Some(found) = knowledge
            .match_media_type(mime)
            .map_err(|_| fail("Telegram content type matching failed"))?
        {
            resolver = Some(found);
            break;
        }
    }
    let resolver = resolver.unwrap_or_else(|| {
        match root.kind.as_str() {
            "photo" | "sticker" => "core.image.v1",
            "animation" | "video" | "video_note" => "core.video.v1",
            "audio" | "voice" => "core.audio.v1",
            _ => "core.file.v1",
        }
        .into()
    });
    let mut tx = core.pool().begin().await.map_err(db)?;
    let current: Option<(String, String)> =
        sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
    if current != Some((metadata.resolver, metadata.content)) {
        return Err(fail("Telegram attachment changed during retrieval"));
    }
    if let Some(existing) = content(&mut tx, id).await? {
        return Ok(existing);
    }
    let live = owner(&mut tx, id).await?;
    if live.id != source.id
        || live.config != source.config
        || State::parse(live.state.clone())?.bot_id != Some(pinned)
    {
        return Err(fail("Telegram Source access changed during retrieval"));
    }
    let storage = target(&mut tx, &live).await?;
    let pointer = knowledge
        .storage_create(&mut tx, storage, &body)
        .await
        .map_err(|_| fail("Telegram target Storage unavailable or not writable"))?;
    let child:Block=sqlx::query_as(&format!("INSERT INTO inkcre.blocks(storage,resolver,content) VALUES($1,$2,$3) RETURNING {BLOCK_COLUMNS}"))
        .bind(storage).bind(resolver).bind(pointer).fetch_one(&mut *tx).await.map_err(db)?;
    sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,'content')")
        .bind(id)
        .bind(child.id)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(child)
}
