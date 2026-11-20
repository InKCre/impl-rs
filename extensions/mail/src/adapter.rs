//! IMAP BODYSTRUCTURE/MIME parsing uses imap-proto and mailparse.
//! Optional OBJECTID/QRESYNC values stay inside this adapter's narrow raw-command seam.
use crate::session::{Session, Stream};
use crate::{Result, fail, schema::*};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use imap_proto::{BodyStructure, ContentEncoding, SectionPath};
use mailparse::MailHeaderMap;
use regex::Regex;
use std::{
    collections::BTreeSet,
    io::{BufReader, Write},
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
fn remote<E>(_: E) -> inkcre_extension_sdk::RpcError {
    fail("Mail server operation failed")
}

pub struct Adapter {
    session: Session,
    objectid: bool,
    qresync: bool,
}
impl Adapter {
    pub fn connect(parameters: &Parameters) -> Result<Self> {
        let addresses = (parameters.host.as_str(), parameters.port)
            .to_socket_addrs()
            .map_err(remote)?;
        let mut socket = None;
        for address in addresses {
            if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(15)) {
                socket = Some(stream);
                break;
            }
        }
        let mut socket = socket.ok_or_else(|| fail("Cannot connect to Mail server"))?;
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(remote)?;
        socket
            .set_write_timeout(Some(Duration::from_secs(30)))
            .map_err(remote)?;
        let starttls = parameters.security == Security::Starttls;
        if starttls {
            let mut reader = BufReader::new(&mut socket);
            let greeting = crate::session::line(&mut reader)?;
            if !greeting.starts_with(b"* OK") {
                return Err(fail("Invalid IMAP greeting"));
            }
            reader
                .get_mut()
                .write_all(b"S0 STARTTLS\r\n")
                .map_err(remote)?;
            reader.get_mut().flush().map_err(remote)?;
            loop {
                let line = crate::session::line(&mut reader)?;
                if line.starts_with(b"S0 ") {
                    if !line.starts_with(b"S0 OK") {
                        return Err(fail("Server rejected STARTTLS"));
                    }
                    break;
                }
            }
        }
        let transport: Box<dyn Stream> = if parameters.security == Security::Plain {
            Box::new(socket)
        } else {
            // Native trust store, certificate-chain validation and hostname verification remain enabled.
            let mut builder = native_tls::TlsConnector::builder();
            // This is trusted process configuration, never a Source-supplied trust override.
            if let Some(path) = std::env::var_os("SSL_CERT_FILE") {
                let metadata = std::fs::metadata(&path)
                    .map_err(|_| fail("Cannot read Mail TLS trust bundle"))?;
                if metadata.len() > 1024 * 1024 {
                    return Err(fail("Mail TLS trust bundle exceeds 1MiB"));
                }
                let bytes =
                    std::fs::read(path).map_err(|_| fail("Cannot read Mail TLS trust bundle"))?;
                let pem =
                    String::from_utf8(bytes).map_err(|_| fail("Invalid Mail TLS trust bundle"))?;
                let certificates =
                    Regex::new(r"(?s)-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----")
                        .expect("static regex");
                let mut count = 0;
                for certificate in certificates.find_iter(&pem) {
                    let certificate =
                        native_tls::Certificate::from_pem(certificate.as_str().as_bytes())
                            .map_err(|_| fail("Invalid Mail TLS trust certificate"))?;
                    builder.add_root_certificate(certificate);
                    count += 1;
                }
                if count == 0 {
                    return Err(fail("Mail TLS trust bundle contains no certificates"));
                }
            }
            let tls = builder.build().map_err(remote)?;
            Box::new(
                tls.connect(parameters.host.trim(), socket)
                    .map_err(remote)?,
            )
        };
        let mut session = Session::new(transport, starttls)?;
        session
            .run_command_and_read_response(format!(
                "LOGIN {} {}",
                quote(&parameters.username)?,
                quote(&parameters.password)?
            ))
            .map_err(|_| fail("Mail authentication failed"))?;
        let capabilities = session.capabilities()?;
        let objectid = capabilities.has_str("OBJECTID");
        let enable = capabilities.has_str("ENABLE") && capabilities.has_str("QRESYNC");
        let qresync = enable
            && session
                .run_command_and_read_response("ENABLE QRESYNC")
                .is_ok();
        Ok(Self {
            session,
            objectid,
            qresync,
        })
    }
    pub fn discover(&mut self, excluded: &Exclusions) -> Result<Vec<(Mailbox, u32)>> {
        let names = self.session.list()?;
        let mut mailboxes = vec![];
        for name in names.iter() {
            let rendered = decode_mailbox(name.name())?;
            let attrs: Vec<String> = name
                .attributes()
                .iter()
                .map(|a| match a {
                    imap_proto::NameAttribute::NoSelect => "\\Noselect".into(),
                    imap_proto::NameAttribute::Extension(v) => v.to_string(),
                    imap_proto::NameAttribute::Sent => "\\Sent".into(),
                    imap_proto::NameAttribute::Drafts => "\\Drafts".into(),
                    imap_proto::NameAttribute::Junk => "\\Junk".into(),
                    imap_proto::NameAttribute::Trash => "\\Trash".into(),
                    imap_proto::NameAttribute::Archive => "\\Archive".into(),
                    _ => String::new(),
                })
                .collect();
            if attrs.iter().any(|a| a.eq_ignore_ascii_case("\\Noselect"))
                || excluded
                    .names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&rendered))
                || attrs.iter().any(|a| {
                    excluded
                        .special_uses
                        .iter()
                        .any(|e| a.eq_ignore_ascii_case(e))
                })
            {
                continue;
            }
            let selected = self.session.examine(name.name())?;
            let epoch = selected
                .uid_validity
                .filter(|v| *v > 0)
                .ok_or_else(|| fail("Mailbox UIDVALIDITY missing"))?;
            let mailbox_id = if self.objectid {
                self.session
                    .run_command_and_read_response(format!(
                        "STATUS {} (MAILBOXID)",
                        quote(name.name())?
                    ))
                    .ok()
                    .and_then(|raw| capture(&raw, r#"(?i)MAILBOXID\s+\(\s*"?([^\s\)"]+)"#))
            } else {
                None
            };
            let mut special_uses: Vec<_> = attrs
                .into_iter()
                .filter(|a| {
                    ["\\Sent", "\\Drafts", "\\Junk", "\\Trash", "\\Archive"].contains(&a.as_str())
                })
                .collect();
            special_uses.sort();
            mailboxes.push((
                Mailbox {
                    name: rendered,
                    special_uses,
                    mailbox_id,
                },
                epoch,
            ));
        }
        Ok(mailboxes)
    }
    pub fn read(
        &mut self,
        mailbox: &Mailbox,
        previous: Option<&Checkpoint>,
        horizon: DateTime<Utc>,
        backfill: Option<&Backfill>,
    ) -> Result<Batch> {
        let name = encode_mailbox(&mailbox.name);
        let selected = self.session.select(&name)?;
        let epoch = selected
            .uid_validity
            .filter(|v| *v > 0)
            .ok_or_else(|| fail("Mailbox UIDVALIDITY missing"))?;
        let old = previous.filter(|p| p.uid_validity == epoch);
        let mut removed = vec![];
        let mut high = None;
        if self.qresync
            && let Some(previous) = old
            && let Some(modseq) = previous.highest_modseq
        {
            let known = if previous.last_uid == 0 {
                String::new()
            } else {
                format!(" 1:{}", previous.last_uid)
            };
            let raw = self.session.run_command_and_read_response(format!(
                "SELECT {} (QRESYNC ({} {}{}))",
                quote(&name)?,
                epoch,
                modseq,
                known
            ))?;
            high = capture(&raw, r"(?i)HIGHESTMODSEQ\s+(\d+)").and_then(|v| v.parse().ok());
            let re =
                Regex::new(r"(?im)^\* VANISHED(?: \(EARLIER\))? ([0-9,:]+)").expect("static regex");
            for capture in re.captures_iter(&String::from_utf8_lossy(&raw)) {
                removed.extend(uid_set(&capture[1])?)
            }
        }
        if high.is_none()
            && let Ok(raw) = self
                .session
                .run_command_and_read_response(format!("STATUS {} (HIGHESTMODSEQ)", quote(&name)?))
        {
            high = capture(&raw, r"(?i)HIGHESTMODSEQ\s+(\d+)").and_then(|v| v.parse().ok())
        }
        let criteria = if let Some(interval) = backfill {
            format!(
                "SINCE {}{}",
                interval.since.format("%d-%b-%Y"),
                interval
                    .before
                    .map(|d| format!(" BEFORE {}", d.format("%d-%b-%Y")))
                    .unwrap_or_default()
            )
        } else if let Some(previous) = old {
            format!("UID {}:*", previous.last_uid.saturating_add(1))
        } else {
            format!("SINCE {}", horizon.format("%d-%b-%Y"))
        };
        let mut uids: Vec<_> = self.session.uid_search(criteria)?.into_iter().collect();
        uids.sort_unstable();
        if backfill.is_none()
            && let Some(previous) = old
        {
            uids.retain(|uid| *uid > previous.last_uid)
        }
        let mut messages = vec![];
        for uid in &uids {
            let fetched = self.session.uid_fetch(
                uid.to_string(),
                "(UID BODY.PEEK[HEADER] BODYSTRUCTURE FLAGS INTERNALDATE)",
            )?;
            let Some(row) = fetched.iter().find(|row| row.uid() == Some(*uid)) else {
                continue;
            };
            if let Some(date) = row.internal_date().map(|date| date.with_timezone(&Utc))
                && (backfill.is_some_and(|interval| {
                    date.date_naive() < interval.since
                        || interval
                            .before
                            .is_some_and(|before| date.date_naive() >= before)
                }) || (backfill.is_none() && old.is_none() && date < horizon))
            {
                continue;
            }
            let (headers, _) =
                mailparse::parse_headers(row.header().unwrap_or_default()).map_err(remote)?;
            let header = |name: &str| {
                headers
                    .get_first_value(name)
                    .filter(|v| !v.trim().is_empty())
            };
            let id = header("Message-ID")
                .map(|s| message_ids(&s))
                .unwrap_or_default()
                .into_iter()
                .next();
            let mut parts = vec![];
            let structure = row
                .bodystructure()
                .ok_or_else(|| fail("Mail BODYSTRUCTURE missing"))?;
            walk(structure, "", &[], &mut parts)?;
            let flags = row
                .flags()
                .iter()
                .map(ToString::to_string)
                .filter(|v| !v.eq_ignore_ascii_case("\\Recent"))
                .collect();
            let mut message = Message {
                uid: *uid,
                root: Email {
                    message_id: id,
                    email_id: None,
                    subject: header("Subject"),
                    authored_at: header("Date")
                        .and_then(|s| mailparse::dateparse(&s).ok())
                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                },
                parts,
                list: mailing_list(&headers),
                participants: participants(&headers),
                parents: header("In-Reply-To")
                    .map(|s| message_ids(&s))
                    .unwrap_or_default(),
                references: header("References")
                    .map(|s| message_ids(&s))
                    .unwrap_or_default(),
                flags,
            };
            if self.objectid {
                let raw = self
                    .session
                    .run_command_and_read_response(format!("UID FETCH {uid} (EMAILID)"))?;
                message.root.email_id = capture(&raw, r#"(?i)EMAILID\s+\(\s*"?([^\s\)"]+)"#);
            }
            for part in &mut message.parts {
                if part.role == "body" {
                    let bytes = self.part(*uid, &part.id)?;
                    part.text = Some(decode_text(&part.metadata, &bytes)?);
                }
            }
            messages.push(message);
        }
        let mut flags = vec![];
        if backfill.is_none()
            && let Some(previous) = old
            && previous.highest_modseq.is_some()
            && high.is_some()
        {
            let command = format!(
                "UID FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE {})",
                previous.highest_modseq.unwrap_or(0)
            );
            if let Ok(raw) = self.session.run_command_and_read_response(command) {
                let mut rest = raw.as_slice();
                while let Ok((next, response)) = imap_proto::parser::parse_response(rest) {
                    if let imap_proto::Response::Fetch(_, attrs) = response {
                        let uid = attrs.iter().find_map(|a| {
                            if let imap_proto::AttributeValue::Uid(u) = a {
                                Some(*u)
                            } else {
                                None
                            }
                        });
                        let values = attrs.iter().find_map(|a| {
                            if let imap_proto::AttributeValue::Flags(values) = a {
                                Some(
                                    values
                                        .iter()
                                        .filter(|v| !v.eq_ignore_ascii_case("\\Recent"))
                                        .map(|v| v.to_string())
                                        .collect(),
                                )
                            } else {
                                None
                            }
                        });
                        if let (Some(uid), Some(values)) = (uid, values)
                            && !uids.contains(&uid)
                        {
                            flags.push((uid, values));
                        }
                    }
                    if next.len() == rest.len() {
                        break;
                    }
                    rest = next;
                }
            }
        }
        let last_uid = if backfill.is_some() {
            old.map_or(0, |p| p.last_uid)
        } else {
            uids.iter()
                .copied()
                .chain(old.map(|p| p.last_uid))
                .chain(
                    old.is_none()
                        .then_some(selected.uid_next.unwrap_or(1).saturating_sub(1)),
                )
                .max()
                .unwrap_or(0)
        };
        Ok(Batch {
            messages,
            flags,
            removed,
            next: Checkpoint {
                uid_validity: epoch,
                last_uid,
                highest_modseq: high,
            },
        })
    }
    pub fn select_epoch(&mut self, name: &str) -> Result<u32> {
        self.session
            .examine(encode_mailbox(name))?
            .uid_validity
            .ok_or_else(|| fail("UIDVALIDITY missing"))
    }
    pub fn part(&mut self, uid: u32, id: &str) -> Result<Vec<u8>> {
        let path: Vec<u32> = id
            .split('.')
            .map(|v| {
                v.parse::<u32>()
                    .ok()
                    .filter(|v| *v > 0)
                    .ok_or_else(|| fail("Invalid MIME part path"))
            })
            .collect::<Result<_>>()?;
        let result = self
            .session
            .uid_fetch(uid.to_string(), format!("BODY.PEEK[{id}]"))?;
        let bytes = result
            .iter()
            .find(|row| row.uid() == Some(uid))
            .and_then(|row| row.section(&SectionPath::Part(path, None)))
            .ok_or_else(|| fail("MIME part is unavailable"))?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(fail("MIME part exceeds 64MiB"));
        }
        Ok(bytes.to_vec())
    }
    pub fn seen(&mut self, name: &str, epoch: u32, uids: &[u32]) -> Result<()> {
        if self
            .session
            .select(encode_mailbox(name))?
            .uid_validity
            .ok_or_else(|| fail("UIDVALIDITY missing"))?
            != epoch
        {
            return Err(fail("Mailbox UIDVALIDITY changed before marking seen"));
        }
        for uid in uids {
            self.session
                .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Seen)")?;
        }
        Ok(())
    }
}

pub(crate) fn quote(s: &str) -> Result<String> {
    if s.contains(['\r', '\n', '\0']) {
        return Err(fail("Invalid IMAP string"));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
fn capture(bytes: &[u8], pattern: &str) -> Option<String> {
    Regex::new(pattern)
        .expect("static regex")
        .captures(&String::from_utf8_lossy(bytes))
        .map(|v| v[1].trim_matches('"').into())
}
fn uid_set(value: &str) -> Result<Vec<u32>> {
    let mut set = BTreeSet::new();
    for piece in value.split(',') {
        let mut limits = piece.split(':');
        let start: u32 = limits.next().unwrap_or("").parse().map_err(remote)?;
        let end: u32 = limits.next().unwrap_or(piece).parse().map_err(remote)?;
        if end.abs_diff(start) > 1_000_000 {
            return Err(fail("VANISHED range exceeds admission budget"));
        }
        set.extend(start.min(end)..=start.max(end));
    }
    Ok(set.into_iter().collect())
}
fn parameter(params: &imap_proto::BodyParams<'_>, name: &str) -> Option<String> {
    params
        .as_ref()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.to_string())
}
fn walk(
    body: &BodyStructure<'_>,
    path: &str,
    groups: &[String],
    out: &mut Vec<Part>,
) -> Result<()> {
    if out.len() > 4096 || path.split('.').count() > 32 {
        return Err(fail("MIME structure exceeds admission budget"));
    }
    let (common, other) = match body {
        BodyStructure::Multipart { common, bodies, .. } => {
            let mut next = groups.to_vec();
            if common.ty.subtype.eq_ignore_ascii_case("alternative") {
                next.push(if path.is_empty() {
                    "root".into()
                } else {
                    path.into()
                })
            }
            for (i, body) in bodies.iter().enumerate() {
                walk(
                    body,
                    &if path.is_empty() {
                        (i + 1).to_string()
                    } else {
                        format!("{path}.{}", i + 1)
                    },
                    &next,
                    out,
                )?
            }
            return Ok(());
        }
        BodyStructure::Basic { common, other, .. }
        | BodyStructure::Text { common, other, .. }
        | BodyStructure::Message { common, other, .. } => (common, other),
    };
    let disposition = common.disposition.as_ref();
    let filename = disposition
        .and_then(|d| parameter(&d.params, "filename"))
        .or_else(|| parameter(&common.ty.params, "name"));
    let is_attachment = disposition.is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
    let media = format!(
        "{}/{}",
        common.ty.ty.to_lowercase(),
        common.ty.subtype.to_lowercase()
    );
    let role = if !is_attachment
        && filename.is_none()
        && ["text/plain", "text/html"].contains(&media.as_str())
    {
        "body"
    } else if is_attachment {
        "attachment"
    } else {
        "inline"
    };
    let encoding = match &other.transfer_encoding {
        ContentEncoding::SevenBit => "7bit",
        ContentEncoding::EightBit => "8bit",
        ContentEncoding::Binary => "binary",
        ContentEncoding::Base64 => "base64",
        ContentEncoding::QuotedPrintable => "quoted-printable",
        ContentEncoding::Other(v) => v,
    };
    out.push(Part {
        id: if path.is_empty() {
            "1".into()
        } else {
            path.into()
        },
        role: role.into(),
        metadata: MimePart {
            media_type: media,
            charset: parameter(&common.ty.params, "charset"),
            filename: filename.map(|v| decode_header(&v)),
            content_id: other
                .id
                .as_deref()
                .map(|v| v.trim_matches(['<', '>']).into()),
            description: other.description.as_deref().map(decode_header),
            transfer_encoding: Some(encoding.to_lowercase()),
            encoded_size: Some(other.octets),
            content_location: common.location.as_deref().map(str::to_owned),
        },
        text: None,
        alternatives: groups.to_vec(),
    });
    Ok(())
}
fn decode_header(s: &str) -> String {
    mailparse::parse_header(format!("x: {s}").as_bytes())
        .map(|(h, _)| h.get_value())
        .unwrap_or_else(|_| s.into())
}
pub fn decode_bytes(part: &MimePart, raw: &[u8]) -> Result<Vec<u8>> {
    let header = format!(
        "Content-Type: {}; charset={}\r\nContent-Transfer-Encoding: {}\r\n\r\n",
        part.media_type,
        part.charset.as_deref().unwrap_or("utf-8"),
        part.transfer_encoding.as_deref().unwrap_or("7bit")
    );
    let mut message = header.into_bytes();
    message.extend(raw);
    mailparse::parse_mail(&message)
        .map_err(remote)?
        .get_body_raw()
        .map_err(remote)
}
pub fn decode_text(part: &MimePart, raw: &[u8]) -> Result<String> {
    let header = format!(
        "Content-Type: {}; charset={}\r\nContent-Transfer-Encoding: {}\r\n\r\n",
        part.media_type,
        part.charset.as_deref().unwrap_or("utf-8"),
        part.transfer_encoding.as_deref().unwrap_or("7bit")
    );
    let mut message = header.into_bytes();
    message.extend(raw);
    mailparse::parse_mail(&message)
        .map_err(remote)?
        .get_body()
        .map_err(remote)
}
fn message_ids(s: &str) -> Vec<String> {
    Regex::new(r"<\s*([^<>\s]+)\s*>")
        .expect("static regex")
        .captures_iter(s)
        .map(|v| v[1].into())
        .collect()
}
fn participants(headers: &[mailparse::MailHeader<'_>]) -> Vec<Participant> {
    let mut result = vec![];
    for (header, role) in [
        ("From", "from"),
        ("Sender", "sender"),
        ("Reply-To", "reply_to"),
        ("To", "to"),
        ("Cc", "cc"),
        ("Bcc", "bcc"),
    ] {
        let mut order = 0;
        for value in headers.get_all_values(header) {
            if let Ok(addresses) = mailparse::addrparse(&value) {
                let mut flat = vec![];
                for address in addresses.iter() {
                    match address {
                        mailparse::MailAddr::Single(v) => flat.push(v),
                        mailparse::MailAddr::Group(v) => flat.extend(v.addrs.iter()),
                    }
                }
                for address in flat {
                    let index = order;
                    order += 1;
                    if let Some((local, domain)) = address.addr.rsplit_once('@')
                        && !local.is_empty()
                    {
                        let domain = if domain.starts_with('[') && domain.ends_with(']') {
                            Some(domain.to_lowercase())
                        } else {
                            url::Host::parse(domain).ok().map(|host| host.to_string())
                        };
                        if let Some(domain) = domain {
                            result.push(Participant {
                                role,
                                order: index,
                                address: format!("{local}@{domain}"),
                                name: address.display_name.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    result
}
fn mailing_list(headers: &[mailparse::MailHeader<'_>]) -> Option<MailingList> {
    let ids = headers.get_all_values("List-ID");
    if ids.len() == 1 {
        let re = Regex::new(r"^\s*([^<>]*)<([^<>]+)>\s*$").ok()?;
        let capture = re.captures(&ids[0])?;
        let id = capture[2]
            .split_whitespace()
            .collect::<String>()
            .to_lowercase();
        let valid =
            Regex::new(r"^[A-Za-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[A-Za-z0-9!#$%&'*+/=?^_`{|}~-]+)+$")
                .ok()?;
        if id.len() > 255 || !valid.is_match(&id) {
            return None;
        }
        let name = capture[1].trim().trim_matches('"');
        return Some(MailingList {
            list_id: id,
            name: (!name.is_empty()).then(|| decode_header(name)),
        });
    }
    if !ids.is_empty() {
        return None;
    }
    let markers = headers.get_all_values("X-Beehiiv-Type");
    let identities = headers.get_all_values("X-Newsletter-ID");
    if markers.len() != 1
        || !markers[0].trim().eq_ignore_ascii_case("newsletter")
        || identities.len() != 1
    {
        return None;
    }
    let raw = identities[0].trim();
    if raw.chars().any(char::is_whitespace) {
        return None;
    }
    let url = url::Url::parse(raw).ok()?;
    if !["https", "http"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(MailingList {
        list_id: format!("beehiiv:{url}"),
        name: url.host_str().map(str::to_owned),
    })
}
fn decode_mailbox(s: &str) -> Result<String> {
    let mut output = String::new();
    let mut rest = s;
    while let Some((prefix, tail)) = rest.split_once('&') {
        output.push_str(prefix);
        let (encoded, tail) = tail
            .split_once('-')
            .ok_or_else(|| fail("Invalid modified UTF-7 mailbox name"))?;
        if encoded.is_empty() {
            output.push('&')
        } else {
            let bytes = STANDARD
                .decode(format!(
                    "{}{}",
                    encoded.replace(',', "/"),
                    "=".repeat((4 - encoded.len() % 4) % 4)
                ))
                .map_err(remote)?;
            if bytes.len() % 2 != 0 {
                return Err(fail("Invalid modified UTF-7 mailbox name"));
            }
            let words = bytes
                .chunks_exact(2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .collect::<Vec<_>>();
            output.push_str(&String::from_utf16(&words).map_err(remote)?);
        }
        rest = tail;
    }
    output.push_str(rest);
    Ok(output)
}
fn encode_mailbox(s: &str) -> String {
    let mut result = String::new();
    let mut unicode = String::new();
    fn flush(result: &mut String, unicode: &mut String) {
        if !unicode.is_empty() {
            let bytes: Vec<u8> = unicode.encode_utf16().flat_map(u16::to_be_bytes).collect();
            result.push('&');
            result.push_str(
                &STANDARD
                    .encode(bytes)
                    .trim_end_matches('=')
                    .replace('/', ","),
            );
            result.push('-');
            unicode.clear();
        }
    }
    for c in s.chars() {
        if c.is_ascii() && !c.is_ascii_control() {
            flush(&mut result, &mut unicode);
            if c == '&' {
                result.push_str("&-")
            } else {
                result.push(c)
            }
        } else {
            unicode.push(c)
        }
    }
    flush(&mut result, &mut unicode);
    result
}
