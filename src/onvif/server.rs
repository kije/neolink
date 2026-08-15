//! Axum HTTP/SOAP server for the ONVIF bridge.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::onvif::services::{device, events, imaging, media, ptz};
use crate::onvif::snapshot;
use crate::onvif::soap::{
    fault_envelope, is_unauth_allowed, parse_envelope, verify_token, FaultCode,
};
use crate::onvif::state::OnvifState;

/// SOAP responses are always 200 OK with `application/soap+xml; charset=utf-8`,
/// even for SOAP faults. Surveillance VMS clients break otherwise.
const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";

/// Cap on incoming SOAP envelopes. Real ONVIF requests are <8 KiB; 64 KiB is
/// generous, but bounded enough to prevent an attacker from holding a worker
/// with a multi-MiB POST.
const SOAP_BODY_LIMIT: usize = 64 * 1024;

/// How long a connection may sit without sending (the start of) a request
/// before it is closed.
///
/// This is the FD guard. ONVIF SOAP is stateless and clients poll on human
/// timescales, so hyper's default of holding keep-alive connections open
/// indefinitely lets a handful of VMS clients accumulate hundreds of idle FDs
/// until `accept()` starts failing with EMFILE.
///
/// The previous fix for that was to send `Connection: close` on every response,
/// which does bound the FDs but makes every single SOAP call pay a fresh TCP
/// handshake — and a client walks through eight to fifteen of them per poll
/// cycle (`GetCapabilities`, `GetProfiles`, `GetStreamUri`, ...). Bounding the
/// *idle* time instead keeps the FD ceiling and lets that burst share one
/// connection.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Ceiling on simultaneously served connections. Reached only by something
/// pathological; the point is that the bridge applies backpressure (stops
/// accepting until one frees) rather than running the process out of FDs.
const MAX_CONNECTIONS: usize = 256;

/// Pause after a failed `accept()` before trying again. Short enough to be
/// invisible for the transient case, long enough that a persistent failure
/// doesn't become a spin loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How long to let in-flight requests finish after a shutdown is requested.
/// A `PullMessages` long-poll can legitimately be parked for a while, but not
/// forever, and shutdown must not hang on one.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub(crate) async fn run(state: OnvifState, cancel: CancellationToken) -> Result<()> {
    run_with(state, cancel, IDLE_TIMEOUT).await
}

/// `run`, with the idle timeout injectable so tests can exercise the reaping
/// behaviour without waiting out the production value.
async fn run_with(
    state: OnvifState,
    cancel: CancellationToken,
    idle_timeout: Duration,
) -> Result<()> {
    let (bind_addr, bind_port) = {
        let g = state.inner().globals.read().await;
        (g.bind_addr.clone(), g.bind_port)
    };

    let app = Router::new()
        .route("/onvif/:camera/device_service", post(device_service_route))
        .route("/onvif/:camera/media_service", post(media_service_route))
        .route("/onvif/:camera/ptz_service", post(ptz_service_route))
        .route("/onvif/:camera/events_service", post(events_service_route))
        .route(
            "/onvif/:camera/imaging_service",
            post(imaging_service_route),
        )
        .route(
            "/onvif/:camera/subscription/:sub_id",
            post(subscription_route),
        )
        .layer(DefaultBodyLimit::max(SOAP_BODY_LIMIT))
        .route("/onvif/:camera/snapshot/:stream", get(snapshot::handler))
        .route("/onvif/:camera", get(camera_index))
        .route("/", get(root_index))
        .with_state(state);

    let bind: SocketAddr = format!("{bind_addr}:{bind_port}").parse()?;
    log::info!("ONVIF HTTP listening on {bind}");
    let listener = TcpListener::bind(bind).await?;

    // Hand-rolled rather than `axum::serve` because that offers no way to
    // configure hyper's connection behaviour, and the two knobs below are the
    // whole reason keep-alive can be left on. See `IDLE_TIMEOUT`.
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .keep_alive(true)
        .header_read_timeout(Some(idle_timeout));

    let graceful = GracefulShutdown::new();
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    loop {
        // Take the slot before accepting, so a full table leaves the connection
        // queued in the kernel backlog instead of being accepted and dropped.
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            p = limit.clone().acquire_owned() => match p {
                Ok(p) => p,
                // Only if the semaphore is closed, which nothing does.
                Err(_) => break,
            },
        };

        let stream = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => stream,
                Err(e) => {
                    // Per-connection errors (a client that vanished between the
                    // SYN and the accept) must not take the listener down.
                    log::debug!("ONVIF HTTP: accept failed: {e}");
                    // Process-wide conditions like EMFILE fail *immediately* and
                    // keep failing, so retrying without a pause would spin a
                    // core until whatever exhausted the FDs lets go.
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            },
        };

        let conn =
            http.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app.clone()));
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                // Includes the ordinary "client hung up mid-request" case.
                log::debug!("ONVIF HTTP: connection error: {e}");
            }
            drop(permit);
        });
    }

    // Stop accepting, then let what is already in flight drain.
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => {}
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
            log::debug!("ONVIF HTTP: shutdown grace expired with connections still open");
        }
    }
    Ok(())
}

async fn root_index() -> &'static str {
    "neolink ONVIF bridge"
}

async fn camera_index(Path(cam): Path<String>) -> String {
    format!("neolink ONVIF device for camera '{cam}'")
}

async fn device_service_route(
    State(state): State<OnvifState>,
    Path(cam): Path<String>,
    body: String,
) -> Response {
    dispatch_service(state, cam, ServiceKind::Device, body).await
}

async fn media_service_route(
    State(state): State<OnvifState>,
    Path(cam): Path<String>,
    body: String,
) -> Response {
    dispatch_service(state, cam, ServiceKind::Media, body).await
}

async fn ptz_service_route(
    State(state): State<OnvifState>,
    Path(cam): Path<String>,
    body: String,
) -> Response {
    dispatch_service(state, cam, ServiceKind::Ptz, body).await
}

async fn events_service_route(
    State(state): State<OnvifState>,
    Path(cam): Path<String>,
    body: String,
) -> Response {
    dispatch_service(state, cam, ServiceKind::Events, body).await
}

async fn imaging_service_route(
    State(state): State<OnvifState>,
    Path(cam): Path<String>,
    body: String,
) -> Response {
    dispatch_service(state, cam, ServiceKind::Imaging, body).await
}

async fn subscription_route(
    State(state): State<OnvifState>,
    Path((cam_name, sub_id)): Path<(String, String)>,
    body: String,
) -> Response {
    let Some(cam) = state.camera(&cam_name).await else {
        return soap_fault(FaultCode::Other, &format!("Unknown camera '{cam_name}'"));
    };
    let parsed = match parse_envelope(&body) {
        Ok(p) => p,
        Err(e) => return soap_fault(FaultCode::InvalidArgs, &format!("Bad SOAP envelope: {e}")),
    };
    // Per-subscription endpoints require authentication too.
    if let Err(e) = auth_check(&state, &cam, &parsed.auth, &parsed.action).await {
        return soap_fault(FaultCode::NotAuthorized, e.reason());
    }
    match events::dispatch_subscription(&cam, &sub_id, &parsed.action, parsed.body_xml).await {
        Ok(xml) => soap_ok(xml),
        Err(fb) => soap_fault(fb.code, &fb.reason),
    }
}

enum ServiceKind {
    Device,
    Media,
    Ptz,
    Events,
    Imaging,
}

async fn dispatch_service(
    state: OnvifState,
    cam_name: String,
    service: ServiceKind,
    body: String,
) -> Response {
    let Some(cam) = state.camera(&cam_name).await else {
        return soap_fault(FaultCode::Other, &format!("Unknown camera '{cam_name}'"));
    };

    let parsed = match parse_envelope(&body) {
        Ok(p) => p,
        Err(e) => return soap_fault(FaultCode::InvalidArgs, &format!("Bad SOAP envelope: {e}")),
    };

    if let Err(e) = auth_check(&state, &cam, &parsed.auth, &parsed.action).await {
        return soap_fault(FaultCode::NotAuthorized, e.reason());
    }

    let result = match service {
        ServiceKind::Device => {
            device::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await
        }
        ServiceKind::Media => media::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
        ServiceKind::Ptz => ptz::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
        ServiceKind::Events => {
            events::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await
        }
        ServiceKind::Imaging => imaging::dispatch(&cam, &parsed.action, parsed.body_xml).await,
    };

    match result {
        Ok(xml) => soap_ok(xml),
        Err(fb) => soap_fault(fb.code, &fb.reason),
    }
}

/// Distinct auth-failure modes so the dispatcher can return the matching
/// SOAP fault reason. All three map to `NotAuthorized`; the reason string is
/// what differs and is what makes client troubleshooting tractable.
enum AuthError {
    MissingToken,
    BadCredentials,
    UserNotPermitted,
}

impl AuthError {
    fn reason(&self) -> &'static str {
        match self {
            AuthError::MissingToken => "WS-UsernameToken required",
            AuthError::BadCredentials => "Bad credentials",
            AuthError::UserNotPermitted => "User not permitted",
        }
    }
}

/// Returns `Ok(())` if the request is allowed through. Anonymous access is
/// allowed for the small ONVIF discovery whitelist, and for everything when
/// no users / camera ACLs are configured (matching existing RTSP behaviour).
async fn auth_check(
    state: &OnvifState,
    cam: &crate::onvif::state::CameraEntry,
    auth: &Option<crate::onvif::soap::UsernameToken>,
    action: &str,
) -> Result<(), AuthError> {
    if is_unauth_allowed(action) {
        return Ok(());
    }
    let users_empty = state.inner().users.read().await.is_empty();
    let permitted = cam.permitted_users.clone();
    let no_acl = permitted.as_ref().map(|v| v.is_empty()).unwrap_or(true);
    if users_empty && no_acl {
        return Ok(());
    }
    let Some(tok) = auth.as_ref() else {
        return Err(AuthError::MissingToken);
    };
    let users = state.inner().users.read().await.clone();
    if !verify_token(tok, &users) {
        return Err(AuthError::BadCredentials);
    }
    if let Some(allow) = permitted.as_ref() {
        if !allow.is_empty() && !allow.iter().any(|u| u == &tok.username) {
            return Err(AuthError::UserNotPermitted);
        }
    }
    Ok(())
}

// Connections are kept alive; idle ones are reaped by `IDLE_TIMEOUT` in `run`
// rather than by closing after every response.
fn soap_ok(xml: String) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, SOAP_CONTENT_TYPE)],
        xml,
    )
        .into_response()
}

fn soap_fault(code: FaultCode, reason: &str) -> Response {
    let xml = fault_envelope(code, reason, None);
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, SOAP_CONTENT_TYPE)],
        xml,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Boot the server on a free port and hand back the address.
    async fn serve(idle_timeout: Duration) -> (SocketAddr, CancellationToken) {
        // `run` binds the port itself, so find a free one by briefly holding it
        // and then handing the number over.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let globals = crate::config::OnvifGlobalConfig {
            bind_addr: "127.0.0.1".to_string(),
            bind_port: addr.port(),
            ..Default::default()
        };
        let state = OnvifState::new(globals, 8554);

        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = run_with(state, run_cancel, idle_timeout).await;
        });

        // Wait for the listener to come up.
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (addr, cancel)
    }

    async fn read_response(sock: &mut TcpStream) -> String {
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf))
            .await
            .expect("response arrives")
            .expect("socket readable");
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    /// The point of dropping `Connection: close`: a client must be able to send
    /// a second request down the same socket. A VMS walks through eight to
    /// fifteen calls per poll cycle, and paying a TCP handshake for each is the
    /// latency this change exists to remove.
    #[tokio::test]
    async fn a_connection_serves_more_than_one_request() {
        let (addr, cancel) = serve(IDLE_TIMEOUT).await;
        let mut sock = TcpStream::connect(addr).await.unwrap();

        for i in 0..3 {
            sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let resp = read_response(&mut sock).await;
            assert!(
                resp.starts_with("HTTP/1.1 200"),
                "request {} should succeed on the reused connection, got: {}",
                i,
                resp
            );
            assert!(
                !resp.to_ascii_lowercase().contains("connection: close"),
                "request {} should not close the connection, got: {}",
                i,
                resp
            );
        }
        cancel.cancel();
    }

    /// The other half of the bargain: keep-alive is only safe because an idle
    /// connection is eventually reaped, which is what stops a handful of
    /// polling clients from exhausting the bridge's file descriptors.
    #[tokio::test]
    async fn an_idle_connection_is_eventually_closed() {
        // Short enough to keep the test quick; the production value only
        // differs in magnitude.
        let idle = Duration::from_millis(300);
        let (addr, cancel) = serve(idle).await;
        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let resp = read_response(&mut sock).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);

        // Then sit idle. A closed connection reads as EOF (0 bytes).
        let mut buf = vec![0u8; 1024];
        let n = tokio::time::timeout(idle * 20, sock.read(&mut buf))
            .await
            .expect("the server should close an idle connection rather than hold it open")
            .expect("read succeeds");
        assert_eq!(n, 0, "expected EOF, got: {:?}", &buf[..n]);
        cancel.cancel();
    }
}
