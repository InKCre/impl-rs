use crate::Failure;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::{DirBuilder, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

// Tokens are generated after decoding each response and only resolve against
// this invocation's byte map. Authored JSON has no reserved object shape.
static BINARIES: OnceLock<Mutex<BTreeMap<String, Vec<u8>>>> = OnceLock::new();
pub fn binary(bytes: Vec<u8>) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    BINARIES
        .get_or_init(Default::default)
        .lock()
        .expect("binary registry mutex poisoned")
        .insert(id.clone(), bytes);
    Value::String(id)
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| Failure::usage("无法创建输出文件"))?;
    file.write_all(bytes)
        .map_err(|_| Failure::usage("无法保存输出文件"))?;
    file.sync_all()
        .map_err(|_| Failure::usage("无法完成输出文件写入"))
}
fn map_binaries(
    value: &mut Value,
    blobs: &BTreeMap<String, Vec<u8>>,
    convert: &mut impl FnMut(&[u8]) -> Result<Value, Failure>,
) -> Result<(), Failure> {
    match value {
        Value::String(id) if blobs.contains_key(id) => {
            *value = convert(&blobs[id])?;
        }
        Value::Array(values) => {
            for value in values {
                map_binaries(value, blobs, convert)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                map_binaries(value, blobs, convert)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn save(
    mut value: Value,
    blobs: &BTreeMap<String, Vec<u8>>,
    parent: Option<&Path>,
    entry: bool,
) -> Result<Value, Failure> {
    let parent = parent
        .map(Path::to_owned)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&parent).map_err(|_| Failure::usage("无法创建输出目录"))?;
    let directory = parent
        .canonicalize()
        .map_err(|_| Failure::usage("无法读取输出目录"))?
        .join(format!("inkcre-{}", uuid::Uuid::new_v4()));
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .map_err(|_| Failure::usage("无法创建独占输出目录"))?;
    let result = (|| {
        if entry && let Some(bytes) = value.as_str().and_then(|id| blobs.get(id)) {
            let path = directory.join("result.bin");
            private_write(&path, bytes)?;
            return Ok(json!({"file":path.to_string_lossy()}));
        }
        let mut number = 0;
        map_binaries(&mut value, blobs, &mut |bytes| {
            number += 1;
            let name = format!("{number:03}.bin");
            let path = directory.join(&name);
            private_write(&path, bytes)?;
            Ok(if entry {
                json!({"file":name})
            } else {
                json!({"file":path.to_string_lossy()})
            })
        })?;
        if !entry {
            return Ok(value);
        }
        let (filename, bytes) = if let Some(text) = value.as_str() {
            ("result.txt", Cow::Borrowed(text.as_bytes()))
        } else {
            (
                "result.json",
                Cow::Owned(serde_json::to_vec(&value).map_err(|_| Failure::usage("无法表示输出"))?),
            )
        };
        let path = directory.join(filename);
        private_write(&path, &bytes)?;
        Ok(json!({"file":path.to_string_lossy()}))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&directory);
    }
    result
}

fn preview<'a>(
    value: &'a Value,
    blobs: &BTreeMap<String, Vec<u8>>,
) -> Result<Cow<'a, str>, Failure> {
    if let Some(text) = value.as_str() {
        return Ok(match blobs.get(text) {
            Some(bytes) => Cow::Owned(format!("<{} bytes；完整内容见导出文件>", bytes.len())),
            None => Cow::Borrowed(text),
        });
    }
    let mut view = Cow::Borrowed(value);
    if !blobs.is_empty() {
        map_binaries(view.to_mut(), blobs, &mut |bytes| {
            Ok(json!(format!(
                "<{} bytes；完整内容见导出文件>",
                bytes.len()
            )))
        })?;
    }
    serde_json::to_string_pretty(view.as_ref())
        .map(Cow::Owned)
        .map_err(|_| Failure::usage("无法表示输出"))
}

fn rows(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.as_object()?.values().find_map(Value::as_array))
}
pub fn emit(
    mut value: Value,
    compact: bool,
    output_dir: Option<PathBuf>,
    table: bool,
    fields: Vec<String>,
) -> Result<(), Failure> {
    let blobs = BINARIES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| Failure::usage("输出状态不可用"))?;
    if table {
        if !blobs.is_empty() {
            value = save(value, &blobs, None, false)?;
        }
        let rows = rows(&value).ok_or_else(|| Failure::usage("表格输出需要记录数组"))?;
        let columns = if fields.is_empty() {
            rows.first()
                .and_then(Value::as_object)
                .map(|row| row.keys().cloned().collect())
                .unwrap_or_default()
        } else {
            fields
        };
        println!("{}", columns.join("\t"));
        for row in rows {
            let cells: Vec<_> = columns
                .iter()
                .map(|field| {
                    row.get(field)
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .unwrap_or_default()
                        .replace(['\t', '\r', '\n'], " ")
                })
                .collect();
            println!("{}", cells.join("\t"));
        }
        return Ok(());
    }
    if !fields.is_empty() {
        let filter = |row: &Value| {
            Value::Object(
                fields
                    .iter()
                    .filter_map(|field| row.get(field).cloned().map(|value| (field.clone(), value)))
                    .collect(),
            )
        };
        value = match value.as_array() {
            Some(rows) => Value::Array(rows.iter().map(filter).collect()),
            None => filter(&value),
        };
    }
    if output_dir.is_some() {
        value = save(value, &blobs, output_dir.as_deref(), true)?;
    } else if !compact {
        let text = preview(&value, &blobs)?;
        if text.chars().nth(8_000).is_some() {
            let end = text
                .char_indices()
                .nth(2_000)
                .map_or(text.len(), |(byte, _)| byte);
            let head = text[..end].to_owned();
            let entry = save(value, &blobs, None, true)?;
            let file = entry["file"]
                .as_str()
                .ok_or_else(|| Failure::usage("无法表示输出路径"))?;
            println!("{head}\n…\n完整结果: {file}");
            return Ok(());
        }
        if blobs.is_empty() {
            if !value.is_null() {
                println!("{text}");
            }
            return Ok(());
        }
    }
    if output_dir.is_none() && !blobs.is_empty() {
        value = save(value, &blobs, None, false)?;
    }
    if compact {
        println!("{value}");
    } else if !value.is_null() {
        println!("{}", preview(&value, &BTreeMap::new())?);
    }
    Ok(())
}
