use inkcre_core::Core;
use inkcre_extension_sdk::{
    HttpAuth, HttpBody, HttpHeader, HttpIngress, HttpPathMatch, HttpRequest, HttpResponse,
    HttpRoute, RpcError,
};
use serde::Deserialize;
use serde_json::{Value, json};

const MATERIALIZE: &str = "/rss/enclosures/materialize";

pub fn descriptor() -> HttpIngress {
    HttpIngress {
        routes: vec![HttpRoute {
            method: "POST".into(),
            path: MATERIALIZE.into(),
            path_match: HttpPathMatch::Exact,
            auth: HttpAuth::Peer,
        }],
        peer_inbounds: vec![],
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Materialize {
    enclosure_block_ids: Vec<i32>,
    #[serde(default = "default_storage")]
    target_storage_id: i32,
}

fn default_storage() -> i32 {
    -4
}

pub async fn handle(core: &Core, request: HttpRequest) -> Result<HttpResponse, RpcError> {
    if request.method != "POST" || request.path != MATERIALIZE {
        return Err(RpcError::new("not_found", "RSS HTTP operation not found"));
    }
    let input: Materialize = serde_json::from_slice(&request.body.0)
        .map_err(|_| RpcError::new("invalid_input", "invalid RSS enclosure selection"))?;
    if input.enclosure_block_ids.is_empty() {
        return Err(RpcError::new(
            "invalid_input",
            "RSS requires at least one enclosure",
        ));
    }
    let mut results = Vec::with_capacity(input.enclosure_block_ids.len());
    for id in input.enclosure_block_ids {
        let result = match crate::enrichment::enclosure(core, id, input.target_storage_id).await {
            Ok(result) => serde_json::to_value(result).map_err(crate::Error::from)?,
            Err(error) => {
                let error = RpcError::from(error);
                json!({"enclosure_block_id":id,"content_block_id":Value::Null,"status":"failed",
                    "resolver_id":Value::Null,"error":error.message})
            }
        };
        results.push(result);
    }
    let body = serde_json::to_vec(&json!({"results":results})).map_err(crate::Error::from)?;
    Ok(HttpResponse {
        status: 200,
        headers: vec![HttpHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: HttpBody(body),
    })
}
