use crate::{Result, fail, materialize, newsletter, schema::*};
use inkcre_core::{
    Block, Core, HydratedContent,
    knowledge::{ReadOptions, ReadValue, format_label, html_text},
};
use inkcre_extension_sdk::{Content, ResolverOperation, ResolverRequest, TextContext};
use serde_json::{Value, json};
fn error<E>(_: E) -> inkcre_extension_sdk::RpcError {
    fail("Mail Resolver content unavailable")
}
fn scalar(kind: &str, raw: &str) -> Result<Value> {
    macro_rules! decode {
        ($ty:ty) => {
            serde_json::to_value(serde_json::from_str::<$ty>(raw).map_err(error)?).map_err(error)
        };
    }
    match kind {
        EMAIL => decode!(Email),
        MAILBOX => decode!(Mailbox),
        ADDRESS => decode!(Address),
        FLAG => decode!(Flag),
        PART => decode!(MimePart),
        LIST => decode!(MailingList),
        _ => Err(fail("Unknown Mail Resolver")),
    }
}
async fn raw(core: &Core, block: &Block) -> Result<String> {
    match core
        .knowledge()
        .actual_content(block)
        .await
        .map_err(error)?
    {
        HydratedContent::Text(text) => Ok(text),
        HydratedContent::Bytes(bytes) => {
            inkcre_core::knowledge::decode_unicode_bytes(&bytes).map_err(error)
        }
    }
}
fn object<const N: usize>(fields: [(&str, ReadValue); N]) -> ReadValue {
    ReadValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
struct Budget {
    remaining: usize,
    active: std::collections::BTreeSet<i32>,
}
impl Budget {
    fn root(id: i32) -> Self {
        Self {
            remaining: 512,
            active: std::collections::BTreeSet::from([id]),
        }
    }
}
async fn part_value(
    core: &Core,
    id: i32,
    root: Value,
    materialize_missing: bool,
) -> Result<ReadValue> {
    let child = materialize::child(core, id, materialize_missing).await?;
    let content = if let Some(child) = child {
        let solved = core
            .knowledge()
            .solved_value(
                child.id,
                ReadOptions {
                    refresh: false,
                    materialize_missing: false,
                },
            )
            .await
            .map_err(error)?;
        object([
            ("block", ReadValue::Json(json!(child))),
            ("solved_content", solved),
        ])
    } else {
        ReadValue::Json(Value::Null)
    };
    Ok(object([
        ("root", ReadValue::Json(root)),
        ("content", content),
    ]))
}
async fn shallow(core: &Core, block: Block, budget: &mut Budget) -> Result<ReadValue> {
    if budget.remaining == 0 {
        return Err(fail("Mail graph projection exceeds 512 nodes"));
    }
    budget.remaining -= 1;
    let source = raw(core, &block).await?;
    let value = if block.resolver == NEWS_HTML {
        ReadValue::Json(json!(newsletter::html(&source)))
    } else if block.resolver == NEWS_TEXT {
        ReadValue::Json(json!(newsletter::text(&source)))
    } else if ["core.text.v1", "core.html.v1"].contains(&block.resolver.as_str()) {
        ReadValue::Json(json!(source))
    } else if block.resolver == PART {
        part_value(core, block.id, scalar(PART, &source)?, false).await?
    } else if block.resolver == EMAIL {
        let root = scalar(EMAIL, &source)?;
        if budget.active.contains(&block.id) {
            ReadValue::Json(root)
        } else {
            if budget.active.len() >= 16 {
                return Err(fail("Mail reply graph projection exceeds 16 levels"));
            }
            budget.active.insert(block.id);
            let result = Box::pin(email(core, block.id, root, budget)).await;
            budget.active.remove(&block.id);
            result?
        }
    } else {
        ReadValue::Json(scalar(&block.resolver, &source)?)
    };
    Ok(object([
        ("block", ReadValue::Json(json!(block))),
        ("solved_content", value),
    ]))
}
async fn email(core: &Core, id: i32, root: Value, budget: &mut Budget) -> Result<ReadValue> {
    let edges = core.incident_relations(id).await.map_err(error)?;
    let mut bodies = vec![];
    let mut lists = vec![];
    let mut parts = vec![];
    let mut participants = vec![];
    let mut mailboxes = vec![];
    let mut flags = vec![];
    let mut parents = vec![];
    let mut references = vec![];
    for edge in edges {
        let outgoing = edge.from_ == Some(id);
        let Some(target) = (if outgoing { edge.to_ } else { edge.from_ }) else {
            continue;
        };
        let Some(block) = core.block_get(target).await.map_err(error)? else {
            continue;
        };
        let value: Value = serde_json::from_str(&edge.content).unwrap_or(Value::Null);
        if outgoing {
            match value["role"].as_str() {
                Some("body")
                    if ["core.text.v1", "core.html.v1", NEWS_HTML, NEWS_TEXT]
                        .contains(&block.resolver.as_str()) =>
                {
                    let mut body = shallow(core, block, budget).await?;
                    if let ReadValue::Object(fields) = &mut body {
                        fields.insert("relation".into(), ReadValue::Json(value));
                    }
                    bodies.push(body);
                }
                Some("attachment" | "inline") if block.resolver == PART => {
                    parts.push(shallow(core, block, budget).await?)
                }
                Some("from" | "sender" | "reply_to" | "to" | "cc" | "bcc")
                    if block.resolver == ADDRESS =>
                {
                    participants.push(object([
                        ("relation", ReadValue::Json(value)),
                        ("address", shallow(core, block, budget).await?),
                    ]))
                }
                _ => {
                    if block.resolver == EMAIL {
                        if edge.content.starts_with("parent:") {
                            parents.push(shallow(core, block, budget).await?)
                        } else if edge.content.starts_with("reference:") {
                            references.push(shallow(core, block, budget).await?)
                        }
                    }
                }
            }
        } else if edge.content == "publishes" && block.resolver == LIST {
            lists.push(shallow(core, block, budget).await?)
        } else if value["type"] == "contains" && block.resolver == MAILBOX {
            mailboxes.push(object([
                ("relation", ReadValue::Json(value)),
                ("mailbox", shallow(core, block, budget).await?),
            ]))
        } else if edge.content == "tags" && block.resolver == FLAG {
            flags.push(object([("flag", shallow(core, block, budget).await?)]))
        }
    }
    Ok(object([
        ("root", ReadValue::Json(root)),
        ("bodies", ReadValue::Array(bodies)),
        ("mailing_lists", ReadValue::Array(lists)),
        ("mime_parts", ReadValue::Array(parts)),
        ("participants", ReadValue::Array(participants)),
        ("mailboxes", ReadValue::Array(mailboxes)),
        ("flags", ReadValue::Array(flags)),
        ("parents", ReadValue::Array(parents)),
        ("references", ReadValue::Array(references)),
    ]))
}
async fn email_value(core: &Core, id: i32, root: Value) -> Result<ReadValue> {
    email(core, id, root, &mut Budget::root(id)).await
}
pub async fn read_value(core: &Core, request: ResolverRequest) -> Result<ReadValue> {
    if let ResolverOperation::Solve {
        materialize_missing,
        ..
    } = request.operation
        && [EMAIL, PART].contains(&request.resolver.as_str())
    {
        let source = match &request.input.content {
            Content::Text(text) => text.clone(),
            Content::Bytes(bytes) => {
                inkcre_core::knowledge::decode_unicode_bytes(bytes).map_err(error)?
            }
        };
        let root = scalar(&request.resolver, &source)?;
        return if request.resolver == EMAIL {
            email_value(core, request.input.block.id, root).await
        } else {
            part_value(core, request.input.block.id, root, materialize_missing).await
        };
    }
    read_json(core, request).await.map(ReadValue::Json)
}
pub async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    read_value(core, request).await.map(ReadValue::into_json)
}
fn reading(bodies: &[Value]) -> Vec<&Value> {
    let mut sorted: Vec<_> = bodies.iter().collect();
    let path = |value: &Value| {
        value["relation"]["part_id"]
            .as_str()
            .unwrap_or("")
            .split('.')
            .filter_map(|v| v.parse::<usize>().ok())
            .collect::<Vec<_>>()
    };
    sorted.sort_by_key(|v| path(v));
    let mut preferred = std::collections::BTreeMap::new();
    for body in &sorted {
        for group in body["relation"]["alternative_groups"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let depth = if group == "root" {
                0
            } else {
                group.split('.').count()
            };
            let branch = path(body).get(depth).copied().unwrap_or(0);
            let html = ["core.html.v1", NEWS_HTML]
                .iter()
                .any(|r| body["block"]["resolver"] == *r);
            if html || !preferred.contains_key(group) {
                preferred.insert(group, branch);
            }
        }
    }
    sorted
        .into_iter()
        .filter(|body| {
            body["relation"]["alternative_groups"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .all(|group| {
                    let depth = if group == "root" {
                        0
                    } else {
                        group.split('.').count()
                    };
                    preferred.get(group).copied() == path(body).get(depth).copied()
                })
        })
        .collect()
}
async fn read_json(core: &Core, request: ResolverRequest) -> Result<Value> {
    let source = match &request.input.content {
        Content::Text(text) => text.clone(),
        Content::Bytes(bytes) => {
            inkcre_core::knowledge::decode_unicode_bytes(bytes).map_err(error)?
        }
    };
    let kind = request.resolver.as_str();
    if [NEWS_HTML, NEWS_TEXT].contains(&kind) {
        let cleaned = if kind == NEWS_HTML {
            newsletter::html(&source)
        } else {
            newsletter::text(&source)
        };
        return match request.operation {
            ResolverOperation::Solve { .. } => Ok(json!(cleaned)),
            ResolverOperation::Text { context, .. } => Ok(json!(if kind == NEWS_HTML {
                if matches!(context, TextContext::Lexical) {
                    newsletter::visible(&cleaned)
                } else {
                    html_text(&cleaned).map_err(error)?
                }
            } else {
                cleaned
            })),
            ResolverOperation::Label { .. } => Ok(json!(if kind == NEWS_HTML {
                let document = scraper::Html::parse_document(&cleaned);
                let titles = scraper::Selector::parse("title").expect("static selector");
                let headings =
                    scraper::Selector::parse("h1,h2,h3,h4,h5,h6").expect("static selector");
                let identifier = document
                    .select(&titles)
                    .chain(document.select(&headings))
                    .flat_map(|element| element.text())
                    .find(|text| !text.trim().is_empty());
                format_label("html", identifier, false)
            } else {
                format_label("text", Some(&cleaned), true)
            })),
            _ => Err(fail("Unknown Mail read method")),
        };
    }
    let root = scalar(kind, &source)?;
    match request.operation {
        ResolverOperation::Solve {
            materialize_missing,
            ..
        } => match kind {
            EMAIL => email_value(core, request.input.block.id, root)
                .await
                .map(ReadValue::into_json),
            LIST => {
                let identity = root["list_id"].as_str().unwrap_or("");
                let rows: Vec<(i32, String)> = sqlx::query_as(
                    "SELECT id,content FROM inkcre.blocks WHERE resolver=$1 ORDER BY id",
                )
                .bind(LIST)
                .fetch_all(core.pool())
                .await
                .map_err(error)?;
                let mut ids = std::collections::BTreeSet::new();
                for (id, content) in rows {
                    if serde_json::from_str::<MailingList>(&content)
                        .is_ok_and(|v| v.list_id == identity)
                    {
                        for edge in core.incident_relations(id).await.map_err(error)? {
                            if edge.from_ == Some(id)
                                && edge.content == "publishes"
                                && let Some(target) = edge.to_
                            {
                                ids.insert(target);
                            }
                        }
                    }
                }
                let mut emails = vec![];
                for id in ids {
                    if let Some(block) = core.block_get(id).await.map_err(error)?
                        && block.resolver == EMAIL
                    {
                        emails.push(json!({"solved_content":scalar(EMAIL,&raw(core,&block).await?)?,"block":block}))
                    }
                }
                Ok(json!({"root":root,"emails":emails}))
            }
            PART => part_value(core, request.input.block.id, root, materialize_missing)
                .await
                .map(ReadValue::into_json),
            _ => Ok(root),
        },
        ResolverOperation::Text { context, .. } => {
            let text = match kind {
                EMAIL => {
                    let mut parts = vec![];
                    if let Some(subject) = root["subject"].as_str() {
                        parts.push(subject.to_owned())
                    }
                    if !matches!(context, TextContext::Lexical) {
                        let solved = email_value(core, request.input.block.id, root.clone())
                            .await?
                            .into_json();
                        for body in reading(
                            solved["bodies"]
                                .as_array()
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                        ) {
                            let text = body["solved_content"].as_str().unwrap_or("");
                            let text = if ["core.html.v1", NEWS_HTML]
                                .iter()
                                .any(|r| body["block"]["resolver"] == *r)
                            {
                                html_text(text).map_err(error)?
                            } else {
                                text.into()
                            };
                            if !text.is_empty() {
                                parts.push(text)
                            }
                        }
                    }
                    if parts.is_empty() {
                        root["message_id"].as_str().unwrap_or("email").into()
                    } else {
                        parts.join("\n\n")
                    }
                }
                MAILBOX => {
                    let name = root["name"].as_str().unwrap_or("");
                    let roles = root["special_uses"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    if roles.is_empty() {
                        name.into()
                    } else {
                        format!("{name}\nSpecial uses: {roles}")
                    }
                }
                ADDRESS => root["address"].as_str().unwrap_or("").into(),
                FLAG => {
                    let name = root["name"].as_str().unwrap_or("");
                    root["description"].as_str().map_or_else(
                        || name.to_owned(),
                        |description| format!("{name}: {description}"),
                    )
                }
                PART => [
                    root["filename"].as_str(),
                    root["description"].as_str(),
                    root["media_type"].as_str(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n"),
                LIST => [root["name"].as_str(), root["list_id"].as_str()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return Err(fail("Unknown Mail Resolver")),
            };
            Ok(json!(text))
        }
        ResolverOperation::Label { .. } => {
            let (prefix, keys) = match kind {
                EMAIL => ("email", vec!["subject", "message_id"]),
                MAILBOX => ("mailbox", vec!["name"]),
                ADDRESS => ("email address", vec!["address"]),
                FLAG => ("mail flag", vec!["name"]),
                PART => (
                    "mail MIME part",
                    vec!["filename", "description", "media_type"],
                ),
                _ => ("mailing list", vec!["name", "list_id"]),
            };
            Ok(json!(format_label(
                prefix,
                keys.iter().find_map(|key| root[*key].as_str()),
                false
            )))
        }
        _ => Err(fail("Unknown Mail read method")),
    }
}
