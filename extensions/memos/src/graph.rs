use crate::{Error, Result, schema::*};
use inkcre_core::{Block, Core, Relation};
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::{BTreeMap, HashSet};

const BLOCK: &str = "id,storage,resolver,content,created_at,updated_at";
const RELATION: &str = "id,from_,to_,content,updated_at";

pub async fn block(
    connection: &mut PgConnection,
    id: i32,
    resolver: &str,
    lock: bool,
) -> Result<Block> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let row: Block = sqlx::query_as(&format!(
        "SELECT {BLOCK} FROM inkcre.blocks WHERE id=$1 AND resolver=$2{suffix}"
    ))
    .bind(id)
    .bind(resolver)
    .fetch_optional(connection)
    .await?
    .ok_or(Error::NotFound)?;
    if row.storage.is_some() {
        return Err(Error::Integrity("Memo metadata must be inline"));
    }
    Ok(row)
}

pub async fn relations(connection: &mut PgConnection, id: i32) -> Result<Vec<Relation>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {RELATION} FROM inkcre.relations WHERE from_=$1 OR to_=$1 ORDER BY id"
    ))
    .bind(id)
    .fetch_all(connection)
    .await?)
}

pub fn links(id: i32, relations: &[Relation]) -> Result<(Vec<i32>, Option<i32>, Vec<i32>)> {
    let mut attachments = BTreeMap::new();
    let mut parent = None;
    let mut references = Vec::new();
    for relation in relations.iter().filter(|r| r.from_ == Some(id)) {
        if let Some(index) = relation.content.strip_prefix(ATTACHMENT_PREFIX) {
            if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Integrity("Invalid attachment relation"));
            }
            let index: usize = index
                .parse()
                .map_err(|_| Error::Integrity("Invalid attachment position"))?;
            if attachments.insert(index, endpoint(relation.to_)?).is_some() {
                return Err(Error::Integrity("Duplicate attachment position"));
            }
        } else if relation.content == "parent" {
            if parent.replace(endpoint(relation.to_)?).is_some() {
                return Err(Error::Integrity("Multiple memo parents"));
            }
        } else if relation.content == "reference" {
            references.push(endpoint(relation.to_)?);
        }
    }
    let mut ordered = Vec::new();
    for (expected, (position, id)) in attachments.into_iter().enumerate() {
        if expected != position {
            return Err(Error::Integrity("Attachment positions must be contiguous"));
        }
        ordered.push(id);
    }
    Ok((ordered, parent, references))
}
fn endpoint(id: Option<i32>) -> Result<i32> {
    id.ok_or(Error::Integrity("Missing graph endpoint"))
}

pub async fn solve_attachment(
    connection: &mut PgConnection,
    id: i32,
    core: &Core,
) -> Result<SolvedAttachment> {
    let metadata = block(connection, id, ATTACHMENT, false).await?;
    let canonical: CanonicalAttachment = serde_json::from_str(&metadata.content)?;
    let relations = relations(connection, id).await?;
    let owners: Vec<_> = relations
        .iter()
        .filter(|r| r.to_ == Some(id) && r.content.starts_with(ATTACHMENT_PREFIX))
        .collect();
    let content: Vec<_> = relations
        .iter()
        .filter(|r| r.from_ == Some(id) && r.content == "content")
        .collect();
    if owners.len() > 1 || content.len() != 1 {
        return Err(Error::Integrity(
            "Attachment owner/content relation is ambiguous",
        ));
    }
    let content_id = endpoint(content[0].to_)?;
    let child: Block = sqlx::query_as(&format!("SELECT {BLOCK} FROM inkcre.blocks WHERE id=$1"))
        .bind(content_id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(Error::Integrity("Attachment content is absent"))?;
    core.knowledge()
        .registry()
        .resolver(&child.resolver)
        .map_err(|_| Error::Integrity("Attachment semantic resolver is unavailable"))?;
    Ok(SolvedAttachment {
        block_id: id,
        content_block_id: content_id,
        canonical,
        owner_memo_id: owners.first().map(|r| endpoint(r.from_)).transpose()?,
    })
}

pub async fn solve(connection: &mut PgConnection, id: i32, core: &Core) -> Result<SolvedMemo> {
    let root = block(connection, id, MEMO, false).await?;
    let canonical = serde_json::from_str(&root.content)?;
    let (attachments, parent, references) = links(id, &relations(connection, id).await?)?;
    let mut solved = Vec::new();
    for attachment in attachments {
        solved.push(solve_attachment(connection, attachment, core).await?);
    }
    Ok(SolvedMemo {
        block_id: id,
        canonical,
        attachments: solved,
        parent_id: parent,
        reference_ids: references,
    })
}

async fn insert(
    connection: &mut PgConnection,
    resolver: &str,
    storage: Option<i32>,
    content: String,
) -> Result<i32> {
    Ok(sqlx::query_scalar(
        "INSERT INTO inkcre.blocks(resolver,storage,content) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(resolver)
    .bind(storage)
    .bind(content)
    .fetch_one(connection)
    .await?)
}
async fn edge(connection: &mut PgConnection, from: i32, to: i32, content: &str) -> Result<()> {
    sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3)")
        .bind(from)
        .bind(to)
        .bind(content)
        .execute(connection)
        .await?;
    Ok(())
}

pub async fn set_attachments(
    connection: &mut PgConnection,
    memo: i32,
    requested: &[i32],
    core: &Core,
) -> Result<()> {
    let unique: HashSet<_> = requested.iter().copied().collect();
    if unique.len() != requested.len() {
        return Err(Error::BadRequest("Duplicate attachment identity"));
    }
    block(connection, memo, MEMO, true).await?;
    let mut ordered: Vec<_> = unique.iter().copied().collect();
    ordered.sort_unstable();
    for id in ordered {
        block(connection, id, ATTACHMENT, true).await?;
        let owners: Vec<_> = relations(connection, id)
            .await?
            .into_iter()
            .filter(|r| r.to_ == Some(id) && r.content.starts_with(ATTACHMENT_PREFIX))
            .collect();
        if owners.len() > 1 || owners.first().is_some_and(|r| r.from_ != Some(memo)) {
            return Err(Error::BadRequest("Attachment already has another owner"));
        }
    }
    let current = relations(connection, memo).await?;
    let (old, _, _) = links(memo, &current)?;
    for (position, id) in requested.iter().enumerate() {
        let label = format!("{ATTACHMENT_PREFIX}{position}");
        if let Some(existing) = current.iter().find(|r| {
            r.from_ == Some(memo)
                && r.content
                    .strip_prefix(ATTACHMENT_PREFIX)
                    .and_then(|v| v.parse::<usize>().ok())
                    == Some(position)
        }) {
            sqlx::query("UPDATE inkcre.relations SET to_=$2 WHERE id=$1")
                .bind(existing.id)
                .bind(id)
                .execute(&mut *connection)
                .await?;
        } else {
            edge(connection, memo, *id, &label).await?;
        }
    }
    for r in current
        .iter()
        .filter(|r| r.from_ == Some(memo) && r.content.starts_with(ATTACHMENT_PREFIX))
    {
        let position: usize = r.content[ATTACHMENT_PREFIX.len()..]
            .parse()
            .map_err(|_| Error::Integrity("Invalid attachment index"))?;
        if position >= requested.len() {
            sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
                .bind(r.id)
                .execute(&mut *connection)
                .await?;
        }
    }
    for id in old.into_iter().filter(|id| !unique.contains(id)) {
        delete_attachment(connection, id, core).await?;
    }
    Ok(())
}

pub async fn create(
    core: &Core,
    mut canonical: CanonicalMemo,
    attachments: &[i32],
    parent: Option<i32>,
) -> Result<SolvedMemo> {
    let mut tx = core.pool().begin().await?;
    if let Some(parent) = parent {
        let root = block(&mut tx, parent, MEMO, false).await?;
        let p: CanonicalMemo = serde_json::from_str(&root.content)?;
        canonical.visibility = p.visibility;
    }
    let id = insert(&mut tx, MEMO, None, serde_json::to_string(&canonical)?).await?;
    if let Some(parent) = parent {
        edge(&mut tx, id, parent, "parent").await?;
    }
    set_attachments(&mut tx, id, attachments, core).await?;
    let solved = solve(&mut tx, id, core).await?;
    tx.commit().await?;
    Ok(solved)
}

pub async fn update(
    core: &Core,
    id: i32,
    patch: Option<Value>,
    attachments: Option<Vec<i32>>,
) -> Result<SolvedMemo> {
    let mut tx = core.pool().begin().await?;
    let root = block(&mut tx, id, MEMO, true).await?;
    if let Some(patch) = patch {
        let mut canonical: Value =
            serde_json::to_value(serde_json::from_str::<CanonicalMemo>(&root.content)?)?;
        for (key, value) in patch
            .as_object()
            .ok_or(Error::BadRequest("Invalid memo patch"))?
        {
            canonical[key] = value.clone();
        }
        let (_, parent, _) = links(id, &relations(&mut tx, id).await?)?;
        if let Some(parent) = parent {
            let p = block(&mut tx, parent, MEMO, false).await?;
            let p: CanonicalMemo = serde_json::from_str(&p.content)?;
            canonical["visibility"] = json!(p.visibility);
        }
        let valid: CanonicalMemo = serde_json::from_value(canonical)?;
        sqlx::query("UPDATE inkcre.blocks SET content=$2 WHERE id=$1")
            .bind(id)
            .bind(serde_json::to_string(&valid)?)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(attachments) = attachments {
        set_attachments(&mut tx, id, &attachments, core).await?;
    }
    let solved = solve(&mut tx, id, core).await?;
    tx.commit().await?;
    Ok(solved)
}

pub async fn create_attachment(
    core: &Core,
    filename: String,
    media_type: String,
    bytes: &[u8],
    memo: Option<i32>,
) -> Result<SolvedAttachment> {
    let mut tx = core.pool().begin().await?;
    if let Some(memo) = memo {
        block(&mut tx, memo, MEMO, true).await?;
    }
    let pointer = core
        .knowledge()
        .storage_create(&mut tx, -4, bytes)
        .await
        .map_err(|_| Error::Integrity("PostgreSQL binary storage is unavailable"))?;
    let canonical = CanonicalAttachment {
        filename,
        media_type: media_type.clone(),
        size: bytes.len() as u64,
        created_at: chrono::Utc::now(),
    };
    let metadata = insert(
        &mut tx,
        ATTACHMENT,
        None,
        serde_json::to_string(&canonical)?,
    )
    .await?;
    let resolver = core
        .knowledge()
        .match_media_type(Some(&media_type))
        .map_err(|_| Error::Integrity("Resolver matching failed"))?
        .unwrap_or_else(|| "core.file.v1".into());
    let content = insert(&mut tx, &resolver, Some(-4), pointer).await?;
    edge(&mut tx, metadata, content, "content").await?;
    if let Some(memo) = memo {
        let (mut attachments, _, _) = links(memo, &relations(&mut tx, memo).await?)?;
        attachments.push(metadata);
        set_attachments(&mut tx, memo, &attachments, core).await?;
    }
    let solved = solve_attachment(&mut tx, metadata, core).await?;
    tx.commit().await?;
    Ok(solved)
}

pub async fn delete_attachment(
    connection: &mut PgConnection,
    id: i32,
    core: &Core,
) -> Result<bool> {
    match block(connection, id, ATTACHMENT, true).await {
        Ok(_) => (),
        Err(Error::NotFound) => return Ok(false),
        Err(e) => return Err(e),
    }
    let solved = solve_attachment(connection, id, core).await?;
    let shared:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inkcre.relations WHERE to_=$1 AND from_ IS DISTINCT FROM $2 AND content='content')").bind(solved.content_block_id).bind(id).fetch_one(&mut *connection).await?;
    sqlx::query("DELETE FROM inkcre.blocks WHERE id=$1")
        .bind(id)
        .execute(&mut *connection)
        .await?;
    if !shared {
        let child: Block =
            sqlx::query_as(&format!("SELECT {BLOCK} FROM inkcre.blocks WHERE id=$1"))
                .bind(solved.content_block_id)
                .fetch_one(&mut *connection)
                .await?;
        core.knowledge()
            .storage_delete(
                connection,
                child
                    .storage
                    .ok_or(Error::Integrity("Attachment content storage absent"))?,
                &child.content,
            )
            .await
            .map_err(|_| Error::Integrity("Attachment content storage is not writable"))?;
        sqlx::query("DELETE FROM inkcre.blocks WHERE id=$1")
            .bind(child.id)
            .execute(connection)
            .await?;
    }
    Ok(true)
}

pub async fn delete(core: &Core, id: i32) -> Result<()> {
    let mut tx = core.pool().begin().await?;
    block(&mut tx, id, MEMO, true).await?;
    let mut visited = HashSet::new();
    let mut comments = Vec::new();
    let mut attachments = HashSet::new();
    let mut stack = vec![(id, false)];
    while let Some((memo, expanded)) = stack.pop() {
        if expanded {
            if memo != id {
                comments.push(memo);
            }
            continue;
        }
        if !visited.insert(memo) {
            continue;
        }
        let edges = relations(&mut tx, memo).await?;
        for r in edges
            .iter()
            .filter(|r| r.from_ == Some(memo) && r.content.starts_with(ATTACHMENT_PREFIX))
        {
            let attachment = endpoint(r.to_)?;
            if block(&mut tx, attachment, ATTACHMENT, false).await.is_err() {
                continue;
            }
            let owners: Vec<_> = relations(&mut tx, attachment)
                .await?
                .into_iter()
                .filter(|r| r.to_ == Some(attachment) && r.content.starts_with(ATTACHMENT_PREFIX))
                .collect();
            if owners.len() == 1 && owners[0].from_ == Some(memo) {
                attachments.insert(attachment);
            }
        }
        stack.push((memo, true));
        for r in edges
            .iter()
            .filter(|r| r.to_ == Some(memo) && r.content == "parent")
        {
            let child = endpoint(r.from_)?;
            let parents: Vec<_> = relations(&mut tx, child)
                .await?
                .into_iter()
                .filter(|r| r.from_ == Some(child) && r.content == "parent")
                .collect();
            if parents.len() == 1
                && parents[0].to_ == Some(memo)
                && block(&mut tx, child, MEMO, false).await.is_ok()
            {
                stack.push((child, false));
            }
        }
    }
    sqlx::query("DELETE FROM inkcre.blocks WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    for comment in comments {
        let result = async {
            sqlx::query("DELETE FROM inkcre.blocks WHERE id=$1")
                .bind(comment)
                .execute(core.pool())
                .await?;
            Ok::<_, sqlx::Error>(())
        }
        .await;
        if result.is_err() {
            eprintln!("Memos comment cleanup failed");
        }
    }
    for attachment in attachments {
        let result = async {
            let mut tx = core.pool().begin().await?;
            delete_attachment(&mut tx, attachment, core).await?;
            tx.commit().await?;
            Ok::<_, Error>(())
        }
        .await;
        if result.is_err() {
            eprintln!("Memos attachment cleanup failed");
        }
    }
    Ok(())
}

pub async fn all_memos(connection: &mut PgConnection) -> Result<Vec<Block>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {BLOCK} FROM inkcre.blocks WHERE resolver=$1"
    ))
    .bind(MEMO)
    .fetch_all(connection)
    .await?)
}

pub async fn list_attachments(core: &Core) -> Result<Vec<SolvedAttachment>> {
    let mut tx = core.pool().begin().await?;
    let ids: Vec<i32> = sqlx::query_scalar(
        "SELECT id FROM inkcre.blocks WHERE resolver=$1 ORDER BY created_at DESC,id DESC",
    )
    .bind(ATTACHMENT)
    .fetch_all(&mut *tx)
    .await?;
    let mut result = Vec::new();
    for id in ids {
        result.push(solve_attachment(&mut tx, id, core).await?);
    }
    Ok(result)
}
