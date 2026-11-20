use crate::error::{ApiError, ApiResult};
use axum::{
    Json,
    http::header,
    response::{IntoResponse, Response},
};
use inkcre_core::knowledge::ReadValue;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn response(value: ReadValue) -> ApiResult<Response> {
    if let ReadValue::Bytes(bytes) = value {
        return Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response());
    }
    let (value, parts) = value.into_parts();
    project(value, parts)
}

pub fn project(value: Value, parts: Vec<(String, Vec<u8>)>) -> ApiResult<Response> {
    if parts.is_empty() {
        return Ok(Json(value).into_response());
    }
    let boundary = Uuid::new_v4().simple().to_string();
    let mut content =
        format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n").into_bytes();
    let pointers: serde_json::Map<_, _> = parts
        .iter()
        .enumerate()
        .map(|(index, (pointer, _))| (pointer.clone(), json!(part_id(index))))
        .collect();
    serde_json::to_writer(&mut content, &json!({"value":value,"parts":pointers}))
        .map_err(|_| ApiError::invalid("unable to represent binary content"))?;
    for (index, (_, bytes)) in parts.into_iter().enumerate() {
        let id = part_id(index);
        let marker = format!(
            "\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-ID: <{id}>\r\n\r\n"
        );
        content.extend_from_slice(marker.as_bytes());
        content.extend(bytes);
    }
    content.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((
        [(
            header::CONTENT_TYPE,
            format!("multipart/related; boundary={boundary}; type=\"application/json\""),
        )],
        content,
    )
        .into_response())
}

fn part_id(index: usize) -> String {
    format!("body-{}@inkcre", index + 1)
}
