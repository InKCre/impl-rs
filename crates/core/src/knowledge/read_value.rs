use serde_json::Value;
use std::collections::BTreeMap;

/// A Resolver result that retains binary values independently of authored JSON.
#[derive(Clone, Debug)]
pub enum ReadValue {
    Json(Value),
    Bytes(Vec<u8>),
    Object(BTreeMap<String, Self>),
    Array(Vec<Self>),
}

impl ReadValue {
    /// Preserve the JSON projection of older SDK callers, including byte arrays.
    pub fn into_json(self) -> Value {
        match self {
            Self::Json(value) => value,
            Self::Bytes(bytes) => Value::Array(bytes.into_iter().map(Value::from).collect()),
            Self::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(name, value)| (name, value.into_json()))
                    .collect(),
            ),
            Self::Array(values) => Value::Array(values.into_iter().map(Self::into_json).collect()),
        }
    }

    /// Retain the exact positions of binary values using RFC 6901 JSON Pointers.
    /// The empty pointer is used for a top-level byte value.
    pub fn into_parts(self) -> (Value, Vec<(String, Vec<u8>)>) {
        let mut parts = Vec::new();
        let value = self.extract("", &mut parts);
        (value, parts)
    }

    fn extract(self, pointer: &str, parts: &mut Vec<(String, Vec<u8>)>) -> Value {
        match self {
            Self::Json(value) => value,
            Self::Bytes(bytes) => {
                parts.push((pointer.into(), bytes));
                Value::Null
            }
            Self::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(name, value)| {
                        let child =
                            format!("{pointer}/{}", name.replace('~', "~0").replace('/', "~1"));
                        (name, value.extract(&child, parts))
                    })
                    .collect(),
            ),
            Self::Array(values) => Value::Array(
                values
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| value.extract(&format!("{pointer}/{index}"), parts))
                    .collect(),
            ),
        }
    }

    pub(crate) fn media_content(content: Vec<u8>, metadata: Value) -> Self {
        let Value::Object(metadata) = metadata else {
            unreachable!("media metadata is an object")
        };
        let mut fields: BTreeMap<_, _> = metadata
            .into_iter()
            .map(|(key, value)| (key, Self::Json(value)))
            .collect();
        fields.insert("content".into(), Self::Bytes(content));
        Self::Object(fields)
    }
}
