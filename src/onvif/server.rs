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

pub(crate) async fn run(state: OnvifState, cancel: CancellationToken) -> Result<()> {
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
    if !auth_ok(&state, &cam, &parsed.auth, &parsed.action).await {
        return soap_fault(FaultCode::NotAuthorized, "WS-UsernameToken required");
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

    if !auth_ok(&state, &cam, &parsed.auth, &parsed.action).await {
        return soap_fault(FaultCode::NotAuthorized, "WS-UsernameToken required");
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

/// Returns true if the request is allowed through. Anonymous access is
/// allowed for the small ONVIF discovery whitelist, and for everything when
/// no users / camera ACLs are configured (matching existing RTSP behaviour).
async fn auth_ok(
    state: &OnvifState,
    cam: &crate::onvif::state::CameraEntry,
    auth: &Option<crate::onvif::soap::UsernameToken>,
    action: &str,
) -> bool {
    if is_unauth_allowed(action) {
        return true;
    }
    let users_empty = state.inner().users.read().await.is_empty();
    let permitted = cam.permitted_users.clone();
    let no_acl = permitted.as_ref().map(|v| v.is_empty()).unwrap_or(true);
    if users_empty && no_acl {
        return true;
    }
    let Some(tok) = auth.as_ref() else {
        return false;
    };
    let users = state.inner().users.read().await.clone();
    if !verify_token(tok, &users) {
        return false;
    }
    if let Some(allow) = permitted.as_ref() {
        if !allow.is_empty() && !allow.iter().any(|u| u == &tok.username) {
            return false;
        }
    }
    true
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
