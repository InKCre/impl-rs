use crate::Failure;
use serde_json::{Map, Value, json};

pub(crate) fn omit(schema: &mut Value, fields: &[&str]) {
    for field in fields {
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove(*field);
        }
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|field| field.as_str().is_none_or(|name| !fields.contains(&name)));
    }
}

pub(crate) fn nested(schema: &mut Value, field: &str, mut child: Value) -> Result<(), Failure> {
    if !child.is_object() && !child.is_boolean() {
        return Err(Failure::usage("类型目录未提供有效的输入 schema"));
    }
    fn rebase(value: &mut Value, prefix: &str) {
        match value {
            Value::Object(object) if !object.contains_key("$id") => {
                for (key, value) in object {
                    match (key.as_str(), value.as_str()) {
                        ("$ref", Some(reference))
                            if reference == "#" || reference.starts_with("#/") =>
                        {
                            *value = Value::String(format!("{prefix}{}", &reference[1..]));
                        }
                        _ => rebase(value, prefix),
                    }
                }
            }
            Value::Array(items) => {
                for child in items {
                    rebase(child, prefix);
                }
            }
            _ => {}
        }
    }
    let escaped = field.replace('~', "~0").replace('/', "~1");
    rebase(&mut child, &format!("#/properties/{escaped}"));
    if schema.get("properties").is_none() {
        schema["properties"] = json!({});
    }
    schema["properties"]
        .as_object_mut()
        .ok_or_else(|| Failure::usage("输入 schema 的 properties 无效"))?
        .insert(field.into(), child);
    Ok(())
}

pub(crate) fn include_definitions(schema: &mut Value, document: &Value) -> Result<(), Failure> {
    let mut definitions = Map::new();
    let mut pending = vec![&*schema];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                    && let Some(pointer) = reference.strip_prefix("#/components/schemas/")
                {
                    let name = pointer.split('/').next().unwrap_or_default();
                    let key = name.replace("~1", "/").replace("~0", "~");
                    if !definitions.contains_key(&key) {
                        let target = document["components"]["schemas"]
                            .get(&key)
                            .ok_or_else(|| Failure::usage("schema 引用不存在"))?;
                        definitions.insert(key, target.clone());
                        pending.push(target);
                    }
                }
                pending.extend(object.values());
            }
            Value::Array(array) => pending.extend(array),
            _ => {}
        }
    }
    if !definitions.is_empty() {
        schema["components"] = json!({"schemas": definitions});
    }
    Ok(())
}
