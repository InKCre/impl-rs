use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const CAPABILITY: &str = "io.modelcontextprotocol/skills";
pub const URI: &str = "skill://inkcre/use-inkcre/SKILL.md";

const DESCRIPTION: &str = "Use InKCre when information the human collected earlier could improve the quality or efficiency of the current productive or creative work.";
const MARKDOWN: &str = include_str!("use-inkcre.md");

pub fn list(params: &Value) -> Value {
    if params["cursor"].is_null() {
        json!({"skills":[entry()]})
    } else {
        json!({"skills":[]})
    }
}

pub fn get(params: &Value) -> Value {
    let result = (params["uri"] == URI).then(entry);
    json!({"skill":result})
}

pub fn resource() -> Value {
    json!({"contents":[{"uri":URI,"mimeType":"text/markdown","text":MARKDOWN}]})
}

fn entry() -> Value {
    let digest = format!("sha256:{:x}", Sha256::digest(MARKDOWN.as_bytes()));
    json!({"uri":URI,"frontmatter":{"name":"use-inkcre","description":DESCRIPTION},
        "resources":[{"uri":URI,"digest":digest}]})
}
