//! WS-Discovery (UDP multicast 239.255.255.250:3702) responder.
//!
//! On startup we send a `Hello` for each enabled camera. On shutdown we send a
//! `Bye`. When a multicast `Probe` arrives, we answer with a `ProbeMatches`
//! that lists every enabled camera and its `XAddrs` URL.
//!
//! The responder is best-effort — if we can't bind the multicast socket
//! (because something else is using port 3702, or because we're in a
//! container without multicast), we log a warning and continue. The HTTP/SOAP
//! server still works; clients can be pointed at the device URL manually.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::onvif::soap::xml_escape;
use crate::onvif::state::{url_path_segment, OnvifState};

const WS_DISCOVERY_PORT: u16 = 3702;
const WS_DISCOVERY_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

pub(crate) async fn run(state: OnvifState, cancel: CancellationToken) -> Result<()> {
    let socket = match build_multicast_socket() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("ONVIF WS-Discovery: cannot bind multicast socket: {e:?}");
            // Sleep until cancelled so the JoinSet stays balanced.
            cancel.cancelled().await;
            return Ok(());
        }
    };
    let socket = Arc::new(socket);

    // Hello on startup for each camera.
    send_hello_for_all(&state, &socket).await;

    let inbound_state = state.clone();
    let inbound_sock = socket.clone();
    let inbound_cancel = cancel.clone();
    let inbound = tokio::spawn(async move {
        loop {
            let mut buf = vec![0u8; 8192];
            tokio::select! {
                _ = inbound_cancel.cancelled() => break,
                r = inbound_sock.recv_from(&mut buf) => {
                    match r {
                        Ok((n, src)) => {
                            let xml = String::from_utf8_lossy(&buf[..n]);
                            if let Some(msg_id) = extract_msg_id(&xml) {
                                if is_probe(&xml) {
                                    handle_probe(&inbound_state, &inbound_sock, src, &msg_id).await;
                                }
                            }
                        }
                        Err(e) => {
                            log::debug!("WS-Discovery recv error: {e:?}");
                        }
                    }
                }
            }
        }
    });

    cancel.cancelled().await;
    inbound.abort();
    send_bye_for_all(&state, &socket).await;
    Ok(())
}

fn build_multicast_socket() -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true).ok();
    sock.set_nonblocking(true)?;
    let bind: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, WS_DISCOVERY_PORT).into();
    sock.bind(&bind.into())
        .context("Binding WS-Discovery UDP socket")?;
    sock.join_multicast_v4(&WS_DISCOVERY_ADDR, &Ipv4Addr::UNSPECIFIED)
        .context("Joining ONVIF discovery multicast group")?;
    sock.set_multicast_loop_v4(true).ok();
    sock.set_multicast_ttl_v4(4).ok();
    let std_sock: std::net::UdpSocket = sock.into();
    let tok = UdpSocket::from_std(std_sock).context("Tokio UDP wrap")?;
    Ok(tok)
}

fn is_probe(xml: &str) -> bool {
    xml.contains("Probe") && !xml.contains("ProbeMatches")
}

fn extract_msg_id(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_id = false;
    loop {
        match reader.read_event() {
            Err(_) | Ok(Event::Eof) => return None,
            Ok(Event::Start(e)) => {
                let name = e.name();
                let s = std::str::from_utf8(name.into_inner()).unwrap_or("");
                if s.rsplit(':').next().unwrap_or(s) == "MessageID" {
                    in_id = true;
                }
            }
            Ok(Event::End(_)) => in_id = false,
            Ok(Event::Text(t)) if in_id => {
                return Some(t.unescape().unwrap_or_default().to_string());
            }
            _ => {}
        }
    }
}

async fn handle_probe(state: &OnvifState, sock: &UdpSocket, src: SocketAddr, relates_to: &str) {
    let Ok(authority) = state.advertise_authority().await else {
        log::debug!("WS-Discovery: no advertise host yet, skipping Probe");
        return;
    };
    for cam in state.all_cameras().await {
        let body = probe_match_envelope(&cam.uuid, &cam.name, &authority, relates_to);
        let _ = sock.send_to(body.as_bytes(), src).await;
    }
}

async fn send_hello_for_all(state: &OnvifState, sock: &UdpSocket) {
    let Ok(authority) = state.advertise_authority().await else {
        return;
    };
    let dst = SocketAddrV4::new(WS_DISCOVERY_ADDR, WS_DISCOVERY_PORT);
    for cam in state.all_cameras().await {
        let body = hello_envelope(&cam.uuid, &cam.name, &authority);
        let _ = sock.send_to(body.as_bytes(), SocketAddr::V4(dst)).await;
    }
}

async fn send_bye_for_all(state: &OnvifState, sock: &UdpSocket) {
    let dst = SocketAddrV4::new(WS_DISCOVERY_ADDR, WS_DISCOVERY_PORT);
    for cam in state.all_cameras().await {
        let body = bye_envelope(&cam.uuid);
        let _ = sock.send_to(body.as_bytes(), SocketAddr::V4(dst)).await;
    }
}

fn scopes_for(cam: &str) -> String {
    let cam = xml_escape(cam);
    format!(
        "onvif://www.onvif.org/type/video_encoder \
onvif://www.onvif.org/Profile/Streaming \
onvif://www.onvif.org/name/{cam} \
onvif://www.onvif.org/hardware/neolink \
onvif://www.onvif.org/location/neolink"
    )
}

fn xaddr(authority: &str, cam: &str) -> String {
    // URL-encode the camera name so names with spaces / special chars produce
    // a syntactically valid URI; then XML-escape what we drop into the
    // SOAP envelope.
    let cam = xml_escape(&url_path_segment(cam));
    format!("http://{authority}/onvif/{cam}/device_service")
}

fn probe_match_envelope(uuid: &Uuid, cam: &str, authority: &str, relates_to: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\">\
<s:Header>\
<a:Action s:mustUnderstand=\"1\">http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</a:Action>\
<a:MessageID>urn:uuid:{msg_id}</a:MessageID>\
<a:RelatesTo>{relates}</a:RelatesTo>\
<a:To s:mustUnderstand=\"1\">http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</a:To>\
</s:Header>\
<s:Body>\
<d:ProbeMatches>\
<d:ProbeMatch>\
<a:EndpointReference><a:Address>urn:uuid:{cam_uuid}</a:Address></a:EndpointReference>\
<d:Types>dn:NetworkVideoTransmitter</d:Types>\
<d:Scopes>{scopes}</d:Scopes>\
<d:XAddrs>{xaddr}</d:XAddrs>\
<d:MetadataVersion>1</d:MetadataVersion>\
</d:ProbeMatch>\
</d:ProbeMatches>\
</s:Body>\
</s:Envelope>",
        msg_id = Uuid::new_v4(),
        relates = xml_escape(relates_to),
        cam_uuid = uuid,
        scopes = scopes_for(cam),
        xaddr = xml_escape(&xaddr(authority, cam)),
    )
}

fn hello_envelope(uuid: &Uuid, cam: &str, authority: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\">\
<s:Header>\
<a:Action s:mustUnderstand=\"1\">http://schemas.xmlsoap.org/ws/2005/04/discovery/Hello</a:Action>\
<a:MessageID>urn:uuid:{msg_id}</a:MessageID>\
<a:To s:mustUnderstand=\"1\">urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To>\
</s:Header>\
<s:Body>\
<d:Hello>\
<a:EndpointReference><a:Address>urn:uuid:{cam_uuid}</a:Address></a:EndpointReference>\
<d:Types>dn:NetworkVideoTransmitter</d:Types>\
<d:Scopes>{scopes}</d:Scopes>\
<d:XAddrs>{xaddr}</d:XAddrs>\
<d:MetadataVersion>1</d:MetadataVersion>\
</d:Hello>\
</s:Body>\
</s:Envelope>",
        msg_id = Uuid::new_v4(),
        cam_uuid = uuid,
        scopes = scopes_for(cam),
        xaddr = xml_escape(&xaddr(authority, cam)),
    )
}

fn bye_envelope(uuid: &Uuid) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:a=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\">\
<s:Header>\
<a:Action s:mustUnderstand=\"1\">http://schemas.xmlsoap.org/ws/2005/04/discovery/Bye</a:Action>\
<a:MessageID>urn:uuid:{msg_id}</a:MessageID>\
<a:To s:mustUnderstand=\"1\">urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To>\
</s:Header>\
<s:Body>\
<d:Bye>\
<a:EndpointReference><a:Address>urn:uuid:{cam_uuid}</a:Address></a:EndpointReference>\
</d:Bye>\
</s:Body>\
</s:Envelope>",
        msg_id = Uuid::new_v4(),
        cam_uuid = uuid,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_probe() {
        assert!(is_probe("<wsd:Probe xmlns:wsd=\"...\"/>"));
        assert!(!is_probe("<wsd:ProbeMatches xmlns:wsd=\"...\"/>"));
        assert!(!is_probe("<wsd:Hello/>"));
    }

    #[test]
    fn extracts_message_id() {
        let xml = r#"<a:MessageID xmlns:a="x">urn:uuid:abc-123</a:MessageID>"#;
        assert_eq!(extract_msg_id(xml).as_deref(), Some("urn:uuid:abc-123"));
    }
}
