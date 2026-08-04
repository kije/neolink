//! Axum HTTP/SOAP server for the ONVIF bridge.

use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::onvif::services::{device, events, media, ptz};
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

/// Build the ONVIF router.
///
/// Kept separate from [`run`] so tests can construct it without binding a port.
/// That is not cosmetic: `Router::route` *panics* on a malformed path pattern
/// rather than failing to compile, and axum has changed its path syntax across
/// a major version before. Without a test that reaches this function, the
/// failure mode is a panic on the first ONVIF start, in production.
fn build_router(state: OnvifState) -> Router {
    Router::new()
        .route("/onvif/{camera}/device_service", post(device_service_route))
        .route("/onvif/{camera}/media_service", post(media_service_route))
        .route("/onvif/{camera}/ptz_service", post(ptz_service_route))
        .route("/onvif/{camera}/events_service", post(events_service_route))
        .route(
            "/onvif/{camera}/subscription/{sub_id}",
            post(subscription_route),
        )
        .layer(DefaultBodyLimit::max(SOAP_BODY_LIMIT))
        .route("/onvif/{camera}/snapshot/{stream}", get(snapshot::handler))
        .route("/onvif/{camera}", get(camera_index))
        .route("/", get(root_index))
        .with_state(state)
}

pub(crate) async fn run(state: OnvifState, cancel: CancellationToken) -> Result<()> {
    let (bind_addr, bind_port) = {
        let g = state.inner().globals.read().await;
        (g.bind_addr.clone(), g.bind_port)
    };

    let app = build_router(state);

    let bind: SocketAddr = format!("{bind_addr}:{bind_port}").parse()?;
    log::info!("ONVIF HTTP listening on {bind}");
    let listener = TcpListener::bind(bind).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await?;
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
        ServiceKind::Device => device::dispatch(&state, &cam, &parsed.action).await,
        ServiceKind::Media => media::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
        ServiceKind::Ptz => ptz::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
        ServiceKind::Events => {
            events::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await
        }
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

fn soap_ok(xml: String) -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, SOAP_CONTENT_TYPE),
            // Force-close the TCP connection after each SOAP response. ONVIF
            // SOAP traffic is stateless and very low-rate (clients poll on
            // human timescales), so HTTP keep-alive buys nothing. Letting
            // hyper keep idle connections open for the default 75s lets even
            // a handful of polling VMS clients accumulate hundreds of idle
            // FDs in the bridge until accept() starts failing with EMFILE.
            (axum::http::header::CONNECTION, "close"),
        ],
        xml,
    )
        .into_response()
}

fn soap_fault(code: FaultCode, reason: &str) -> Response {
    let xml = fault_envelope(code, reason, None);
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, SOAP_CONTENT_TYPE),
            (axum::http::header::CONNECTION, "close"),
        ],
        xml,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OnvifGlobalConfig;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn test_state() -> OnvifState {
        OnvifState::new(OnvifGlobalConfig::default(), 8554)
    }

    /// `Router::route` rejects a malformed path pattern by panicking, not by
    /// failing to compile, so simply reaching this function is the assertion.
    #[test]
    fn router_builds() {
        let _ = build_router(test_state());
    }

    /// Send a request through the router and report the status.
    ///
    /// The assertions below deliberately check only "did the router match this
    /// path", not what the handler returned: with no cameras configured the
    /// handlers answer with SOAP faults and 404s of their own, and those are
    /// not what this test is about.
    async fn respond(method: Method, uri: &str) -> (StatusCode, String) {
        let resp = build_router(test_state())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), SOAP_BODY_LIMIT)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        (status, body)
    }

    async fn status_of(method: Method, uri: &str) -> StatusCode {
        respond(method, uri).await.0
    }

    #[tokio::test]
    async fn index_routes_respond() {
        assert_eq!(status_of(Method::GET, "/").await, StatusCode::OK);
        assert_eq!(
            status_of(Method::GET, "/onvif/frontdoor").await,
            StatusCode::OK
        );
    }

    /// Every parameterised route must actually capture its segment. A path
    /// pattern that axum no longer understands shows up here as a 404 from the
    /// router itself.
    #[tokio::test]
    async fn parameterised_routes_match() {
        for uri in [
            "/onvif/frontdoor/device_service",
            "/onvif/frontdoor/media_service",
            "/onvif/frontdoor/ptz_service",
            "/onvif/frontdoor/events_service",
            "/onvif/frontdoor/subscription/sub-1",
        ] {
            let status = status_of(Method::POST, uri).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "router failed to match POST {uri}"
            );
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "router matched POST {uri} to the wrong method"
            );
        }

        // The snapshot route needs the body, not just the status: with no
        // cameras configured its handler answers 404 too, so the status alone
        // cannot tell a matched route from an unmatched one. The router's own
        // 404 has an empty body; this one names the camera it could not find,
        // which also proves both path parameters were captured -- the handler
        // extracts `Path<(String, String)>`, and a pattern axum no longer
        // understands would fail that extraction instead.
        let (status, body) = respond(Method::GET, "/onvif/frontdoor/snapshot/main").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body, "Unknown camera",
            "snapshot route did not reach its handler"
        );
    }

    /// The counterpart: paths we do *not* serve must still 404, so the tests
    /// above cannot pass simply because everything matches everything.
    #[tokio::test]
    async fn unknown_routes_are_not_matched() {
        assert_eq!(
            status_of(Method::GET, "/onvif/frontdoor/nope").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(Method::GET, "/onvif").await,
            StatusCode::NOT_FOUND
        );
    }

    /// The service routes are POST-only, which is worth pinning separately:
    /// it is the property that proves `status_of` above is reading real
    /// routing decisions.
    #[tokio::test]
    async fn service_routes_reject_get() {
        assert_eq!(
            status_of(Method::GET, "/onvif/frontdoor/device_service").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}
