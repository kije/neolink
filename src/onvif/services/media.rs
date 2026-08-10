//! ONVIF Media service. Builds the MediaProfile list and hands out the RTSP
//! and snapshot URIs that point back to neolink's existing servers.

use anyhow::Result;
use neolink_core::bc::xml::EncodeTable;
use neolink_core::bc_protocol::BcCamera;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::config::AudioFormat;
use crate::onvif::capabilities::{capabilities, CameraCapabilities};
use crate::onvif::services::device::FaultBody;
use crate::onvif::services::osd;
use crate::onvif::services::ptz::render_ptz_configuration_xml;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{url_path_segment, CameraEntry, OnvifState, OnvifStream};

/// Per-stream descriptor used to build profile XML.
struct StreamDesc {
    stream: OnvifStream,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate_kbps: u32,
    /// Every framerate the camera lists for this stream, for
    /// `GetVideoEncoderConfigurationOptions`. Empty when the camera didn't
    /// answer, in which case only the current value is offered.
    framerates: Vec<u32>,
    /// Every bitrate the camera lists for this stream, in kbps.
    bitrates: Vec<u32>,
}

impl StreamDesc {
    fn fallback(stream: OnvifStream) -> Self {
        let (w, h, fps, br) = match stream {
            OnvifStream::Main => (2560, 1440, 25, 4096),
            OnvifStream::Sub => (640, 480, 15, 512),
            OnvifStream::Extern => (1280, 720, 20, 1024),
        };
        Self {
            stream,
            width: w,
            height: h,
            framerate: fps,
            bitrate_kbps: br,
            framerates: vec![],
            bitrates: vec![],
        }
    }

    /// The framerates to advertise as selectable, always including the current
    /// one so the reported configuration is inside its own option set.
    fn framerate_options(&self) -> Vec<u32> {
        options_including(&self.framerates, self.framerate)
    }

    fn bitrate_options(&self) -> Vec<u32> {
        options_including(&self.bitrates, self.bitrate_kbps)
    }
}

/// Sort, dedupe, and guarantee `current` is present.
///
/// A configuration a client cannot re-select is a configuration it will refuse
/// to display, so the current value belongs in the option list even when the
/// camera's own table omitted it.
fn options_including(values: &[u32], current: u32) -> Vec<u32> {
    let mut out: Vec<u32> = values.iter().copied().filter(|v| *v > 0).collect();
    out.push(current);
    out.sort_unstable();
    out.dedup();
    out
}

async fn read_stream_descs(cam: &CameraEntry) -> Vec<StreamDesc> {
    // Try the camera first. If it errors (or is offline), fall back to
    // plausible defaults so the bridge can still answer profile queries.
    let res = cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_stream_info().await?) }))
        .await;
    let tables: Vec<EncodeTable> = match res {
        Ok(list) => list
            .stream_infos
            .into_iter()
            .flat_map(|s| s.encode_tables.into_iter())
            .collect(),
        Err(_) => Vec::new(),
    };

    cam.streams
        .iter()
        .map(|s| {
            if let Some(t) = tables.iter().find(|t| t.name == s.reolink_name()) {
                let framerates = csv_u32(&t.framerate_table);
                let bitrates = csv_u32(&t.bitrate_table);
                let fps = resolve_default(&framerates, t.default_framerate);
                let br = resolve_default(&bitrates, t.default_bitrate);
                StreamDesc {
                    stream: *s,
                    width: t.resolution.width,
                    height: t.resolution.height,
                    framerate: fps,
                    bitrate_kbps: br,
                    framerates,
                    bitrates,
                }
            } else {
                StreamDesc::fallback(*s)
            }
        })
        .collect()
}

/// Parse one of Reolink's comma-separated option tables.
fn csv_u32(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
}

/// Resolve a `defaultFramerate` / `defaultBitrate` against its table.
///
/// Reolink overloads these fields: on most firmwares the number is an *index*
/// into the matching table, but on some it is the value itself. Taking the
/// table's first entry — as this used to — reports the camera's current
/// setting correctly only when the selected index happens to be zero.
///
/// The rule here is the one `src/rtsp/factory.rs` already applies when it
/// builds the RTSP stream config: treat it as an index when it lands inside
/// the table, and as a literal value when it doesn't. That way the ONVIF
/// profile and the RTSP stream describe the same encoder settings.
fn resolve_default(table: &[u32], default: u32) -> u32 {
    table
        .get(default as usize)
        .copied()
        .unwrap_or(default)
        // A camera that reports neither a usable table nor a usable default
        // still needs a positive number here: zero is not a frame rate, and
        // clients reject a configuration they cannot re-select.
        .max(1)
}

fn profile_token(cam: &str, s: OnvifStream) -> String {
    format!("profile_{}_{}", cam, s.token_suffix())
}

fn video_source_token(cam: &str) -> String {
    format!("vs_{cam}")
}
fn video_encoder_token(cam: &str, s: OnvifStream) -> String {
    format!("vec_{}_{}", cam, s.token_suffix())
}
fn audio_source_token(cam: &str) -> String {
    format!("as_{cam}")
}
fn audio_encoder_token(cam: &str) -> String {
    format!("aec_{cam}")
}

/// How the camera's configured RTSP audio format is described in ONVIF terms.
///
/// This has to follow the *stream*, not the camera: `GetStreamUri` hands out a
/// URL with no `?audio=` override, so what a client receives is whatever
/// `audio_format` resolves to for that camera — and the default profile
/// resolves to [`AudioFormat::Pcm`], i.e. decoded L16, not passthrough AAC.
/// Hardcoding AAC therefore misdescribed the stream on every default install.
///
/// ONVIF ver10's `tt:AudioEncoding` is a closed enum — `G711`, `G726`, `AAC` —
/// with no value for L16, so the L16 case is reported as `G711`, the only
/// uncompressed-audio token available. That is the honest limit of the
/// vocabulary rather than a claim about the payload: every client reads the
/// real codec from the RTSP SDP, and uses the profile only to decide whether to
/// ask for audio at all. The bitrate and sample rate are reported truthfully in
/// both cases, so a client sizing its buffers gets the right numbers.
struct AudioDesc {
    encoding: &'static str,
    bitrate_kbps: u32,
    samplerate_khz: u32,
}

fn audio_desc(format: AudioFormat) -> AudioDesc {
    match format {
        // Passthrough: the camera's own AAC, reframed but not re-encoded.
        AudioFormat::Mpeg4Generic | AudioFormat::Latm => AudioDesc {
            encoding: "AAC",
            bitrate_kbps: 32,
            samplerate_khz: 16,
        },
        // Decoded to raw samples. 16 kHz mono L16 is 256 kbps on the wire.
        AudioFormat::Pcm => AudioDesc {
            encoding: "G711",
            bitrate_kbps: 256,
            samplerate_khz: 16,
        },
        // Several tracks in one SDP. A profile describes one encoder, so it
        // describes the first track offered — the `MPEG4-GENERIC` AAC one —
        // which is also the track a negotiating client picks when it can.
        AudioFormat::All => AudioDesc {
            encoding: "AAC",
            bitrate_kbps: 32,
            samplerate_khz: 16,
        },
    }
}

fn render_audio_source_configuration(cam_name: &str, element: &str) -> String {
    format!(
        "<{element} token=\"{tok}\">\
<tt:Name>{name}</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:SourceToken>asrc_{cam_name}</tt:SourceToken>\
</{element}>",
        tok = audio_source_token(cam_name),
        name = xml_escape(&format!("{cam_name}-audio")),
    )
}

fn render_audio_encoder_configuration(
    cam_name: &str,
    format: AudioFormat,
    element: &str,
) -> String {
    let desc = audio_desc(format);
    format!(
        "<{element} token=\"{tok}\">\
<tt:Name>{name}</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:Encoding>{enc}</tt:Encoding>\
<tt:Bitrate>{br}</tt:Bitrate>\
<tt:SampleRate>{sr}</tt:SampleRate>\
<tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address><tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast>\
<tt:SessionTimeout>PT30S</tt:SessionTimeout>\
</{element}>",
        tok = audio_encoder_token(cam_name),
        name = xml_escape(&format!("{cam_name}-audio")),
        enc = desc.encoding,
        br = desc.bitrate_kbps,
        sr = desc.samplerate_khz,
    )
}

fn render_video_source_configuration(cam: &CameraEntry, descs: &[StreamDesc]) -> String {
    // Bounds = main stream resolution if available, otherwise the first.
    let (w, h) = descs
        .iter()
        .find(|d| d.stream == OnvifStream::Main)
        .or_else(|| descs.first())
        .map(|d| (d.width, d.height))
        .unwrap_or((1920, 1080));
    let tok = video_source_token(&cam.name);
    let name = xml_escape(&cam.name);
    let uc = descs.len();
    let src = format!("vsrc_{}", cam.name);
    format!(
        "<tt:VideoSourceConfiguration token=\"{tok}\">\
<tt:Name>{name}</tt:Name>\
<tt:UseCount>{uc}</tt:UseCount>\
<tt:SourceToken>{src}</tt:SourceToken>\
<tt:Bounds x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\"/>\
</tt:VideoSourceConfiguration>"
    )
}

fn render_video_encoder_configuration(cam_name: &str, d: &StreamDesc) -> String {
    format!(
        "<tt:VideoEncoderConfiguration token=\"{tok}\">\
<tt:Name>{name}</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:Encoding>H264</tt:Encoding>\
<tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
<tt:Quality>5</tt:Quality>\
<tt:RateControl><tt:FrameRateLimit>{fps}</tt:FrameRateLimit><tt:EncodingInterval>1</tt:EncodingInterval><tt:BitrateLimit>{br}</tt:BitrateLimit></tt:RateControl>\
<tt:H264><tt:GovLength>50</tt:GovLength><tt:H264Profile>High</tt:H264Profile></tt:H264>\
<tt:Multicast><tt:Address><tt:Type>IPv4</tt:Type><tt:IPv4Address>0.0.0.0</tt:IPv4Address></tt:Address><tt:Port>0</tt:Port><tt:TTL>1</tt:TTL><tt:AutoStart>false</tt:AutoStart></tt:Multicast>\
<tt:SessionTimeout>PT30S</tt:SessionTimeout>\
</tt:VideoEncoderConfiguration>",
        tok = video_encoder_token(cam_name, d.stream),
        name = xml_escape(&format!("{}-{}", cam_name, d.stream.token_suffix())),
        w = d.width,
        h = d.height,
        fps = d.framerate,
        br = d.bitrate_kbps,
    )
}

/// The pieces of a `tt:Profile`, kept separate from the order they go in.
///
/// `tt:Profile` is an xs:sequence — Name, VideoSourceConfiguration,
/// AudioSourceConfiguration, VideoEncoderConfiguration,
/// AudioEncoderConfiguration, VideoAnalyticsConfiguration, PTZConfiguration —
/// so the audio pair is *interleaved* with the video pair rather than following
/// it. Emitting them in the order they read most naturally produces XML a
/// strict client rejects wholesale, and nothing about the output looks wrong
/// until one does. Splitting the fragments from their assembly makes that
/// ordering a single testable rule.
struct ProfileParts {
    token: String,
    name: String,
    video_source: String,
    audio_source: Option<String>,
    video_encoder: String,
    audio_encoder: Option<String>,
    ptz: Option<String>,
}

impl ProfileParts {
    /// Render under the element name the enclosing response needs: the profile
    /// list uses `trt:Profiles`, the single-profile response `trt:Profile`.
    fn render(&self, element: &str) -> String {
        let mut out = format!(
            "<{element} fixed=\"true\" token=\"{tok}\"><tt:Name>{name}</tt:Name>",
            tok = xml_escape(&self.token),
            name = xml_escape(&self.name),
        );
        out.push_str(&self.video_source);
        if let Some(x) = &self.audio_source {
            out.push_str(x);
        }
        out.push_str(&self.video_encoder);
        if let Some(x) = &self.audio_encoder {
            out.push_str(x);
        }
        if let Some(x) = &self.ptz {
            out.push_str(x);
        }
        out.push_str(&format!("</{element}>"));
        out
    }
}

fn profile_parts(
    cam: &CameraEntry,
    d: &StreamDesc,
    vs_xml: &str,
    caps: &CameraCapabilities,
) -> ProfileParts {
    ProfileParts {
        token: profile_token(&cam.name, d.stream),
        name: format!("{}_{}", cam.name, d.stream.token_suffix()),
        video_source: vs_xml.to_string(),
        // The RTSP stream carries audio whenever the camera has a microphone,
        // but a VMS decides whether to even ask for it from these two
        // configurations — without them, Frigate, Synology and Milestone all
        // pull video only.
        audio_source: caps
            .audio
            .then(|| render_audio_source_configuration(&cam.name, "tt:AudioSourceConfiguration")),
        video_encoder: render_video_encoder_configuration(&cam.name, d),
        audio_encoder: caps.audio.then(|| {
            render_audio_encoder_configuration(
                &cam.name,
                cam.audio_format,
                "tt:AudioEncoderConfiguration",
            )
        }),
        // A profile carries a PTZConfiguration only if the camera has something
        // to move; clients key their PTZ UI off its presence.
        ptz: caps
            .ptz()
            .then(|| render_ptz_configuration_xml(&cam.name, caps, "tt:PTZConfiguration")),
    }
}

pub(crate) async fn dispatch(
    state: &OnvifState,
    cam: &CameraEntry,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let descs = read_stream_descs(cam).await;
    let caps = capabilities(cam).await;
    let vs_xml = render_video_source_configuration(cam, &descs);

    let body = match action {
        "GetProfiles" => {
            let profiles: String = descs
                .iter()
                .map(|d| profile_parts(cam, d, &vs_xml, &caps).render("trt:Profiles"))
                .collect();
            format!("<trt:GetProfilesResponse>{profiles}</trt:GetProfilesResponse>")
        }
        "GetProfile" => {
            let token = read_first_text_element(body_xml, "ProfileToken")
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing ProfileToken".to_string(),
                })?;
            let stream = descs
                .iter()
                .find(|d| profile_token(&cam.name, d.stream) == token)
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: format!("Unknown profile token '{token}'"),
                })?;
            // The single-profile response uses `Profile`, not `Profiles`.
            let p = profile_parts(cam, stream, &vs_xml, &caps).render("trt:Profile");
            format!("<trt:GetProfileResponse>{p}</trt:GetProfileResponse>")
        }
        "GetStreamUri" => {
            let token = read_first_text_element(body_xml, "ProfileToken")
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing ProfileToken".to_string(),
                })?;
            let stream = descs
                .iter()
                .find(|d| profile_token(&cam.name, d.stream) == token)
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: format!("Unknown profile token '{token}'"),
                })?;
            let host = state.advertise_host().await.map_err(other_fault)?;
            let port = state.rtsp_port().await;
            let uri = format!(
                "rtsp://{host}:{port}/{cam_name}/{path}",
                cam_name = url_path_segment(&cam.name),
                path = stream.stream.as_rtsp_path(),
            );
            format!(
                "<trt:GetStreamUriResponse><trt:MediaUri>\
<tt:Uri>{uri}</tt:Uri>\
<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>\
<tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>\
<tt:Timeout>PT60S</tt:Timeout>\
</trt:MediaUri></trt:GetStreamUriResponse>",
                uri = xml_escape(&uri),
            )
        }
        "GetSnapshotUri" => {
            let token = read_first_text_element(body_xml, "ProfileToken")
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing ProfileToken".to_string(),
                })?;
            let stream = descs
                .iter()
                .find(|d| profile_token(&cam.name, d.stream) == token)
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: format!("Unknown profile token '{token}'"),
                })?;
            let authority = state.advertise_authority().await.map_err(other_fault)?;
            let uri = format!(
                "http://{authority}/onvif/{cam}/snapshot/{path}",
                cam = url_path_segment(&cam.name),
                path = stream.stream.as_rtsp_path(),
            );
            format!(
                "<trt:GetSnapshotUriResponse><trt:MediaUri>\
<tt:Uri>{uri}</tt:Uri>\
<tt:InvalidAfterConnect>false</tt:InvalidAfterConnect>\
<tt:InvalidAfterReboot>false</tt:InvalidAfterReboot>\
<tt:Timeout>PT60S</tt:Timeout>\
</trt:MediaUri></trt:GetSnapshotUriResponse>",
                uri = xml_escape(&uri),
            )
        }
        "GetVideoSources" => {
            // The source is the sensor, so describe it with the highest-fidelity
            // stream the camera exposes rather than a fixed 1080p30.
            let best = descs
                .iter()
                .find(|d| d.stream == OnvifStream::Main)
                .or_else(|| descs.first());
            format!(
                "<trt:GetVideoSourcesResponse><tt:VideoSources token=\"vsrc_{cam}\">\
<tt:Framerate>{fps}</tt:Framerate><tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
</tt:VideoSources></trt:GetVideoSourcesResponse>",
                cam = xml_escape(&cam.name),
                fps = best.map(|d| d.framerate).unwrap_or(25),
                w = best.map(|d| d.width).unwrap_or(1920),
                h = best.map(|d| d.height).unwrap_or(1080),
            )
        }
        "GetVideoSourceConfigurations" => format!(
            "<trt:GetVideoSourceConfigurationsResponse>{vs}</trt:GetVideoSourceConfigurationsResponse>",
            vs = vs_xml.replace("<tt:VideoSourceConfiguration", "<trt:Configurations")
                .replace("</tt:VideoSourceConfiguration>", "</trt:Configurations>"),
        ),
        "GetVideoEncoderConfigurations" => {
            let configs: String = descs
                .iter()
                .map(|d| {
                    render_video_encoder_configuration(&cam.name, d)
                        .replace("<tt:VideoEncoderConfiguration", "<trt:Configurations")
                        .replace("</tt:VideoEncoderConfiguration>", "</trt:Configurations>")
                })
                .collect();
            format!("<trt:GetVideoEncoderConfigurationsResponse>{configs}</trt:GetVideoEncoderConfigurationsResponse>")
        }
        "GetVideoEncoderConfiguration" => {
            let token = read_first_text_element(body_xml, "ConfigurationToken")
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing ConfigurationToken".to_string(),
                })?;
            let d = descs
                .iter()
                .find(|d| video_encoder_token(&cam.name, d.stream) == token)
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: format!("Unknown configuration token '{token}'"),
                })?;
            let c = render_video_encoder_configuration(&cam.name, d)
                .replace("<tt:VideoEncoderConfiguration", "<trt:Configuration")
                .replace("</tt:VideoEncoderConfiguration>", "</trt:Configuration>");
            format!("<trt:GetVideoEncoderConfigurationResponse>{c}</trt:GetVideoEncoderConfigurationResponse>")
        }
        "GetVideoEncoderConfigurationOptions" => {
            // Scope the options the way the request does: a client may ask
            // about one configuration, one profile, or the whole device. The
            // ranges come from the camera's own `EncodeTable`, so what we offer
            // is what it will actually accept.
            let selected = select_descs_for_options(cam, &descs, body_xml)?;
            format!(
                "<trt:GetVideoEncoderConfigurationOptionsResponse>{}\
</trt:GetVideoEncoderConfigurationOptionsResponse>",
                render_video_encoder_options(&selected)
            )
        }
        "GetAudioSources" if caps.audio => format!(
            "<trt:GetAudioSourcesResponse><tt:AudioSources token=\"asrc_{cam}\">\
<tt:Channels>1</tt:Channels></tt:AudioSources></trt:GetAudioSourcesResponse>",
            cam = xml_escape(&cam.name),
        ),
        "GetAudioSourceConfigurations" if caps.audio => format!(
            "<trt:GetAudioSourceConfigurationsResponse>{c}</trt:GetAudioSourceConfigurationsResponse>",
            c = render_audio_source_configuration(&cam.name, "trt:Configurations"),
        ),
        "GetAudioEncoderConfigurations" if caps.audio => format!(
            "<trt:GetAudioEncoderConfigurationsResponse>{c}</trt:GetAudioEncoderConfigurationsResponse>",
            c = render_audio_encoder_configuration(&cam.name, cam.audio_format, "trt:Configurations"),
        ),
        "GetAudioEncoderConfigurationOptions" if caps.audio => {
            // One option, matching the one configuration: the audio format is
            // a neolink setting, not something an ONVIF client may change.
            let desc = audio_desc(cam.audio_format);
            format!(
                "<trt:GetAudioEncoderConfigurationOptionsResponse><trt:Options>\
<tt:Options><tt:Encoding>{enc}</tt:Encoding>\
<tt:BitrateList><tt:Items>{br}</tt:Items></tt:BitrateList>\
<tt:SampleRateList><tt:Items>{sr}</tt:Items></tt:SampleRateList>\
</tt:Options></trt:Options></trt:GetAudioEncoderConfigurationOptionsResponse>",
                enc = desc.encoding,
                br = desc.bitrate_kbps,
                sr = desc.samplerate_khz,
            )
        }
        // A camera with no microphone answers these with an empty list rather
        // than a fault: "this device has no audio" is a valid, useful answer,
        // and a fault makes clients log an error and sometimes abandon the
        // whole profile.
        "GetAudioSources" => "<trt:GetAudioSourcesResponse/>".to_string(),
        "GetAudioSourceConfigurations" => {
            "<trt:GetAudioSourceConfigurationsResponse/>".to_string()
        }
        "GetAudioEncoderConfigurations" => {
            "<trt:GetAudioEncoderConfigurationsResponse/>".to_string()
        }
        "GetAudioEncoderConfigurationOptions" => {
            "<trt:GetAudioEncoderConfigurationOptionsResponse><trt:Options/>\
</trt:GetAudioEncoderConfigurationOptionsResponse>"
                .to_string()
        }
        "GetOSDs" | "GetOSD" | "GetOSDOptions" | "SetOSD" => {
            return osd::dispatch(cam, &caps, action, body_xml).await
        }
        "GetServiceCapabilities" => format!(
            "<trt:GetServiceCapabilitiesResponse><trt:Capabilities SnapshotUri=\"true\" \
Rotation=\"false\" VideoSourceMode=\"false\" OSD=\"{osd}\">\
<trt:ProfileCapabilities MaximumNumberOfProfiles=\"{n}\"/>\
<trt:StreamingCapabilities RTPMulticast=\"false\" RTP_TCP=\"true\" RTP_RTSP_TCP=\"true\" \
NonAggregateControl=\"false\" NoRTSPStreaming=\"false\"/>\
</trt:Capabilities></trt:GetServiceCapabilitiesResponse>",
            // The profiles are fixed and derived from the configured streams,
            // so the maximum is however many we actually hand out.
            n = descs.len(),
            osd = caps.osd,
        ),
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Media action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
}

/// Narrow the stream list to whatever the `GetVideoEncoderConfigurationOptions`
/// request asked about. Both selectors are optional and mutually exclusive in
/// practice; with neither, the answer covers the whole device.
fn select_descs_for_options<'a>(
    cam: &CameraEntry,
    descs: &'a [StreamDesc],
    body_xml: &str,
) -> Result<Vec<&'a StreamDesc>, FaultBody> {
    if let Some(token) = read_first_text_element(body_xml, "ConfigurationToken") {
        return descs
            .iter()
            .find(|d| video_encoder_token(&cam.name, d.stream) == token)
            .map(|d| vec![d])
            .ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: format!("Unknown configuration token '{token}'"),
            });
    }
    if let Some(token) = read_first_text_element(body_xml, "ProfileToken") {
        return descs
            .iter()
            .find(|d| profile_token(&cam.name, d.stream) == token)
            .map(|d| vec![d])
            .ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: format!("Unknown profile token '{token}'"),
            });
    }
    Ok(descs.iter().collect())
}

/// Render `VideoEncoderConfigurationOptions` for a set of streams.
///
/// The response carries a single options block, so several streams merge into
/// one: every resolution is listed, and the framerate and bitrate ranges span
/// the union of the camera's own tables.
fn render_video_encoder_options(descs: &[&StreamDesc]) -> String {
    let resolutions: String = {
        let mut seen: Vec<(u32, u32)> = descs.iter().map(|d| (d.width, d.height)).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.into_iter()
            .map(|(w, h)| {
                format!(
                    "<tt:ResolutionsAvailable><tt:Width>{w}</tt:Width>\
<tt:Height>{h}</tt:Height></tt:ResolutionsAvailable>"
                )
            })
            .collect()
    };
    let (fps_min, fps_max) = span(descs.iter().flat_map(|d| d.framerate_options()), 1, 30);
    let (br_min, br_max) = span(descs.iter().flat_map(|d| d.bitrate_options()), 64, 8192);

    // The base H264 block and the one inside Extension are the same options;
    // the extension exists only because ONVIF added the bitrate range later and
    // could not change the original type. Real cameras send both, and clients
    // read whichever they know about.
    let h264 = format!(
        "{resolutions}\
<tt:GovLengthRange><tt:Min>1</tt:Min><tt:Max>100</tt:Max></tt:GovLengthRange>\
<tt:FrameRateRange><tt:Min>{fps_min}</tt:Min><tt:Max>{fps_max}</tt:Max></tt:FrameRateRange>\
<tt:EncodingIntervalRange><tt:Min>1</tt:Min><tt:Max>1</tt:Max></tt:EncodingIntervalRange>\
<tt:H264ProfilesSupported>High</tt:H264ProfilesSupported>"
    );
    format!(
        "<trt:Options>\
<tt:QualityRange><tt:Min>1</tt:Min><tt:Max>6</tt:Max></tt:QualityRange>\
<tt:H264>{h264}</tt:H264>\
<tt:Extension><tt:H264>{h264}\
<tt:BitrateRange><tt:Min>{br_min}</tt:Min><tt:Max>{br_max}</tt:Max></tt:BitrateRange>\
</tt:H264></tt:Extension>\
</trt:Options>"
    )
}

/// Min and max of an iterator, falling back to the given bounds when it is
/// empty.
fn span(values: impl Iterator<Item = u32>, default_min: u32, default_max: u32) -> (u32, u32) {
    let mut min = u32::MAX;
    let mut max = 0;
    for v in values {
        min = min.min(v);
        max = max.max(v);
    }
    if max == 0 {
        (default_min, default_max)
    } else {
        (min, max)
    }
}

fn other_fault(e: anyhow::Error) -> FaultBody {
    FaultBody {
        code: FaultCode::Other,
        reason: e.to_string(),
    }
}

/// Pull the inner text of the first element with the given local name from a
/// SOAP body fragment. Tolerates namespace prefixes.
pub(crate) fn read_first_text_element(xml: &str, local: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_target = false;
    loop {
        match reader.read_event() {
            Err(_) => return None,
            Ok(Event::Eof) => return None,
            Ok(Event::Start(e)) => {
                let n = e.name();
                let name = n.into_inner();
                let s = std::str::from_utf8(name).unwrap_or("");
                let local_name = s.rsplit(':').next().unwrap_or(s);
                if local_name == local {
                    in_target = true;
                }
            }
            Ok(Event::End(_)) => in_target = false,
            Ok(Event::Text(t)) if in_target => {
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
    fn read_text_element() {
        let xml = r#"<trt:GetStreamUri xmlns:trt="x"><trt:ProfileToken>profile_foo_main</trt:ProfileToken></trt:GetStreamUri>"#;
        assert_eq!(
            read_first_text_element(xml, "ProfileToken").as_deref(),
            Some("profile_foo_main")
        );
    }

    #[test]
    fn csv_helper() {
        assert_eq!(csv_u32("30,25,20"), vec![30, 25, 20]);
        assert!(csv_u32("").is_empty());
        assert_eq!(csv_u32(" 25 , 20"), vec![25, 20]);
        // A table with a value we can't read loses that entry, not the table.
        assert_eq!(csv_u32("30,fast,20"), vec![30, 20]);
    }

    fn desc(stream: OnvifStream, framerates: Vec<u32>, bitrates: Vec<u32>) -> StreamDesc {
        StreamDesc {
            stream,
            width: 1920,
            height: 1080,
            framerate: framerates.first().copied().unwrap_or(25),
            bitrate_kbps: bitrates.first().copied().unwrap_or(2048),
            framerates,
            bitrates,
        }
    }

    /// Reolink overloads `defaultFramerate`/`defaultBitrate` as an index into
    /// the matching table. Taking the table's first entry reported the wrong
    /// current setting for every camera whose selected index was not zero, and
    /// disagreed with what `src/rtsp/factory.rs` puts in the RTSP stream.
    #[test]
    fn the_default_is_resolved_as_an_index_into_its_table() {
        // Index 2 of [30, 25, 20, 15] is 20fps — not 30.
        assert_eq!(resolve_default(&[30, 25, 20, 15], 2), 20);
        assert_eq!(resolve_default(&[30, 25, 20, 15], 0), 30);

        // Out of range means it was the literal value all along, which is the
        // other shape the field takes.
        assert_eq!(resolve_default(&[30, 25], 4096), 4096);
        assert_eq!(resolve_default(&[], 25), 25);
    }

    /// Zero is never a usable frame rate or bitrate, however it arose.
    #[test]
    fn a_resolved_default_is_never_zero() {
        assert_eq!(resolve_default(&[], 0), 1);
        assert_eq!(resolve_default(&[0, 30], 0), 1);
    }

    /// The profile has to describe the stream `GetStreamUri` actually hands
    /// out. That URL carries no `?audio=` override, so the camera's configured
    /// format decides — and the default profile is L16, not passthrough AAC.
    #[test]
    fn the_audio_description_follows_the_configured_format() {
        let aac = audio_desc(AudioFormat::Mpeg4Generic);
        assert_eq!(aac.encoding, "AAC");
        assert_eq!(aac.bitrate_kbps, 32);
        assert_eq!(audio_desc(AudioFormat::Latm).encoding, "AAC");

        // The default. ONVIF ver10 has no L16 token, but the bitrate must
        // still describe the ~256 kbps a client will actually receive.
        let pcm = audio_desc(AudioFormat::Pcm);
        assert_eq!(pcm.bitrate_kbps, 256);
        assert_ne!(
            pcm.bitrate_kbps, aac.bitrate_kbps,
            "L16 must not be described with the AAC bitrate"
        );

        // Multi-track: the profile describes the first track offered.
        assert_eq!(audio_desc(AudioFormat::All).encoding, "AAC");
    }

    /// Whatever we report has to be a value from ONVIF ver10's closed
    /// `tt:AudioEncoding` enum, or a strict client rejects the configuration.
    #[test]
    fn every_audio_encoding_is_a_legal_onvif_token() {
        for format in [
            AudioFormat::Mpeg4Generic,
            AudioFormat::Latm,
            AudioFormat::Pcm,
            AudioFormat::All,
        ] {
            let enc = audio_desc(format).encoding;
            assert!(
                matches!(enc, "G711" | "G726" | "AAC"),
                "{} is not in tt:AudioEncoding",
                enc
            );
        }
    }

    /// A client that cannot re-select the configuration the device just
    /// reported treats it as invalid, so the current value has to appear in
    /// its own option list even when the camera's table left it out.
    #[test]
    fn the_current_value_is_always_a_selectable_option() {
        assert_eq!(options_including(&[10, 20], 15), vec![10, 15, 20]);
        assert_eq!(options_including(&[], 25), vec![25]);
        // Zeroes are placeholders in Reolink's tables, not real settings.
        assert_eq!(options_including(&[0, 30], 30), vec![30]);
        assert_eq!(options_including(&[20, 20, 10], 20), vec![10, 20]);
    }

    /// The whole point of reading `EncodeTable`: the ranges we offer are the
    /// camera's own, not invented bounds.
    #[test]
    fn encoder_options_span_the_cameras_tables() {
        let main = desc(OnvifStream::Main, vec![30, 25, 15], vec![8192, 4096]);
        let sub = desc(OnvifStream::Sub, vec![15, 10], vec![1024, 512]);
        let xml = render_video_encoder_options(&[&main, &sub]);

        assert!(
            xml.contains("<tt:FrameRateRange><tt:Min>10</tt:Min><tt:Max>30</tt:Max>"),
            "{}",
            xml
        );
        assert!(
            xml.contains("<tt:BitrateRange><tt:Min>512</tt:Min><tt:Max>8192</tt:Max>"),
            "{}",
            xml
        );
        // Both streams' resolutions are offered, deduped.
        assert_eq!(
            xml.matches("<tt:ResolutionsAvailable>").count(),
            2,
            "{}",
            xml
        );
        // The bitrate range only exists in the extension block, so a client
        // that reads only the base H264 element still gets valid options.
        assert!(xml.contains("<tt:Extension>"), "{}", xml);
    }

    /// A camera that never answered leaves empty tables; the options still have
    /// to be a usable range rather than an empty or inverted one.
    #[test]
    fn encoder_options_fall_back_when_the_camera_said_nothing() {
        let d = StreamDesc::fallback(OnvifStream::Main);
        let xml = render_video_encoder_options(&[&d]);
        // `fallback` reports 25fps/4096kbps, and those are the only values
        // known, so they bound the range.
        assert!(
            xml.contains("<tt:FrameRateRange><tt:Min>25</tt:Min><tt:Max>25</tt:Max>"),
            "{}",
            xml
        );
        assert!(
            xml.contains("<tt:BitrateRange><tt:Min>4096</tt:Min><tt:Max>4096</tt:Max>"),
            "{}",
            xml
        );
    }

    #[test]
    fn span_falls_back_only_when_there_is_nothing_to_span() {
        assert_eq!(span(std::iter::empty(), 1, 30), (1, 30));
        assert_eq!(span(vec![7u32].into_iter(), 1, 30), (7, 7));
        assert_eq!(span(vec![7u32, 3, 9].into_iter(), 1, 30), (3, 9));
    }

    /// Audio is what makes a VMS request the audio track at all, so the two
    /// configurations have to be inside the profile — and absent, not faked,
    /// on a camera with no microphone.
    #[test]
    fn audio_configurations_follow_the_capability() {
        let src = render_audio_source_configuration("cam", "tt:AudioSourceConfiguration");
        assert!(src.contains("token=\"as_cam\""), "{}", src);
        assert!(
            src.contains("<tt:SourceToken>asrc_cam</tt:SourceToken>"),
            "{}",
            src
        );

        let enc = render_audio_encoder_configuration(
            "cam",
            AudioFormat::Mpeg4Generic,
            "tt:AudioEncoderConfiguration",
        );
        assert!(enc.contains("token=\"aec_cam\""), "{}", enc);
        assert!(enc.contains("<tt:Encoding>AAC</tt:Encoding>"), "{}", enc);
    }

    fn parts(audio: bool, ptz: bool) -> ProfileParts {
        ProfileParts {
            token: "profile_cam_main".to_string(),
            name: "cam_main".to_string(),
            video_source: "<tt:VideoSourceConfiguration/>".to_string(),
            audio_source: audio.then(|| "<tt:AudioSourceConfiguration/>".to_string()),
            video_encoder: "<tt:VideoEncoderConfiguration/>".to_string(),
            audio_encoder: audio.then(|| "<tt:AudioEncoderConfiguration/>".to_string()),
            ptz: ptz.then(|| "<tt:PTZConfiguration/>".to_string()),
        }
    }

    /// `tt:Profile` is an xs:sequence, and the audio configurations are
    /// interleaved with the video ones rather than appended after them. Nothing
    /// about the output looks wrong until a strict client rejects the whole
    /// profile, so the order is pinned here.
    #[test]
    fn a_profile_emits_its_configurations_in_schema_order() {
        let xml = parts(true, true).render("trt:Profiles");
        let order = [
            "<tt:Name>",
            "<tt:VideoSourceConfiguration/>",
            "<tt:AudioSourceConfiguration/>",
            "<tt:VideoEncoderConfiguration/>",
            "<tt:AudioEncoderConfiguration/>",
            "<tt:PTZConfiguration/>",
        ];
        let mut last = 0;
        for element in order {
            let at = xml
                .find(element)
                .unwrap_or_else(|| panic!("{} should be present in {}", element, xml));
            assert!(at >= last, "{} is out of order in {}", element, xml);
            last = at;
        }
    }

    /// The optional halves drop out without disturbing the rest of the order.
    #[test]
    fn a_profile_without_audio_or_ptz_is_still_well_formed() {
        let xml = parts(false, false).render("trt:Profiles");
        assert!(!xml.contains("Audio"), "{}", xml);
        assert!(!xml.contains("PTZ"), "{}", xml);
        assert!(xml.starts_with("<trt:Profiles fixed=\"true\" token=\"profile_cam_main\">"));
        assert!(xml.ends_with("</trt:Profiles>"), "{}", xml);
    }

    /// `GetProfile` returns the same body under a singular element name; this
    /// used to be a pair of string replacements on the rendered output.
    #[test]
    fn the_same_parts_render_under_either_element_name() {
        let p = parts(true, false);
        let single = p.render("trt:Profile");
        assert!(single.starts_with("<trt:Profile fixed="), "{}", single);
        assert!(single.ends_with("</trt:Profile>"), "{}", single);
        // The contents are identical — only the wrapper differs.
        let plural = p.render("trt:Profiles");
        assert_eq!(
            single
                .replace("trt:Profile ", "trt:Profiles ")
                .replace("</trt:Profile>", "</trt:Profiles>"),
            plural
        );
    }

    /// The audio source token has to be the one the events code already uses
    /// for baby-cry notifications, or a client cannot tie the two together.
    #[test]
    fn the_audio_source_token_matches_the_events_surface() {
        let src = render_audio_source_configuration("cam", "tt:AudioSourceConfiguration");
        assert!(src.contains("asrc_cam"), "{}", src);
    }
}
