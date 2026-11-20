//! Bounded command framing, independent of the parser's optional-extension vocabulary.
//! imap-proto owns LIST/FETCH/MIME grammar; OBJECTID/QRESYNC remain opaque bytes here.
use crate::{Result, fail};
use chrono::{DateTime, FixedOffset};
use imap_proto::{
    AttributeValue, BodyStructure, MailboxDatum, MessageSection, NameAttribute, Response,
    SectionPath,
};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Read, Write},
};
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}
const MAX_LINE: usize = 64 * 1024;
const MAX_RESPONSE: usize = 65 * 1024 * 1024;
pub fn line(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut line = vec![];
    loop {
        let bytes = reader
            .fill_buf()
            .map_err(|_| fail("IMAP response read failed"))?;
        if bytes.is_empty() {
            return Err(fail("IMAP connection closed"));
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if line.len() + count > MAX_LINE {
            return Err(fail("IMAP response line exceeds 64KiB"));
        }
        let done = bytes[count - 1] == b'\n';
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if done {
            return Ok(line);
        }
    }
}
pub struct Session {
    reader: BufReader<Box<dyn Stream>>,
    tag: u64,
    poisoned: bool,
}
pub struct Capabilities(BTreeSet<String>);
impl Capabilities {
    pub fn has_str(&self, value: &str) -> bool {
        self.0.contains(value)
    }
}
pub struct Selected {
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
}
pub struct Listed {
    name: String,
    attributes: Vec<NameAttribute<'static>>,
}
impl Listed {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn attributes(&self) -> &[NameAttribute<'_>] {
        &self.attributes
    }
}
pub struct Fetched {
    attributes: Vec<AttributeValue<'static>>,
}
impl Fetched {
    pub fn uid(&self) -> Option<u32> {
        self.attributes.iter().find_map(|a| {
            if let AttributeValue::Uid(uid) = a {
                Some(*uid)
            } else {
                None
            }
        })
    }
    pub fn flags(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .find_map(|a| {
                if let AttributeValue::Flags(values) = a {
                    Some(values.iter().map(|v| v.as_ref()).collect())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
    pub fn header(&self) -> Option<&[u8]> {
        self.attributes.iter().find_map(|a| match a {
            AttributeValue::BodySection {
                section: Some(SectionPath::Full(MessageSection::Header)),
                data: Some(data),
                ..
            }
            | AttributeValue::Rfc822Header(Some(data)) => Some(data.as_ref()),
            _ => None,
        })
    }
    pub fn internal_date(&self) -> Option<DateTime<FixedOffset>> {
        self.attributes.iter().find_map(|a| {
            if let AttributeValue::InternalDate(value) = a {
                DateTime::parse_from_str(value, "%d-%b-%Y %H:%M:%S %z").ok()
            } else {
                None
            }
        })
    }
    pub fn bodystructure(&self) -> Option<&BodyStructure<'_>> {
        self.attributes.iter().find_map(|a| {
            if let AttributeValue::BodyStructure(value) = a {
                Some(value)
            } else {
                None
            }
        })
    }
    pub fn section(&self, path: &SectionPath) -> Option<&[u8]> {
        self.attributes.iter().find_map(|a| {
            if let AttributeValue::BodySection {
                section: Some(section),
                data: Some(data),
                ..
            } = a
            {
                (section == path).then_some(data.as_ref())
            } else {
                None
            }
        })
    }
}
impl Session {
    pub fn new(stream: Box<dyn Stream>, greeting_consumed: bool) -> Result<Self> {
        let mut session = Self {
            reader: BufReader::new(stream),
            tag: 0,
            poisoned: false,
        };
        if !greeting_consumed && !line(&mut session.reader)?.starts_with(b"* OK") {
            return Err(fail("Invalid IMAP greeting"));
        }
        Ok(session)
    }
    pub fn run_command_and_read_response(&mut self, command: impl AsRef<str>) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(fail("IMAP session is no longer synchronized"));
        }
        let command = command.as_ref();
        if command.contains(['\r', '\n', '\0']) {
            return Err(fail("Invalid IMAP command"));
        }
        self.tag = self
            .tag
            .checked_add(1)
            .ok_or_else(|| fail("IMAP command counter exhausted"))?;
        let tag = format!("I{}", self.tag);
        self.poisoned = true;
        self.reader
            .get_mut()
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .map_err(|_| fail("IMAP command write failed"))?;
        self.reader
            .get_mut()
            .flush()
            .map_err(|_| fail("IMAP command write failed"))?;
        let mut response = Vec::new();
        let expected = format!("{tag} ");
        loop {
            let line = line(&mut self.reader)?;
            if line.starts_with(expected.as_bytes()) {
                self.poisoned = false;
                if line[expected.len()..].starts_with(b"OK ")
                    || line[expected.len()..].starts_with(b"OK\r\n")
                {
                    return Ok(response);
                }
                return Err(fail("IMAP server rejected the operation"));
            }
            if !line.starts_with(b"*")
                && !line.starts_with(b")")
                && !line.starts_with(b" ")
                && response.is_empty()
            {
                return Err(fail("IMAP completion tag mismatch"));
            }
            if line.starts_with(b"* BYE") {
                return Err(fail("IMAP server closed the session"));
            }
            if response.len() + line.len() > MAX_RESPONSE {
                return Err(fail("IMAP response exceeds 65MiB"));
            }
            response.extend_from_slice(&line);
            let trimmed = line.strip_suffix(b"\r\n").unwrap_or(&line);
            let response_text = [b"* OK ".as_slice(), b"* NO ", b"* BAD ", b"* PREAUTH "]
                .iter()
                .any(|prefix| trimmed.starts_with(prefix));
            if !response_text
                && trimmed.last() == Some(&b'}')
                && let Some(open) = trimmed.iter().rposition(|b| *b == b'{')
            {
                let token = &trimmed[open + 1..trimmed.len() - 1];
                let token = token.strip_suffix(b"+").unwrap_or(token);
                let count = std::str::from_utf8(token)
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(|| fail("Invalid IMAP literal size"))?;
                if count > 64 * 1024 * 1024 || count > MAX_RESPONSE.saturating_sub(response.len()) {
                    return Err(fail("IMAP literal exceeds response budget"));
                }
                let start = response.len();
                response.resize(start + count, 0);
                self.reader
                    .read_exact(&mut response[start..])
                    .map_err(|_| fail("IMAP literal read failed"))?;
            }
        }
    }
    pub fn capabilities(&mut self) -> Result<Capabilities> {
        let raw = self.run_command_and_read_response("CAPABILITY")?;
        Ok(Capabilities(
            String::from_utf8_lossy(&raw)
                .split_whitespace()
                .map(|v| v.to_uppercase())
                .collect(),
        ))
    }
    pub fn list(&mut self) -> Result<Vec<Listed>> {
        let raw = self.run_command_and_read_response("LIST \"\" \"*\"")?;
        Ok(parse(&raw)?
            .into_iter()
            .filter_map(|response| {
                if let Response::MailboxData(MailboxDatum::List {
                    name,
                    name_attributes,
                    ..
                }) = response
                {
                    Some(Listed {
                        name: name.into_owned(),
                        attributes: name_attributes,
                    })
                } else {
                    None
                }
            })
            .collect())
    }
    fn selected(&mut self, name: &str, readonly: bool) -> Result<Selected> {
        let raw = self.run_command_and_read_response(format!(
            "{} {}",
            if readonly { "EXAMINE" } else { "SELECT" },
            super::adapter::quote(name)?
        ))?;
        let capture = |key: &str| {
            regex::Regex::new(&format!(r"(?i)\[{key}\s+(\d+)\]"))
                .expect("static regex")
                .captures(&String::from_utf8_lossy(&raw))
                .and_then(|v| v[1].parse().ok())
        };
        Ok(Selected {
            uid_validity: capture("UIDVALIDITY"),
            uid_next: capture("UIDNEXT"),
        })
    }
    pub fn select(&mut self, name: impl AsRef<str>) -> Result<Selected> {
        self.selected(name.as_ref(), false)
    }
    pub fn examine(&mut self, name: impl AsRef<str>) -> Result<Selected> {
        self.selected(name.as_ref(), true)
    }
    pub fn uid_search(&mut self, criteria: impl AsRef<str>) -> Result<BTreeSet<u32>> {
        let raw =
            self.run_command_and_read_response(format!("UID SEARCH {}", criteria.as_ref()))?;
        Ok(parse(&raw)?
            .into_iter()
            .filter_map(|response| {
                if let Response::MailboxData(MailboxDatum::Search(ids)) = response {
                    Some(ids)
                } else {
                    None
                }
            })
            .flatten()
            .collect())
    }
    pub fn uid_fetch(
        &mut self,
        uid: impl AsRef<str>,
        query: impl AsRef<str>,
    ) -> Result<Vec<Fetched>> {
        let raw = self.run_command_and_read_response(format!(
            "UID FETCH {} {}",
            uid.as_ref(),
            query.as_ref()
        ))?;
        Ok(parse(&raw)?
            .into_iter()
            .filter_map(|response| {
                if let Response::Fetch(_, attributes) = response {
                    Some(Fetched { attributes })
                } else {
                    None
                }
            })
            .collect())
    }
    pub fn uid_store(&mut self, uid: impl AsRef<str>, value: &str) -> Result<()> {
        self.run_command_and_read_response(format!("UID STORE {} {value}", uid.as_ref()))
            .map(|_| ())
    }
}
fn parse(raw: &[u8]) -> Result<Vec<Response<'static>>> {
    let mut rest = raw;
    let mut result = vec![];
    while !rest.is_empty() {
        let (next, response) = imap_proto::parser::parse_response(rest)
            .map_err(|_| fail("Cannot parse IMAP protocol response"))?;
        if next.len() >= rest.len() {
            return Err(fail("IMAP parser made no progress"));
        }
        result.push(response.into_owned());
        rest = next;
    }
    Ok(result)
}
