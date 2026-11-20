//! Offline reading projections; authored body Blocks retain every original byte.
use regex::Regex;
use scraper::{Html, Node, Selector};
pub fn clean_link(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.into();
    };
    if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() {
        return value.into();
    }
    let keys: Vec<_> = url
        .query_pairs()
        .map(|(key, _)| key.to_lowercase())
        .collect();
    if keys.iter().any(|key| {
        ["signature", "sig", "x-amz-signature", "x-goog-signature"].contains(&key.as_str())
    }) {
        return value.into();
    }
    let Some(query) = url.query() else {
        return value.into();
    };
    let kept: Vec<_> = query
        .split('&')
        .filter(|item| {
            let key = item.split('=').next().unwrap_or("");
            let key = url::form_urlencoded::parse(key.as_bytes())
                .next()
                .map(|(key, _)| key.to_lowercase())
                .unwrap_or_default();
            !key.starts_with("utm_")
                && !["fbclid", "gclid", "mc_cid", "mc_eid"].contains(&key.as_str())
        })
        .collect();
    let retained = kept.join("&");
    if query == retained {
        return value.into();
    }
    url.set_query((!retained.is_empty()).then_some(retained.as_str()));
    url.into()
}
fn control() -> Regex {
    Regex::new(r"(?i)^(?:unsubscribe(?: from (?:this list|these emails))?|manage (?:your )?(?:email )?(?:preferences|subscription)|(?:email|subscription) preferences|update your email preferences or unsubscribe here|click here to unsubscribe\.?|read online|share (?:on|via) (?:facebook|twitter|x|threads|linkedin)|view (?:this email |this message )?in (?:your )?browser|退订|取消订阅|管理订阅|管理订阅偏好|在浏览器中(?:查看|阅读))$").expect("static regex")
}
pub fn text(source: &str) -> String {
    let controls = control();
    let links = Regex::new(r"https?://[^\s<>]+").expect("static regex");
    source
        .lines()
        .filter(|line| {
            let label = line.trim();
            if controls.is_match(label) {
                return false;
            }
            if let Some(link) = links.find(label)
                && controls.is_match(label[..link.start()].trim().trim_end_matches(':').trim())
            {
                return false;
            }
            true
        })
        .map(|line| {
            links
                .replace_all(line, |m: &regex::Captures<'_>| clean_link(&m[0]))
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .into()
}
pub fn html(source: &str) -> String {
    let mut document = Html::parse_document(source);
    let selector = Selector::parse("*").expect("static selector");
    let controls = control();
    let hidden=Regex::new(r"(?i)(?:^|;)\s*(?:display\s*:\s*none|visibility\s*:\s*hidden|opacity\s*:\s*0(?:\.0+)?|max-height\s*:\s*0(?:px)?)\s*(?:!important\s*)?(?:;|$)").expect("static regex");
    let actions: Vec<_> = document
        .select(&selector)
        .map(|element| {
            let value = element.value();
            let name = value.name();
            let pixel = name == "img"
                && ["width", "height"].iter().any(|key| {
                    value
                        .attr(key)
                        .is_some_and(|v| ["0", "1", "0px", "1px"].contains(&v.trim()))
                });
            let label = element
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let remove = ["head", "script", "style", "template"].contains(&name)
                || value.attr("hidden").is_some()
                || value.attr("aria-hidden") == Some("true")
                || hidden.is_match(value.attr("style").unwrap_or(""))
                || pixel
                || (["a", "p"].contains(&name) && controls.is_match(&label));
            (
                element.id(),
                remove,
                (name == "a")
                    .then(|| value.attr("href").map(clean_link))
                    .flatten(),
            )
        })
        .collect();
    for (id, remove, link) in actions {
        if let Some(mut node) = document.tree.get_mut(id) {
            if remove {
                node.detach()
            } else if let Some(link) = link
                && let Node::Element(element) = node.value()
            {
                for (name, value) in &mut element.attrs {
                    if name.local.as_ref() == "href" {
                        *value = link.as_str().into();
                    }
                }
            }
        }
    }
    document
        .select(&Selector::parse("body").expect("static selector"))
        .next()
        .map(|body| body.inner_html())
        .unwrap_or_default()
        .trim()
        .into()
}
pub fn visible(source: &str) -> String {
    Html::parse_fragment(source)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
