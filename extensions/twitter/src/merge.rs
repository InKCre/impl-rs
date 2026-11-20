use serde_json::{Map, Value};
fn merge(target: &mut Map<String, Value>, incoming: &Map<String, Value>) {
    for (key, value) in incoming {
        if let (Some(current), Some(incoming)) = (
            target.get_mut(key).and_then(Value::as_object_mut),
            value.as_object(),
        ) {
            merge(current, incoming)
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
}
fn millis(time: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|d| d.timestamp_millis())
}
pub fn record(existing: &Value, incoming: &Value) -> Value {
    let mut result = existing.as_object().cloned().unwrap_or_default();
    let mut content = incoming.as_object().cloned().unwrap_or_default();
    content.remove("counts");
    content.remove("countsObservedAt");
    merge(&mut result, &content);
    let Some(incoming_counts) = incoming["counts"].as_object() else {
        return Value::Object(result);
    };
    let mut counts = result
        .get("counts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut times = result
        .get("countsObservedAt")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in incoming_counts {
        let observed = incoming["countsObservedAt"][key].as_str();
        let current = times.get(key).and_then(Value::as_str);
        if let (Some(new), Some(old)) = (observed.and_then(millis), current.and_then(millis))
            && new < old
        {
            continue;
        }
        let changed = counts.get(key) != Some(value);
        if changed {
            counts.insert(key.clone(), value.clone());
        }
        if let Some(time) = observed {
            times.insert(key.clone(), Value::String(time.into()));
        } else if changed {
            times.remove(key);
        }
    }
    if !counts.is_empty() {
        result.insert("counts".into(), Value::Object(counts));
    }
    if times.is_empty() {
        result.remove("countsObservedAt");
    } else {
        result.insert("countsObservedAt".into(), Value::Object(times));
    }
    Value::Object(result)
}
