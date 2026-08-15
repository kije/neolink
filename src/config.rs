use crate::mqtt::Discoveries;
#[cfg(feature = "gstreamer")]
use neolink_core::bc_protocol::StreamKind;
use neolink_core::bc_protocol::{DiscoveryMethods, PrintFormat};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::clone::Clone;
use std::collections::HashSet;
use validator::Validate;
use validator::ValidationError;

static RE_TLS_CLIENT_AUTH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(none|request|require)$").unwrap());
static RE_PAUSE_MODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(black|still|test|none)$").unwrap());
static RE_MAXENC_SRC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([nN]one|[Aa][Ee][Ss]|[Bb][Cc][Ee][Nn][Cc][Rr][Yy][Pp][Tt])$").unwrap()
});

#[derive(Debug, Deserialize, Serialize, Validate, Clone, PartialEq)]
pub(crate) struct Config {
    #[validate(nested)]
    pub(crate) cameras: Vec<CameraConfig>,

    #[serde(rename = "bind", default = "default_bind_addr")]
    pub(crate) bind_addr: String,

    #[validate(range(min = 0, max = 65535, message = "Invalid port", code = "bind_port"))]
    #[serde(default = "default_bind_port")]
    pub(crate) bind_port: u16,

    #[serde(default = "default_tokio_console")]
    pub(crate) tokio_console: bool,

    #[serde(default = "default_certificate")]
    pub(crate) certificate: Option<String>,

    #[serde(default = "Default::default")]
    pub(crate) mqtt: Option<MqttServerConfig>,

    #[validate(regex(
        path = *RE_TLS_CLIENT_AUTH,
        message = "Incorrect tls auth",
        code = "tls_client_auth"
    ))]
    #[serde(default = "default_tls_client_auth")]
    pub(crate) tls_client_auth: String,

    #[validate(nested)]
    #[serde(default)]
    pub(crate) users: Vec<UserConfig>,

    #[validate(nested)]
    #[serde(default)]
    pub(crate) onvif: OnvifGlobalConfig,

    /// DSCP class applied to every socket that talks to a camera.
    ///
    /// Accepts a number (`44`) or a standard class name (`"VOICE-ADMIT"`,
    /// `"EF"`, `"AF41"`, `"CS5"`, ...). Unset means the traffic is left
    /// unmarked, which is what neolink has always done.
    ///
    /// Note this covers the *whole* camera conversation, not just commands:
    /// Baichuan carries control messages and the video substream on one
    /// connection, so there is nothing finer to mark. See `dscp` in the core
    /// crate.
    #[validate(custom(function = "validate_dscp"))]
    #[serde(default)]
    pub(crate) dscp: Option<String>,
}

/// The value has to resolve at load time, or a typo like `"VOCIE-ADMIT"` would
/// silently leave the traffic unmarked and look like the feature is broken.
fn validate_dscp(v: &str) -> Result<(), ValidationError> {
    if neolink_core::dscp::parse_dscp(v).is_some() {
        return Ok(());
    }
    let mut err = ValidationError::new("dscp");
    err.message = Some(
        "Must be 0-63 or a class name such as VOICE-ADMIT, EF, AF41, CS5"
            .to_string()
            .into(),
    );
    Err(err)
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct OnvifGlobalConfig {
    #[serde(default = "default_false")]
    pub(crate) enabled: bool,

    #[serde(rename = "bind", default = "default_onvif_bind_addr")]
    pub(crate) bind_addr: String,

    #[validate(range(min = 0, max = 65535, message = "Invalid port", code = "bind_port"))]
    #[serde(default = "default_onvif_bind_port")]
    pub(crate) bind_port: u16,

    #[serde(default = "default_true")]
    pub(crate) discovery: bool,

    /// Hostname or `host:port` to advertise in ONVIF responses. Use `"auto"` to
    /// pick the first non-loopback IPv4 address found on the machine.
    #[serde(default = "default_advertise_host")]
    pub(crate) advertise_host: String,
}

impl Default for OnvifGlobalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: default_onvif_bind_addr(),
            bind_port: default_onvif_bind_port(),
            discovery: true,
            advertise_host: default_advertise_host(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct OnvifCameraConfig {
    #[serde(default = "default_true", alias = "enable")]
    pub(crate) enabled: bool,

    /// Fixed UUID for stable ONVIF device identity. `"auto"` derives a UUID
    /// from the camera name via UUIDv5.
    #[serde(default = "default_uuid")]
    pub(crate) uuid: String,
}

impl Default for OnvifCameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            uuid: default_uuid(),
        }
    }
}

fn default_onvif_bind_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_onvif_bind_port() -> u16 {
    8000
}

fn default_advertise_host() -> String {
    "auto".to_string()
}

fn default_uuid() -> String {
    "auto".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
#[validate(schema(function = "validate_mqtt_server", skip_on_field_errors = true))]
pub(crate) struct MqttServerConfig {
    #[serde(alias = "server")]
    pub(crate) broker_addr: String,

    pub(crate) port: u16,

    #[serde(default, skip_serializing)]
    pub(crate) credentials: Option<(String, String)>,

    #[serde(default, skip_serializing)]
    pub(crate) ca: Option<std::path::PathBuf>,

    #[serde(default, skip_serializing)]
    pub(crate) client_auth: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StreamConfig {
    #[serde(alias = "none")]
    None,
    #[serde(alias = "all")]
    All,
    #[serde(alias = "both")]
    Both,
    #[serde(
        alias = "main",
        alias = "mainStream",
        alias = "mainstream",
        alias = "MainStream"
    )]
    Main,
    #[serde(
        alias = "sub",
        alias = "subStream",
        alias = "substream",
        alias = "SubStream"
    )]
    Sub,
    #[serde(
        alias = "extern",
        alias = "externStream",
        alias = "externstream",
        alias = "ExternStream"
    )]
    Extern,
}

impl StreamConfig {
    #[cfg(feature = "gstreamer")]
    pub(crate) fn as_stream_kinds(&self) -> Vec<StreamKind> {
        match self {
            StreamConfig::All => {
                vec![StreamKind::Main, StreamKind::Extern, StreamKind::Sub]
            }
            StreamConfig::Both => {
                vec![StreamKind::Main, StreamKind::Sub]
            }
            StreamConfig::Main => {
                vec![StreamKind::Main]
            }
            StreamConfig::Sub => {
                vec![StreamKind::Sub]
            }
            StreamConfig::Extern => {
                vec![StreamKind::Extern]
            }
            StreamConfig::None => {
                vec![]
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Validate, Clone, PartialEq)]
#[validate(schema(function = "validate_camera_config"))]
pub(crate) struct CameraConfig {
    pub(crate) name: String,

    #[serde(rename = "address")]
    pub(crate) camera_addr: Option<String>,

    #[serde(rename = "uid")]
    pub(crate) camera_uid: Option<String>,

    pub(crate) username: String,

    #[serde(alias = "pass", skip_serializing, default)]
    pub(crate) password: Option<String>,

    #[serde(default = "default_stream")]
    pub(crate) stream: StreamConfig,

    pub(crate) permitted_users: Option<Vec<String>>,

    #[validate(range(min = 0, max = 31, message = "Invalid channel", code = "channel_id"))]
    #[serde(default = "default_channel_id", alias = "channel")]
    pub(crate) channel_id: u8,

    #[validate(nested)]
    #[serde(default = "default_mqtt")]
    pub(crate) mqtt: MqttConfig,

    #[validate(nested)]
    #[serde(default = "default_pause")]
    pub(crate) pause: PauseConfig,

    #[serde(default = "default_discovery")]
    pub(crate) discovery: DiscoveryMethods,

    #[serde(default = "default_maxenc")]
    #[validate(regex(
        path = *RE_MAXENC_SRC,
        message = "Invalid maximum encryption method",
        code = "max_encryption"
    ))]
    pub(crate) max_encryption: String,

    #[serde(default = "default_strict")]
    /// If strict then the media stream will error in the event that the media packets are not as expected
    pub(crate) strict: bool,

    #[serde(default = "default_print", alias = "print")]
    pub(crate) print_format: PrintFormat,

    #[serde(default = "default_update_time", alias = "time")]
    pub(crate) update_time: bool,

    #[validate(range(
        min = 1,
        max = 15000,
        message = "Invalid buffer duration (it's in ms)",
        code = "buffer_duration"
    ))]
    /// Buffer duration in ms. Unset takes the [`Compat`] profile's answer.
    /// Read through [`CameraConfig::buffer_duration`].
    #[serde(default, alias = "duration", alias = "buffer")]
    buffer_duration: Option<u64>,

    #[serde(default = "default_true", alias = "enable")]
    pub(crate) enabled: bool,

    #[serde(default = "default_false", alias = "verbose")]
    pub(crate) debug: bool,

    /// Whether to serve the "Stream not Ready" placeholder while the
    /// camera is being set up. Unset takes the [`Compat`] profile's answer.
    /// Read through [`CameraConfig::use_splash`].
    #[serde(default, alias = "splash")]
    use_splash: Option<bool>,

    #[serde(default = "default_splash", alias = "pattern")]
    pub(crate) splash_pattern: SplashPattern,

    /// How AAC audio is delivered over RTSP. See [`AudioFormat`].
    ///
    /// Unset takes the [`Compat`] profile's answer. Read through
    /// [`CameraConfig::audio_format`].
    ///
    /// Ignored for ADPCM cameras, which have no RTP passthrough format and
    /// are always decoded to L16.
    #[serde(default, alias = "audio", alias = "aud_format")]
    audio_format: Option<AudioFormat>,

    #[serde(
        default = "default_max_discovery_retries",
        alias = "retries",
        alias = "max_retries"
    )]
    pub(crate) max_discovery_retries: usize,

    #[serde(default = "default_true", alias = "push", alias = "push_noti")]
    pub(crate) push_notifications: bool,

    #[serde(default = "default_false", alias = "idle", alias = "idle_disc")]
    pub(crate) idle_disconnect: bool,

    /// Limit the RTSP output to at most this many frames per second.
    /// When set, the ingest path drops excess video frames before they enter
    /// the GStreamer pipeline, reducing both CPU load and RTSP bandwidth.
    /// Audio is never throttled. Strictly opt-in: `null` / omitted (or `0`)
    /// means no limit, and no limiter is constructed at all.
    ///
    /// Frames are dropped rather than re-encoded, so only the tail of a group
    /// of pictures is ever dropped and everything the client receives stays
    /// decodable. The costs are bursty output and a floor at the camera's
    /// keyframe rate. Intended for bandwidth caps and still grabs, not for a
    /// live view. See `GopLimiter` and the README for the full picture.
    #[serde(default, alias = "fps_limit")]
    pub(crate) max_fps: Option<u32>,

    /// Which downstream consumer this camera is being tuned for. Changes
    /// the *defaults* of the settings above; anything set explicitly still
    /// wins. See [`Compat`].
    #[serde(default, alias = "profile", alias = "tuned_for")]
    pub(crate) compat: Compat,

    #[validate(nested)]
    #[serde(default)]
    pub(crate) onvif: OnvifCameraConfig,
}

/// Which downstream consumer a camera is being tuned for.
///
/// neolink's defaults suit lenient end consumers — Blue Iris, VLC, ffmpeg —
/// which retry a 404, sit on a stalled socket for a minute and decode almost
/// any RTP payload format. A republisher like go2rtc is neither lenient nor
/// the end consumer, and wants close to the opposite settings. Rather than
/// make every such user find each knob separately, this picks a coherent set
/// of *defaults* for them. Every individual setting still overrides it.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq, Default)]
pub(crate) enum Compat {
    /// What neolink has always done.
    #[default]
    #[serde(alias = "default", alias = "none", alias = "standard")]
    Default,
    /// Tuned for go2rtc, and so for Home Assistant and Frigate through it.
    ///
    /// Offers every audio format so go2rtc's MP4/HLS output can take the
    /// AAC untouched while its WebRTC output takes the `L16` track it needs;
    /// keeps the server-side queues short, because a WebRTC viewer is not a
    /// recorder and three seconds of buffer is three seconds of delay; and
    /// drops the MJPEG placeholder, which advertises a codec browsers cannot
    /// play and then ends, sending go2rtc into a reconnect loop.
    #[serde(alias = "go2rtc", alias = "webrtc", alias = "frigate")]
    Go2rtc,
}

impl Compat {
    /// The default audio format for this profile.
    fn audio_format(&self) -> AudioFormat {
        match self {
            // One L16 track: understood by every RTSP client.
            Self::Default => AudioFormat::Pcm,
            // go2rtc's two outputs want different things from one camera.
            Self::Go2rtc => AudioFormat::All,
        }
    }

    /// The default server-side queue depth, in milliseconds.
    fn buffer_duration(&self) -> u64 {
        match self {
            // Rides out congestion; the right trade for a recorder.
            Self::Default => 3000,
            // Ahead of a WebRTC consumer, queue depth is just delay.
            Self::Go2rtc => 250,
        }
    }

    /// Whether to serve the placeholder stream while the camera starts.
    fn use_splash(&self) -> bool {
        match self {
            // Blue Iris gives up permanently on a 404, so it needs this.
            Self::Default => true,
            // go2rtc retries a failed DESCRIBE happily, and would otherwise
            // cache an MJPEG-only media list its consumers cannot use.
            Self::Go2rtc => false,
        }
    }

    /// Whether to restrict RTSP to TCP interleaved.
    ///
    /// go2rtc dials TCP by default (`Protocol = "rtsp+tcp"` unless
    /// `?transport=udp`), so this costs it nothing and takes UDP packet loss
    /// off the table for anything else sharing the mount.
    pub(crate) fn tcp_only(&self) -> bool {
        matches!(self, Self::Go2rtc)
    }

    /// Whether to tear a media down as soon as its client disconnects.
    ///
    /// go2rtc drops the TCP connection without a TEARDOWN when its read
    /// deadline fires. Waiting out the session timeout leaves the pipeline —
    /// and its camera subscription — alive, and under reconnect churn those
    /// stack up on a device with few connections to spare.
    pub(crate) fn stop_on_disconnect(&self) -> bool {
        matches!(self, Self::Go2rtc)
    }
}

impl std::fmt::Display for Compat {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Compat::Default => "default",
            Compat::Go2rtc => "go2rtc",
        };
        write!(f, "{}", s)
    }
}

impl CameraConfig {
    /// How this camera's audio is delivered, resolving the profile default.
    pub(crate) fn audio_format(&self) -> AudioFormat {
        self.audio_format
            .unwrap_or_else(|| self.compat.audio_format())
    }

    /// How much media the server-side queues may hold, in milliseconds,
    /// resolving the profile default.
    pub(crate) fn buffer_duration(&self) -> u64 {
        self.buffer_duration
            .unwrap_or_else(|| self.compat.buffer_duration())
    }

    /// Whether to serve the placeholder stream, resolving the profile
    /// default.
    pub(crate) fn use_splash(&self) -> bool {
        self.use_splash.unwrap_or_else(|| self.compat.use_splash())
    }
}

#[derive(Debug, Deserialize, Serialize, Validate, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UserConfig {
    #[validate(custom(function = "validate_username"))]
    #[serde(alias = "username")]
    pub(crate) name: String,

    #[serde(alias = "password", skip_serializing, default)]
    pub(crate) pass: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct MqttConfig {
    #[serde(default = "default_true")]
    pub(crate) enable_motion: bool,
    /// Publish the per-AI-type detections (people, vehicle, dog_cat, ...).
    ///
    /// These arrive on the same camera subscription as motion, so this costs
    /// no extra traffic; it only controls whether the `status/ai` topics are
    /// published. Has no effect when `enable_motion` is off.
    #[serde(default = "default_true", alias = "enable_ai_detection")]
    pub(crate) enable_ai: bool,
    #[serde(default = "default_true")]
    pub(crate) enable_light: bool,
    #[serde(default = "default_true")]
    pub(crate) enable_battery: bool,
    /// Update time in ms
    #[serde(default = "default_2000")]
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "battery_update"
    ))]
    pub(crate) battery_update: u64,
    #[serde(default = "default_true")]
    pub(crate) enable_preview: bool,
    /// Update time in ms
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "preview_update"
    ))]
    #[serde(default = "default_2000")]
    pub(crate) preview_update: u64,

    /// Enable the flood light tasks status
    /// Will not do anything if no floodlight
    /// is detected
    #[serde(default = "default_true")]
    pub(crate) enable_floodlight: bool,
    /// Update time in ms
    #[validate(range(
        min = 500,
        message = "Update ms should be > 500",
        code = "floodlight_update"
    ))]
    #[serde(default = "default_2000")]
    pub(crate) floodlight_update: u64,

    #[serde(default)]
    pub(crate) discovery: Option<MqttDiscoveryConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Validate, PartialEq, Eq)]
pub(crate) struct MqttDiscoveryConfig {
    pub(crate) topic: String,

    pub(crate) features: HashSet<Discoveries>,
}

fn validate_mqtt_server(config: &MqttServerConfig) -> Result<(), ValidationError> {
    if config.ca.is_some() && config.client_auth.is_some() {
        Err(ValidationError::new(
            "Cannot have both ca and client_auth set",
        ))
    } else {
        Ok(())
    }
}

const fn default_true() -> bool {
    true
}

const fn default_false() -> bool {
    false
}

fn default_mqtt() -> MqttConfig {
    MqttConfig {
        enable_motion: true,
        enable_ai: true,
        enable_light: true,
        enable_battery: true,
        battery_update: 2000,
        enable_preview: true,
        preview_update: 2000,
        enable_floodlight: true,
        floodlight_update: 2000,
        discovery: Default::default(),
    }
}

fn default_print() -> PrintFormat {
    PrintFormat::None
}

fn default_discovery() -> DiscoveryMethods {
    DiscoveryMethods::Relay
}

fn default_maxenc() -> String {
    "Aes".to_string()
}

#[derive(Debug, Deserialize, Serialize, Validate, Clone, PartialEq)]
pub(crate) struct PauseConfig {
    #[serde(default = "default_on_motion")]
    pub(crate) on_motion: bool,

    #[serde(default = "default_on_disconnect", alias = "on_client")]
    pub(crate) on_disconnect: bool,

    #[serde(default = "default_motion_timeout", alias = "timeout")]
    pub(crate) motion_timeout: f64,

    #[serde(default = "default_pause_mode")]
    #[validate(regex(
        path = *RE_PAUSE_MODE,
        message = "Incorrect pause mode",
        code = "mode"
    ))]
    pub(crate) mode: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SplashPattern {
    #[serde(alias = "smpte")]
    Smpte,
    #[serde(alias = "snow")]
    Snow,
    #[serde(alias = "black")]
    Black,
    #[serde(alias = "white")]
    White,
    #[serde(alias = "red")]
    Red,
    #[serde(alias = "green")]
    Green,
    #[serde(alias = "blue")]
    Blue,
    #[serde(alias = "checkers-1")]
    Checkers1,
    #[serde(alias = "checkers-2")]
    Checkers2,
    #[serde(alias = "checkers-4")]
    Checkers4,
    #[serde(alias = "checkers-8")]
    Checkers8,
    #[serde(alias = "circular")]
    Circular,
    #[serde(alias = "blink")]
    Blink,
    #[serde(alias = "smpte75")]
    Smpte75,
    #[serde(alias = "zone-plate")]
    ZonePlate,
    #[serde(alias = "gamut")]
    Gamut,
    #[serde(alias = "chroma-zone-plate")]
    ChromaZonePlate,
    #[serde(alias = "solid-color")]
    SolidColor,
    #[serde(alias = "ball")]
    Ball,
    #[serde(alias = "smpte100")]
    Smpte100,
    #[serde(alias = "bar")]
    Bar,
    #[serde(alias = "pinwheel")]
    Pinwheel,
    #[serde(alias = "spokes")]
    Spokes,
    #[serde(alias = "gradient")]
    Gradient,
    #[serde(alias = "colors")]
    Colors,
    #[serde(alias = "smpte-rp-219")]
    SmpteRp219,
}

impl std::fmt::Display for SplashPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            SplashPattern::Smpte => "smpte",
            SplashPattern::Snow => "snow",
            SplashPattern::Black => "black",
            SplashPattern::White => "white",
            SplashPattern::Red => "red",
            SplashPattern::Green => "green",
            SplashPattern::Blue => "blue",
            SplashPattern::Checkers1 => "checkers-1",
            SplashPattern::Checkers2 => "checkers-2",
            SplashPattern::Checkers4 => "checkers-4",
            SplashPattern::Checkers8 => "checkers-8",
            SplashPattern::Circular => "circular",
            SplashPattern::Blink => "blink",
            SplashPattern::Smpte75 => "smpte75",
            SplashPattern::ZonePlate => "zone-plate",
            SplashPattern::Gamut => "gamut",
            SplashPattern::ChromaZonePlate => "chroma-zone-plate",
            SplashPattern::SolidColor => "solid-color",
            SplashPattern::Ball => "ball",
            SplashPattern::Smpte100 => "smpte100",
            SplashPattern::Bar => "bar",
            SplashPattern::Pinwheel => "pinwheel",
            SplashPattern::Spokes => "spokes",
            SplashPattern::Gradient => "gradient",
            SplashPattern::Colors => "colors",
            SplashPattern::SmpteRp219 => "smpte-rp-219",
        }
        .to_string();
        write!(f, "{}", s)
    }
}

/// How the camera's audio is delivered over RTSP.
///
/// Reolink cameras emit either AAC (in ADTS framing) or DVI4 ADPCM. ADPCM
/// always has to be decoded, since there is no standard RTP payload format
/// for it, but AAC can be forwarded to the client untouched.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Eq, PartialEq, Default)]
pub(crate) enum AudioFormat {
    /// Pass AAC through untouched, payloaded as `MPEG4-GENERIC`
    /// (RFC 3640, `mode=AAC-hbr`).
    ///
    /// The passthrough to reach for. No decode, no resample and no
    /// re-encode, so the only work done on the audio is RTP framing, and
    /// the bandwidth is whatever the camera encoded at (typically
    /// 16-32kbps) rather than ~256kbps of raw L16.
    ///
    /// This is the payload format native RTSP cameras overwhelmingly use
    /// for AAC, so client support is the broadest of the passthrough
    /// options: ffmpeg/ffprobe, VLC, Blue Iris — and go2rtc, which
    /// recognises AAC *only* under this name (see [`AudioFormat::Latm`]).
    #[serde(
        alias = "mpeg4-generic",
        alias = "mpeg4_generic",
        alias = "mpeg4generic",
        alias = "rfc3640",
        alias = "aac-hbr",
        alias = "generic"
    )]
    Mpeg4Generic,
    /// Pass AAC through untouched, payloaded as `MP4A-LATM` (RFC 6416).
    ///
    /// The same passthrough as [`AudioFormat::Mpeg4Generic`] — same
    /// frames, same absence of any decode — differing only in how RTP
    /// frames them. Understood by ffmpeg/ffprobe, VLC and Blue Iris.
    ///
    /// **Not understood by go2rtc**, and therefore not by Home Assistant
    /// or Frigate through it: go2rtc identifies AAC solely by the rtpmap
    /// name `MPEG4-GENERIC`, so a `MP4A-LATM` track is parsed as an
    /// unknown codec and dropped without an error. Use
    /// [`AudioFormat::Mpeg4Generic`] (or `all`) for those.
    #[serde(alias = "latm", alias = "aac", alias = "passthrough")]
    Latm,
    /// Decode the audio to raw samples and send it as `L16` (RFC 3551).
    ///
    /// The default, and what neolink did unconditionally before
    /// `audio_format` existed: one track, in a format every RTSP client
    /// understands, at the cost of decode latency and a much larger RTP
    /// bitrate. ADPCM always uses this path.
    #[default]
    #[serde(alias = "pcm", alias = "l16", alias = "raw")]
    Pcm,
    /// Offer everything we can, as separate audio tracks in the SDP, and
    /// let the client decide.
    ///
    /// A `MPEG4-GENERIC` track, a `MP4A-LATM` track and an `L16` track are
    /// advertised, in that order, and a client that negotiates sets up only
    /// the one it wants. go2rtc does — it issues `SETUP` per track, on
    /// demand — so this lets one camera serve passthrough AAC to whatever
    /// go2rtc muxes into MP4/HLS and `L16` to its WebRTC output, which
    /// cannot take AAC in any framing.
    ///
    /// Opt-in, because a client that does not negotiate sets up every
    /// track in the SDP and so receives all of them at once. Every branch
    /// also runs server-side regardless of what is subscribed, so the
    /// decode that the passthrough formats avoid is paid anyway, and a
    /// decoder failure takes the passthrough tracks and the video with it.
    #[serde(
        alias = "all",
        alias = "auto",
        alias = "both",
        alias = "dual",
        alias = "offer_both"
    )]
    All,
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            AudioFormat::Mpeg4Generic => "mpeg4-generic",
            AudioFormat::Latm => "latm",
            AudioFormat::Pcm => "pcm",
            AudioFormat::All => "all",
        };
        write!(f, "{}", s)
    }
}

impl AudioFormat {
    /// Parse the `?audio=` parameter a client put on its RTSP URL.
    ///
    /// Accepts the same spellings as the config file, case-insensitively,
    /// and returns `None` for anything it does not recognise so the caller
    /// can say so and carry on with the camera's configured format.
    pub(crate) fn from_request(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mpeg4-generic" | "mpeg4_generic" | "mpeg4generic" | "rfc3640" | "aac-hbr"
            | "generic" => Some(Self::Mpeg4Generic),
            "latm" | "aac" | "passthrough" => Some(Self::Latm),
            "pcm" | "l16" | "raw" => Some(Self::Pcm),
            "all" | "auto" | "both" | "dual" | "offer_both" => Some(Self::All),
            _ => None,
        }
    }
}

fn default_bind_addr() -> String {
    "0.0.0.0".to_string()
}

fn default_bind_port() -> u16 {
    8554
}

fn default_stream() -> StreamConfig {
    StreamConfig::All
}

fn default_certificate() -> Option<String> {
    None
}

fn default_tls_client_auth() -> String {
    "none".to_string()
}

fn default_tokio_console() -> bool {
    false
}

fn default_channel_id() -> u8 {
    0
}

fn default_update_time() -> bool {
    false
}

fn default_motion_timeout() -> f64 {
    1.
}

fn default_on_disconnect() -> bool {
    false
}

fn default_on_motion() -> bool {
    false
}

fn default_pause_mode() -> String {
    "none".to_string()
}

fn default_strict() -> bool {
    false
}

fn default_pause() -> PauseConfig {
    PauseConfig {
        on_motion: default_on_motion(),
        on_disconnect: default_on_disconnect(),
        motion_timeout: default_motion_timeout(),
        mode: default_pause_mode(),
    }
}

fn default_max_discovery_retries() -> usize {
    10
}

fn default_2000() -> u64 {
    2000
}

fn default_splash() -> SplashPattern {
    SplashPattern::Snow
}

pub(crate) static RESERVED_NAMES: &[&str] = &["anyone", "anonymous"];
fn validate_username(name: &str) -> Result<(), ValidationError> {
    if name.trim().is_empty() {
        return Err(ValidationError::new("username cannot be empty"));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(ValidationError::new("This is a reserved username"));
    }
    Ok(())
}

fn validate_camera_config(camera_config: &CameraConfig) -> Result<(), ValidationError> {
    match (&camera_config.camera_addr, &camera_config.camera_uid) {
        (None, None) => Err(ValidationError::new(
            "Either camera address or uid must be given",
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(extra: &str) -> CameraConfig {
        let toml = format!(
            r#"
            name = "Camera01"
            username = "admin"
            password = "password"
            uid = "ABCDEF0123456789"
            {extra}
            "#
        );
        toml::from_str(&toml).expect("camera config should parse")
    }

    /// The default has to be the one every RTSP client can play: anything
    /// else is a stream some client cannot hear, chosen on its behalf.
    #[test]
    fn audio_format_defaults_to_the_universally_understood_one() {
        assert_eq!(camera("").audio_format(), AudioFormat::Pcm);
    }

    #[test]
    fn audio_format_accepts_its_spellings() {
        for spelling in [
            "mpeg4-generic",
            "Mpeg4Generic",
            "mpeg4_generic",
            "mpeg4generic",
            "rfc3640",
            "aac-hbr",
            "generic",
        ] {
            assert_eq!(
                camera(&format!("audio_format = \"{spelling}\"")).audio_format(),
                AudioFormat::Mpeg4Generic,
                "{spelling} should select MPEG4-GENERIC"
            );
        }
        for spelling in ["latm", "Latm", "aac", "passthrough"] {
            assert_eq!(
                camera(&format!("audio_format = \"{spelling}\"")).audio_format(),
                AudioFormat::Latm,
                "{spelling} should select LATM"
            );
        }
        for spelling in ["pcm", "Pcm", "l16", "raw"] {
            assert_eq!(
                camera(&format!("audio_format = \"{spelling}\"")).audio_format(),
                AudioFormat::Pcm,
                "{spelling} should select PCM"
            );
        }
        for spelling in ["all", "All", "auto", "both", "dual", "offer_both"] {
            assert_eq!(
                camera(&format!("audio_format = \"{spelling}\"")).audio_format(),
                AudioFormat::All,
                "{spelling} should offer every format"
            );
        }
    }

    /// What a client can put on its URL, which has to line up with what the
    /// config file accepts or the two would disagree about the same word.
    #[test]
    fn a_client_can_ask_for_a_format_by_name() {
        for spelling in ["mpeg4-generic", "MPEG4-Generic", " rfc3640 ", "aac-hbr"] {
            assert_eq!(
                AudioFormat::from_request(spelling),
                Some(AudioFormat::Mpeg4Generic)
            );
        }
        for spelling in ["latm", "LATM", "aac", " passthrough "] {
            assert_eq!(AudioFormat::from_request(spelling), Some(AudioFormat::Latm));
        }
        for spelling in ["pcm", "PCM", "l16", "raw"] {
            assert_eq!(AudioFormat::from_request(spelling), Some(AudioFormat::Pcm));
        }
        for spelling in ["all", "auto", "both", "dual"] {
            assert_eq!(AudioFormat::from_request(spelling), Some(AudioFormat::All));
        }
        // Unknown asks are ignored rather than guessed at.
        assert_eq!(AudioFormat::from_request("opus"), None);
        assert_eq!(AudioFormat::from_request(""), None);
    }

    #[test]
    fn audio_format_has_the_documented_aliases() {
        // Documented in sample_config.toml / README.
        assert_eq!(camera("audio = \"pcm\"").audio_format(), AudioFormat::Pcm);
        assert_eq!(
            camera("aud_format = \"pcm\"").audio_format(),
            AudioFormat::Pcm
        );
    }

    #[test]
    fn buffer_duration_defaults_to_three_seconds() {
        assert_eq!(camera("").buffer_duration(), 3000);
        assert_eq!(camera("buffer_duration = 250").buffer_duration(), 250);
    }

    /// The whole point of the profile: it moves *defaults*, and anything
    /// set explicitly still wins.
    #[test]
    fn compat_moves_defaults_but_never_overrides() {
        let plain = camera("");
        assert_eq!(plain.compat, Compat::Default);
        assert_eq!(plain.audio_format(), AudioFormat::Pcm);
        assert_eq!(plain.buffer_duration(), 3000);
        assert!(plain.use_splash());

        let go2rtc = camera("compat = \"go2rtc\"");
        assert_eq!(go2rtc.audio_format(), AudioFormat::All);
        assert_eq!(go2rtc.buffer_duration(), 250);
        assert!(!go2rtc.use_splash());

        // Explicit settings survive the profile, in both directions.
        let overridden = camera(
            "compat = \"go2rtc\"\n             audio_format = \"pcm\"\n             buffer_duration = 1500\n             use_splash = true",
        );
        assert_eq!(overridden.audio_format(), AudioFormat::Pcm);
        assert_eq!(overridden.buffer_duration(), 1500);
        assert!(overridden.use_splash());

        let overridden = camera("audio_format = \"all\"\nbuffer_duration = 100");
        assert_eq!(overridden.audio_format(), AudioFormat::All);
        assert_eq!(overridden.buffer_duration(), 100);
    }

    #[test]
    fn compat_accepts_its_spellings() {
        for spelling in ["go2rtc", "Go2rtc", "webrtc", "frigate"] {
            assert_eq!(
                camera(&format!("compat = \"{spelling}\"")).compat,
                Compat::Go2rtc,
                "{spelling} should select the go2rtc profile"
            );
        }
        for spelling in ["default", "Default", "none", "standard"] {
            assert_eq!(
                camera(&format!("compat = \"{spelling}\"")).compat,
                Compat::Default
            );
        }
        // Documented aliases for the key itself.
        assert_eq!(camera("profile = \"go2rtc\"").compat, Compat::Go2rtc);
        assert_eq!(camera("tuned_for = \"go2rtc\"").compat, Compat::Go2rtc);
    }

    /// These two are read straight by the RTSP factory rather than through
    /// a resolver, so they get their own check.
    #[test]
    fn compat_carries_the_factory_settings() {
        assert!(!Compat::Default.tcp_only());
        assert!(!Compat::Default.stop_on_disconnect());
        assert!(Compat::Go2rtc.tcp_only());
        assert!(Compat::Go2rtc.stop_on_disconnect());
    }

    #[test]
    fn an_unknown_compat_profile_is_rejected() {
        let toml = r#"
            name = "Camera01"
            username = "admin"
            password = "password"
            uid = "ABCDEF0123456789"
            compat = "blueiris"
        "#;
        assert!(toml::from_str::<CameraConfig>(toml).is_err());
    }

    /// `buffer_duration` became an Option; its range validation has to
    /// still bite.
    #[test]
    fn buffer_duration_is_still_range_checked() {
        use validator::Validate;
        assert!(camera("buffer_duration = 250").validate().is_ok());
        assert!(camera("buffer_duration = 0").validate().is_err());
        assert!(camera("buffer_duration = 20000").validate().is_err());
        // Unset means "take the profile default", not "out of range".
        assert!(camera("").validate().is_ok());
    }

    #[test]
    fn an_unknown_audio_format_is_rejected() {
        let toml = r#"
            name = "Camera01"
            username = "admin"
            password = "password"
            uid = "ABCDEF0123456789"
            audio_format = "opus"
        "#;
        assert!(toml::from_str::<CameraConfig>(toml).is_err());
    }
}
