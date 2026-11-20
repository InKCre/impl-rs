use super::*;
use chrono::{DateTime, Utc};
use sqlx::FromRow;
const KIND: &str = "inkcre.record.kind";
const EVENT: &str = "inkcre.event.name";
const ID: &str = "inkcre.record.id";
pub(super) async fn write(core: &Core, records: Vec<Record>) -> Result<()> {
    let mut tx = core.pool().begin().await.map_err(|_| Error::Unavailable)?;
    for record in records {
        let mut attrs = record.attributes;
        attrs.insert(KIND.into(), json!(record.kind));
        attrs.insert(EVENT.into(), json!(record.event_name));
        attrs.insert(ID.into(), json!(record.id));
        sqlx::query("INSERT INTO inkcre.logs(timestamp,severity_number,severity_text,body,trace_id,span_id,attributes) VALUES($1,$2,$3,$4,$5,$6,$7)")
   .bind(record.timestamp).bind(record.severity_number as i16).bind(record.severity_text).bind(record.body).bind(record.trace_id).bind(record.span_id).bind(Value::Object(attrs)).execute(&mut *tx).await.map_err(|_|Error::Unavailable)?;
    }
    tx.commit().await.map_err(|_| Error::Unavailable)?;
    Ok(())
}
#[derive(FromRow)]
struct Row {
    id: i64,
    timestamp: DateTime<Utc>,
    severity_number: i16,
    severity_text: String,
    body: String,
    trace_id: Option<String>,
    span_id: Option<String>,
    attributes: Value,
}
pub(super) async fn query(core: &Core, query: Query) -> Result<Page> {
    let escaped = query.text.as_ref().filter(|v| !v.is_empty()).map(|s| {
        format!(
            "%{}%",
            s.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    let mut rows:Vec<Row>=sqlx::query_as("SELECT id,timestamp,severity_number,severity_text,body,trace_id,span_id,attributes FROM inkcre.logs WHERE timestamp >= $1 AND timestamp < $2 AND ($3::text IS NULL OR COALESCE(attributes->>'inkcre.record.kind','log')=$3) AND ($4::smallint IS NULL OR severity_number >= $4) AND ($5::text IS NULL OR attributes->>'inkcre.job.id'=$5 OR attributes->>'job_id'=$5 OR trace_id='job.'||$5) AND ($6::text IS NULL OR trace_id=$6) AND ($7::text IS NULL OR attributes->>'inkcre.peer.id'=$7) AND ($8::text IS NULL OR attributes->>'inkcre.operation'=$8) AND ($9::text IS NULL OR body ILIKE $9 ESCAPE '\\') ORDER BY timestamp DESC,id DESC LIMIT $10")
  .bind(query.since).bind(query.until).bind(query.kind).bind(query.severity_min.map(|n|n as i16)).bind(query.job_id.map(|n|n.to_string())).bind(query.trace_id).bind(query.peer_id.map(|id|id.to_string())).bind(query.operation).bind(escaped).bind(query.limit+1).fetch_all(core.pool()).await.map_err(|_|Error::Unavailable)?;
    let truncated = rows.len() as i64 > query.limit;
    rows.truncate(query.limit as usize);
    let records = rows
        .into_iter()
        .map(|row| {
            let mut attrs = row.attributes.as_object().cloned().unwrap_or_default();
            let id = attrs
                .remove(ID)
                .and_then(|v| v.as_str().map(str::to_owned))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| format!("pg.{}", row.id));
            let kind = attrs
                .remove(KIND)
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "log".into());
            let event_name = attrs
                .remove(EVENT)
                .and_then(|v| v.as_str().map(str::to_owned));
            Record {
                id,
                timestamp: row.timestamp,
                kind,
                event_name,
                severity_number: row.severity_number as i32,
                severity_text: row.severity_text,
                body: row.body,
                trace_id: row.trace_id,
                span_id: row.span_id,
                attributes: attrs,
            }
        })
        .collect();
    Ok(Page {
        records,
        truncated,
        since: query.since,
        until: query.until,
        backend: "postgresql".into(),
    })
}
