use crate::{Result, db, fail, markup, materialize, remote::Bot, schema::*};
use inkcre_core::{Core, source::Source};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

#[derive(Default)]
struct Report {
    counts: [usize; 6],
    diagnostics: Vec<Value>,
}
impl Report {
    fn value(&self) -> Value {
        json!({"counts":{"saved":self.counts[0],"partial":self.counts[1],"unsupported":self.counts[2],"unauthorized":self.counts[3],"duplicates":self.counts[4],"failed":self.counts[5]},"diagnostics":self.diagnostics})
    }
    fn diagnostic(&mut self, id: i64, scope: &str, message: &str) {
        if self.diagnostics.len() < 50 {
            self.diagnostics
                .push(json!({"update_id":id,"scope":scope,"message":message}));
        }
    }
    async fn save(&self, core: &Core, job: i64) -> Result<()> {
        core.job_patch_state(job, &self.value())
            .await
            .map_err(|_| fail("Telegram Job report failed"))
    }
}
async fn locked(
    tx: &mut Transaction<'_, Postgres>,
    source: &Source,
) -> Result<(State, Option<i32>, Option<String>)> {
    let (kind, config, state, anchor, nickname): (
        String,
        Value,
        Value,
        Option<i32>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT type,config,state,block,nickname FROM inkcre.sources WHERE id=$1 FOR UPDATE",
    )
    .bind(source.id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| fail("Telegram Source no longer exists"))?;
    if kind != SOURCE || config != source.config {
        return Err(fail("Telegram Source configuration changed"));
    }
    Ok((State::parse(state)?, anchor, nickname))
}
async fn save_state(tx: &mut Transaction<'_, Postgres>, id: i32, state: &State) -> Result<()> {
    sqlx::query("UPDATE inkcre.sources SET state=$2 WHERE id=$1")
        .bind(id)
        .bind(serde_json::to_value(state).map_err(|_| fail("Telegram state encoding failed"))?)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
async fn pin(core: &Core, source: &Source, bot: i64) -> Result<State> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    let (mut state, _, _) = locked(&mut tx, source).await?;
    if state.bot_id.is_some_and(|id| id != bot) {
        return Err(fail("Telegram Source token resolves to another bot"));
    }
    state.bot_id = Some(bot);
    save_state(&mut tx, source.id, &state).await?;
    tx.commit().await.map_err(db)?;
    Ok(state)
}
struct Outcome {
    duplicate: bool,
    saved: bool,
    attachment: Option<i32>,
    state: State,
}
async fn block(tx: &mut Transaction<'_, Postgres>, resolver: &str, content: &str) -> Result<i32> {
    sqlx::query_scalar("INSERT INTO inkcre.blocks(resolver,content) VALUES($1,$2) RETURNING id")
        .bind(resolver)
        .bind(content)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)
}
async fn relation(
    tx: &mut Transaction<'_, Postgres>,
    from: i32,
    to: i32,
    content: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3)")
        .bind(from)
        .bind(to)
        .bind(content)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
async fn persist(
    core: &Core,
    source: &Source,
    id: i64,
    text: Option<(String, String)>,
    attachment: Option<Attachment>,
    caption: Option<(String, String)>,
) -> Result<Outcome> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    let (mut state, anchor, nickname) = locked(&mut tx, source).await?;
    if state.last_update_id.is_some_and(|last| last >= id) {
        return Ok(Outcome {
            duplicate: true,
            saved: false,
            attachment: None,
            state,
        });
    }
    let saved = text.is_some() || attachment.is_some();
    if let Some((resolver, text)) = text {
        block(&mut tx, &resolver, &text).await?;
    }
    let mut attachment_id = None;
    if let Some(metadata) = attachment {
        let content = json!({"id":source.id,"type":source.type_,"nickname":nickname}).to_string();
        let anchor = if let Some(anchor) = anchor {
            let changed=sqlx::query("UPDATE inkcre.blocks SET resolver='core.source.v1',storage=NULL,content=$2 WHERE id=$1").bind(anchor).bind(&content).execute(&mut *tx).await.map_err(db)?;
            if changed.rows_affected() != 1 {
                return Err(fail("Telegram Source anchor missing"));
            }
            anchor
        } else {
            let anchor = block(&mut tx, "core.source.v1", &content).await?;
            sqlx::query("UPDATE inkcre.sources SET block=$2 WHERE id=$1")
                .bind(source.id)
                .bind(anchor)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            anchor
        };
        let child = block(
            &mut tx,
            ATTACHMENT,
            &serde_json::to_string(&metadata)
                .map_err(|_| fail("Telegram attachment encoding failed"))?,
        )
        .await?;
        relation(&mut tx, anchor, child, "collects").await?;
        attachment_id = Some(child);
        if let Some((resolver, caption)) = caption {
            let caption = block(&mut tx, &resolver, &caption).await?;
            relation(&mut tx, child, caption, "caption").await?;
        }
    }
    state.last_update_id = Some(id);
    save_state(&mut tx, source.id, &state).await?;
    tx.commit().await.map_err(db)?;
    Ok(Outcome {
        duplicate: false,
        saved,
        attachment: attachment_id,
        state,
    })
}
fn semantic(message: &Value, caption: bool) -> Result<Option<(String, String)>> {
    let key = if caption { "caption" } else { "text" };
    let Some(text) = message[key].as_str() else {
        return Ok(None);
    };
    let key = if caption {
        "caption_entities"
    } else {
        "entities"
    };
    if message[key].is_null() {
        return Ok(Some(("core.text.v1".into(), text.into())));
    }
    let entities = message[key]
        .as_array()
        .ok_or_else(|| fail("Invalid Telegram entities"))?;
    if entities.is_empty() {
        Ok(Some(("core.text.v1".into(), text.into())))
    } else {
        Ok(Some((
            "core.html.v1".into(),
            markup::render(text, entities)?,
        )))
    }
}
fn attachment(message: &Value) -> Result<Option<Attachment>> {
    let picked = if let Some(photos) = message["photo"].as_array().filter(|p| !p.is_empty()) {
        photos
            .iter()
            .max_by_key(|v| {
                v["width"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_mul(v["height"].as_u64().unwrap_or(0))
            })
            .map(|v| ("photo", v))
    } else {
        [
            "animation",
            "audio",
            "document",
            "sticker",
            "video",
            "video_note",
            "voice",
        ]
        .into_iter()
        .find_map(|key| message.get(key).filter(|v| v.is_object()).map(|v| (key, v)))
    };
    let Some((kind, value)) = picked else {
        return Ok(None);
    };
    let mut object = value
        .as_object()
        .cloned()
        .ok_or_else(|| fail("Invalid Telegram file"))?;
    object.insert("kind".into(), json!(kind));
    object.insert("filename".into(), value["file_name"].clone());
    object.retain(|key, _| {
        [
            "kind",
            "file_id",
            "file_unique_id",
            "filename",
            "mime_type",
            "file_size",
            "width",
            "height",
            "duration",
            "title",
            "performer",
            "emoji",
        ]
        .contains(&key.as_str())
    });
    let metadata: Attachment = serde_json::from_value(Value::Object(object))
        .map_err(|_| fail("Invalid Telegram file metadata"))?;
    Ok(Some(metadata.validate()?))
}
pub async fn run(core: &Core, source: Source, job: i64) -> Result<Value> {
    let setup = Config::parse(source.config.clone())?;
    State::parse(source.state.clone())?;
    let mut report = Report::default();
    report.save(core, job).await?;
    let bot = Bot::new(setup.bot_token)?;
    let mut state = pin(core, &source, bot.identity().await?).await?;
    loop {
        let before = state.last_update_id;
        let offset = state
            .last_update_id
            .map(|id| {
                id.checked_add(1)
                    .ok_or_else(|| fail("Telegram cursor overflow"))
            })
            .transpose()?;
        let updates = bot
            .call(
                "getUpdates",
                json!({"offset":offset,"limit":100,"timeout":0,"allowed_updates":["message"]}),
            )
            .await?;
        let mut updates = updates
            .as_array()
            .cloned()
            .filter(|u| u.len() <= 100)
            .ok_or_else(|| fail("Invalid Telegram updates"))?;
        for value in &updates {
            if value["update_id"].as_i64().is_none_or(|n| n < 0) {
                return Err(fail("Invalid Telegram update ID"));
            }
        }
        updates.sort_by_key(|v| v["update_id"].as_i64());
        for update in &updates {
            let id = update["update_id"]
                .as_i64()
                .ok_or_else(|| fail("Telegram update ID missing"))?;
            let message = &update["message"];
            if message.is_null()
                || message["chat"]["type"] != "private"
                || message["from"]["id"].as_i64() != Some(setup.bound_user_id)
            {
                let outcome = persist(core, &source, id, None, None, None).await?;
                if !outcome.duplicate {
                    report.counts[if message.is_null() { 2 } else { 3 }] += 1
                }
                state = outcome.state;
                report.save(core, job).await?;
                continue;
            }
            let result: Result<State> = async {
                if message["text"] == "/start" {
                    let outcome = persist(core, &source, id, None, None, None).await?;
                    if !outcome.duplicate && bot.notify(message, "start").await.is_err() {
                        report.diagnostic(
                            id,
                            "acknowledgement",
                            "Telegram completion notification failed",
                        )
                    }
                    return Ok(outcome.state);
                }
                let metadata = attachment(message)?;
                let text = if metadata.is_none() {
                    semantic(message, false)?
                } else {
                    None
                };
                let caption = if metadata.is_some() {
                    semantic(message, true)?
                } else {
                    None
                };
                let outcome = persist(core, &source, id, text, metadata, caption).await?;
                let notification = if outcome.duplicate {
                    report.counts[4] += 1;
                    None
                } else if !outcome.saved {
                    report.counts[2] += 1;
                    Some("unsupported")
                } else if setup.download_attachments && outcome.attachment.is_some() {
                    if materialize::run(
                        core,
                        outcome
                            .attachment
                            .ok_or_else(|| fail("Telegram attachment disappeared"))?,
                    )
                    .await
                    .is_err()
                    {
                        report.counts[1] += 1;
                        report.diagnostic(
                            id,
                            "materialization",
                            "Attachment metadata saved; bytes unavailable",
                        );
                        Some("partial")
                    } else {
                        report.counts[0] += 1;
                        Some("saved")
                    }
                } else {
                    report.counts[0] += 1;
                    Some("saved")
                };
                if let Some(notification) = notification
                    && bot.notify(message, notification).await.is_err()
                {
                    report.diagnostic(
                        id,
                        "acknowledgement",
                        "Telegram completion notification failed",
                    )
                }
                Ok(outcome.state)
            }
            .await;
            match result {
                Ok(next) => state = next,
                Err(error) => {
                    report.counts[5] += 1;
                    report.diagnostic(id, "primary", "Telegram message persistence failed");
                    if bot.notify(message, "failed").await.is_err() {
                        report.diagnostic(
                            id,
                            "failure_notification",
                            "Telegram failure notification unavailable",
                        )
                    }
                    report.save(core, job).await?;
                    return Err(error);
                }
            }
            report.save(core, job).await?;
        }
        if updates.len() < 100 {
            break;
        }
        if before == state.last_update_id {
            return Err(fail("Telegram update cursor did not advance"));
        }
    }
    Ok(report.value())
}
