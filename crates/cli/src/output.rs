use crate::Failure;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
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
fn extract(
    value: &mut Value,
    blobs: &BTreeMap<String, Vec<u8>>,
    directory: &Path,
    number: &mut usize,
) -> Result<(), Failure> {
    match value {
        Value::String(id) if blobs.contains_key(id) => {
            *number += 1;
            let path = directory.join(format!("{number:03}.bin"));
            private_write(&path, &blobs[id])?;
            *value = json!({"file":path.to_string_lossy()});
        }
        Value::Array(values) => {
            for value in values {
                extract(value, blobs, directory, number)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                extract(value, blobs, directory, number)?;
            }
        }
        _ => {}
    }
    Ok(())
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
    if output_dir.is_some() || !blobs.is_empty() {
        let parent = output_dir.clone().unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&parent).map_err(|_| Failure::usage("无法创建输出目录"))?;
        let directory = parent.join(format!("inkcre-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).map_err(|_| Failure::usage("无法创建独占输出目录"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Failure::usage("无法设置输出目录权限"))?;
        }
        if let Some(bytes) = value.as_str().and_then(|id| blobs.get(id)) {
            let path = directory.join("result.bin");
            private_write(&path, bytes)?;
            value = json!({"file":path.to_string_lossy()});
        } else {
            extract(&mut value, &blobs, &directory, &mut 0)?;
        }
        if output_dir.is_some() {
            let (filename, bytes) = if let Some(text) = value.as_str() {
                ("result.txt", text.as_bytes().to_vec())
            } else {
                (
                    "result.json",
                    serde_json::to_vec(&value).map_err(|_| Failure::usage("无法表示输出"))?,
                )
            };
            let path = directory.join(filename);
            private_write(&path, &bytes)?;
            value = json!({"file":path.to_string_lossy()});
        }
    }
    if table {
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
    if !value.is_null() {
        println!(
            "{}",
            if compact {
                value.to_string()
            } else {
                serde_json::to_string_pretty(&value).map_err(|_| Failure::usage("无法表示输出"))?
            }
        );
    }
    Ok(())
}
