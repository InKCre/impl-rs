//! Mail graph grammar and identity ladder. All shared writes use native transactions.
use crate::{Result, fail, schema::*};
use inkcre_core::{Block, Core};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;
pub type Tx<'a> = Transaction<'a, Postgres>;
fn db<E>(_: E) -> inkcre_extension_sdk::RpcError {
    fail("Mail graph database operation failed")
}
pub async fn begin(core: &Core) -> Result<Tx<'_>> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(17856292)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}
pub async fn create(tx: &mut Tx<'_>, resolver: &str, value: &str) -> Result<i32> {
    sqlx::query_scalar("INSERT INTO inkcre.blocks(resolver,content) VALUES($1,$2) RETURNING id")
        .bind(resolver)
        .bind(value)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)
}
pub async fn edge(tx: &mut Tx<'_>, from: i32, to: i32, content: Value) -> Result<()> {
    let raw = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        content.to_string()
    };
    sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) SELECT $1,$2,$3 WHERE NOT EXISTS(SELECT 1 FROM inkcre.relations WHERE from_=$1 AND to_=$2 AND content=$3)").bind(from).bind(to).bind(raw).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
pub async fn relations(tx: &mut Tx<'_>, from: i32) -> Result<Vec<(i32, i32, String)>> {
    sqlx::query_as("SELECT id,to_,content FROM inkcre.relations WHERE from_=$1 AND to_ IS NOT NULL ORDER BY id").bind(from).fetch_all(&mut **tx).await.map_err(db)
}
async fn candidates(tx: &mut Tx<'_>, resolver: &str, key: &str, value: &str) -> Result<Vec<Block>> {
    let blocks:Vec<Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE resolver=$1 ORDER BY id").bind(resolver).fetch_all(&mut **tx).await.map_err(db)?;
    Ok(blocks
        .into_iter()
        .filter(|b| {
            serde_json::from_str::<Value>(&b.content)
                .ok()
                .is_some_and(|v| v[key] == value)
        })
        .collect())
}
async fn unique(
    tx: &mut Tx<'_>,
    resolver: &str,
    key: &str,
    value: &str,
    content: Value,
) -> Result<i32> {
    let found = candidates(tx, resolver, key, value).await?;
    if found.len() == 1 {
        Ok(found[0].id)
    } else {
        create(tx, resolver, &content.to_string()).await
    }
}
async fn remove_edge(tx: &mut Tx<'_>, id: i32) -> Result<()> {
    sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
async fn replace_block(tx: &mut Tx<'_>, id: i32, resolver: &str, content: &str) -> Result<()> {
    sqlx::query("UPDATE inkcre.blocks SET resolver=$2,content=$3,storage=NULL WHERE id=$1")
        .bind(id)
        .bind(resolver)
        .bind(content)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
pub async fn effective(
    core: &Core,
    id: i32,
    defaults: &Exclusions,
) -> Result<(inkcre_core::source::Source, Config)> {
    let mut tx = begin(core).await?;
    let mut source:inkcre_core::source::Source=sqlx::query_as("SELECT id,type,nickname,config,state,storage,block,created_at,updated_at FROM inkcre.sources WHERE id=$1 FOR UPDATE").bind(id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(||fail("Mail Source no longer exists"))?;
    if source.type_ != SOURCE {
        return Err(fail("Mail Source type changed"));
    }
    let mut config = Config::parse(source.config.clone())?;
    if config.excluded_mailboxes.is_none() {
        config.excluded_mailboxes = Some(defaults.clone());
    }
    let normalized = serde_json::to_value(&config).map_err(db)?;
    if source.config != normalized {
        source.config = normalized;
        sqlx::query("UPDATE inkcre.sources SET config=$2 WHERE id=$1")
            .bind(id)
            .bind(&source.config)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    let state: State = serde_json::from_value(source.state.clone())
        .map_err(|_| fail("Invalid Mail Source state"))?;
    if state
        .checkpoints
        .values()
        .any(|checkpoint| checkpoint.uid_validity == 0 || checkpoint.highest_modseq == Some(0))
    {
        return Err(fail("Invalid Mail Source checkpoint"));
    }
    if state
        .binding
        .as_ref()
        .is_some_and(|b| *b != config.binding())
    {
        return Err(fail(
            "Mail Source access binding changed; create a new Source",
        ));
    }
    tx.commit().await.map_err(db)?;
    Ok((source, config))
}
pub async fn prepare(
    core: &Core,
    id: i32,
    config: &Config,
    mailboxes: &[(Mailbox, u32)],
) -> Result<Vec<(i32, Option<Checkpoint>)>> {
    let mut tx = begin(core).await?;
    let (stored, mut state, anchor) = source_context(&mut tx, id).await?;
    if stored != serde_json::to_value(config).map_err(db)? {
        return Err(fail("Mail Source configuration changed"));
    }
    if state
        .binding
        .as_ref()
        .is_some_and(|b| *b != config.binding())
    {
        return Err(fail("Mail Source access binding changed"));
    }
    state.binding = Some(config.binding());
    let mut result = vec![];
    for (mailbox, epoch) in mailboxes {
        let managed = relations(&mut tx, anchor).await?;
        let mut matches = vec![];
        for (_, child, content) in managed {
            if content != "manages" {
                continue;
            }
            let row: Option<(String, String)> =
                sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1")
                    .bind(child)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            if let Some((kind, content)) = row
                && kind == MAILBOX
                && let Ok(value) = serde_json::from_str::<Mailbox>(&content)
                && (if let Some(identity) = &mailbox.mailbox_id {
                    value.mailbox_id.as_ref() == Some(identity)
                } else {
                    value.mailbox_id.is_none() && value.name == mailbox.name
                })
            {
                matches.push(child)
            }
        }
        let block = if matches.len() == 1 {
            matches[0]
        } else {
            create(&mut tx, MAILBOX, &json!(mailbox).to_string()).await?
        };
        replace_block(&mut tx, block, MAILBOX, &json!(mailbox).to_string()).await?;
        edge(&mut tx, anchor, block, json!("manages")).await?;
        let previous = state.checkpoints.get(&block.to_string()).cloned();
        if previous.as_ref().is_some_and(|p| p.uid_validity != *epoch) {
            for (edge_id, email, content) in relations(&mut tx, block).await? {
                if let Ok(value) = serde_json::from_str::<Value>(&content)
                    && value["type"] == "contains"
                    && value["uid_validity"] != *epoch
                {
                    remove_edge(&mut tx, edge_id).await?;
                    clear_flags(&mut tx, block, email).await?;
                }
            }
        }
        result.push((block, previous));
    }
    save_state(&mut tx, id, &state).await?;
    tx.commit().await.map_err(db)?;
    Ok(result)
}
async fn source_context(tx: &mut Tx<'_>, id: i32) -> Result<(Value, State, i32)> {
    let (kind, nickname, config, state, anchor): (
        String,
        Option<String>,
        Value,
        Value,
        Option<i32>,
    ) = sqlx::query_as(
        "SELECT type,nickname,config,state,block FROM inkcre.sources WHERE id=$1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| fail("Mail Source disappeared"))?;
    if kind != SOURCE {
        return Err(fail("Mail Source type changed"));
    }
    let content = json!({"id":id,"type":kind,"nickname":nickname}).to_string();
    let anchor = if let Some(anchor) = anchor {
        replace_block(tx, anchor, "core.source.v1", &content).await?;
        anchor
    } else {
        let block = create(tx, "core.source.v1", &content).await?;
        sqlx::query("UPDATE inkcre.sources SET block=$2 WHERE id=$1")
            .bind(id)
            .bind(block)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        block
    };
    Ok((
        config,
        serde_json::from_value(state).map_err(|_| fail("Invalid Mail Source state"))?,
        anchor,
    ))
}
async fn save_state(tx: &mut Tx<'_>, id: i32, state: &State) -> Result<()> {
    sqlx::query("UPDATE inkcre.sources SET state=$2 WHERE id=$1")
        .bind(id)
        .bind(json!(state))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
async fn locator(
    tx: &mut Tx<'_>,
    mailbox: i32,
    epoch: u32,
    uid: u32,
) -> Result<Option<(i32, i32)>> {
    let found: Vec<_> = relations(tx, mailbox)
        .await?
        .into_iter()
        .filter(|(_, _, content)| {
            serde_json::from_str::<Value>(content)
                .ok()
                .is_some_and(|v| {
                    v["type"] == "contains" && v["uid_validity"] == epoch && v["uid"] == uid
                })
        })
        .collect();
    Ok((found.len() == 1).then(|| (found[0].0, found[0].1)))
}
pub async fn apply(
    core: &Core,
    source: i32,
    config: &Config,
    mailbox: i32,
    previous: Option<&Checkpoint>,
    batch: &Batch,
    backfill: bool,
) -> Result<(usize, usize)> {
    let mut tx = begin(core).await?;
    let (stored, mut state, anchor) = source_context(&mut tx, source).await?;
    if stored != json!(config) || state.binding.as_ref() != Some(&config.binding()) {
        return Err(fail(
            "Mail configuration or binding changed during collection",
        ));
    }
    if previous.is_some_and(|checkpoint| checkpoint.uid_validity != batch.next.uid_validity) {
        for (id, email, raw) in relations(&mut tx, mailbox).await? {
            if let Ok(value) = serde_json::from_str::<Value>(&raw)
                && value["type"] == "contains"
                && value["uid_validity"] != batch.next.uid_validity
            {
                remove_edge(&mut tx, id).await?;
                clear_flags(&mut tx, mailbox, email).await?;
            }
        }
    }
    for message in &batch.messages {
        reconcile_message(&mut tx, anchor, mailbox, batch.next.uid_validity, message).await?;
    }
    let mut flags = 0;
    let mut removed = 0;
    for (uid, values) in &batch.flags {
        if let Some((_, email)) = locator(&mut tx, mailbox, batch.next.uid_validity, *uid).await? {
            replace_flags(&mut tx, mailbox, email, values).await?;
            flags += 1;
        }
    }
    if config.synchronize_deletions {
        for uid in &batch.removed {
            if let Some((edge_id, email)) =
                locator(&mut tx, mailbox, batch.next.uid_validity, *uid).await?
            {
                remove_edge(&mut tx, edge_id).await?;
                clear_flags(&mut tx, mailbox, email).await?;
                removed += 1;
            }
        }
    }
    if !backfill && json!(state.checkpoints.get(&mailbox.to_string())) == json!(previous) {
        state
            .checkpoints
            .insert(mailbox.to_string(), batch.next.clone());
        save_state(&mut tx, source, &state).await?;
    }
    tx.commit().await.map_err(db)?;
    Ok((flags, removed))
}
async fn reconcile_message(
    tx: &mut Tx<'_>,
    anchor: i32,
    mailbox: i32,
    epoch: u32,
    message: &Message,
) -> Result<()> {
    let mut selected = None;
    if let Some((_, id)) = locator(tx, mailbox, epoch, message.uid).await? {
        let raw: String =
            sqlx::query_scalar("SELECT content FROM inkcre.blocks WHERE id=$1 AND resolver=$2")
                .bind(id)
                .bind(EMAIL)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let prior: Email = serde_json::from_str(&raw).map_err(db)?;
        if compatible(&prior, &message.root) {
            selected = Some(id)
        }
    }
    for (key, value, strong) in [
        ("email_id", message.root.email_id.as_deref(), true),
        ("message_id", message.root.message_id.as_deref(), false),
    ] {
        if selected.is_some() {
            break;
        }
        let Some(value) = value else { continue };
        let mut valid = vec![];
        for candidate in candidates(tx, EMAIL, key, value).await? {
            let prior: Email = serde_json::from_str(&candidate.content).map_err(db)?;
            if !compatible(&prior, &message.root) {
                continue;
            }
            if strong {
                let present:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inkcre.relations a JOIN inkcre.relations b ON b.from_=a.to_ JOIN inkcre.blocks mb ON mb.id=a.to_ WHERE a.from_=$1 AND a.content='manages' AND mb.resolver=$2 AND b.to_=$3)").bind(anchor).bind(MAILBOX).bind(candidate.id).fetch_one(&mut **tx).await.map_err(db)?;
                if !present {
                    continue;
                }
            }
            let incoming: Vec<(i32, String)> = sqlx::query_as(
                "SELECT from_,content FROM inkcre.relations WHERE to_=$1 AND from_ IS NOT NULL",
            )
            .bind(candidate.id)
            .fetch_all(&mut **tx)
            .await
            .map_err(db)?;
            if incoming.iter().any(|(from, content)| {
                *from == mailbox
                    && serde_json::from_str::<Value>(content)
                        .ok()
                        .is_some_and(|v| {
                            v["type"] == "contains"
                                && (v["uid_validity"] != epoch || v["uid"] != message.uid)
                        })
            }) {
                continue;
            }
            if !strong
                && message.parts.iter().any(|p| p.role != "body")
                && (incoming.iter().any(|(_, content)| {
                    serde_json::from_str::<Value>(content)
                        .ok()
                        .is_some_and(|v| v["type"] == "contains")
                }) || prior.email_id.is_some()
                    || prior.subject.is_some()
                    || prior.authored_at.is_some())
            {
                continue;
            }
            valid.push(candidate.id);
        }
        if valid.len() == 1 {
            selected = Some(valid[0])
        }
    }
    let email = if let Some(id) = selected {
        let raw: String = sqlx::query_scalar("SELECT content FROM inkcre.blocks WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
        let old: Email = serde_json::from_str(&raw).map_err(db)?;
        let root = Email {
            message_id: old.message_id.or(message.root.message_id.clone()),
            email_id: old.email_id.or(message.root.email_id.clone()),
            subject: message.root.subject.clone().or(old.subject),
            authored_at: message.root.authored_at.or(old.authored_at),
        };
        replace_block(tx, id, EMAIL, &json!(root).to_string()).await?;
        id
    } else {
        create(tx, EMAIL, &json!(message.root).to_string()).await?
    };
    // A changed intrinsic identity replaces only this remote occurrence, preserving shared information.
    if let Some((edge_id, old)) = locator(tx, mailbox, epoch, message.uid).await?
        && old != email
    {
        remove_edge(tx, edge_id).await?;
        clear_flags(tx, mailbox, old).await?;
    }
    for (edge_id, child, content) in relations(tx, mailbox).await? {
        if child == email
            && serde_json::from_str::<Value>(&content)
                .ok()
                .is_some_and(|v| v["type"] == "contains")
        {
            remove_edge(tx, edge_id).await?;
        }
    }
    edge(
        tx,
        mailbox,
        email,
        json!({"type":"contains","uid_validity":epoch,"uid":message.uid}),
    )
    .await?;
    let existing = relations(tx, email).await?;
    let mut parts = BTreeMap::new();
    let mut html = vec![];
    for part in &message.parts {
        let kind = if part.role == "body" {
            match (message.list.is_some(), part.metadata.media_type.as_str()) {
                (true, "text/html") => NEWS_HTML,
                (true, _) => NEWS_TEXT,
                (false, "text/html") => "core.html.v1",
                _ => "core.text.v1",
            }
        } else {
            PART
        };
        let content = if part.role == "body" {
            part.text.clone().unwrap_or_default()
        } else {
            json!(part.metadata).to_string()
        };
        let relation = existing.iter().find(|(_, _, value)| {
            serde_json::from_str::<Value>(value).ok().is_some_and(|v| {
                v["part_id"] == part.id
                    && ["body", "attachment", "inline"]
                        .iter()
                        .any(|role| v["role"] == *role)
            })
        });
        let child = if let Some((_, id, _)) = relation {
            let prior: String =
                sqlx::query_scalar("SELECT resolver FROM inkcre.blocks WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(db)?;
            if prior == kind
                || (part.role == "body"
                    && ["core.text.v1", "core.html.v1", NEWS_HTML, NEWS_TEXT]
                        .contains(&prior.as_str()))
            {
                replace_block(tx, *id, kind, &content).await?;
                *id
            } else {
                create(tx, kind, &content).await?
            }
        } else {
            create(tx, kind, &content).await?
        };
        if let Some((id, _, _)) = relation {
            remove_edge(tx, *id).await?
        }
        edge(tx,email,child,json!({"role":part.role,"part_id":part.id,"alternative_groups":if part.role=="body"{json!(part.alternatives)}else{Value::Null}})).await?;
        if part.role == "body" && part.metadata.media_type == "text/html" {
            html.push((child, content));
        }
        parts.insert(part.id.clone(), (child, &part.metadata));
    }
    if let Some(list) = &message.list {
        let list_id = unique(tx, LIST, "list_id", &list.list_id, json!(list)).await?;
        edge(tx, list_id, email, json!("publishes")).await?;
    }
    for (id, _, raw) in &existing {
        let value = serde_json::from_str::<Value>(raw).unwrap_or(Value::Null);
        if ["from", "sender", "reply_to", "to", "cc", "bcc"]
            .iter()
            .any(|role| value["role"] == *role)
            || raw.starts_with("parent:")
            || raw.starts_with("reference:")
        {
            remove_edge(tx, *id).await?
        }
    }
    for p in &message.participants {
        let address = unique(
            tx,
            ADDRESS,
            "address",
            &p.address,
            json!({"address":p.address}),
        )
        .await?;
        edge(
            tx,
            email,
            address,
            json!({"role":p.role,"order":p.order,"display_name":p.name}),
        )
        .await?;
    }
    for (prefix, ids) in [
        ("parent", &message.parents),
        ("reference", &message.references),
    ] {
        for (order, id) in ids.iter().enumerate() {
            let target = unique(
                tx,
                EMAIL,
                "message_id",
                id,
                json!(Email {
                    message_id: Some(id.clone()),
                    ..Email::default()
                }),
            )
            .await?;
            edge(tx, email, target, json!(format!("{prefix}:{order}"))).await?;
        }
    }
    replace_flags(tx, mailbox, email, &message.flags).await?;
    let re =
        regex::Regex::new(r#"(?i)(?:src|href)\s*=\s*['"]\s*([^'"\s>]+)"#).expect("static regex");
    for (body, content) in html {
        for capture in re.captures_iter(&content) {
            let reference = &capture[1];
            for (child, metadata) in parts.values() {
                if metadata
                    .content_id
                    .as_ref()
                    .is_some_and(|id| reference.eq_ignore_ascii_case(&format!("cid:{id}")))
                    || metadata.content_location.as_deref() == Some(reference)
                {
                    edge(
                        tx,
                        body,
                        *child,
                        json!({"type":"embeds","reference":reference}),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}
fn compatible(a: &Email, b: &Email) -> bool {
    [
        (a.message_id.as_ref(), b.message_id.as_ref()),
        (a.email_id.as_ref(), b.email_id.as_ref()),
    ]
    .iter()
    .all(|(a, b)| a.is_none() || b.is_none() || a == b)
}
async fn clear_flags(tx: &mut Tx<'_>, mailbox: i32, email: i32) -> Result<()> {
    sqlx::query("DELETE FROM inkcre.relations tags USING inkcre.relations owns WHERE owns.from_=$1 AND owns.content='has' AND tags.from_=owns.to_ AND tags.to_=$2 AND tags.content='tags'").bind(mailbox).bind(email).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
async fn replace_flags(tx: &mut Tx<'_>, mailbox: i32, email: i32, values: &[String]) -> Result<()> {
    clear_flags(tx, mailbox, email).await?;
    let owned = relations(tx, mailbox).await?;
    let mut seen = std::collections::BTreeSet::new();
    for name in values {
        let normalized = name.to_lowercase();
        if normalized == "\\recent" || !seen.insert(normalized.clone()) {
            continue;
        }
        let mut matches = vec![];
        for (_, id, content) in &owned {
            if content != "has" {
                continue;
            }
            let raw: Option<String> =
                sqlx::query_scalar("SELECT content FROM inkcre.blocks WHERE id=$1 AND resolver=$2")
                    .bind(id)
                    .bind(FLAG)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(db)?;
            if raw
                .and_then(|s| serde_json::from_str::<Flag>(&s).ok())
                .is_some_and(|v| v.name.eq_ignore_ascii_case(name))
            {
                matches.push(*id)
            }
        }
        let flag = if matches.len() == 1 {
            matches[0]
        } else {
            let description = match normalized.as_str() {
                "\\seen" => Some("The message has been read."),
                "\\answered" => Some("The message has been answered."),
                "\\flagged" => Some("The message is marked for special attention."),
                "\\deleted" => Some("The message is marked for removal from this mailbox."),
                "\\draft" => Some("The message is a draft."),
                _ => None,
            };
            let id = create(
                tx,
                FLAG,
                &json!({"name":name,"description":description}).to_string(),
            )
            .await?;
            edge(tx, mailbox, id, json!("has")).await?;
            id
        };
        edge(tx, flag, email, json!("tags")).await?;
    }
    Ok(())
}
pub async fn record_seen(
    core: &Core,
    mailbox: i32,
    epoch: u32,
    messages: &[Message],
) -> Result<()> {
    let mut tx = begin(core).await?;
    for message in messages {
        if let Some((_, email)) = locator(&mut tx, mailbox, epoch, message.uid).await? {
            let mut flags = message.flags.clone();
            flags.push("\\Seen".into());
            replace_flags(&mut tx, mailbox, email, &flags).await?;
        }
    }
    tx.commit().await.map_err(db)
}
