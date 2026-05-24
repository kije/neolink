//! HTTP snapshot proxy. ONVIF clients hit GET /onvif/<camera>/snapshot/<stream>
//! and we turn that into a Reolink BC `get_snapshot()` call.
//!
//! Auth: HTTP Basic against the same user table used for SOAP. SOAP-level
//! WS-UsernameToken doesn't apply to plain GET. This matches what other ONVIF
//! bridges do.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use neolink_core::bc_protocol::BcCamera;

use crate::onvif::soap::constant_time_eq;
use crate::onvif::state::{CameraEntry, OnvifState};

pub(crate) async fn handler(
    State(state): State<OnvifState>,
    Path((cam_name, _stream)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    // Fetch the camera entry once: a config-reload between the auth check and
    // the fetch could otherwise validate against one entry and serve from a
    // different one.
    let Some(cam) = state.camera(&cam_name).await else {
        return (StatusCode::NOT_FOUND, "Unknown camera").into_response();
    };

    if !authenticated(&state, &cam, &headers).await {
        let mut resp = (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"neolink-onvif\"".parse().unwrap(),
        );
        return resp;
    }

    let jpeg = match cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_snapshot().await?) }))
        .await
    {
        Ok(j) => j,
        Err(e) => {
            log::warn!("ONVIF snapshot: camera fetch failed: {e:?}");
            return (StatusCode::BAD_GATEWAY, "Snapshot unavailable").into_response();
        }
    };

    ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response()
}

async fn authenticated(state: &OnvifState, cam: &Arc<CameraEntry>, headers: &HeaderMap) -> bool {
    // No users configured at all → allow (matches RTSP's behaviour).
    let users_empty = state.inner().users.read().await.is_empty();
    let permitted = cam.permitted_users.as_ref();
    let no_acl = permitted.map(|v| v.is_empty()).unwrap_or(true);
    if users_empty && no_acl {
        return true;
    }

    let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(rest) = auth.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = B64.decode(rest.trim()) else {
        return false;
    };
    let s = String::from_utf8_lossy(&decoded);
    let mut split = s.splitn(2, ':');
    let user = split.next().unwrap_or("");
    let pass = split.next().unwrap_or("");

    let Some(expected) = state.user_password(user).await else {
        return false;
    };
    if !constant_time_eq(expected.as_bytes(), pass.as_bytes()) {
        return false;
    }
    if let Some(allow) = permitted {
        if !allow.is_empty() && !allow.iter().any(|u| u == user) {
            return false;
        }
    }
    true
}
