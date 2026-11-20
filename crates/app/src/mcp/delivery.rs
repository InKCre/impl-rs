//! Native MCP content blocks over live typed Resolver values.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use inkcre_core::knowledge::ReadValue;
use serde_json::{Value, json};

pub(super) const INLINE_BUDGET: usize = 64 * 1024;

pub(super) struct ToolOutput {
    pub payload: Value,
    pub content: Vec<Value>,
}

impl ToolOutput {
    pub fn plain(payload: Value) -> Self {
        Self {
            payload,
            content: vec![],
        }
    }
    pub fn into_result(mut self, failed: bool) -> Value {
        if self.content.is_empty() {
            self.content
                .push(json!({"type":"text","text":python_json(&self.payload)}));
        }
        json!({"content":self.content,"structuredContent":self.payload,"isError":failed})
    }
}

pub(super) fn deliver(value: ReadValue, stem: &str, title: &str) -> (Value, Vec<Value>, String) {
    let binary_size = match &value {
        ReadValue::Bytes(bytes) => Some(bytes.len()),
        _ => None,
    };
    let (mut projected, resources) = project_links(value, stem);
    let (encoded, mime_type) = match projected.as_str() {
        Some(text) => (text.to_owned(), "text/plain"),
        None => (python_json(&projected), "application/json"),
    };
    let kind = if binary_size.is_some() {
        "blob"
    } else {
        "text"
    };
    let root_uri = format!("{stem}/{kind}/root");
    let mut content = Vec::new();
    if binary_size.is_none() && encoded.len() <= INLINE_BUDGET {
        content.push(json!({"type":"resource","resource":{"uri":root_uri,"mimeType":mime_type,"text":encoded}}));
    } else {
        let size = binary_size.unwrap_or(encoded.len());
        content.push(json!({"type":"resource_link","name":title,"uri":root_uri,"size":size}));
        projected = json!({"resource":root_uri,"size":size});
    }
    for resource in resources {
        if resource["uri"] == root_uri {
            continue;
        }
        let mut link = json!({"type":"resource_link","name":format!("{title} part"),"uri":resource["uri"],"size":resource["size"]});
        if let Some(mime_type) = resource.get("mime_type") {
            link["mimeType"] = mime_type.clone();
        }
        content.push(link);
    }
    (projected, content, root_uri)
}

pub(super) fn project_links(value: ReadValue, stem: &str) -> (Value, Vec<Value>) {
    let (mut value, parts) = value.into_parts();
    let mut resources = Vec::new();
    for (pointer, bytes) in parts {
        let keys = pointer_keys(&value, &pointer);
        let mime_type = inherited_media_type(&value, &keys);
        let selector = if keys.is_empty() {
            "root".into()
        } else {
            URL_SAFE_NO_PAD.encode(python_json(&json!(keys)))
        };
        let uri = format!("{stem}/blob/{selector}");
        let mut descriptor = json!({"resource":uri,"size":bytes.len()});
        let mut resource = json!({"uri":uri,"size":bytes.len()});
        if let Some(mime_type) = mime_type {
            descriptor["mime_type"] = json!(mime_type);
            resource["mime_type"] = json!(mime_type);
        }
        if let Some(field) = value.pointer_mut(&pointer) {
            *field = descriptor;
        }
        resources.push(resource);
    }
    (value, resources)
}

fn inherited_media_type(value: &Value, keys: &[Value]) -> Option<String> {
    let mut current = value;
    let mut found = None;
    for key in keys {
        if let Some(mime) = ["detected_media_type", "media_type", "mime_type"]
            .into_iter()
            .find_map(|name| current.get(name).and_then(Value::as_str))
        {
            found = Some(mime.to_owned());
        }
        current = if let Some(index) = key.as_u64() {
            current.get(index as usize)?
        } else {
            current.get(key.as_str()?)?
        };
    }
    found
}

pub(super) fn pointer_keys(value: &Value, pointer: &str) -> Vec<Value> {
    let mut keys = Vec::new();
    let mut current = value;
    for token in pointer.split('/').skip(1) {
        let key = token.replace("~1", "/").replace("~0", "~");
        if let (true, Ok(index)) = (current.is_array(), token.parse::<usize>()) {
            keys.push(json!(index));
            current = &current[index];
        } else {
            current = &current[&key];
            keys.push(json!(key));
        }
    }
    keys
}

/// Python's ensure_ascii=False JSON separators also define the inline byte budget.
pub(super) fn python_json(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(key, value)| format!("{}: {}", json!(key), python_json(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}
