//! Bounded RFC recurrence evaluation, separate from HTTP and graph persistence.
use crate::{
    Error, Result,
    codec::{Component, Property},
};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use roxmltree::Node;
use rrule::{RRuleSet, Tz};

type TimeRange = (DateTime<Utc>, DateTime<Utc>);

pub fn datetime(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y%m%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
        })
}
fn range(node: Node<'_, '_>) -> Result<TimeRange> {
    let parse = |name| {
        node.attribute(name)
            .map(|v| {
                datetime(v)
                    .map(|v| v.and_utc())
                    .ok_or(Error::Invalid("invalid DAV query date"))
            })
            .transpose()
    };
    let start = parse("start")?.unwrap_or(DateTime::<Utc>::MIN_UTC);
    let end = parse("end")?.unwrap_or(DateTime::<Utc>::MAX_UTC);
    if start >= end {
        return Err(Error::Invalid("invalid DAV time range"));
    }
    Ok((start, end))
}
fn date(p: &Property) -> Result<DateTime<Utc>> {
    let local = datetime(&p.value).ok_or(Error::Invalid("invalid DAV component date"))?;
    if p.value.ends_with('Z') {
        return Ok(local.and_utc());
    }
    if let Some(zone) = p.params.get("TZID").and_then(|v| v.first()) {
        let prefix = format!("DTSTART;TZID={zone}:{}", p.value);
        let set: RRuleSet = prefix
            .parse()
            .map_err(|_| Error::Invalid("unrecognized DAV time zone"))?;
        return Ok(set.get_dt_start().with_timezone(&Utc));
    }
    Ok(local.and_utc())
}
fn duration(value: &str) -> Result<Duration> {
    let (sign, value) = if let Some(v) = value.strip_prefix('-') {
        (-1, v)
    } else {
        (1, value.trim_start_matches('+'))
    };
    let Some(value) = value.strip_prefix('P') else {
        return Err(Error::Invalid("invalid DAV duration"));
    };
    let mut total = 0i64;
    let mut number = String::new();
    let mut time = false;
    for c in value.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        if c == 'T' && number.is_empty() {
            time = true;
            continue;
        }
        let n: i64 = number
            .parse()
            .map_err(|_| Error::Invalid("invalid DAV duration"))?;
        number.clear();
        let factor = match (c, time) {
            ('W', false) => 604800,
            ('D', false) => 86400,
            ('H', true) => 3600,
            ('M', true) => 60,
            ('S', true) => 1,
            _ => return Err(Error::Invalid("invalid DAV duration unit")),
        };
        total = total
            .checked_add(
                n.checked_mul(factor)
                    .ok_or(Error::Invalid("DAV duration overflow"))?,
            )
            .ok_or(Error::Invalid("DAV duration overflow"))?;
    }
    if !number.is_empty() {
        return Err(Error::Invalid("invalid DAV duration"));
    }
    Duration::try_seconds(total * sign).ok_or(Error::Invalid("DAV duration overflow"))
}
fn extent(component: &Component, first: &Property) -> Result<Duration> {
    if let Some(p) = component
        .properties
        .iter()
        .find(|p| p.name == "DTEND" || p.name == "DUE")
    {
        let length = date(p)? - date(first)?;
        if length < Duration::zero() {
            return Err(Error::Invalid("DAV event ends before its start"));
        }
        return Ok(length);
    }
    if let Some(p) = component.properties.iter().find(|p| p.name == "DURATION") {
        return duration(&p.value);
    }
    Ok(if first.value.len() == 8 {
        Duration::days(1)
    } else {
        Duration::zero()
    })
}
fn recurrence(component: &Component) -> Result<RRuleSet> {
    let mut lines = Vec::new();
    for p in component
        .properties
        .iter()
        .filter(|p| ["DTSTART", "RRULE", "RDATE", "EXDATE"].contains(&p.name.as_str()))
    {
        let mut p = p.clone();
        if p.params
            .get("VALUE")
            .is_some_and(|v| v.iter().any(|v| v == "DATE"))
        {
            p.params.remove("VALUE");
            p.value = p
                .value
                .split(',')
                .map(|v| {
                    if v.len() == 8 {
                        format!("{v}T000000")
                    } else {
                        v.into()
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
        }
        lines.push(p.line());
    }
    lines
        .join("\n")
        .parse()
        .map_err(|_| Error::Invalid("invalid or unsupported DAV recurrence"))
}
pub fn matches(node: Node<'_, '_>, component: &Component) -> Result<bool> {
    let (start, end) = range(node)?;
    let first = component
        .properties
        .iter()
        .find(|p| p.name == "DTSTART")
        .or_else(|| component.properties.iter().find(|p| p.name == "DUE"));
    let Some(first) = first else {
        if component.name != "VTODO" {
            return Ok(false);
        }
        let created = component
            .properties
            .iter()
            .find(|p| p.name == "CREATED")
            .map(date)
            .transpose()?;
        let completed = component
            .properties
            .iter()
            .find(|p| p.name == "COMPLETED")
            .map(date)
            .transpose()?;
        return Ok(match (created, completed) {
            (Some(a), Some(b)) => a <= end && b >= start,
            (Some(a), None) => a < end,
            (None, Some(b)) => start <= b && b < end,
            (None, None) => true,
        });
    };
    let length = extent(component, first)?;
    let overlap = |begins: DateTime<Utc>| {
        let finishes = begins
            .checked_add_signed(length)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        if length == Duration::zero() {
            start <= begins && begins < end
        } else {
            begins < end && finishes > start
        }
    };
    if !component
        .properties
        .iter()
        .any(|p| p.name == "RRULE" || p.name == "RDATE")
    {
        return Ok(overlap(date(first)?));
    }
    let after = start
        .checked_sub_signed(length.max(Duration::zero()))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
        .with_timezone(&Tz::UTC);
    let result = recurrence(component)?
        .after(after)
        .before(end.with_timezone(&Tz::UTC))
        .all(10000);
    if result
        .dates
        .iter()
        .any(|date| overlap(date.with_timezone(&Utc)))
    {
        return Ok(true);
    }
    if result.limited {
        return Err(Error::Invalid(
            "DAV recurrence evaluation exceeds its bounded budget",
        ));
    }
    Ok(false)
}
/// Exclude overridden original instances from the master rule while querying each override itself.
pub fn query_document(document: &Component) -> Component {
    let mut document = document.clone();
    let overrides: Vec<_> = document
        .children
        .iter()
        .filter_map(|c| {
            c.properties
                .iter()
                .find(|p| p.name == "RECURRENCE-ID")
                .map(|p| (c.name.clone(), c.value("UID"), p.clone()))
        })
        .collect();
    for c in &mut document.children {
        if c.properties.iter().any(|p| p.name == "RECURRENCE-ID") {
            continue;
        }
        for (name, uid, p) in &overrides {
            if c.name == *name && c.value("UID") == *uid {
                let mut p = p.clone();
                p.name = "EXDATE".into();
                c.properties.push(p);
            }
        }
    }
    document
}

fn occurrences(
    component: &Component,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let Some(first) = component.properties.iter().find(|p| p.name == "DTSTART") else {
        return Ok(vec![]);
    };
    let length = extent(component, first)?;
    let overlap = |date: DateTime<Utc>| {
        let finish = date
            .checked_add_signed(length)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        (date < end
            && if length.is_zero() {
                start <= date
            } else {
                start < finish
            })
        .then_some((date, finish))
    };
    if component
        .properties
        .iter()
        .any(|p| p.name == "RRULE" || p.name == "RDATE")
    {
        let after = start
            .checked_sub_signed(length.max(Duration::zero()))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let result = recurrence(component)?
            .after(after.with_timezone(&Tz::UTC))
            .before(end.with_timezone(&Tz::UTC))
            .all(10000);
        if result.limited {
            return Err(Error::Invalid(
                "DAV recurrence expansion exceeds its bounded budget",
            ));
        }
        Ok(result
            .dates
            .into_iter()
            .filter_map(|date| overlap(date.with_timezone(&Utc)))
            .collect())
    } else {
        Ok(overlap(date(first)?).into_iter().collect())
    }
}
pub fn expand(text: &str, node: Node<'_, '_>) -> Result<String> {
    let (start, end) = range(node)?;
    let mut root = query_document(&crate::codec::parse(text)?);
    let mut expanded = Vec::new();
    for component in &root.children {
        if !["VEVENT", "VTODO", "VJOURNAL"].contains(&component.name.as_str()) {
            expanded.push(component.clone());
            continue;
        }
        let recurring = component
            .properties
            .iter()
            .any(|p| p.name == "RRULE" || p.name == "RDATE");
        for (start, end) in occurrences(component, start, end)? {
            let mut instance = component.clone();
            instance
                .properties
                .retain(|p| !["RRULE", "RDATE", "EXDATE"].contains(&p.name.as_str()));
            let all_day = component
                .properties
                .iter()
                .find(|p| p.name == "DTSTART")
                .is_some_and(|p| p.value.len() == 8);
            let format = if all_day { "%Y%m%d" } else { "%Y%m%dT%H%M%SZ" };
            for p in &mut instance.properties {
                if p.name == "DTSTART" || p.name == "DTEND" || p.name == "DUE" {
                    p.value = if p.name == "DTSTART" { start } else { end }
                        .format(format)
                        .to_string();
                    p.params.remove("TZID");
                }
            }
            if recurring
                && !instance
                    .properties
                    .iter()
                    .any(|p| p.name == "RECURRENCE-ID")
            {
                let mut params = std::collections::BTreeMap::new();
                if all_day {
                    params.insert("VALUE".into(), vec!["DATE".into()]);
                }
                instance.properties.push(Property {
                    name: "RECURRENCE-ID".into(),
                    params,
                    group: None,
                    value: start.format(format).to_string(),
                });
            }
            expanded.push(instance);
        }
    }
    root.children = expanded;
    Ok(root.wire())
}
pub fn free_busy(items: impl Iterator<Item = String>, node: Node<'_, '_>) -> Result<String> {
    let (start, end) = range(node)?;
    let mut grouped = std::collections::BTreeMap::<&str, Vec<TimeRange>>::new();
    for text in items {
        let root = query_document(&crate::codec::parse(&text)?);
        for component in root.children.iter().filter(|c| c.name == "VEVENT") {
            if !["", "OPAQUE"].contains(
                &component
                    .value("TRANSP")
                    .trim()
                    .to_ascii_uppercase()
                    .as_str(),
            ) {
                continue;
            }
            let status = component.value("STATUS").trim().to_ascii_uppercase();
            let kind = match status.as_str() {
                "CANCELLED" => "FREE",
                "TENTATIVE" => "BUSY-TENTATIVE",
                _ => "BUSY",
            };
            for (begins, finishes) in occurrences(component, start, end)? {
                let clipped = (begins.max(start), finishes.min(end));
                if clipped.0 < clipped.1 {
                    grouped.entry(kind).or_default().push(clipped);
                }
            }
        }
    }
    let mut periods = Vec::new();
    for (kind, mut ranges) in grouped {
        ranges.sort();
        let mut merged: Vec<TimeRange> = Vec::new();
        for (begins, finishes) in ranges {
            if let Some(previous) = merged.last_mut()
                && begins <= previous.1
            {
                previous.1 = previous.1.max(finishes);
            } else {
                merged.push((begins, finishes));
            }
        }
        periods.extend(merged.into_iter().map(|(a, b)| (a, b, kind)));
    }
    periods.sort();
    let mut busy = Component {
        name: "VFREEBUSY".into(),
        properties: vec![],
        children: vec![],
    };
    for (name, value) in [
        ("DTSTAMP", Utc::now().format("%Y%m%dT%H%M%SZ").to_string()),
        ("DTSTART", start.format("%Y%m%dT%H%M%SZ").to_string()),
        ("DTEND", end.format("%Y%m%dT%H%M%SZ").to_string()),
    ] {
        busy.properties.push(Property {
            name: name.into(),
            params: Default::default(),
            group: None,
            value,
        });
    }
    for (start, end, kind) in periods {
        if start < end {
            busy.properties.push(Property {
                name: "FREEBUSY".into(),
                params: std::collections::BTreeMap::from([("FBTYPE".into(), vec![kind.into()])]),
                group: None,
                value: format!(
                    "{}/{}",
                    start.format("%Y%m%dT%H%M%SZ"),
                    end.format("%Y%m%dT%H%M%SZ")
                ),
            });
        }
    }
    let mut root = crate::codec::parse(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//InKCre//CalDAV Rust//EN\r\nEND:VCALENDAR\r\n",
    )?;
    root.children.push(busy);
    Ok(root.wire())
}
