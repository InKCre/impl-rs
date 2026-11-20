use crate::{Result, adapter::Adapter, fail, reconcile, schema::*};
use inkcre_core::Core;
use serde_json::{Value, json};
pub async fn run(
    core: &Core,
    id: i32,
    command: Value,
    backfill: bool,
    defaults: &Exclusions,
) -> Result<Value> {
    let interval = if backfill {
        let config: Backfill =
            serde_json::from_value(command).map_err(|_| fail("Invalid Mail backfill interval"))?;
        config.validate()?;
        Some(config)
    } else {
        let _: Collect = serde_json::from_value(command)
            .map_err(|_| fail("Mail collect config must be empty"))?;
        None
    };
    let (source, config) = reconcile::effective(core, id, defaults).await?;
    let setup = config.clone();
    let (mut adapter, mailboxes) = tokio::task::spawn_blocking(move || {
        let mut adapter = Adapter::connect(&setup.parameters)?;
        let mailboxes = adapter.discover(
            setup
                .excluded_mailboxes
                .as_ref()
                .expect("materialized exclusions"),
        )?;
        Ok::<_, inkcre_extension_sdk::RpcError>((adapter, mailboxes))
    })
    .await
    .map_err(|_| fail("Mail adapter task failed"))??;
    let checkpoints = reconcile::prepare(core, id, &config, &mailboxes).await?;
    let mut counts = json!({"messages":0,"flag_changes":0,"removals":0,"mailboxes":0});
    let mut diagnostics = vec![];
    for ((mailbox, _), (block, checkpoint)) in mailboxes.into_iter().zip(checkpoints) {
        let target = mailbox.clone();
        let previous = checkpoint.clone();
        let range = interval.clone();
        let horizon = source.created_at;
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let result = adapter.read(&target, previous.as_ref(), horizon, range.as_ref());
            (adapter, result)
        })
        .await
        .map_err(|_| fail("Mail adapter task failed"))?;
        adapter = returned;
        let batch = match result {
            Ok(batch) => batch,
            Err(error) => {
                diagnostics.push(
                    json!({"scope":"mailbox","mailbox":mailbox.name,"message":error.message}),
                );
                continue;
            }
        };
        let (flags, removed) = match reconcile::apply(
            core,
            id,
            &config,
            block,
            checkpoint.as_ref(),
            &batch,
            backfill,
        )
        .await
        {
            Ok(counts) => counts,
            Err(error) => {
                diagnostics.push(
                    json!({"scope":"mailbox","mailbox":mailbox.name,"message":error.message}),
                );
                continue;
            }
        };
        counts["messages"] =
            json!(counts["messages"].as_u64().unwrap_or(0) + batch.messages.len() as u64);
        counts["flag_changes"] = json!(counts["flag_changes"].as_u64().unwrap_or(0) + flags as u64);
        counts["removals"] = json!(counts["removals"].as_u64().unwrap_or(0) + removed as u64);
        counts["mailboxes"] = json!(counts["mailboxes"].as_u64().unwrap_or(0) + 1);
        if if backfill {
            config.backfill_mark_as_seen
        } else {
            config.ordinary_mark_as_seen
        } {
            let uids: Vec<_> = batch
                .messages
                .iter()
                .filter(|m| !m.flags.iter().any(|f| f.eq_ignore_ascii_case("\\Seen")))
                .map(|m| m.uid)
                .collect();
            if !uids.is_empty() {
                let epoch = batch.next.uid_validity;
                let (returned, result) = tokio::task::spawn_blocking(move || {
                    let result = adapter.seen(&mailbox.name, epoch, &uids);
                    (adapter, result)
                })
                .await
                .map_err(|_| fail("Mail adapter task failed"))?;
                adapter = returned;
                if result.is_ok() {
                    reconcile::record_seen(core, block, epoch, &batch.messages).await?;
                } else {
                    diagnostics
                        .push(json!({"scope":"seen","message":"Cannot mark Mail messages seen"}));
                }
            }
        }
    }
    Ok(if backfill {
        json!({"diagnostics":diagnostics,"messages":counts["messages"]})
    } else {
        json!({"diagnostics":diagnostics,"counts":counts})
    })
}
