//! Telegram entities use UTF-16 code-unit offsets, including astral characters.
use crate::{Result, fail};
use serde_json::Value;
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
pub fn render(text: &str, entities: &[Value]) -> Result<String> {
    let mut boundaries = vec![Some(0)];
    for (i, c) in text.char_indices() {
        if c.len_utf16() == 2 {
            boundaries.push(None)
        }
        boundaries.push(Some(i + c.len_utf8()));
    }
    let mut ranges = Vec::new();
    for entity in entities {
        let a = entity["offset"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| fail("Invalid Telegram entity offset"))?;
        let length = entity["length"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| fail("Invalid Telegram entity length"))?;
        let b = a
            .checked_add(length)
            .ok_or_else(|| fail("Invalid Telegram entity range"))?;
        let start = boundaries
            .get(a)
            .copied()
            .flatten()
            .ok_or_else(|| fail("Telegram entity splits Unicode"))?;
        let end = boundaries
            .get(b)
            .copied()
            .flatten()
            .ok_or_else(|| fail("Telegram entity splits Unicode"))?;
        if start >= end {
            return Err(fail("Empty Telegram entity"));
        }
        ranges.push((start, end, entity));
    }
    ranges.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
    let mut index = 0;
    let output = segment(text, 0, text.len(), &ranges, &mut index)?;
    if index != ranges.len() {
        return Err(fail("Crossing Telegram entities"));
    }
    Ok(output)
}
fn segment(
    text: &str,
    start: usize,
    end: usize,
    ranges: &[(usize, usize, &Value)],
    index: &mut usize,
) -> Result<String> {
    let mut output = String::new();
    let mut cursor = start;
    while let Some(&(a, b, entity)) = ranges.get(*index) {
        if a >= end {
            break;
        }
        if a < cursor || b > end {
            return Err(fail("Crossing Telegram entities"));
        }
        output.push_str(&escape(&text[cursor..a]));
        *index += 1;
        let child = segment(text, a, b, ranges, index)?;
        let attr = |key: &str| escape(entity[key].as_str().unwrap_or_default());
        let wrapped = match entity["type"].as_str().unwrap_or_default() {
            "text_link" => format!("<a href=\"{}\">{child}</a>", attr("url")),
            "text_mention" => format!(
                "<a href=\"tg://user?id={}\">{child}</a>",
                entity["user"]["id"]
                    .as_i64()
                    .ok_or_else(|| fail("Telegram mention identity missing"))?
            ),
            "bold" => format!("<b>{child}</b>"),
            "italic" => format!("<i>{child}</i>"),
            "underline" => format!("<u>{child}</u>"),
            "strikethrough" => format!("<s>{child}</s>"),
            "code" => format!("<code>{child}</code>"),
            "pre" if !attr("language").is_empty() => format!(
                "<pre><code class=\"{}\">{child}</code></pre>",
                attr("language")
            ),
            "pre" => format!("<pre>{child}</pre>"),
            "blockquote" => format!("<blockquote>{child}</blockquote>"),
            "expandable_blockquote" => format!("<blockquote expandable>{child}</blockquote>"),
            "spoiler" => format!("<span class=\"tg-spoiler\">{child}</span>"),
            "custom_emoji" => format!(
                "<tg-emoji emoji-id=\"{}\">{child}</tg-emoji>",
                attr("custom_emoji_id")
            ),
            "date_time" => {
                let time = entity["unix_time"]
                    .as_i64()
                    .ok_or_else(|| fail("Telegram time missing"))?;
                if entity["date_time_format"].as_str().is_some() {
                    format!(
                        "<tg-time unix=\"{time}\" format=\"{}\">{child}</tg-time>",
                        attr("date_time_format")
                    )
                } else {
                    format!("<tg-time unix=\"{time}\">{child}</tg-time>")
                }
            }
            _ => child,
        };
        output.push_str(&wrapped);
        cursor = b;
    }
    output.push_str(&escape(&text[cursor..end]));
    Ok(output)
}
