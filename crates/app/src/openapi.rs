//! Request discovery generated from the same input types consumed by handlers.
use axum::Json;
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

struct Document {
    paths: Map<String, Value>,
    generator: SchemaGenerator,
}

impl Document {
    fn new() -> Self {
        let mut settings = SchemaSettings::draft2020_12();
        settings.definitions_path = "/components/schemas".into();
        Self {
            paths: Map::new(),
            generator: settings.into_generator(),
        }
    }

    fn operation(&mut self, path: &str, method: &str, body: Option<Value>, status: u16) {
        let path_item = self.paths.entry(path).or_insert_with(|| json!({}));
        let parameters: Vec<_> = path
            .split('/')
            .filter_map(|segment| {
                segment
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'))
                    .map(|name| {
                        json!({
                            "name":name,"in":"path","required":true,"schema":{"type":"string"}
                        })
                    })
            })
            .collect();
        let mut operation = json!({
            "parameters":parameters,
            "responses":{status.to_string():{"description":"Successful operation"}},
            "security":[{"peerJwt":[]}]
        });
        if let Some(schema) = body {
            operation["requestBody"] =
                json!({"required":true,"content":{"application/json":{"schema":schema}}});
        }
        path_item[method.to_ascii_lowercase()] = operation;
    }

    fn json<T: JsonSchema>(&mut self, path: &str, method: &str, status: u16) {
        let schema = self.generator.subschema_for::<T>();
        self.operation(path, method, Some(schema.into()), status);
    }

    fn patch<T: JsonSchema>(&mut self, path: &str, omit: &[&str]) {
        let mut schema: Value = T::json_schema(&mut self.generator).into();
        if let Some(object) = schema.as_object_mut() {
            object.remove("required");
        }
        for key in omit {
            if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.remove(*key);
            }
        }
        self.operation(path, "patch", Some(schema), 200);
    }

    fn finish(mut self) -> Value {
        json!({
            "openapi":"3.1.0",
            "info":{"title":"InKCre Rust API","version":env!("CARGO_PKG_VERSION")},
            "paths":self.paths,
            "components":{
                "schemas":self.generator.take_definitions(true),
                "securitySchemes":{"peerJwt":{"type":"http","scheme":"bearer","bearerFormat":"JWT"}}
            }
        })
    }
}

fn build() -> Value {
    use inkcre_core::{self as core, cron, knowledge, source};
    type Object = serde_json::Map<String, Value>;
    let mut doc = Document::new();
    doc.json::<core::BlockInput>("/blocks", "post", 201);
    doc.patch::<core::BlockInput>("/blocks/{block_id}", &[]);
    doc.json::<core::RelationInput>("/relations", "post", 201);
    doc.patch::<core::RelationInput>("/relations/{relation_id}", &[]);
    doc.json::<core::GraphInput>("/graph", "post", 200);
    doc.json::<crate::entity_routes::EntityForm>("/entities/get", "post", 200);
    doc.json::<crate::routes::PathInput>("/graph/path", "post", 200);
    doc.json::<core::graph_components::ComponentsRequest>("/graph/components", "post", 200);
    doc.json::<knowledge::GraphDraft>("/graph/submit", "post", 201);
    doc.json::<crate::knowledge_routes::DraftInput>("/graph/draft", "post", 200);
    doc.json::<crate::knowledge_routes::StorageInput>("/storages", "post", 201);
    doc.json::<core::JobInput>("/jobs", "post", 201);
    let invoke_path = "/blocks/{block_id}/resolver/methods/{method}";
    doc.json::<Object>(invoke_path, "post", 200);
    doc.paths[invoke_path]["post"]["requestBody"]["required"] = json!(false);

    doc.json::<source::SourceInput>("/sources", "post", 201);
    doc.patch::<source::SourceInput>("/sources/{source_id}", &["type"]);
    for command in ["collect", "backfill"] {
        doc.json::<Object>(&format!("/sources/{{source_id}}/{command}"), "post", 201);
    }
    doc.json::<cron::CronInput>("/crons", "post", 201);
    doc.patch::<cron::CronInput>("/crons/{cron_id}", &[]);
    doc.json::<cron::CronConfig>("/cron/config", "put", 200);
    doc.json::<crate::config_routes::Replacement>("/configs/{key}", "put", 200);
    doc.json::<Object>("/configs/{key}", "patch", 200);
    doc.json::<core::agent::Form>("/agents", "post", 201);
    doc.patch::<core::agent::Form>("/agents/{agent_id}", &[]);
    doc.json::<core::sink::SinkInput>("/sinks", "post", 201);
    doc.json::<Object>("/sinks/{sink_id}/config", "put", 200);
    doc.json::<core::sink::QueryInput>("/sinks/{sink_id}/query", "post", 202);
    doc.json::<crate::organization_routes::Request>("/organization/ruminate", "post", 204);
    doc.json::<crate::extension_v2_routes::Selection>(
        "/extensions/{namespace}/{name}/selection",
        "post",
        200,
    );
    doc.json::<crate::extension_v2_routes::Command>("/extension-management/v2", "post", 200);
    for method in ["put", "patch"] {
        doc.json::<Object>("/extensions/{namespace}/{name}/config", method, 200);
    }
    for path in ["/lexical-retrieval", "/retrieval/lexical"] {
        doc.json::<core::lexical::LexicalRequest>(path, "post", 200);
    }
    for path in ["/semantic-retrieval", "/retrieval/semantic"] {
        doc.json::<core::semantic::Request>(path, "post", 200);
    }
    for path in [
        "/blocks/{block_id}",
        "/blocks/recent",
        "/relations/by_block/{block_id}",
        "/blocks/{block_id}/resolver/methods",
        "/resolvers",
        "/resolvers/{resolver}/methods",
        "/sources",
        "/sources/{source_id}",
        "/source-types",
        "/source-types/{type_id}",
        "/jobs",
        "/jobs/{job_id}",
        "/job-types",
        "/job-types/{type_id}",
        "/crons",
        "/crons/{cron_id}",
        "/configs",
        "/configs/{key}",
        "/config-schemas",
        "/config-schemas/{schema_id}",
        "/agents",
        "/agents/{agent_id}",
        "/agent-tools",
        "/agent-tools/{tool_id}",
        "/sink-types",
        "/sink-types/{type_id}",
        "/sinks",
        "/sinks/{sink_id}",
        "/ai/models",
        "/ai/models/{model_id}",
        "/embedding-profiles",
        "/embedding-profiles/{profile_id}",
        "/peers",
        "/peers/self",
        "/peers/{peer_id}",
        "/extensions",
        "/extensions/{namespace}/{name}",
    ] {
        doc.operation(path, "get", None, 200);
    }
    for path in [
        "/blocks/{block_id}",
        "/relations/{relation_id}",
        "/sources/{source_id}",
        "/jobs/{job_id}",
        "/crons/{cron_id}",
        "/configs/{key}",
        "/agents/{agent_id}",
        "/sinks/{sink_id}",
        "/extensions/{namespace}/{name}/selection",
    ] {
        doc.operation(path, "delete", None, 204);
    }
    for path in [
        "/jobs/{job_id}/abort",
        "/crons/{cron_id}/run",
        "/crons/{cron_id}/materialize",
        "/sinks/{sink_id}/enable",
        "/sinks/{sink_id}/disable",
        "/extensions/{namespace}/{name}/enable",
        "/extensions/{namespace}/{name}/disable",
    ] {
        doc.operation(path, "post", None, 200);
    }
    doc.finish()
}

pub async fn document() -> Json<Value> {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    Json(DOCUMENT.get_or_init(build).clone())
}
