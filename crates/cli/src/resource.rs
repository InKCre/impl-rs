use crate::{
    Action, ConfigAction, Input, Resource,
    http::{Client, Failure, InputError},
    schema, segment,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
pub(crate) async fn resource(
    client: &Client,
    group: &str,
    resource: Resource,
) -> Result<Value, Failure> {
    let path = match group {
        "job" => "jobs",
        "cron" => "crons",
        "source" => "sources",
        "peer" => "peers",
        "config" => "configs",
        "extension" => "extensions",
        "agent" => "agents",
        "sink" => "sinks",
        "ai" => "ai/models",
        "embedding-profile" => "embedding-profiles",
        "resolver" => "resolvers",
        _ => return Err(Failure::usage("未知资源")),
    };
    let base = format!("/{path}");
    let schema_route = match &resource.action {
        Action::Create { input, .. } if input.schema => Some(("POST", base.clone())),
        Action::Update { id, input, .. } if input.schema => {
            Some(("PATCH", resource_path(group, &base, id)?))
        }
        Action::Set { id, input } if input.schema => {
            Some(("PUT", resource_path(group, &base, id)?))
        }
        Action::Collect { id, input, .. } if group == "source" && input.schema => {
            return source_command_schema(client, *id, "collect_config_schema").await;
        }
        Action::Backfill { id, input, .. } if group == "source" && input.schema => {
            return source_command_schema(client, *id, "backfill_config_schema").await;
        }
        _ => None,
    };
    if let Some((method, path)) = schema_route {
        return resource_schema(client, method, &path, group, &resource.action).await;
    }
    match resource.action {
        Action::Methods {
            target,
            limit,
            cursor,
        } if group == "resolver" => {
            let route = resolver_methods(&target)?;
            client
                .send(
                    "GET",
                    &route,
                    None,
                    &[("limit", limit.map(|n| n.to_string())), ("cursor", cursor)],
                )
                .await
        }
        Action::Invoke {
            reference,
            method,
            input,
        } if group == "resolver" => {
            let (kind, id) = crate::entity(&reference)?;
            if kind != "block" {
                return Err(Failure::usage("Resolver invoke 需要Block引用"));
            }
            let route = format!("/blocks/{id}/resolver/methods");
            if input.schema {
                let catalog = client.send("GET", &route, None, &[]).await?;
                return catalog["methods"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["name"] == method))
                    .and_then(|row| row.get("input_schema"))
                    .cloned()
                    .ok_or_else(|| Failure::usage("未发现Resolver方法"));
            }
            client
                .send(
                    "POST",
                    &format!("{route}/{}", segment(&method)?),
                    Some(input.read()?),
                    &[],
                )
                .await
        }

        Action::Status { id } if group == "extension" => {
            client
                .send(
                    "POST",
                    "/extension-management/v2",
                    Some(json!({"action":"status","extension":id})),
                    &[],
                )
                .await
        }
        Action::Select {
            id,
            distribution,
            version,
            peer,
        } if group == "extension" => {
            client
                .send(
                    "POST",
                    &format!("{}/selection", resource_path(group, &base, &id)?),
                    Some(json!({"distribution":distribution,"version":version})),
                    &[("route_to_peer", peer)],
                )
                .await
        }
        Action::Unselect { id, peer } if group == "extension" => {
            client
                .send(
                    "DELETE",
                    &format!("{}/selection", resource_path(group, &base, &id)?),
                    None,
                    &[("route_to_peer", peer)],
                )
                .await
        }

        Action::Models { id, limit, cursor } if group == "ai" => {
            client
                .send(
                    "GET",
                    &format!(
                        "/ai/models{}",
                        id.map(|id| segment(&id).map(|id| format!("/{id}")))
                            .transpose()?
                            .unwrap_or_default()
                    ),
                    None,
                    &[("limit", limit.map(|v| v.to_string())), ("cursor", cursor)],
                )
                .await
        }
        Action::Tools { id, limit, cursor } if group == "agent" => {
            client
                .send(
                    "GET",
                    &format!(
                        "/agent-tools{}",
                        id.map(|id| segment(&id).map(|id| format!("/{id}")))
                            .transpose()?
                            .unwrap_or_default()
                    ),
                    None,
                    &[("limit", limit.map(|v| v.to_string())), ("cursor", cursor)],
                )
                .await
        }
        Action::Schemas { id, limit, cursor } if group == "config" => {
            client
                .send(
                    "GET",
                    &format!(
                        "/config-schemas{}",
                        id.map(|id| segment(&id).map(|id| format!("/{id}")))
                            .transpose()?
                            .unwrap_or_default()
                    ),
                    None,
                    &[("limit", limit.map(|v| v.to_string())), ("cursor", cursor)],
                )
                .await
        }
        Action::Replace {
            id,
            schema_id,
            input,
        } if group == "config" => {
            let schema_id =
                schema_id.ok_or_else(|| Failure::usage("config replace 需要 --schema-id"))?;
            if input.schema {
                return client
                    .send(
                        "GET",
                        &format!("/config-schemas/{}", segment(&schema_id)?),
                        None,
                        &[],
                    )
                    .await
                    .map(|v| v["input_schema"].clone());
            }
            client
                .send_projected(
                    "PUT",
                    &format!("/configs/{}", segment(&id)?),
                    Some(json!({"schema":schema_id,"value":input.read()?})),
                    &[],
                    InputError {
                        strip: &["value"],
                        selectors: &[("schema", "--schema-id")],
                    },
                )
                .await
        }
        Action::Config { command } if matches!(group, "sink" | "extension") => match command {
            ConfigAction::Get { id } => Ok(client
                .send("GET", &resource_path(group, &base, &id)?, None, &[])
                .await?["config"]
                .clone()),
            ConfigAction::Replace { id, input } => {
                modify_config(client, group, &base, id, input, false).await
            }
            ConfigAction::Update { id, input } => {
                modify_config(client, group, &base, id, input, true).await
            }
        },
        Action::List {
            limit,
            cursor,
            type_id,
            status,
        } => {
            let limit = if group == "job" {
                limit.or(Some(20))
            } else {
                limit
            };
            let query = [
                ("limit", limit.map(|n| n.to_string())),
                ("cursor", cursor),
                ("type", type_id),
                ("status", status),
            ];
            client.send("GET", &base, None, &query).await
        }
        Action::Get { id } => {
            client
                .send("GET", &resource_path(group, &base, &id)?, None, &[])
                .await
        }
        Action::Create {
            type_id,
            job_type,
            input,
        } => {
            if job_type.is_some() && group != "cron" {
                return Err(Failure::usage("--job-type 仅适用于Cron"));
            }
            let mut body = input.read()?;
            let mut error = InputError::default();
            if let Some(job_type) = job_type {
                body.as_object_mut()
                    .ok_or_else(|| Failure::usage("创建输入必须为对象"))?
                    .insert("job_type".into(), json!(job_type));
                error = InputError::selectors(&[("job_type", "--job-type")]);
            }
            if let Some(type_id) = type_id {
                body.as_object_mut()
                    .ok_or_else(|| Failure::usage("创建输入必须为对象"))?
                    .insert("type".into(), json!(type_id));
                error = InputError::selectors(&[("type", "--type")]);
            }
            client
                .send_projected("POST", &base, Some(body), &[], error)
                .await
        }
        Action::Update {
            id,
            job_type,
            input,
        } => {
            let mut body = input.read()?;
            let mut error = InputError::default();
            if let Some(job_type) = job_type {
                if group != "cron" {
                    return Err(Failure::usage("--job-type 仅适用于Cron"));
                }
                body.as_object_mut()
                    .ok_or_else(|| Failure::usage("更新输入必须为对象"))?
                    .insert("job_type".into(), json!(job_type));
                error = InputError::selectors(&[("job_type", "--job-type")]);
            }
            client
                .send_projected(
                    "PATCH",
                    &resource_path(group, &base, &id)?,
                    Some(body),
                    &[],
                    error,
                )
                .await
        }
        Action::Set { id, input } => {
            client
                .send(
                    "PUT",
                    &resource_path(group, &base, &id)?,
                    Some(input.read()?),
                    &[],
                )
                .await
        }
        Action::Delete { id } => {
            client
                .send("DELETE", &resource_path(group, &base, &id)?, None, &[])
                .await
        }
        Action::Types { id, limit, cursor } => {
            let types = match group {
                "job" => "job-types",
                "source" => "source-types",
                "sink" => "sink-types",
                "config" => "config-schemas",
                _ => return Err(Failure::usage("该资源没有 types 命令")),
            };
            client
                .send(
                    "GET",
                    &format!(
                        "/{types}{}",
                        id.map(|id| segment(&id).map(|id| format!("/{id}")))
                            .transpose()?
                            .unwrap_or_default()
                    ),
                    None,
                    &[("limit", limit.map(|v| v.to_string())), ("cursor", cursor)],
                )
                .await
        }
        Action::Wait { id, seconds } if group == "job" => {
            let deadline = Instant::now() + Duration::from_secs_f64(seconds);
            let mut last = Value::Null;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return observed(last);
                }
                match tokio::time::timeout(
                    remaining,
                    client.send("GET", &format!("/jobs/{id}"), None, &[]),
                )
                .await
                {
                    Ok(result) => last = result?,
                    Err(_) => return observed(last),
                }
                if !matches!(last["status"].as_str(), Some("pending" | "running")) {
                    return Ok(last);
                }
                tokio::time::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(2)),
                )
                .await;
            }
        }
        Action::Abort { id } if group == "job" => {
            client
                .send("POST", &format!("/jobs/{id}/abort"), None, &[])
                .await
        }
        Action::Collect {
            id,
            input,
            timeout_seconds,
        } if group == "source" => {
            client
                .send(
                    "POST",
                    &format!("/sources/{id}/collect"),
                    Some(input.read()?),
                    &[("timeout_seconds", timeout_seconds.map(|n| n.to_string()))],
                )
                .await
        }
        Action::Backfill {
            id,
            input,
            timeout_seconds,
        } if group == "source" => {
            client
                .send(
                    "POST",
                    &format!("/sources/{id}/backfill"),
                    Some(input.read()?),
                    &[("timeout_seconds", timeout_seconds.map(|n| n.to_string()))],
                )
                .await
        }
        Action::Run { id } if group == "cron" => {
            client
                .send("POST", &format!("/crons/{id}/run"), None, &[])
                .await
        }
        Action::Enable { id, peer } if matches!(group, "extension" | "sink") => {
            client
                .send(
                    "POST",
                    &format!("{}/enable", resource_path(group, &base, &id)?),
                    None,
                    &[("route_to_peer", peer)],
                )
                .await
        }
        Action::Disable { id, peer } if matches!(group, "extension" | "sink") => {
            client
                .send(
                    "POST",
                    &format!("{}/disable", resource_path(group, &base, &id)?),
                    None,
                    &[("route_to_peer", peer)],
                )
                .await
        }
        Action::Install { name, version } if group == "extension" => {
            client
                .send(
                    "POST",
                    &format!("{}/selection", resource_path(group, &base, &name)?),
                    Some(json!({"distribution":"rust_stdio","version":version})),
                    &[],
                )
                .await
        }
        _ => Err(Failure::usage("该操作不适用于此资源")),
    }
}

async fn catalog_schema(
    client: &Client,
    catalog: &str,
    id: &str,
    field: &str,
) -> Result<Value, Failure> {
    client
        .send("GET", &format!("/{catalog}/{}", segment(id)?), None, &[])
        .await?
        .get(field)
        .filter(|schema| schema.is_object() || schema.is_boolean())
        .cloned()
        .ok_or_else(|| Failure::usage("类型目录未提供有效的输入 schema"))
}

async fn source_command_schema(client: &Client, id: i64, field: &str) -> Result<Value, Failure> {
    let record = client
        .send("GET", &format!("/sources/{id}"), None, &[])
        .await?;
    catalog_schema(
        client,
        "source-types",
        record_identity(&record, "type")?,
        field,
    )
    .await
}

fn record_identity<'a>(record: &'a Value, field: &str) -> Result<&'a str, Failure> {
    record[field]
        .as_str()
        .ok_or_else(|| Failure::usage("记录未发布类型或 schema 标识"))
}

async fn resource_schema(
    client: &Client,
    method: &str,
    path: &str,
    group: &str,
    action: &Action,
) -> Result<Value, Failure> {
    let record;
    let (catalog, id, field, child_field, omit): (_, _, _, _, &[_]) = match (group, action) {
        (
            "job",
            Action::Create {
                type_id: Some(id), ..
            },
        ) => (
            "job-types",
            id.as_str(),
            "parameters",
            "parameters_schema",
            &["type"],
        ),
        (
            "source",
            Action::Create {
                type_id: Some(id), ..
            },
        ) => (
            "source-types",
            id.as_str(),
            "config",
            "config_schema",
            &["type"],
        ),
        (
            "sink",
            Action::Create {
                type_id: Some(id), ..
            },
        ) => (
            "sink-types",
            id.as_str(),
            "config",
            "config_schema",
            &["type"],
        ),
        (
            "cron",
            Action::Create {
                job_type: Some(id), ..
            },
        )
        | (
            "cron",
            Action::Update {
                job_type: Some(id), ..
            },
        ) => (
            "job-types",
            id.as_str(),
            "job_parameters",
            "parameters_schema",
            &["job_type"],
        ),
        ("cron", Action::Update { .. }) => {
            record = client.send("GET", path, None, &[]).await?;
            (
                "job-types",
                record_identity(&record, "job_type")?,
                "job_parameters",
                "parameters_schema",
                &[],
            )
        }
        ("source", Action::Update { .. }) => {
            record = client.send("GET", path, None, &[]).await?;
            (
                "source-types",
                record_identity(&record, "type")?,
                "config",
                "config_schema",
                &[],
            )
        }
        ("config", Action::Update { .. }) => {
            record = client.send("GET", path, None, &[]).await?;
            let mut schema = catalog_schema(
                client,
                "config-schemas",
                record_identity(&record, "schema")?,
                "input_schema",
            )
            .await?;
            if let Some(object) = schema.as_object_mut() {
                object.remove("required");
            }
            return Ok(schema);
        }
        _ => return client.schema(method, path).await,
    };
    let child = catalog_schema(client, catalog, id, child_field).await?;
    let mut schema = client.schema_without(method, path, omit).await?;
    schema::nested(&mut schema, field, child)?;
    Ok(schema)
}

fn resource_path(group: &str, base: &str, id: &str) -> Result<String, Failure> {
    if group == "extension" {
        let mut parts = id.split('/');
        let namespace = parts
            .next()
            .ok_or_else(|| Failure::usage("扩展使用 namespace/name"))?;
        let name = parts
            .next()
            .ok_or_else(|| Failure::usage("扩展使用 namespace/name"))?;
        if parts.next().is_some() {
            return Err(Failure::usage("扩展使用 namespace/name"));
        }
        Ok(format!(
            "/extensions/{}/{}",
            segment(namespace)?,
            segment(name)?
        ))
    } else {
        Ok(format!("{base}/{}", segment(id)?))
    }
}
async fn modify_config(
    client: &Client,
    group: &str,
    base: &str,
    id: String,
    input: Input,
    patch: bool,
) -> Result<Value, Failure> {
    let record_path = resource_path(group, base, &id)?;
    let path = format!("{record_path}/config");
    if input.schema {
        let mut schema = if group == "extension" {
            let response = client
                .send(
                    "POST",
                    "/extension-management/v2",
                    Some(json!({"action":"status","extension":id})),
                    &[],
                )
                .await?;
            response["extensions"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["name"] == id))
                .and_then(|row| row.get("config_schema"))
                .cloned()
                .filter(|v| v.is_object())
                .ok_or_else(|| Failure::usage("接入Peer尚未发布该扩展的配置schema"))?
        } else {
            let record = client.send("GET", &record_path, None, &[]).await?;
            let type_id = record["type"]
                .as_str()
                .ok_or_else(|| Failure::usage("Sink缺少type"))?;
            client
                .send(
                    "GET",
                    &format!("/sink-types/{}", segment(type_id)?),
                    None,
                    &[],
                )
                .await?["config_schema"]
                .clone()
        };
        if patch && let Some(object) = schema.as_object_mut() {
            object.remove("required");
        }
        return Ok(schema);
    }
    let method = if group == "sink" || !patch {
        "PUT"
    } else {
        "PATCH"
    };
    client.send(method, &path, Some(input.read()?), &[]).await
}

fn observed(last: Value) -> Result<Value, Failure> {
    if last.is_null() {
        Err(Failure {
            http_status: None,
            code: 1,
            detail: json!("观察预算已用尽，尚未取得Job记录"),
        })
    } else {
        Ok(last)
    }
}

fn resolver_methods(target: &str) -> Result<String, Failure> {
    if target.starts_with("block:") {
        let (kind, id) = crate::entity(target)?;
        if kind != "block" {
            return Err(Failure::usage("需要Block引用"));
        }
        Ok(format!("/blocks/{id}/resolver/methods"))
    } else {
        Ok(format!("/resolvers/{}/methods", segment(target)?))
    }
}
