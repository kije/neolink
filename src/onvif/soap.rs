//! SOAP envelope construction, parsing, and WS-UsernameToken auth.
//!
//! ONVIF uses SOAP 1.2 over HTTP POST. Every request body is
//! `<env:Envelope><env:Header>...</env:Header><env:Body>...</env:Body></env:Envelope>`.
//!
//! We build outgoing envelopes by string concatenation because we can pin the
//! namespace prefixes that real clients expect (`tds`, `trt`, `tptz`, ...).
//! Going through serde for the whole envelope would force quick-xml to invent
//! prefixes, which a few VMS clients refuse to parse.
//!
//! Incoming envelopes are parsed by reading the XML stream until we get to the
//! first child element of `Body`; that element's local name is the SOAP
//! operation. We then hand the inner XML slice back to the per-operation
//! handler for deserialization.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use quick_xml::{events::Event, Reader};
use sha1::{Digest, Sha1};

/// Result of parsing an incoming SOAP request.
pub(crate) struct ParsedRequest<'a> {
    /// Local name of the body's first child element (the operation).
    pub(crate) action: String,
    /// Optional UsernameToken extracted from the WS-Security header.
    pub(crate) auth: Option<UsernameToken>,
    /// The raw XML of the body element (the input parameters).
    pub(crate) body_xml: &'a str,
}

pub(crate) struct UsernameToken {
    pub(crate) username: String,
    /// Either the plain password (`PasswordText`) or the SHA1 digest.
    pub(crate) credential: Credential,
    /// Optional parsed `Created` timestamp, used for the freshness/drift
    /// check on digest tokens.
    pub(crate) created: Option<DateTime<Utc>>,
    /// The verbatim text of `<wsu:Created>...</wsu:Created>`. We *must* feed
    /// this into the SHA1, not a reformatted version, because the client's
    /// digest used its own string. ISO8601 has many valid representations
    /// (with/without fractional seconds, `Z` vs `+00:00`, ...) and clients
    /// vary.
    pub(crate) created_text: Option<String>,
    /// Optional `Nonce` (base64-encoded raw bytes).
    pub(crate) nonce: Option<Vec<u8>>,
}

pub(crate) enum Credential {
    Plain(String),
    Digest(String),
}

pub(crate) fn parse_envelope(xml: &str) -> Result<ParsedRequest<'_>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // Scan for: WS-Security UsernameToken (in Header) and the first child of Body.
    let mut auth: Option<UsernameToken> = None;
    let mut in_header = false;
    let mut in_security = false;
    let mut in_username_token = false;
    let mut current_user: Option<String> = None;
    let mut current_pw: Option<Credential> = None;
    let mut current_created: Option<DateTime<Utc>> = None;
    let mut current_created_text: Option<String> = None;
    let mut current_nonce: Option<Vec<u8>> = None;
    let mut current_field: Option<&'static str> = None;

    let mut action: Option<String> = None;
    let mut body_span: Option<(usize, usize)> = None;
    let mut in_body = false;
    let mut body_depth = 0u32;
    let mut body_child_start: Option<usize> = None;

    loop {
        let pos = reader.buffer_position() as usize;
        match reader.read_event() {
            Err(e) => return Err(anyhow!("Bad SOAP XML at offset {pos}: {e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().into_inner());
                if in_body {
                    if body_child_start.is_none() {
                        action = Some(name.clone());
                        body_child_start = Some(pos);
                        body_depth = 1;
                    } else {
                        body_depth += 1;
                    }
                    continue;
                }
                match name.as_str() {
                    "Header" => in_header = true,
                    "Security" if in_header => in_security = true,
                    "UsernameToken" if in_security => in_username_token = true,
                    "Username" if in_username_token => current_field = Some("Username"),
                    "Password" if in_username_token => {
                        // Detect Type attribute for digest vs text.
                        let mut is_digest = false;
                        for attr in e.attributes().flatten() {
                            let key = local_name(attr.key.into_inner());
                            if key == "Type" {
                                let v = attr.unescape_value().unwrap_or_default();
                                if v.contains("#PasswordDigest") {
                                    is_digest = true;
                                }
                            }
                        }
                        current_field = Some(if is_digest {
                            "PasswordDigest"
                        } else {
                            "PasswordText"
                        });
                    }
                    "Nonce" if in_username_token => current_field = Some("Nonce"),
                    "Created" if in_username_token => current_field = Some("Created"),
                    "Body" => in_body = true,
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().into_inner());
                if in_body {
                    if body_depth > 0 {
                        body_depth -= 1;
                        if body_depth == 0 {
                            // End tag for the body's first child element.
                            // Position after reading this end tag is the
                            // position of the byte directly after `</...>`.
                            if let Some(start) = body_child_start {
                                let end = reader.buffer_position() as usize;
                                body_span = Some((start, end));
                            }
                            in_body = false;
                        }
                    } else {
                        // End of Body itself.
                        in_body = false;
                    }
                    continue;
                }
                match name.as_str() {
                    "Header" => in_header = false,
                    "Security" => in_security = false,
                    "UsernameToken" => {
                        in_username_token = false;
                        if let (Some(u), Some(c)) = (current_user.take(), current_pw.take()) {
                            auth = Some(UsernameToken {
                                username: u,
                                credential: c,
                                created: current_created.take(),
                                created_text: current_created_text.take(),
                                nonce: current_nonce.take(),
                            });
                        }
                    }
                    "Username" | "Password" | "Nonce" | "Created" => current_field = None,
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().unwrap_or_default().to_string();
                match current_field {
                    Some("Username") => current_user = Some(text),
                    Some("PasswordText") => current_pw = Some(Credential::Plain(text)),
                    Some("PasswordDigest") => current_pw = Some(Credential::Digest(text)),
                    Some("Nonce") => current_nonce = B64.decode(text.as_bytes()).ok(),
                    Some("Created") => {
                        let trimmed = text.trim().to_string();
                        current_created = DateTime::parse_from_rfc3339(&trimmed)
                            .ok()
                            .map(|t| t.with_timezone(&Utc));
                        current_created_text = Some(trimmed);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if in_body && body_child_start.is_none() => {
                let name = local_name(e.name().into_inner());
                action = Some(name);
                let end = reader.buffer_position() as usize;
                body_span = Some((pos, end));
            }
            _ => {}
        }
    }

    let action = action.ok_or_else(|| anyhow!("SOAP body has no operation element"))?;
    let body_xml = if let Some((s, e)) = body_span {
        // Defensive — make sure the range is valid.
        if s <= e && e <= xml.len() {
            &xml[s..e]
        } else {
            ""
        }
    } else {
        ""
    };

    Ok(ParsedRequest {
        action,
        auth,
        body_xml,
    })
}

fn local_name(qname: &[u8]) -> String {
    let s = std::str::from_utf8(qname).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s).to_string()
}

/// Verifies a UsernameToken against the configured user table.
pub(crate) fn verify_token(token: &UsernameToken, users: &HashMap<String, String>) -> bool {
    let Some(stored) = users.get(&token.username) else {
        return false;
    };
    match &token.credential {
        Credential::Plain(p) => constant_time_eq(p.as_bytes(), stored.as_bytes()),
        Credential::Digest(d) => {
            let (Some(nonce), Some(created), Some(created_text)) =
                (&token.nonce, &token.created, &token.created_text)
            else {
                return false;
            };
            // ONVIF guidance: timestamp must be within a few minutes. We use 5.
            let drift = (Utc::now() - *created).num_minutes().abs();
            if drift > 5 {
                return false;
            }
            // Base64(SHA1(nonce + Created_text + password))
            // We use the verbatim Created text the client sent because clients
            // vary in formatting (with/without fractional seconds, `Z` vs
            // `+00:00`, ...) and the SHA1 has to match byte-for-byte.
            let mut hasher = Sha1::new();
            hasher.update(nonce);
            hasher.update(created_text.as_bytes());
            hasher.update(stored.as_bytes());
            let computed = B64.encode(hasher.finalize());
            constant_time_eq(computed.as_bytes(), d.as_bytes())
        }
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Build the outgoing SOAP envelope around an already-rendered body XML
/// fragment. `xmlns_extra` is appended to the Envelope element to provide
/// namespace prefixes used by the body.
pub(crate) fn wrap_envelope(body_xml: &str, xmlns_extra: &str) -> String {
    let mut out = String::with_capacity(body_xml.len() + 512);
    out.push_str(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<env:Envelope xmlns:env=\"http://www.w3.org/2003/05/soap-envelope\" ",
    );
    out.push_str(xmlns_extra);
    out.push_str("><env:Body>");
    out.push_str(body_xml);
    out.push_str("</env:Body></env:Envelope>");
    out
}

/// Standard SOAP namespace declaration set used by every ONVIF service.
pub(crate) const NS_ALL: &str = "xmlns:tt=\"http://www.onvif.org/ver10/schema\" \
xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\" \
xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\" \
xmlns:tptz=\"http://www.onvif.org/ver20/ptz/wsdl\" \
xmlns:wsa=\"http://www.w3.org/2005/08/addressing\" \
xmlns:tns=\"http://www.onvif.org/ver10/topics\" \
xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\" \
xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\"";

/// Build a SOAP Fault envelope.
pub(crate) fn fault_envelope(code: FaultCode, reason: &str, detail: Option<&str>) -> String {
    let subcode = code.subcode();
    let body = format!(
        "<env:Fault>\
<env:Code>\
<env:Value>env:{primary}</env:Value>\
<env:Subcode><env:Value>ter:{sub}</env:Value></env:Subcode>\
</env:Code>\
<env:Reason><env:Text xml:lang=\"en\">{reason}</env:Text></env:Reason>\
{detail}\
</env:Fault>",
        primary = code.primary(),
        sub = subcode,
        reason = xml_escape(reason),
        detail = detail
            .map(|d| format!(
                "<env:Detail><tt:Text>{}</tt:Text></env:Detail>",
                xml_escape(d)
            ))
            .unwrap_or_default()
    );
    wrap_envelope(
        &body,
        &format!("{NS_ALL} xmlns:ter=\"http://www.onvif.org/ver10/error\""),
    )
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum FaultCode {
    NotAuthorized,
    ActionNotSupported,
    InvalidArgs,
    NoAbsolutePtzSpace,
    Other,
}

impl FaultCode {
    fn primary(&self) -> &'static str {
        match self {
            FaultCode::NotAuthorized => "Sender",
            FaultCode::ActionNotSupported => "Receiver",
            FaultCode::InvalidArgs => "Sender",
            FaultCode::NoAbsolutePtzSpace => "Sender",
            FaultCode::Other => "Receiver",
        }
    }
    fn subcode(&self) -> &'static str {
        match self {
            FaultCode::NotAuthorized => "NotAuthorized",
            FaultCode::ActionNotSupported => "ActionNotSupported",
            FaultCode::InvalidArgs => "InvalidArgs",
            FaultCode::NoAbsolutePtzSpace => "NoAbsolutePTZSpace",
            FaultCode::Other => "Action",
        }
    }
}

/// Minimal XML text escaping — the inputs we feed in are model strings, not
/// arbitrary user input, but we still must escape `&` `<` `>` `"`.
pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Operations that ONVIF mandates be reachable without authentication.
pub(crate) fn is_unauth_allowed(action: &str) -> bool {
    matches!(
        action,
        "GetSystemDateAndTime" | "GetCapabilities" | "GetServices" | "GetServiceCapabilities"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_envelope() {
        let xml = r#"<?xml version="1.0"?>
<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Header></env:Header>
  <env:Body>
    <tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>
  </env:Body>
</env:Envelope>"#;
        let parsed = parse_envelope(xml).unwrap();
        assert_eq!(parsed.action, "GetDeviceInformation");
        assert!(parsed.auth.is_none());
    }

    #[test]
    fn parse_with_username_plain() {
        let xml = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Header>
    <wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
      <wsse:UsernameToken>
        <wsse:Username>me</wsse:Username>
        <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordText">mepass</wsse:Password>
      </wsse:UsernameToken>
    </wsse:Security>
  </env:Header>
  <env:Body>
    <tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>
  </env:Body>
</env:Envelope>"#;
        let parsed = parse_envelope(xml).unwrap();
        assert_eq!(parsed.action, "GetDeviceInformation");
        let tok = parsed.auth.expect("token");
        assert_eq!(tok.username, "me");
        match tok.credential {
            Credential::Plain(p) => assert_eq!(p, "mepass"),
            _ => panic!("expected plain"),
        }
    }

    #[test]
    fn fault_round_trip() {
        let f = fault_envelope(FaultCode::NotAuthorized, "Auth required", None);
        assert!(f.contains("ter:NotAuthorized"));
        assert!(f.contains("env:Sender"));
    }

    #[test]
    fn body_xml_extracted() {
        let xml = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Body>
    <trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl">
      <trt:Token>foo</trt:Token>
    </trt:GetProfiles>
  </env:Body>
</env:Envelope>"#;
        let parsed = parse_envelope(xml).unwrap();
        assert_eq!(parsed.action, "GetProfiles");
        assert!(parsed.body_xml.contains("GetProfiles"));
        assert!(parsed.body_xml.contains("foo"));
    }

    #[test]
    fn empty_body_element() {
        let xml = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Body>
    <trt:GetProfiles xmlns:trt="http://www.onvif.org/ver10/media/wsdl"/>
  </env:Body>
</env:Envelope>"#;
        let parsed = parse_envelope(xml).unwrap();
        assert_eq!(parsed.action, "GetProfiles");
    }

    /// Regression: the digest SHA1 must use the verbatim `Created` text the
    /// client sent. Real clients (python-onvif, gsoap, ONVIF Device Manager,
    /// Frigate) commonly omit fractional seconds — `2026-05-23T12:34:56Z` —
    /// so any reformatting of the timestamp breaks digest verification.
    #[test]
    fn digest_uses_verbatim_created_text() {
        use chrono::Utc;
        let password = "mepass";
        let nonce = b"some-random-nonce";
        let created_text = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(); // NO fractional seconds — matches what real clients send
        let nonce_b64 = B64.encode(nonce);

        let mut hasher = Sha1::new();
        hasher.update(nonce);
        hasher.update(created_text.as_bytes());
        hasher.update(password.as_bytes());
        let digest = B64.encode(hasher.finalize());

        let xml = format!(
            r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Header>
    <wsse:Security xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
      <wsse:UsernameToken>
        <wsse:Username>me</wsse:Username>
        <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>
        <wsse:Nonce>{nonce_b64}</wsse:Nonce>
        <wsu:Created xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created_text}</wsu:Created>
      </wsse:UsernameToken>
    </wsse:Security>
  </env:Header>
  <env:Body><tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/></env:Body>
</env:Envelope>"#
        );
        let parsed = parse_envelope(&xml).unwrap();
        let tok = parsed.auth.expect("token");
        let mut users = HashMap::new();
        users.insert("me".to_string(), password.to_string());
        assert!(verify_token(&tok, &users), "digest should verify");
    }

    #[test]
    fn digest_rejects_wrong_password() {
        use chrono::Utc;
        let nonce = b"some-random-nonce";
        let created_text = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let nonce_b64 = B64.encode(nonce);
        let mut hasher = Sha1::new();
        hasher.update(nonce);
        hasher.update(created_text.as_bytes());
        hasher.update(b"wrongpass");
        let digest = B64.encode(hasher.finalize());

        let xml = format!(
            r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope">
  <env:Header><wsse:Security xmlns:wsse="x"><wsse:UsernameToken>
    <wsse:Username>me</wsse:Username>
    <wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</wsse:Password>
    <wsse:Nonce>{nonce_b64}</wsse:Nonce>
    <wsu:Created xmlns:wsu="y">{created_text}</wsu:Created>
  </wsse:UsernameToken></wsse:Security></env:Header>
  <env:Body><tds:Foo xmlns:tds="x"/></env:Body>
</env:Envelope>"#
        );
        let parsed = parse_envelope(&xml).unwrap();
        let tok = parsed.auth.expect("token");
        let mut users = HashMap::new();
        users.insert("me".to_string(), "mepass".to_string());
        assert!(!verify_token(&tok, &users), "digest should reject bad pw");
    }
}
