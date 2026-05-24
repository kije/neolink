//! ONVIF Media service. Builds the MediaProfile list and hands out the RTSP
//! and snapshot URIs that point back to neolink's existing servers.

use anyhow::Result;
use neolink_core::bc::xml::EncodeTable;
use neolink_core::bc_protocol::BcCamera;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::onvif::services::device::FaultBody;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{url_path_segment, CameraEntry, OnvifState, OnvifStream};

/// Per-stream descriptor used to build profile XML.
struct StreamDesc {
    stream: OnvifStream,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate_kbps: u32,
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
        }
    }
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
                let fps = first_csv_u32(&t.framerate_table).unwrap_or(t.default_framerate.max(1));
                let br = first_csv_u32(&t.bitrate_table).unwrap_or(t.default_bitrate.max(1));
                StreamDesc {
                    stream: *s,
                    width: t.resolution.width,
                    height: t.resolution.height,
                    framerate: fps,
                    bitrate_kbps: br,
                }
            } else {
                StreamDesc::fallback(*s)
            }
        })
        .collect()
}

fn first_csv_u32(s: &str) -> Option<u32> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).next()
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
fn ptz_config_token(cam: &str) -> String {
    format!("ptz_{cam}")
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

fn render_ptz_configuration(cam: &CameraEntry) -> String {
    format!(
        "<tt:PTZConfiguration token=\"{tok}\">\
<tt:Name>{name}</tt:Name>\
<tt:UseCount>1</tt:UseCount>\
<tt:NodeToken>ptz_node_{cam_name}</tt:NodeToken>\
<tt:DefaultContinuousPanTiltVelocitySpace>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:DefaultContinuousPanTiltVelocitySpace>\
<tt:DefaultContinuousZoomVelocitySpace>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:DefaultContinuousZoomVelocitySpace>\
<tt:DefaultPTZSpeed>\
<tt:PanTilt x=\"0.5\" y=\"0.5\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace\"/>\
<tt:Zoom x=\"0.5\" space=\"http://www.onvif.org/ver10/tptz/ZoomSpaces/ZoomGenericSpeedSpace\"/>\
</tt:DefaultPTZSpeed>\
<tt:DefaultPTZTimeout>PT5S</tt:DefaultPTZTimeout>\
</tt:PTZConfiguration>",
        tok = ptz_config_token(&cam.name),
        name = xml_escape(&format!("{}_ptz", cam.name)),
        cam_name = xml_escape(&cam.name),
    )
}

async fn render_profile(cam: &CameraEntry, d: &StreamDesc, vs_xml: &str, has_ptz: bool) -> String {
    let token = profile_token(&cam.name, d.stream);
    let name = format!("{}_{}", cam.name, d.stream.token_suffix());
    let mut out = format!(
        "<trt:Profiles fixed=\"true\" token=\"{tok}\">\
<tt:Name>{name}</tt:Name>",
        tok = xml_escape(&token),
        name = xml_escape(&name),
    );
    out.push_str(vs_xml);
    out.push_str(&render_video_encoder_configuration(&cam.name, d));
    if has_ptz {
        out.push_str(&render_ptz_configuration(cam));
    }
    out.push_str("</trt:Profiles>");
    out
}

async fn has_ptz_capability(cam: &CameraEntry) -> bool {
    cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_abilityinfo().await?) }))
        .await
        .map(|info| info.ptz.is_some())
        .unwrap_or(true) // Default to advertising PTZ — caller can ignore.
}

pub(crate) async fn dispatch(
    state: &OnvifState,
    cam: &CameraEntry,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let descs = read_stream_descs(cam).await;
    let has_ptz = has_ptz_capability(cam).await;
    let vs_xml = render_video_source_configuration(cam, &descs);

    let body = match action {
        "GetProfiles" => {
            let mut profiles = String::new();
            for d in &descs {
                profiles.push_str(&render_profile(cam, d, &vs_xml, has_ptz).await);
            }
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
            let p = render_profile(cam, stream, &vs_xml, has_ptz).await;
            // The single-profile response uses `Profile` not `Profiles`.
            let p = p.replace("<trt:Profiles", "<trt:Profile")
                .replace("</trt:Profiles>", "</trt:Profile>");
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
        "GetVideoSources" => format!(
            "<trt:GetVideoSourcesResponse><tt:VideoSources token=\"vsrc_{cam}\">\
<tt:Framerate>30</tt:Framerate><tt:Resolution><tt:Width>{w}</tt:Width><tt:Height>{h}</tt:Height></tt:Resolution>\
</tt:VideoSources></trt:GetVideoSourcesResponse>",
            cam = xml_escape(&cam.name),
            w = descs.first().map(|d| d.width).unwrap_or(1920),
            h = descs.first().map(|d| d.height).unwrap_or(1080),
        ),
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
        "GetServiceCapabilities" => {
            "<trt:GetServiceCapabilitiesResponse><trt:Capabilities SnapshotUri=\"true\" Rotation=\"false\" VideoSourceMode=\"false\" OSD=\"false\"><trt:ProfileCapabilities MaximumNumberOfProfiles=\"3\"/><trt:StreamingCapabilities RTPMulticast=\"false\" RTP_TCP=\"true\" RTP_RTSP_TCP=\"true\" NonAggregateControl=\"false\" NoRTSPStreaming=\"false\"/></trt:Capabilities></trt:GetServiceCapabilitiesResponse>".to_string()
        }
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Media action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
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
        assert_eq!(first_csv_u32("30,25,20"), Some(30));
        assert_eq!(first_csv_u32(""), None);
        assert_eq!(first_csv_u32(" 25 , 20"), Some(25));
    }
}
