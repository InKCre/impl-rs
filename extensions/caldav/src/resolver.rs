use crate::{Error, Result, codec, graph, schema::*};
use inkcre_core::{Core, knowledge::format_label};
use inkcre_extension_sdk::{Content, ResolverOperation, ResolverRequest};
use serde_json::{Value, json};

pub async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    let content = match &request.input.content {
        Content::Text(v) => v.as_str(),
        Content::Bytes(_) => return Err(Error::Integrity("DAV roots require inline text")),
    };
    if [COLLECTION, LEGACY_COLLECTION].contains(&request.resolver.as_str()) {
        let root: CollectionContent = serde_json::from_str(content)?;
        let text = root
            .props
            .get("D:displayname")
            .unwrap_or(&root.path)
            .clone();
        return match request.operation {
            ResolverOperation::Solve { .. } => Ok(serde_json::to_value(root)?),
            ResolverOperation::Text { .. } => Ok(json!(text)),
            ResolverOperation::Label { .. } => Ok(json!(format_label("DAV", Some(&text), false))),
            _ => Err(Error::Invalid("DAV Resolver has no custom read methods")),
        };
    }
    if ![CALENDAR, CONTACT, LEGACY_CALENDAR, LEGACY_CONTACT].contains(&request.resolver.as_str()) {
        return Err(Error::Invalid("unknown DAV Resolver"));
    }
    let legacy = [LEGACY_CALENDAR, LEGACY_CONTACT].contains(&request.resolver.as_str());
    if matches!(request.operation, ResolverOperation::Label { .. }) {
        let text = if legacy {
            serde_json::from_str::<ResourceContent>(content)?.text
        } else {
            serde_json::from_str::<GraphContent>(content)?.remainder
        };
        let document = codec::parse(&text)?;
        let (kind, text) = if [CONTACT, LEGACY_CONTACT].contains(&request.resolver.as_str()) {
            ("contact".to_owned(), document.value("FN"))
        } else {
            let component = document
                .children
                .iter()
                .find(|c| ["VEVENT", "VTODO", "VJOURNAL"].contains(&c.name.as_str()))
                .ok_or(Error::Integrity(
                    "calendar resource has no supported component",
                ))?;
            (component.name.clone(), component.value("SUMMARY"))
        };
        return Ok(json!(format_label(&kind, Some(&text), false)));
    }
    let solved = if legacy {
        serde_json::from_str::<ResourceContent>(content)?
    } else {
        graph::solve_resource(core, request.input.block.id).await?
    };
    match request.operation {
        ResolverOperation::Solve { .. } => Ok(serde_json::to_value(solved)?),
        ResolverOperation::Text { .. } => Ok(json!(solved.text)),
        _ => Err(Error::Invalid("DAV Resolver has no custom read methods")),
    }
}
