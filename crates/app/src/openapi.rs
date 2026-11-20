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
                            "name":name,"in":"path","required":true,"schema":path_schema(name)
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

    /// Query extraction is route-owned in Axum, rather than embedded in body DTOs.
    /// Keep these parameter declarations next to the public operation registrations;
    /// request bodies continue to derive from the actual input types.
    fn query(&mut self, path: &str, method: &str, name: &str, schema: Value) {
        let parameter = json!({"name":name,"in":"query","required":false,"schema":schema,"style":"form","explode":true});
        self.paths[path][method]["parameters"]
            .as_array_mut()
            .expect("registered operation")
            .push(parameter);
    }
    fn page(&mut self, path: &str, cursor: Value) {
        self.query(
            path,
            "get",
            "limit",
            nullable(json!({"type":"integer","exclusiveMinimum":0})),
        );
        self.query(path, "get", "cursor", nullable(cursor));
    }

    fn location(&mut self, path: &str, status: u16) {
        self.paths[path]["post"]["responses"][status.to_string()]["headers"]["Location"] = json!({
            "description":"Created resource URI, resolved against this server origin",
            "required":true,
            "schema":{"type":"string","format":"uri-reference"}
        });
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
    // POST accepts the persisted-endpoint producer form (including the runtime's
    // from_/to_ aliases). PATCH publishes the canonical long-name REST DTO.
    doc.json::<core::RelationInput>("/relations", "post", 200);
    doc.patch::<crate::routes::RelationRequest>("/relations/{relation_id}", &[]);
    doc.json::<crate::entity_routes::CanonicalGraphForm>("/graph", "post", 200);
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
        let path = format!("/sources/{{source_id}}/{command}");
        doc.json::<Object>(&path, "post", 201);
        // source_routes::collect supplies {} when no body is sent; backfill is
        // required, matching the frozen Python command admission contract.
        if command == "collect" {
            doc.paths[&path]["post"]["requestBody"]["required"] = json!(false);
        }
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
        "/blocks/{block_id}/neighborhood",
        "/relations/{relation_id}",
        "/relations/{relation_id}/neighborhood",
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
        "/cron/config",
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
    ] {
        doc.operation(path, "delete", None, 204);
    }
    for path in [
        "/jobs/{job_id}/abort",
        "/crons/{cron_id}/materialize",
        "/sinks/{sink_id}/enable",
        "/sinks/{sink_id}/disable",
        "/extensions/{namespace}/{name}/enable",
        "/extensions/{namespace}/{name}/disable",
    ] {
        doc.operation(path, "post", None, 200);
    }
    doc.operation("/crons/{cron_id}/run", "post", None, 201);
    doc.operation(
        "/extensions/{namespace}/{name}/selection",
        "delete",
        None,
        200,
    );
    // Public probes do not require Peer JWT. /heartbeat is an intentionally hidden alias.
    doc.operation("/livez", "get", None, 200);
    doc.paths["/livez"]["get"]["security"] = json!([]);
    doc.paths["/livez"]["get"]["responses"]["200"]["description"] =
        json!("Process liveness; status is ok");
    doc.operation("/readyz", "get", None, 200);
    doc.paths["/readyz"]["get"]["security"] = json!([]);
    doc.paths["/readyz"]["get"]["responses"] = json!({
        "200":{"description":"Ready; includes observed native PostgreSQL contract, profile, migration, runtime and lease diagnostics"},
        "503":{"description":"Not ready; diagnostics identify unavailable prerequisites"}
    });
    // Canonical writes in source_routes/routes/agent_routes (d55a929), plus
    // Sink query's existing asynchronous Job response. Do not advertise a
    // Location for writes whose actual controller does not emit one.
    for (path, status) in [
        ("/sources", 201),
        ("/sources/{source_id}/collect", 201),
        ("/sources/{source_id}/backfill", 201),
        ("/jobs", 201),
        ("/crons", 201),
        ("/crons/{cron_id}/run", 201),
        ("/agents", 201),
        ("/sinks/{sink_id}/query", 202),
    ] {
        doc.location(path, status);
    }
    queries(&mut doc);
    doc.finish()
}

fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}

fn path_schema(name: &str) -> Value {
    match name {
        "peer_id" => json!({"type":"string","format":"uuid"}),
        "block_id" | "relation_id" | "source_id" | "job_id" | "cron_id" | "agent_id"
        | "sink_id" | "model_id" | "profile_id" => json!({"type":"integer"}),
        _ => json!({"type":"string"}),
    }
}

fn queries(doc: &mut Document) {
    // Canonical list handlers in source/agent/ai/config/knowledge/extension routes
    // and routes::resource_page omit a default limit to request all records.
    for path in [
        "/sources",
        "/crons",
        "/agents",
        "/ai/models",
        "/embedding-profiles",
    ] {
        doc.page(path, json!({"type":"integer"}));
    }
    for path in [
        "/source-types",
        "/job-types",
        "/agent-tools",
        "/resolvers",
        "/resolvers/{resolver}/methods",
        "/blocks/{block_id}/resolver/methods",
        "/configs",
        "/config-schemas",
        "/extensions",
    ] {
        doc.page(path, json!({"type":"string"}));
    }
    doc.page("/peers", json!({"type":"string","format":"uuid"}));
    // routes::JobPageQuery adds exact type/status filters and the canonical 20-row default.
    doc.query(
        "/jobs",
        "get",
        "limit",
        json!({"type":"integer","exclusiveMinimum":0,"default":20}),
    );
    doc.query(
        "/jobs",
        "get",
        "cursor",
        nullable(json!({"type":"integer"})),
    );
    doc.query("/jobs", "get", "type", nullable(json!({"type":"string"})));
    use inkcre_core::JobStatus;
    doc.query("/jobs","get","status",nullable(json!({"type":"string","enum":[JobStatus::Pending,JobStatus::Running,JobStatus::Finished,JobStatus::Failed,JobStatus::TimedOut,JobStatus::Aborted]})));
    // routes::RecentBlocksQuery and block_neighborhood's repeated query parser.
    doc.query(
        "/blocks/recent",
        "get",
        "num",
        json!({"type":"integer","default":10}),
    );
    doc.query(
        "/blocks/recent",
        "get",
        "resolver",
        nullable(json!({"type":"string"})),
    );
    let path = "/blocks/{block_id}/neighborhood";
    doc.query(
        path,
        "get",
        "direction",
        json!({"type":"string","enum":["in","out","both"],"default":"both"}),
    );
    doc.query(
        path,
        "get",
        "contents",
        json!({"type":"array","items":{"type":"string"}}),
    );
    doc.query(
        path,
        "get",
        "limit",
        json!({"type":"integer","minimum":1,"maximum":100,"default":20}),
    );
    doc.query(path, "get", "cursor", nullable(json!({"type":"integer"})));
    // source_routes::CommandQuery; zero/negative values are rejected by Job admission.
    for command in ["collect", "backfill"] {
        doc.query(
            &format!("/sources/{{source_id}}/{command}"),
            "post",
            "timeout_seconds",
            nullable(json!({"type":"integer","exclusiveMinimum":0})),
        );
    }
    // The peer-routing DTO is shared by the two retrieval controllers, and the
    // extension management controller owns the same routing field independently.
    for (path, method) in [
        ("/retrieval/lexical", "post"),
        ("/retrieval/semantic", "post"),
        ("/extensions/{namespace}/{name}/selection", "post"),
        ("/extensions/{namespace}/{name}/selection", "delete"),
        ("/extensions/{namespace}/{name}/enable", "post"),
        ("/extensions/{namespace}/{name}/disable", "post"),
    ] {
        doc.query(
            path,
            method,
            "route_to_peer",
            nullable(json!({"type":"string","format":"uuid"})),
        );
    }
}

pub async fn document() -> Json<Value> {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    Json(DOCUMENT.get_or_init(build).clone())
}
