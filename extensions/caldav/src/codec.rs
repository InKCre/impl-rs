//! Bounded content-line codec. Unknown native properties and nested components survive edits.
use crate::{Error, Result, schema::*};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Property {
    pub name: String,
    pub params: BTreeMap<String, Vec<String>>,
    pub group: Option<String>,
    pub value: String,
}
impl Property {
    fn parse(line: &str) -> Result<Self> {
        let parts = split_quoted(line, ':');
        if parts.len() < 2 {
            return Err(Error::Invalid("DAV content line has no value"));
        }
        let head = parts[0];
        let value = line[head.len() + 1..].to_owned();
        let mut params = split_quoted(head, ';').into_iter();
        let identity = params
            .next()
            .ok_or(Error::Invalid("DAV property name is missing"))?;
        let (group, name) = identity
            .rsplit_once('.')
            .map_or((None, identity), |(g, n)| (Some(g.into()), n));
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(Error::Invalid("invalid DAV property name"));
        }
        let mut parsed = BTreeMap::new();
        for param in params {
            let (key, value) = param
                .split_once('=')
                .ok_or(Error::Invalid("DAV parameter requires a value"))?;
            parsed.insert(
                key.to_ascii_uppercase(),
                split_quoted(value, ',')
                    .into_iter()
                    .map(|v| v.trim_matches('"').to_owned())
                    .collect(),
            );
        }
        Ok(Self {
            name: name.to_ascii_uppercase(),
            params: parsed,
            group,
            value,
        })
    }
    pub fn line(&self) -> String {
        let mut out = String::new();
        if let Some(group) = &self.group {
            out.push_str(group);
            out.push('.')
        };
        out.push_str(&self.name);
        for (name, values) in &self.params {
            out.push(';');
            out.push_str(name);
            out.push('=');
            out.push_str(
                &values
                    .iter()
                    .map(|v| {
                        if v.contains([':', ';', ',']) {
                            format!("\"{v}\"")
                        } else {
                            v.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        out.push(':');
        out.push_str(&self.value);
        out
    }
    pub fn text(&self) -> String {
        unescape(&self.value)
    }
}
#[derive(Clone, Debug)]
pub struct Component {
    pub name: String,
    pub properties: Vec<Property>,
    pub children: Vec<Component>,
}
impl Component {
    pub fn value(&self, name: &str) -> String {
        self.properties
            .iter()
            .find(|p| p.name == name)
            .map(Property::text)
            .unwrap_or_default()
    }
    fn serialize(&self, out: &mut String) {
        fold(&format!("BEGIN:{}", self.name), out);
        let mut properties: Vec<_> = self.properties.iter().collect();
        properties.sort_by(|a, b| a.name.cmp(&b.name));
        for p in properties {
            fold(&p.line(), out)
        }
        for child in &self.children {
            child.serialize(out)
        }
        fold(&format!("END:{}", self.name), out);
    }
    pub fn wire(&self) -> String {
        let mut out = String::new();
        self.serialize(&mut out);
        out
    }
}
fn split_quoted(value: &str, separator: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, c) in value.char_indices() {
        if c == '"' && !escaped {
            quoted = !quoted
        }
        if c == separator && !quoted {
            result.push(&value[start..index]);
            start = index + c.len_utf8()
        }
        escaped = c == '\\' && !escaped;
    }
    result.push(&value[start..]);
    result
}
fn fold(line: &str, out: &mut String) {
    let mut length = 0;
    for c in line.chars() {
        if length + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            length = 1
        }
        out.push(c);
        length += c.len_utf8();
    }
    out.push_str("\r\n");
}
pub fn parse(text: &str) -> Result<Component> {
    if text.len() > 8 * 1024 * 1024 {
        return Err(Error::Invalid("DAV resource exceeds 8 MiB"));
    }
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            lines
                .last_mut()
                .ok_or(Error::Invalid("orphan DAV folded line"))?
                .push_str(&line[1..]);
        } else if !line.is_empty() {
            lines.push(line.into());
        }
    }
    let mut stack: Vec<Component> = Vec::new();
    let mut root = None;
    for line in lines {
        let p = Property::parse(&line)?;
        if p.name == "BEGIN" {
            if stack.len() >= 16 {
                return Err(Error::Invalid("DAV component nesting exceeds 16"));
            }
            stack.push(Component {
                name: p.value.to_ascii_uppercase(),
                properties: vec![],
                children: vec![],
            });
        } else if p.name == "END" {
            let c = stack
                .pop()
                .ok_or(Error::Invalid("unbalanced DAV component"))?;
            if !c.name.eq_ignore_ascii_case(&p.value) {
                return Err(Error::Invalid("mismatched DAV component"));
            }
            if let Some(parent) = stack.last_mut() {
                parent.children.push(c)
            } else if root.replace(c).is_some() {
                return Err(Error::Invalid("DAV resource has multiple roots"));
            }
        } else {
            stack
                .last_mut()
                .ok_or(Error::Invalid("DAV property outside component"))?
                .properties
                .push(p)
        }
    }
    if !stack.is_empty() {
        return Err(Error::Invalid("unterminated DAV component"));
    }
    let root = root.ok_or(Error::Invalid("DAV resource has no root"))?;
    if !["VCARD", "VCALENDAR"].contains(&root.name.as_str()) {
        return Err(Error::Invalid("unsupported DAV root"));
    }
    Ok(root)
}
fn identity(c: &Component) -> (String, String, String) {
    (
        c.name.clone(),
        c.value("UID"),
        c.properties
            .iter()
            .find(|p| p.name == "RECURRENCE-ID")
            .map(|p| p.line())
            .unwrap_or_default(),
    )
}
fn is_calendar(c: &Component) -> bool {
    ["VEVENT", "VTODO", "VJOURNAL"].contains(&c.name.as_str())
}
pub fn uid(text: &str) -> Result<String> {
    let root = parse(text)?;
    if root.name == "VCARD" {
        return Ok(root.value("UID"));
    }
    let ids: BTreeSet<_> = root
        .children
        .iter()
        .filter(|c| is_calendar(c))
        .map(|c| c.value("UID"))
        .collect();
    if ids.len() != 1 {
        return Err(Error::Invalid(
            "DAV calendar resource must have exactly one UID",
        ));
    }
    Ok(ids.into_iter().next().unwrap_or_default())
}
pub fn validate(text: &str, tag: &str) -> Result<Component> {
    let root = parse(text)?;
    if (tag == "VADDRESSBOOK") != (root.name == "VCARD") {
        return Err(Error::Invalid("DAV resource type differs from collection"));
    }
    if uid(text)?.is_empty() {
        return Err(Error::Invalid("DAV resource requires UID"));
    }
    if root.name == "VCALENDAR" && !root.children.iter().any(is_calendar) {
        return Err(Error::Invalid(
            "calendar resource requires an event, task or journal",
        ));
    }
    Ok(root)
}
fn unescape(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(c) => out.push(c),
                None => out.push('\\'),
            }
        } else {
            out.push(c)
        }
    }
    out
}
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}
fn media_type(p: &Property) -> Option<String> {
    for name in ["FMTTYPE", "MEDIATYPE"] {
        if let Some(v) = p.params.get(name).and_then(|v| v.first()) {
            return Some(v.to_ascii_lowercase());
        }
    }
    for kind in p.params.get("TYPE").into_iter().flatten() {
        let kind = kind.to_ascii_lowercase();
        if kind.contains('/') {
            return Some(kind);
        }
        let mime = match kind.as_str() {
            "jpeg" | "jpg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "wav" => "audio/x-wav",
            "mp3" => "audio/mpeg",
            "pdf" => "application/pdf",
            _ => continue,
        };
        return Some(mime.into());
    }
    None
}
pub(crate) fn percent_bytes(value: &str) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    let mut bytes = value.bytes();
    while let Some(c) = bytes.next() {
        if c == b'%' {
            let high = bytes
                .next()
                .and_then(|c| (c as char).to_digit(16))
                .ok_or(Error::Invalid("invalid DAV data URI"))?;
            let low = bytes
                .next()
                .and_then(|c| (c as char).to_digit(16))
                .ok_or(Error::Invalid("invalid DAV data URI"))?;
            result.push((high * 16 + low) as u8)
        } else {
            result.push(c)
        }
    }
    Ok(result)
}
fn extract_properties(
    target: &mut Component,
    key: Option<(String, String, String)>,
    alarm: Option<usize>,
    out: &mut Vec<PropertyValue>,
) -> Result<()> {
    let mut positions: BTreeMap<String, usize> = BTreeMap::new();
    let mut retained = Vec::new();
    for p in std::mem::take(&mut target.properties) {
        let text = ["TEL", "EMAIL", "URL", "NOTE", "DESCRIPTION"].contains(&p.name.as_str());
        let media = ["PHOTO", "LOGO", "SOUND", "ATTACH"].contains(&p.name.as_str());
        if !text && !media && p.name != "RELATED-TO" {
            retained.push(p);
            continue;
        }
        let position = positions.entry(p.name.clone()).or_default();
        let mut representation = if p.name == "RELATED-TO" {
            Representation::Reference
        } else if p.name == "URL" {
            Representation::Uri
        } else {
            Representation::Text
        };
        let mut kind = media_type(&p);
        let mut value = PropertyData::Text(if text && p.name != "URL" {
            p.text()
        } else {
            p.value.clone()
        });
        if media {
            if p.value.to_ascii_lowercase().starts_with("data:") {
                let (header, payload) = p
                    .value
                    .split_once(',')
                    .ok_or(Error::Invalid("invalid DAV data URI"))?;
                kind = Some(
                    header[5..]
                        .split(';')
                        .next()
                        .filter(|v| !v.is_empty())
                        .unwrap_or("text/plain")
                        .into(),
                );
                value = PropertyData::Bytes(if header.to_ascii_lowercase().ends_with(";base64") {
                    STANDARD
                        .decode(payload.split_whitespace().collect::<String>())
                        .map_err(|_| Error::Invalid("invalid DAV base64"))?
                } else {
                    percent_bytes(payload)?
                });
                representation = Representation::DataUri;
            } else if p.params.get("ENCODING").is_some_and(|v| {
                v.iter()
                    .any(|v| v.eq_ignore_ascii_case("BASE64") || v.eq_ignore_ascii_case("b"))
            }) {
                value = PropertyData::Bytes(
                    STANDARD
                        .decode(p.value.split_whitespace().collect::<String>())
                        .map_err(|_| Error::Invalid("invalid DAV base64"))?,
                );
                representation = Representation::Binary;
            } else {
                representation = Representation::Uri
            }
        }
        out.push(PropertyValue {
            link: PropertyLink {
                component: key.clone(),
                alarm,
                name: p.name,
                params: p.params,
                group: p.group,
                position: *position,
                representation,
                media_type: kind,
            },
            value,
        });
        *position += 1;
    }
    target.properties = retained;
    Ok(())
}
pub fn extract(text: &str) -> Result<(String, Vec<PropertyValue>)> {
    let mut root = parse(text)?;
    let mut properties = Vec::new();
    if root.name == "VCARD" {
        extract_properties(&mut root, None, None, &mut properties)?
    } else {
        let mut keys = BTreeSet::new();
        for c in root.children.iter_mut().filter(|c| is_calendar(c)) {
            let key = identity(c);
            if !keys.insert(key.clone()) {
                return Err(Error::Invalid("duplicate DAV component identity"));
            }
            extract_properties(c, Some(key.clone()), None, &mut properties)?;
            for (index, alarm) in c
                .children
                .iter_mut()
                .filter(|c| c.name == "VALARM")
                .enumerate()
            {
                extract_properties(alarm, Some(key.clone()), Some(index), &mut properties)?
            }
        }
    }
    Ok((root.wire(), properties))
}
fn target<'a>(root: &'a mut Component, link: &PropertyLink) -> Result<&'a mut Component> {
    let component = match &link.component {
        None if root.name == "VCARD" => root,
        Some(key) => root
            .children
            .iter_mut()
            .find(|c| identity(c) == *key)
            .ok_or(Error::Integrity(
                "DAV property refers to a missing component",
            ))?,
        _ => return Err(Error::Integrity("DAV property has invalid component")),
    };
    if let Some(index) = link.alarm {
        component
            .children
            .iter_mut()
            .filter(|c| c.name == "VALARM")
            .nth(index)
            .ok_or(Error::Integrity("DAV property refers to a missing alarm"))
    } else {
        Ok(component)
    }
}
pub fn compose(remainder: &str, properties: &[PropertyValue]) -> Result<String> {
    let mut root = parse(remainder)?;
    let mut seen = BTreeSet::new();
    for field in properties {
        if target(&mut root, &field.link)?
            .properties
            .iter()
            .any(|p| p.name == field.link.name)
        {
            return Err(Error::Integrity(
                "DAV property has both native and graph authority",
            ));
        }
    }
    let mut fields: Vec<_> = properties.iter().collect();
    fields.sort_by_key(|f| f.link.position);
    for field in fields {
        let link = &field.link;
        if !seen.insert((
            link.component.clone(),
            link.alarm,
            link.name.clone(),
            link.position,
        )) {
            return Err(Error::Integrity("duplicate DAV property position"));
        }
        let value = match (&link.representation, &field.value) {
            (Representation::Binary, PropertyData::Bytes(v)) => STANDARD.encode(v),
            (Representation::DataUri, PropertyData::Bytes(v)) => format!(
                "data:{};base64,{}",
                link.media_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
                STANDARD.encode(v)
            ),
            (Representation::Text, PropertyData::Text(v)) => escape(v),
            (Representation::Uri | Representation::Reference, PropertyData::Text(v)) => v.clone(),
            _ => {
                return Err(Error::Integrity(
                    "DAV property value has wrong representation",
                ));
            }
        };
        target(&mut root, link)?.properties.push(Property {
            name: link.name.clone(),
            params: link.params.clone(),
            group: link.group.clone(),
            value,
        });
    }
    Ok(root.wire())
}
