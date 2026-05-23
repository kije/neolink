//! Axum HTTP/SOAP server for the ONVIF bridge.

use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::onvif::services::{device, media, ptz};
use crate::onvif::snapshot;
use crate::onvif::soap::{
    fault_envelope, is_unauth_allowed, parse_envelope, verify_token, FaultCode,
};
use crate::onvif::state::OnvifState;

/// SOAP responses are always 200 OK with `application/soap+xml; charset=utf-8`,
/// even for SOAP faults. Surveillance VMS clients break otherwise.
const SOAP_CONTENT_TYPE: &str = "application/soap+xml; charset=utf-8";

pub(crate) async fn run(state: OnvifState, cancel: CancellationToken) -> Result<()> {
    let (bind_addr, bind_port) = {
        let g = state.inner().globals.read().await;
        (g.bind_addr.clone(), g.bind_port)
    };

    let app = Router::new()
        .route("/onvif/:camera/device_service", post(device_service_route))
        .route("/onvif/:camera/media_service", post(media_service_route))
        .route("/onvif/:camera/ptz_service", post(ptz_service_route))
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

enum ServiceKind {
    Device,
    Media,
    Ptz,
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

    // Auth check: skip for the small whitelist of operations that ONVIF
    // explicitly permits anonymously.
    if !is_unauth_allowed(&parsed.action) {
        let users_empty = state.inner().users.read().await.is_empty();
        let permitted = cam.permitted_users.clone();
        let no_acl = permitted.as_ref().map(|v| v.is_empty()).unwrap_or(true);
        // If neither global users nor camera ACL is configured we allow
        // anonymous access (matches the existing RTSP behaviour).
        if !(users_empty && no_acl) {
            let Some(tok) = parsed.auth.as_ref() else {
                return soap_fault(FaultCode::NotAuthorized, "WS-UsernameToken required");
            };
            let users = state.inner().users.read().await.clone();
            if !verify_token(tok, &users) {
                return soap_fault(FaultCode::NotAuthorized, "Bad credentials");
            }
            if let Some(allow) = permitted.as_ref() {
                if !allow.is_empty() && !allow.iter().any(|u| u == &tok.username) {
                    return soap_fault(FaultCode::NotAuthorized, "User not permitted");
                }
            }
        }
    }

    let result = match service {
        ServiceKind::Device => device::dispatch(&state, &cam, &parsed.action).await,
        ServiceKind::Media => media::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
        ServiceKind::Ptz => ptz::dispatch(&state, &cam, &parsed.action, parsed.body_xml).await,
    };

    match result {
        Ok(xml) => soap_ok(xml),
        Err(fb) => soap_fault(fb.code, &fb.reason),
    }
}

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
