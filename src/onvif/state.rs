//! Per-camera ONVIF state and the shared `OnvifState` handed to every handler.
//!
//! All Reolink-side queries are funneled through `NeoInstance::run_task` so the
//! ONVIF code never has to know about reconnection / retries — that's already
//! handled by the camera actor.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::common::{NeoInstance, NeoReactor};
use crate::config::{CameraConfig, Config, OnvifGlobalConfig, StreamConfig};
use neolink_core::bc_protocol::BcCamera;

/// Hard cap for a single SOAP-time camera read. The shared `NeoInstance`
/// retries forever; ONVIF clients shouldn't hang on a flaky camera.
pub(crate) const CAMERA_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The three logical streams a Reolink camera can expose. We mirror the names
/// the existing RTSP module uses on its mount paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum OnvifStream {
    Main,
    Sub,
    Extern,
}

impl OnvifStream {
    pub(crate) fn as_rtsp_path(&self) -> &'static str {
        match self {
            OnvifStream::Main => "main",
            OnvifStream::Sub => "sub",
            OnvifStream::Extern => "extern",
        }
    }

    /// The ONVIF MediaProfile token suffix. Stable across restarts.
    pub(crate) fn token_suffix(&self) -> &'static str {
        match self {
            OnvifStream::Main => "main",
            OnvifStream::Sub => "sub",
            OnvifStream::Extern => "extern",
        }
    }

    pub(crate) fn from_stream_config(s: StreamConfig) -> Vec<OnvifStream> {
        match s {
            StreamConfig::None => vec![],
            StreamConfig::All => {
                vec![OnvifStream::Main, OnvifStream::Sub, OnvifStream::Extern]
            }
            StreamConfig::Both => vec![OnvifStream::Main, OnvifStream::Sub],
            StreamConfig::Main => vec![OnvifStream::Main],
            StreamConfig::Sub => vec![OnvifStream::Sub],
            StreamConfig::Extern => vec![OnvifStream::Extern],
        }
    }

    /// The string the Reolink BC protocol uses for stream identification.
    pub(crate) fn reolink_name(&self) -> &'static str {
        match self {
            OnvifStream::Main => "mainStream",
            OnvifStream::Sub => "subStream",
            OnvifStream::Extern => "externStream",
        }
    }
}

impl CameraEntry {
    /// Run a camera task with a short hard timeout. The shared NeoInstance
    /// retries indefinitely on disconnect; that's the right behaviour for
    /// long-lived MQTT/RTSP loops but it would make ONVIF clients hang while
    /// a camera is offline. ONVIF clients expect quick failure so they can
    /// reconnect.
    pub(crate) async fn run<F, T>(&self, task: F) -> anyhow::Result<T>
    where
        F: for<'a> Fn(
                &'a BcCamera,
            ) -> std::pin::Pin<
                Box<dyn futures::Future<Output = anyhow::Result<T>> + Send + 'a>,
            > + Send
            + Sync,
        T: Send,
    {
        tokio::time::timeout(CAMERA_READ_TIMEOUT, self.instance.run_task(task))
            .await
            .map_err(|_| anyhow::anyhow!("camera read timed out"))?
    }
}

/// Camera entry used by every handler. Cheaply cloneable.
pub(crate) struct CameraEntry {
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) channel_id: u8,
    pub(crate) uuid: Uuid,
    pub(crate) streams: Vec<OnvifStream>,
    pub(crate) permitted_users: Option<Vec<String>>,
    pub(crate) instance: NeoInstance,
    /// Tracks an in-flight continuous-PTZ background task so the next
    /// `Stop` / replacement can abort it cleanly.
    pub(crate) zoom_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

/// Shared state for every ONVIF handler (axum + WS-Discovery).
#[derive(Clone)]
pub(crate) struct OnvifState {
    inner: Arc<OnvifStateInner>,
}

pub(crate) struct OnvifStateInner {
    /// `name` -> `CameraEntry`.
    pub(crate) cameras: RwLock<HashMap<String, Arc<CameraEntry>>>,
    /// Global users for SOAP authentication.
    pub(crate) users: RwLock<HashMap<String, String>>,
    /// The settings used when building stream / device URLs.
    pub(crate) globals: RwLock<OnvifGlobalConfig>,
    /// The neolink RTSP port (so we can build `rtsp://...` URIs).
    pub(crate) rtsp_port: RwLock<u16>,
}

impl OnvifState {
    pub(crate) fn new(globals: OnvifGlobalConfig, rtsp_port: u16) -> Self {
        Self {
            inner: Arc::new(OnvifStateInner {
                cameras: RwLock::new(HashMap::new()),
                users: RwLock::new(HashMap::new()),
                globals: RwLock::new(globals),
                rtsp_port: RwLock::new(rtsp_port),
            }),
        }
    }

    pub(crate) fn inner(&self) -> &OnvifStateInner {
        &self.inner
    }

    pub(crate) async fn camera(&self, name: &str) -> Option<Arc<CameraEntry>> {
        self.inner.cameras.read().await.get(name).cloned()
    }

    pub(crate) async fn all_cameras(&self) -> Vec<Arc<CameraEntry>> {
        self.inner.cameras.read().await.values().cloned().collect()
    }

    /// Reconcile the camera map against the live config. Cameras that have
    /// disappeared (or had `enabled` flipped off) are removed; new ones are
    /// added by attaching to the reactor.
    pub(crate) async fn sync_with_config(
        &self,
        config: &Config,
        reactor: &NeoReactor,
    ) -> Result<()> {
        // Update simple fields first.
        {
            let mut g = self.inner.globals.write().await;
            *g = config.onvif.clone();
        }
        {
            let mut p = self.inner.rtsp_port.write().await;
            *p = config.bind_port;
        }
        {
            let mut users = self.inner.users.write().await;
            users.clear();
            for u in &config.users {
                if let Some(pass) = &u.pass {
                    users.insert(u.name.clone(), pass.clone());
                }
            }
        }

        // Build the new camera set.
        let mut new_set: HashMap<String, Arc<CameraEntry>> = HashMap::new();
        if config.onvif.enabled {
            let existing = self.inner.cameras.read().await.clone();
            for cam_cfg in &config.cameras {
                if !cam_cfg.enabled || !cam_cfg.onvif.enabled {
                    continue;
                }
                let streams = OnvifStream::from_stream_config(cam_cfg.stream);
                if streams.is_empty() {
                    continue;
                }
                let uuid = build_camera_uuid(cam_cfg);
                let permitted_users = cam_cfg.permitted_users.clone();
                let entry = if let Some(prev) = existing.get(&cam_cfg.name) {
                    // Reuse the existing NeoInstance + zoom task handle; just
                    // refresh the descriptor fields that come from config.
                    Arc::new(CameraEntry {
                        name: cam_cfg.name.clone(),
                        channel_id: cam_cfg.channel_id,
                        uuid,
                        streams,
                        permitted_users,
                        instance: prev.instance.clone(),
                        zoom_task: prev.zoom_task.clone(),
                    })
                } else {
                    let instance = reactor
                        .get(&cam_cfg.name)
                        .await
                        .with_context(|| format!("ONVIF: attaching to camera {}", cam_cfg.name))?;
                    Arc::new(CameraEntry {
                        name: cam_cfg.name.clone(),
                        channel_id: cam_cfg.channel_id,
                        uuid,
                        streams,
                        permitted_users,
                        instance,
                        zoom_task: Arc::new(Mutex::new(None)),
                    })
                };
                new_set.insert(cam_cfg.name.clone(), entry);
            }
        }

        let mut cams = self.inner.cameras.write().await;
        *cams = new_set;
        Ok(())
    }

    /// Returns the `host:port` to embed in ONVIF URLs.
    pub(crate) async fn advertise_authority(&self) -> Result<String> {
        let g = self.inner.globals.read().await;
        let port = g.bind_port;
        let host = match g.advertise_host.as_str() {
            "auto" => detect_local_ipv4()?.to_string(),
            other if other.contains(':') => return Ok(other.to_string()),
            other => other.to_string(),
        };
        Ok(format!("{}:{}", host, port))
    }

    /// Returns the `host` to embed in RTSP URLs (no port).
    pub(crate) async fn advertise_host(&self) -> Result<String> {
        let g = self.inner.globals.read().await;
        let host = match g.advertise_host.as_str() {
            "auto" => detect_local_ipv4()?.to_string(),
            other if other.contains(':') => other.split(':').next().unwrap_or(other).to_string(),
            other => other.to_string(),
        };
        Ok(host)
    }

    pub(crate) async fn rtsp_port(&self) -> u16 {
        *self.inner.rtsp_port.read().await
    }

    pub(crate) async fn user_password(&self, name: &str) -> Option<String> {
        self.inner.users.read().await.get(name).cloned()
    }
}

fn build_camera_uuid(cfg: &CameraConfig) -> Uuid {
    match cfg.onvif.uuid.as_str() {
        "auto" => {
            // UUIDv5 over a fixed namespace so we get a stable UUID across
            // restarts without needing user-supplied state. Mixing in the
            // camera_uid (when present) keeps NVR sub-channels distinct.
            let raw = format!(
                "{}|{}|{}",
                cfg.name,
                cfg.camera_uid.as_deref().unwrap_or(""),
                cfg.channel_id
            );
            Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes())
        }
        other => Uuid::parse_str(other).unwrap_or_else(|_| {
            // Bad config — never fail the whole bridge for this; generate a
            // deterministic fallback from the name.
            Uuid::new_v5(&Uuid::NAMESPACE_OID, other.as_bytes())
        }),
    }
}

/// Pick a non-loopback IPv4 address to advertise.
pub(crate) fn detect_local_ipv4() -> Result<IpAddr> {
    use std::net::UdpSocket;
    let s = UdpSocket::bind("0.0.0.0:0")?;
    // Connecting a UDP socket doesn't send anything; it just selects an
    // outbound interface. We use a public reserved address.
    s.connect("192.0.2.1:1")
        .map_err(|e| anyhow!("Failed to detect local IPv4: {e}"))?;
    let addr = s.local_addr()?;
    Ok(addr.ip())
}
