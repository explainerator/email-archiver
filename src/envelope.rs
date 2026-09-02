//! Extracting index metadata from a raw message.
//!
//! The raw bytes in S3 are authoritative. Everything here is derived, and can
//! be recomputed for every message by re-reading the bucket — which is the same
//! path a rebuild takes. So if these representations turn out to be wrong or
//! insufficient when the IMAP server is built, fixing them is a reindex, not a
//! data-loss event.
//!
//! In particular the `bodystructure` here is a *simplified* MIME tree, not
//! IMAP's exact `BODYSTRUCTURE` wire format. Producing that is the IMAP
//! server's job; this is enough to answer questions about a message without
//! fetching it from S3.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use mail_parser::{Address, MessageParser, MimeHeaders};
use serde_json::{json, Value};

pub struct Indexed {
    pub internaldate: DateTime<Utc>,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub envelope: Value,
    pub bodystructure: Value,
}

/// Parse a raw RFC 5322 message into the fields the index needs.
///
/// `received` is the fallback INTERNALDATE for messages whose `Date:` header is
/// missing or unparseable — twenty years of mail contains plenty of both, and a
/// message with a broken header must still be archived rather than rejected.
pub fn index(raw: &[u8], received: DateTime<Utc>) -> Result<Indexed> {
    let parsed = MessageParser::default()
        .parse(raw)
        .context("message could not be parsed at all")?;

    let internaldate = parsed
        .date()
        .and_then(|d| Utc.timestamp_opt(d.to_timestamp(), 0).single())
        .unwrap_or(received);

    let subject = parsed.subject().map(str::to_string);
    let from_addr = first_address(parsed.from());

    let envelope = json!({
        "date": internaldate.to_rfc3339(),
        "subject": subject,
        "message_id": parsed.message_id(),
        "from": addresses(parsed.from()),
        "sender": addresses(parsed.sender()),
        "reply_to": addresses(parsed.reply_to()),
        "to": addresses(parsed.to()),
        "cc": addresses(parsed.cc()),
        "bcc": addresses(parsed.bcc()),
        "in_reply_to": parsed.in_reply_to().as_text().map(str::to_string),
    });

    let bodystructure = json!({
        "parts": parsed
            .parts
            .iter()
            .map(|part| {
                json!({
                    "content_type": part.content_type().map(|ct| {
                        match ct.subtype() {
                            Some(sub) => format!("{}/{}", ct.ctype(), sub),
                            None => ct.ctype().to_string(),
                        }
                    }),
                    "is_attachment": part.attachment_name().is_some(),
                    "filename": part.attachment_name(),
                    "size": part.len(),
                })
            })
            .collect::<Vec<_>>(),
    });

    Ok(Indexed {
        internaldate,
        subject,
        from_addr,
        envelope,
        bodystructure,
    })
}

fn addresses(addr: Option<&Address>) -> Value {
    let Some(addr) = addr else {
        return json!([]);
    };
    let list: Vec<Value> = addr
        .iter()
        .map(|a| {
            json!({
                "name": a.name().map(str::to_string),
                "email": a.address().map(str::to_string),
            })
        })
        .collect();
    json!(list)
}

fn first_address(addr: Option<&Address>) -> Option<String> {
    addr?.iter().find_map(|a| a.address().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"From: Ken <ken@twoducks.ca>\r\n\
To: Someone <someone@example.com>\r\n\
Subject: Hello there\r\n\
Date: Tue, 1 Sep 2026 12:00:00 +0000\r\n\
Message-ID: <abc@twoducks.ca>\r\n\
\r\n\
Body text.\r\n";

    #[test]
    fn extracts_the_basics() {
        let out = index(SAMPLE, Utc::now()).unwrap();
        assert_eq!(out.subject.as_deref(), Some("Hello there"));
        assert_eq!(out.from_addr.as_deref(), Some("ken@twoducks.ca"));
        assert_eq!(out.envelope["to"][0]["email"], "someone@example.com");
        assert_eq!(out.internaldate.to_rfc3339(), "2026-09-01T12:00:00+00:00");
    }

    #[test]
    fn falls_back_when_date_is_missing() {
        let raw = b"From: a@b.c\r\nSubject: No date\r\n\r\nbody\r\n";
        let received = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let out = index(raw, received).unwrap();
        assert_eq!(out.internaldate, received);
    }

    #[test]
    fn survives_a_message_with_nothing_useful() {
        // Real archives contain messages like this. Losing one because a header
        // is missing would be worse than indexing it with empty metadata.
        let out = index(b"\r\n\r\njust a body", Utc::now()).unwrap();
        assert!(out.subject.is_none());
        assert!(out.from_addr.is_none());
        assert_eq!(out.envelope["to"], json!([]));
    }
}
