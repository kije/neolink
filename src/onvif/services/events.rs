//! ONVIF Events service (PullPoint subscription model).
//!
//! Two endpoints serve events:
//!
//! * `events_service`  — entry point. Handles `GetServiceCapabilities`,
//!   `GetEventProperties`, and `CreatePullPointSubscription`. The last one
//!   returns a per-subscription URI under `subscription/{id}`.
//! * `subscription/{id}` — per-subscription endpoint. Handles `PullMessages`,
//!   `Renew`, `Unsubscribe`.
//!
//! Only one topic is published: `tns1:VideoSource/MotionAlarm`, fed from the
//! existing motion-detection watcher in `src/common/mdthread.rs` via
//! `NeoInstance::motion()`. See `src/onvif/events.rs` for the manager.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::onvif::events::{Notification, MAX_PULL_TIMEOUT};
use crate::onvif::services::device::FaultBody;
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{CameraEntry, OnvifState};

/// Extra XML namespace declarations needed for event payloads.
///
/// `wsa` is intentionally NOT redeclared here — `NS_ALL` already declares it.
/// Repeating it on the `Envelope` element would emit a duplicate `xmlns:wsa`
/// attribute, which is not well-formed XML and is rejected by stricter
/// ONVIF clients.
const NS_EVENT: &str = "xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" \
xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" \
xmlns:tns1=\"http://www.onvif.org/ver10/topics\"";

/// Handle a request to the per-camera `events_service` endpoint.
pub(crate) async fn dispatch(
    state: &OnvifState,
    cam: &Arc<CameraEntry>,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let body = match action {
        "GetServiceCapabilities" => "<tev:GetServiceCapabilitiesResponse>\
<tev:Capabilities WSSubscriptionPolicySupport=\"false\" \
WSPullPointSupport=\"true\" \
WSPausableSubscriptionManagerInterfaceSupport=\"false\" \
MaxNotificationProducers=\"0\" \
MaxPullPoints=\"32\" \
PersistentNotificationStorage=\"false\"/>\
</tev:GetServiceCapabilitiesResponse>"
            .to_string(),

        "GetEventProperties" => get_event_properties_xml(),

        "CreatePullPointSubscription" => {
            let ttl = parse_initial_termination(body_xml);
            let sub = cam
                .events
                .create_subscription(ttl)
                .await
                .map_err(|e| FaultBody {
                    code: FaultCode::Other,
                    reason: e.to_string(),
                })?;
            let authority = state.advertise_authority().await.map_err(|e| FaultBody {
                code: FaultCode::Other,
                reason: format!("Failed to determine advertise host: {e}"),
            })?;
            let endpoint = format!(
                "http://{authority}/onvif/{cam}/subscription/{sub_id}",
                cam = cam.name,
                sub_id = sub.id,
            );
            create_pullpoint_subscription_response(
                &endpoint,
                sub.created_at,
                sub.termination_time(),
            )
        }

        // The WS-BaseNotification "Subscribe" verb is sometimes hit at this
        // endpoint by clients that don't know about the PullPoint extension.
        // For simplicity we treat it identically to CreatePullPointSubscription.
        "Subscribe" => {
            let ttl = parse_initial_termination(body_xml);
            let sub = cam
                .events
                .create_subscription(ttl)
                .await
                .map_err(|e| FaultBody {
                    code: FaultCode::Other,
                    reason: e.to_string(),
                })?;
            let authority = state.advertise_authority().await.map_err(|e| FaultBody {
                code: FaultCode::Other,
                reason: format!("Failed to determine advertise host: {e}"),
            })?;
            let endpoint = format!(
                "http://{authority}/onvif/{cam}/subscription/{sub_id}",
                cam = cam.name,
                sub_id = sub.id,
            );
            subscribe_response(&endpoint, sub.created_at, sub.termination_time())
        }

        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Events action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, &format!("{NS_ALL} {NS_EVENT}")))
}

/// Handle a request to a per-subscription endpoint (`subscription/{id}`).
pub(crate) async fn dispatch_subscription(
    cam: &Arc<CameraEntry>,
    sub_id: &str,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let Some(sub) = cam.events.get(sub_id).await else {
        return Err(FaultBody {
            code: FaultCode::Other,
            reason: format!("Unknown subscription '{sub_id}'"),
        });
    };

    // Expired? Drop and report.
    if sub.termination_time() < Utc::now() {
        cam.events.remove(sub_id).await;
        return Err(FaultBody {
            code: FaultCode::Other,
            reason: "Subscription has expired".to_string(),
        });
    }

    let body = match action {
        "PullMessages" => {
            let (timeout, limit) = parse_pull_messages(body_xml);
            let msgs = sub.pull(limit, timeout).await;
            let term = sub.termination_time();
            render_pull_messages_response(term, msgs)
        }
        "Renew" => {
            let ttl = parse_termination_time(body_xml).unwrap_or(Duration::from_secs(60));
            let new_term = sub.renew(ttl);
            render_renew_response(sub.created_at, new_term)
        }
        "Unsubscribe" => {
            cam.events.remove(sub_id).await;
            "<wsnt:UnsubscribeResponse/>".to_string()
        }
        // Pause/Resume aren't supported (we don't pause notification generation).
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Subscription action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, &format!("{NS_ALL} {NS_EVENT}")))
}

fn get_event_properties_xml() -> String {
    // We expose a single concrete topic. The TopicNamespaceLocation is the
    // canonical ONVIF topic-namespace URL; clients use it for documentation
    // only — they don't need to fetch it.
    "<tev:GetEventPropertiesResponse>\
<tev:TopicNamespaceLocation>http://www.onvif.org/onvif/ver10/topics/topicns.xml</tev:TopicNamespaceLocation>\
<wsnt:FixedTopicSet>true</wsnt:FixedTopicSet>\
<wstop:TopicSet xmlns:wstop=\"http://docs.oasis-open.org/wsn/t-1\">\
<tns1:VideoSource wstop:topic=\"false\">\
<MotionAlarm wstop:topic=\"true\">\
<tt:MessageDescription IsProperty=\"true\">\
<tt:Source>\
<tt:SimpleItemDescription Name=\"Source\" Type=\"tt:ReferenceToken\"/>\
</tt:Source>\
<tt:Data>\
<tt:SimpleItemDescription Name=\"State\" Type=\"xsd:boolean\"/>\
</tt:Data>\
</tt:MessageDescription>\
</MotionAlarm>\
</tns1:VideoSource>\
</wstop:TopicSet>\
<wsnt:TopicExpressionDialect>http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet</wsnt:TopicExpressionDialect>\
<wsnt:TopicExpressionDialect>http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete</wsnt:TopicExpressionDialect>\
<tev:MessageContentFilterDialect>http://www.onvif.org/ver10/tev/messageContentFilter/ItemFilter</tev:MessageContentFilterDialect>\
<tev:MessageContentSchemaLocation>http://www.onvif.org/onvif/ver10/schema/onvif.xsd</tev:MessageContentSchemaLocation>\
</tev:GetEventPropertiesResponse>"
        .to_string()
}

fn create_pullpoint_subscription_response(
    endpoint: &str,
    created: DateTime<Utc>,
    termination: DateTime<Utc>,
) -> String {
    format!(
        "<tev:CreatePullPointSubscriptionResponse>\
<tev:SubscriptionReference>\
<wsa:Address>{addr}</wsa:Address>\
</tev:SubscriptionReference>\
<wsnt:CurrentTime>{now}</wsnt:CurrentTime>\
<wsnt:TerminationTime>{term}</wsnt:TerminationTime>\
</tev:CreatePullPointSubscriptionResponse>",
        addr = xml_escape(endpoint),
        now = format_dt(created),
        term = format_dt(termination),
    )
}

fn subscribe_response(
    endpoint: &str,
    created: DateTime<Utc>,
    termination: DateTime<Utc>,
) -> String {
    format!(
        "<wsnt:SubscribeResponse>\
<wsnt:SubscriptionReference>\
<wsa:Address>{addr}</wsa:Address>\
</wsnt:SubscriptionReference>\
<wsnt:CurrentTime>{now}</wsnt:CurrentTime>\
<wsnt:TerminationTime>{term}</wsnt:TerminationTime>\
</wsnt:SubscribeResponse>",
        addr = xml_escape(endpoint),
        now = format_dt(created),
        term = format_dt(termination),
    )
}

fn render_renew_response(now: DateTime<Utc>, new_term: DateTime<Utc>) -> String {
    let _ = now;
    format!(
        "<wsnt:RenewResponse>\
<wsnt:TerminationTime>{term}</wsnt:TerminationTime>\
<wsnt:CurrentTime>{now}</wsnt:CurrentTime>\
</wsnt:RenewResponse>",
        term = format_dt(new_term),
        now = format_dt(Utc::now()),
    )
}

fn render_pull_messages_response(term: DateTime<Utc>, msgs: Vec<Notification>) -> String {
    let now = Utc::now();
    let items: String = msgs.iter().map(render_notification).collect();
    format!(
        "<tev:PullMessagesResponse>\
<tev:CurrentTime>{now}</tev:CurrentTime>\
<tev:TerminationTime>{term}</tev:TerminationTime>\
{items}\
</tev:PullMessagesResponse>",
        now = format_dt(now),
        term = format_dt(term),
    )
}

fn render_notification(n: &Notification) -> String {
    format!(
        "<wsnt:NotificationMessage>\
<wsnt:Topic Dialect=\"http://docs.oasis-open.org/wsn/t-1/TopicExpression/Concrete\">{topic}</wsnt:Topic>\
<wsnt:Message>\
<tt:Message UtcTime=\"{ts}\" PropertyOperation=\"{op}\">\
<tt:Source><tt:SimpleItem Name=\"{sname}\" Value=\"{sval}\"/></tt:Source>\
<tt:Data><tt:SimpleItem Name=\"{dname}\" Value=\"{dval}\"/></tt:Data>\
</tt:Message>\
</wsnt:Message>\
</wsnt:NotificationMessage>",
        topic = n.topic,
        ts = format_dt(n.utc_time),
        op = n.property_op,
        sname = n.source_name,
        sval = xml_escape(&n.source_value),
        dname = n.data_name,
        dval = xml_escape(&n.data_value),
    )
}

fn format_dt(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn parse_initial_termination(body_xml: &str) -> Option<Duration> {
    let raw = read_first_text_element(body_xml, "InitialTerminationTime")?;
    parse_termination_string(&raw)
}

fn parse_termination_time(body_xml: &str) -> Option<Duration> {
    let raw = read_first_text_element(body_xml, "TerminationTime")?;
    parse_termination_string(&raw)
}

/// Parse an ONVIF "termination" value. Per WS-BaseNotification this is either
/// an `xs:duration` (relative, like `PT1M`) or an `xs:dateTime` (absolute).
fn parse_termination_string(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.starts_with('P') {
        parse_xs_duration(s)
    } else if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        let now = Utc::now();
        let dur = dt.with_timezone(&Utc) - now;
        if dur.num_seconds() <= 0 {
            None
        } else {
            Some(Duration::from_secs(dur.num_seconds() as u64))
        }
    } else {
        None
    }
}

/// Best-effort xs:duration parser. We only honour years/months/days/hours/
/// minutes/seconds and treat all months as 30 days, years as 365 (ONVIF
/// subscriptions are short-lived anyway and we cap the absolute max).
///
/// Negative xs:durations (`-PT5S` and similar) are rejected — they have no
/// meaningful interpretation as an ONVIF subscription TTL and silently
/// flipping the sign would let a client request an "expired" subscription
/// and still get a live one.
fn parse_xs_duration(s: &str) -> Option<Duration> {
    if !s.starts_with('P') {
        return None;
    }
    let s = s.trim_start_matches('P');
    let mut seconds: i64 = 0;
    let mut in_time = false;
    let mut acc = String::new();
    for c in s.chars() {
        match c {
            'T' => {
                in_time = true;
            }
            '0'..='9' | '.' => acc.push(c),
            unit => {
                let v: f64 = acc.parse().ok()?;
                acc.clear();
                let mult: f64 = match (in_time, unit) {
                    (false, 'Y') => 365.0 * 24.0 * 3600.0,
                    (false, 'M') => 30.0 * 24.0 * 3600.0,
                    (false, 'D') => 24.0 * 3600.0,
                    (false, 'W') => 7.0 * 24.0 * 3600.0,
                    (true, 'H') => 3600.0,
                    (true, 'M') => 60.0,
                    (true, 'S') => 1.0,
                    _ => return None,
                };
                seconds += (v * mult) as i64;
            }
        }
    }
    if seconds <= 0 {
        None
    } else {
        Some(Duration::from_secs(seconds as u64))
    }
}

/// Pull out `Timeout` (xs:duration) and `MessageLimit` (int) from a
/// PullMessages body.
fn parse_pull_messages(body_xml: &str) -> (Duration, usize) {
    let timeout = read_first_text_element(body_xml, "Timeout")
        .and_then(|s| parse_xs_duration(&s))
        .unwrap_or(Duration::from_secs(1));
    let timeout = timeout.min(MAX_PULL_TIMEOUT);
    let limit: usize = read_first_text_element(body_xml, "MessageLimit")
        .and_then(|s| s.trim().parse().ok())
        .map(|v: usize| v.clamp(1, 1024))
        .unwrap_or(16);
    (timeout, limit)
}

/// Action mapping for the per-subscription endpoint. SOAP requests there
/// don't have to declare the namespace prefix we expect; they're routed by
/// local-name only. The body's first child element name is used directly.
#[allow(dead_code)]
pub(crate) fn local_action_only(action: &str) -> &str {
    action.rsplit(':').next().unwrap_or(action)
}

/// Extract the SOAP `wsa:Action` header from an incoming envelope, if any.
/// Used for routing subscription endpoint requests when the body element
/// itself is unhelpful (some clients send everything as `<wsnt:PullMessages>`,
/// in which case the action is the body's local name and we don't need the
/// header).
#[allow(dead_code)]
pub(crate) fn read_wsa_action(envelope: &str) -> Option<String> {
    let mut reader = Reader::from_str(envelope);
    reader.config_mut().trim_text(true);
    let mut in_action = false;
    let mut in_header = false;
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return None,
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == "Header" {
                    in_header = true;
                }
                if in_header && local == "Action" {
                    in_action = true;
                }
            }
            Ok(Event::End(e)) => {
                let name = std::str::from_utf8(e.name().into_inner()).unwrap_or("");
                let local = name.rsplit(':').next().unwrap_or(name);
                if local == "Action" {
                    in_action = false;
                }
                if local == "Header" {
                    in_header = false;
                }
            }
            Ok(Event::Text(t)) if in_action => {
                return Some(t.unescape().unwrap_or_default().to_string());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xs_duration_minutes() {
        assert_eq!(parse_xs_duration("PT1M"), Some(Duration::from_secs(60)));
        assert_eq!(parse_xs_duration("PT30S"), Some(Duration::from_secs(30)));
        assert_eq!(parse_xs_duration("PT1H"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_xs_duration("PT1H30M"),
            Some(Duration::from_secs(5400))
        );
    }

    #[test]
    fn rejects_negative_xs_duration() {
        // A negative TTL has no sensible meaning for an ONVIF subscription;
        // we must not silently treat it as positive.
        assert_eq!(parse_xs_duration("-PT5S"), None);
        assert_eq!(parse_xs_duration("-P1D"), None);
        assert_eq!(parse_termination_string("-PT5S"), None);
    }

    #[test]
    fn parses_initial_termination_relative() {
        let xml = r#"<tev:CreatePullPointSubscription>
            <wsnt:InitialTerminationTime>PT60S</wsnt:InitialTerminationTime>
        </tev:CreatePullPointSubscription>"#;
        assert_eq!(
            parse_initial_termination(xml),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn missing_initial_termination_yields_none() {
        let xml = r#"<tev:CreatePullPointSubscription/>"#;
        assert_eq!(parse_initial_termination(xml), None);
    }

    #[test]
    fn pull_messages_defaults() {
        let xml = r#"<tev:PullMessages/>"#;
        let (t, l) = parse_pull_messages(xml);
        assert_eq!(t, Duration::from_secs(1));
        assert_eq!(l, 16);
    }

    #[test]
    fn pull_messages_with_fields() {
        let xml = r#"<tev:PullMessages>
            <tev:Timeout>PT5S</tev:Timeout>
            <tev:MessageLimit>10</tev:MessageLimit>
        </tev:PullMessages>"#;
        let (t, l) = parse_pull_messages(xml);
        assert_eq!(t, Duration::from_secs(5));
        assert_eq!(l, 10);
    }

    #[test]
    fn extracts_wsa_action() {
        let xml = r#"<env:Envelope xmlns:env="x" xmlns:wsa="y">
            <env:Header>
                <wsa:Action>http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest</wsa:Action>
            </env:Header>
            <env:Body><x:PullMessages/></env:Body>
        </env:Envelope>"#;
        assert!(read_wsa_action(xml).unwrap().contains("PullMessages"));
    }

    #[test]
    fn renders_notification_xml() {
        let n = Notification {
            utc_time: Utc::now(),
            topic: "tns1:VideoSource/MotionAlarm",
            source_name: "Source",
            source_value: "vsrc_cam1".to_string(),
            data_name: "State",
            data_value: "true".to_string(),
            property_op: "Changed",
        };
        let xml = render_notification(&n);
        assert!(xml.contains("tns1:VideoSource/MotionAlarm"));
        assert!(xml.contains("State"));
        assert!(xml.contains("true"));
        assert!(xml.contains("vsrc_cam1"));
    }
}
