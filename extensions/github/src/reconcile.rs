use crate::{Result, fail, schema::*};
use inkcre_core::{Block, Core};
use serde_json::{Value, json};
use sqlx::PgConnection;
use std::collections::{BTreeMap, BTreeSet};
type Edge = (i32, i32, String);
#[derive(Default)]
struct Counts {
    created: usize,
    updated: usize,
    added: usize,
    removed: usize,
}
pub async fn run(
    core: &Core,
    source_id: i32,
    snapshot: Snapshot,
    observed_config: Value,
) -> Result<Value> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    // Node identities are global across GitHub sources in the shared graph.
    sqlx::query("SELECT pg_advisory_xact_lock(17856291)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let (type_id, nickname, state, anchor, config): (
        String,
        Option<String>,
        Value,
        Option<i32>,
        Value,
    ) = sqlx::query_as(
        "SELECT type,nickname,state,block,config FROM inkcre.sources WHERE id=$1 FOR UPDATE",
    )
    .bind(source_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(|| fail("Source no longer exists"))?;
    if type_id != SOURCE || config != observed_config {
        return Err(fail("Source configuration changed during snapshot"));
    }
    if state
        .as_object()
        .is_none_or(|o| o.keys().any(|k| k != "account_node_id"))
    {
        return Err(fail("Invalid GitHub source state"));
    }
    if state
        .get("account_node_id")
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(snapshot.account.node_id.as_str()))
    {
        return Err(fail("GitHub Source account changed; create another Source"));
    }
    let anchor_content = json!({"id":source_id,"type":type_id,"nickname":nickname}).to_string();
    let source_anchor = if let Some(id) = anchor {
        let changed = sqlx::query("UPDATE inkcre.blocks SET resolver='core.source.v1',content=$2,storage=NULL WHERE id=$1")
            .bind(id).bind(&anchor_content).execute(&mut *tx).await.map_err(db)?;
        if changed.rows_affected() != 1 {
            return Err(fail("Source anchor missing"));
        }
        id
    } else {
        let id: i32 = sqlx::query_scalar(
            "INSERT INTO inkcre.blocks(resolver,content) VALUES('core.source.v1',$1) RETURNING id",
        )
        .bind(&anchor_content)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE inkcre.sources SET block=$2 WHERE id=$1")
            .bind(source_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        id
    };
    let all:Vec<Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks WHERE resolver=ANY($1)").bind(vec![ACCOUNT,REPOSITORY,LIST]).fetch_all(&mut *tx).await.map_err(db)?;
    let mut existing = BTreeMap::new();
    let mut kinds = BTreeMap::new();
    for block in all {
        let content = crate::decode(&block.resolver, &block.content)?;
        let node = content["node_id"]
            .as_str()
            .ok_or_else(|| fail("GitHub node ID missing"))?
            .to_owned();
        kinds.insert(block.id, block.resolver.clone());
        if existing
            .insert((block.resolver.clone(), node), block)
            .is_some()
        {
            return Err(fail("Ambiguous GitHub node identity"));
        }
    }
    let mut counts = Counts::default();
    let mut accounts = BTreeMap::new();
    let mut repositories = BTreeMap::new();
    let mut lists = BTreeMap::new();
    for fact in &snapshot.repositories {
        accounts.insert(
            fact.owner.node_id.clone(),
            serde_json::to_value(&fact.owner).map_err(|_| fail("Account encoding failed"))?,
        );
        repositories.insert(
            fact.repository.node_id.clone(),
            serde_json::to_value(&fact.repository)
                .map_err(|_| fail("Repository encoding failed"))?,
        );
    }
    accounts.insert(
        snapshot.account.node_id.clone(),
        serde_json::to_value(&snapshot.account).map_err(|_| fail("Account encoding failed"))?,
    );
    for fact in &snapshot.lists {
        lists.insert(
            fact.list.node_id.clone(),
            serde_json::to_value(&fact.list).map_err(|_| fail("List encoding failed"))?,
        );
    }
    let repositories = upsert(
        &mut tx,
        REPOSITORY,
        repositories,
        &existing,
        &mut kinds,
        &mut counts,
    )
    .await?;
    let accounts = upsert(
        &mut tx,
        ACCOUNT,
        accounts,
        &existing,
        &mut kinds,
        &mut counts,
    )
    .await?;
    let lists = upsert(&mut tx, LIST, lists, &existing, &mut kinds, &mut counts).await?;
    let account = accounts[&snapshot.account.node_id];
    let prior_lists: Vec<i32> = sqlx::query_scalar("SELECT r.to_ FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.to_ WHERE r.from_=$1 AND r.content='owns' AND b.resolver=$2")
        .bind(account).bind(LIST).fetch_all(&mut *tx).await.map_err(db)?;
    let mut managed_lists: BTreeSet<i32> = lists.values().copied().collect();
    managed_lists.extend(prior_lists);
    let mut roots = vec![source_anchor, account];
    roots.extend(managed_lists.iter().copied());
    let repo_ids: Vec<i32> = repositories.values().copied().collect();
    let relations: Vec<(i32, Option<i32>, Option<i32>, String)> = sqlx::query_as("SELECT id,from_,to_,content FROM inkcre.relations WHERE (from_=ANY($1) OR to_=ANY($2)) AND content=ANY($3) ORDER BY id")
        .bind(roots).bind(&repo_ids).bind(vec!["collects", "stars", "owns", "contains"])
        .fetch_all(&mut *tx).await.map_err(db)?;
    let mut desired = BTreeSet::from([(source_anchor, account, "collects".into())]);
    for id in &snapshot.stars {
        desired.insert((
            account,
            *repositories
                .get(id)
                .ok_or_else(|| fail("Star repository unresolved"))?,
            "stars".into(),
        ));
    }
    for id in lists.values() {
        desired.insert((account, *id, "owns".into()));
    }
    for fact in &snapshot.repositories {
        desired.insert((
            accounts[&fact.owner.node_id],
            repositories[&fact.repository.node_id],
            "owns".into(),
        ));
    }
    for fact in &snapshot.lists {
        for id in &fact.repositories {
            desired.insert((
                lists[&fact.list.node_id],
                *repositories
                    .get(id)
                    .ok_or_else(|| fail("List repository unresolved"))?,
                "contains".into(),
            ));
        }
    }
    let mut retained = BTreeSet::<Edge>::new();
    for (id, from, to, kind) in relations {
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let managed = match kind.as_str() {
            "collects" => from == source_anchor && kinds.get(&to).is_some_and(|r| r == ACCOUNT),
            "stars" => from == account && kinds.get(&to).is_some_and(|r| r == REPOSITORY),
            "owns" => {
                (from == account && kinds.get(&to).is_some_and(|r| r == LIST))
                    || (repositories.values().any(|r| *r == to)
                        && kinds.get(&from).is_some_and(|r| r == ACCOUNT))
            }
            "contains" => {
                managed_lists.contains(&from) && kinds.get(&to).is_some_and(|r| r == REPOSITORY)
            }
            _ => false,
        };
        if !managed {
            continue;
        }
        let key = (from, to, kind);
        if desired.contains(&key) && retained.insert(key) {
            continue;
        }
        sqlx::query("DELETE FROM inkcre.relations WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        counts.removed += 1;
    }
    for (from, to, kind) in desired.difference(&retained) {
        sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3)")
            .bind(from)
            .bind(to)
            .bind(kind)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        counts.added += 1;
    }
    sqlx::query("UPDATE inkcre.sources SET state=$2 WHERE id=$1")
        .bind(source_id)
        .bind(json!({"account_node_id":snapshot.account.node_id}))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(
        json!({"account":snapshot.account.login,"stars":snapshot.stars.len(),"lists":snapshot.lists.len(),"memberships":snapshot.lists.iter().map(|l|l.repositories.len()).sum::<usize>(),"blocks_created":counts.created,"blocks_updated":counts.updated,"relations_created":counts.added,"relations_deleted":counts.removed}),
    )
}
async fn upsert(
    c: &mut PgConnection,
    kind: &str,
    values: BTreeMap<String, Value>,
    existing: &BTreeMap<(String, String), Block>,
    kinds: &mut BTreeMap<i32, String>,
    counts: &mut Counts,
) -> Result<BTreeMap<String, i32>> {
    let mut result = BTreeMap::new();
    for (node, value) in values {
        let content = value.to_string();
        let id = if let Some(block) = existing.get(&(kind.into(), node.clone())) {
            if crate::decode(kind, &block.content)? != value || block.storage.is_some() {
                sqlx::query("UPDATE inkcre.blocks SET content=$2,storage=NULL WHERE id=$1")
                    .bind(block.id)
                    .bind(&content)
                    .execute(&mut *c)
                    .await
                    .map_err(db)?;
                counts.updated += 1;
            }
            block.id
        } else {
            counts.created += 1;
            sqlx::query_scalar(
                "INSERT INTO inkcre.blocks(resolver,content) VALUES($1,$2) RETURNING id",
            )
            .bind(kind)
            .bind(content)
            .fetch_one(&mut *c)
            .await
            .map_err(db)?
        };
        kinds.insert(id, kind.into());
        result.insert(node, id);
    }
    Ok(result)
}
fn db(_: sqlx::Error) -> inkcre_extension_sdk::RpcError {
    fail("GitHub graph operation failed")
}
