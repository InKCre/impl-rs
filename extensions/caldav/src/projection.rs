//! Requested DAV data projections transform only the wire view, never persisted graph values.
use crate::{
    Error, Result,
    codec::{self, Component},
    protocol::{CAL, CARD},
};
use roxmltree::Node;

fn component_projection(component: &mut Component, request: Node<'_, '_>) -> Result<()> {
    if request.children().any(|n| {
        n.is_element() && !["prop", "allprop", "comp", "allcomp"].contains(&n.tag_name().name())
    }) {
        return Err(Error::Invalid("unsupported calendar component projection"));
    }
    let requested: Vec<_> = request
        .children()
        .filter(|n| n.has_tag_name((CAL, "prop")))
        .collect();
    if !request.children().any(|n| n.has_tag_name((CAL, "allprop"))) && !requested.is_empty() {
        component.properties.retain(|p| {
            requested.iter().any(|n| {
                n.attribute("name")
                    .is_some_and(|name| p.name.eq_ignore_ascii_case(name))
            })
        });
        for p in &mut component.properties {
            if requested.iter().any(|n| {
                n.attribute("name")
                    .is_some_and(|name| p.name.eq_ignore_ascii_case(name))
                    && n.attribute("novalue") == Some("yes")
            }) {
                p.value.clear();
            }
        }
    }
    let children: Vec<_> = request
        .children()
        .filter(|n| n.has_tag_name((CAL, "comp")))
        .collect();
    if !request.children().any(|n| n.has_tag_name((CAL, "allcomp"))) && !children.is_empty() {
        component.children.retain(|c| {
            children.iter().any(|n| {
                n.attribute("name")
                    .is_some_and(|name| c.name.eq_ignore_ascii_case(name))
            })
        });
        for c in &mut component.children {
            if let Some(request) = children.iter().find(|n| {
                n.attribute("name")
                    .is_some_and(|name| c.name.eq_ignore_ascii_case(name))
            }) {
                component_projection(c, *request)?;
            }
        }
    }
    Ok(())
}
pub fn data(text: &str, request: Node<'_, '_>) -> Result<Option<String>> {
    if let Some(expand) = request.children().find(|n| n.has_tag_name((CAL, "expand"))) {
        return Ok(Some(crate::temporal::expand(text, expand)?));
    }
    if let Some(selection) = request.children().find(|n| n.has_tag_name((CAL, "comp"))) {
        let mut document = codec::parse(text)?;
        if selection.attribute("name") != Some(document.name.as_str()) {
            return Err(Error::Invalid("calendar data projection root differs"));
        }
        component_projection(&mut document, selection)?;
        return Ok(Some(document.wire()));
    }
    let properties: Vec<_> = request
        .children()
        .filter(|n| n.has_tag_name((CARD, "prop")))
        .collect();
    if !properties.is_empty() {
        let mut document = codec::parse(text)?;
        document.properties.retain(|p| {
            properties.iter().any(|n| {
                n.attribute("name")
                    .is_some_and(|name| p.name.eq_ignore_ascii_case(name))
            })
        });
        return Ok(Some(document.wire()));
    }
    // Canonical Radicale ignores unimplemented data selectors and returns the full value.
    Ok(None)
}
