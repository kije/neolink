//! ONVIF Device service. Describes the camera and carries the handful of
//! device-level controls the Baichuan protocol can back:
//!
//! * relay outputs — the floodlight, siren and LEDs, which is how a VMS gets
//!   switches for them (`GetRelayOutputs` / `SetRelayOutputState`)
//! * `SystemReboot`
//! * the camera's own clock (`GetSystemDateAndTime` / `SetSystemDateAndTime`)

use anyhow::Result;
use neolink_core::bc::xml::SystemGeneral;
use neolink_core::bc_protocol::{BcCamera, LightState};

use crate::onvif::capabilities::{capabilities, CameraCapabilities};
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::{url_path_segment, CameraEntry, OnvifState};

pub(crate) struct DeviceInfo {
    pub(crate) manufacturer: String,
    pub(crate) model: String,
    pub(crate) firmware_version: String,
    pub(crate) serial_number: String,
    pub(crate) hardware_id: String,
}

/// Pull DeviceInformation off the camera. Falls back to neutral strings on any
/// transient read failure so a quirky camera doesn't bring the bridge down.
async fn read_device_info(cam: &CameraEntry) -> DeviceInfo {
    let mut info = DeviceInfo {
        manufacturer: "Reolink".to_string(),
        model: "Unknown".to_string(),
        firmware_version: "Unknown".to_string(),
        serial_number: "Unknown".to_string(),
        hardware_id: "Unknown".to_string(),
    };
    let r = cam
        .run(|c: &BcCamera| {
            Box::pin(async move {
                let v = c.version().await?;
                Ok::<_, anyhow::Error>(v)
            })
        })
        .await;
    if let Ok(v) = r {
        if let Some(m) = v.model {
            info.model = m;
        }
        info.firmware_version = v.firmwareVersion;
        info.serial_number = v.serialNumber;
        info.hardware_id = v.hardwareVersion;
    }
    info
}

pub(crate) async fn dispatch(
    state: &OnvifState,
    cam: &CameraEntry,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    let body = match action {
        "GetDeviceInformation" => {
            let info = read_device_info(cam).await;
            format!(
                "<tds:GetDeviceInformationResponse>\
<tds:Manufacturer>{m}</tds:Manufacturer>\
<tds:Model>{mo}</tds:Model>\
<tds:FirmwareVersion>{fw}</tds:FirmwareVersion>\
<tds:SerialNumber>{sn}</tds:SerialNumber>\
<tds:HardwareId>{hw}</tds:HardwareId>\
</tds:GetDeviceInformationResponse>",
                m = xml_escape(&info.manufacturer),
                mo = xml_escape(&info.model),
                fw = xml_escape(&info.firmware_version),
                sn = xml_escape(&info.serial_number),
                hw = xml_escape(&info.hardware_id),
            )
        }
        "GetSystemDateAndTime" => render_system_date_and_time(read_camera_clock(cam).await),
        "SetSystemDateAndTime" => {
            let wanted = parse_set_system_date_and_time(body_xml).ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: "SetSystemDateAndTime needs a UTCDateTime (DateTimeType 'Manual')"
                    .to_string(),
            })?;
            write_camera_clock(cam, wanted).await?;
            "<tds:SetSystemDateAndTimeResponse/>".to_string()
        }
        "SystemReboot" => {
            cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.reboot().await?) }))
                .await
                .map_err(other_fault)?;
            "<tds:SystemRebootResponse><tds:Message>Rebooting</tds:Message>\
</tds:SystemRebootResponse>"
                .to_string()
        }
        "GetCapabilities" | "GetServices" => {
            let authority = state.advertise_authority().await.map_err(|e| FaultBody {
                code: FaultCode::Other,
                reason: format!("Failed to determine advertise host: {e}"),
            })?;
            let cam_seg = url_path_segment(&cam.name);
            let dev = format!("http://{authority}/onvif/{cam_seg}/device_service");
            let media = format!("http://{authority}/onvif/{cam_seg}/media_service");
            let ptz = format!("http://{authority}/onvif/{cam_seg}/ptz_service");
            let evt = format!("http://{authority}/onvif/{cam_seg}/events_service");
            let img = format!("http://{authority}/onvif/{cam_seg}/imaging_service");
            // A camera with no motor gets no PTZ service address at all. This
            // is the switch most clients actually look at: Home Assistant and
            // Frigate decide whether to offer PTZ controls from the presence
            // of this entry, long before they ever call GetNodes. The Imaging
            // address follows the same rule: it only appears for a camera with
            // an IR cut filter or a focus motor to drive.
            //
            // `tt:Capabilities` is an xs:sequence — Device, Events, Imaging,
            // Media, PTZ — so the optional blocks below have to be spliced in
            // at their schema position, not appended. gSOAP-based clients
            // (ONVIF Device Manager among them) reject the whole response
            // otherwise.
            let caps = capabilities(cam).await;
            let has_ptz = caps.ptz();
            let has_imaging = caps.imaging();
            if action == "GetCapabilities" {
                let ptz_xml = if has_ptz {
                    format!("<tt:PTZ><tt:XAddr>{ptz}</tt:XAddr></tt:PTZ>")
                } else {
                    String::new()
                };
                let img_xml = if has_imaging {
                    format!("<tt:Imaging><tt:XAddr>{img}</tt:XAddr></tt:Imaging>")
                } else {
                    String::new()
                };
                format!(
                    "<tds:GetCapabilitiesResponse><tds:Capabilities>\
<tt:Device><tt:XAddr>{dev}</tt:XAddr>\
<tt:Network><tt:IPFilter>false</tt:IPFilter><tt:ZeroConfiguration>false</tt:ZeroConfiguration><tt:IPVersion6>false</tt:IPVersion6><tt:DynDNS>false</tt:DynDNS></tt:Network>\
<tt:System><tt:DiscoveryResolve>false</tt:DiscoveryResolve><tt:DiscoveryBye>true</tt:DiscoveryBye><tt:RemoteDiscovery>false</tt:RemoteDiscovery><tt:SystemBackup>false</tt:SystemBackup><tt:SystemLogging>false</tt:SystemLogging><tt:FirmwareUpgrade>false</tt:FirmwareUpgrade></tt:System>\
{io_xml}\
</tt:Device>\
<tt:Events><tt:XAddr>{evt}</tt:XAddr><tt:WSSubscriptionPolicySupport>false</tt:WSSubscriptionPolicySupport><tt:WSPullPointSupport>true</tt:WSPullPointSupport><tt:WSPausableSubscriptionManagerInterfaceSupport>false</tt:WSPausableSubscriptionManagerInterfaceSupport></tt:Events>\
{img_xml}\
<tt:Media><tt:XAddr>{media}</tt:XAddr><tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media>\
{ptz_xml}\
</tds:Capabilities></tds:GetCapabilitiesResponse>",
                    // Only claim an IO section when there is something in it:
                    // a client that sees `RelayOutputs 0` and a client that
                    // sees no IO block at all both draw no switches, but the
                    // second is what a camera without relays actually sends.
                    io_xml = if caps.relays() {
                        format!(
                            "<tt:IO><tt:InputConnectors>0</tt:InputConnectors>\
<tt:RelayOutputs>{}</tt:RelayOutputs></tt:IO>",
                            relay_outputs(&caps).len()
                        )
                    } else {
                        String::new()
                    },
                )
            } else {
                let ptz_xml = if has_ptz {
                    format!(
                        "<tds:Service><tds:Namespace>http://www.onvif.org/ver20/ptz/wsdl</tds:Namespace><tds:XAddr>{ptz}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>"
                    )
                } else {
                    String::new()
                };
                let img_xml = if has_imaging {
                    format!(
                        "<tds:Service><tds:Namespace>http://www.onvif.org/ver20/imaging/wsdl</tds:Namespace><tds:XAddr>{img}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>"
                    )
                } else {
                    String::new()
                };
                format!(
                    "<tds:GetServicesResponse>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace><tds:XAddr>{dev}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace><tds:XAddr>{media}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver10/events/wsdl</tds:Namespace><tds:XAddr>{evt}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
{img_xml}\
{ptz_xml}\
</tds:GetServicesResponse>"
                )
            }
        }
        "GetServiceCapabilities" => "<tds:GetServiceCapabilitiesResponse><tds:Capabilities>\
<tds:Network IPFilter=\"false\" ZeroConfiguration=\"false\" IPVersion6=\"false\" DynDNS=\"false\"/>\
<tds:Security TLS1.0=\"false\" TLS1.1=\"false\" TLS1.2=\"false\" OnboardKeyGeneration=\"false\" AccessPolicyConfig=\"false\" X.509Token=\"false\" SAMLToken=\"false\" KerberosToken=\"false\" UsernameToken=\"true\" HttpDigest=\"false\" RELToken=\"false\"/>\
<tds:System DiscoveryResolve=\"false\" DiscoveryBye=\"true\" RemoteDiscovery=\"false\" SystemBackup=\"false\" SystemLogging=\"false\" FirmwareUpgrade=\"false\"/>\
</tds:Capabilities></tds:GetServiceCapabilitiesResponse>".to_string(),
        "GetHostname" => format!(
            "<tds:GetHostnameResponse><tds:HostnameInformation><tt:FromDHCP>false</tt:FromDHCP><tt:Name>{}</tt:Name></tds:HostnameInformation></tds:GetHostnameResponse>",
            xml_escape(&cam.name)
        ),
        "GetScopes" => {
            let mut scopes = vec![
                "onvif://www.onvif.org/Profile/Streaming".to_string(),
                "onvif://www.onvif.org/type/video_encoder".to_string(),
                "onvif://www.onvif.org/type/Network_Video_Transmitter".to_string(),
                "onvif://www.onvif.org/location/neolink".to_string(),
                "onvif://www.onvif.org/hardware/neolink".to_string(),
                format!("onvif://www.onvif.org/name/{}", scope_safe(&cam.name)),
            ];
            // The PTZ scope has to track the same capability probe the rest of
            // the device does, or a discovery-driven client and a
            // GetCapabilities-driven one end up disagreeing about the same
            // camera.
            if capabilities(cam).await.ptz() {
                scopes.push("onvif://www.onvif.org/type/ptz".to_string());
            }
            // Add the model as a hardware scope if known.
            if let Ok(v) = cam
                .run(|c| Box::pin(async move { Ok(c.version().await?) }))
                .await
            {
                if let Some(m) = v.model {
                    scopes.push(format!("onvif://www.onvif.org/hardware/{}", scope_safe(&m)));
                }
            }
            let items: String = scopes
                .into_iter()
                .map(|s| {
                    format!(
                        "<tt:Scopes><tt:ScopeDef>Fixed</tt:ScopeDef><tt:ScopeItem>{}</tt:ScopeItem></tt:Scopes>",
                        xml_escape(&s)
                    )
                })
                .collect();
            format!("<tds:GetScopesResponse>{items}</tds:GetScopesResponse>")
        }
        "GetRelayOutputs" => {
            let outputs: String = relay_outputs(&capabilities(cam).await)
                .iter()
                .map(|r| r.render("tds:RelayOutputs"))
                .collect();
            format!("<tds:GetRelayOutputsResponse>{outputs}</tds:GetRelayOutputsResponse>")
        }
        "GetRelayOutputOptions" => {
            let outputs: String = relay_outputs(&capabilities(cam).await)
                .iter()
                .map(|r| r.render_options())
                .collect();
            format!(
                "<tds:GetRelayOutputOptionsResponse>{outputs}</tds:GetRelayOutputOptionsResponse>"
            )
        }
        "SetRelayOutputState" => {
            let token = read_first_text_element(body_xml, "RelayOutputToken").ok_or_else(|| {
                FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing RelayOutputToken".to_string(),
                }
            })?;
            let logical = read_first_text_element(body_xml, "LogicalState").ok_or_else(|| {
                FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "Missing LogicalState".to_string(),
                }
            })?;
            let active = match logical.trim() {
                "active" => true,
                "inactive" => false,
                other => {
                    return Err(FaultBody {
                        code: FaultCode::InvalidArgs,
                        reason: format!("LogicalState must be 'active' or 'inactive', got '{other}'"),
                    })
                }
            };
            let caps = capabilities(cam).await;
            let relay = relay_outputs(&caps)
                .into_iter()
                .find(|r| r.token == token)
                .ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: format!("Unknown relay output token '{token}'"),
                })?;
            drive_relay(cam, relay.kind, active).await?;
            "<tds:SetRelayOutputStateResponse/>".to_string()
        }
        "GetNetworkInterfaces" => {
            let host = state.advertise_host().await.map_err(other_fault)?;
            render_network_interfaces(cam, &host)
        }
        "GetUsers" => {
            // Usernames and levels only — a password never leaves the bridge.
            // A camera with an ACL reports just the users that ACL admits, so
            // the list matches who can actually reach this device.
            let all = state.inner().users.read().await.clone();
            let mut names: Vec<&String> = match cam.permitted_users.as_ref() {
                Some(allow) if !allow.is_empty() => {
                    all.keys().filter(|n| allow.contains(n)).collect()
                }
                _ => all.keys().collect(),
            };
            names.sort();
            let users: String = names
                .into_iter()
                .map(|n| {
                    format!(
                        "<tds:User><tt:Username>{}</tt:Username>\
<tt:UserLevel>Administrator</tt:UserLevel></tds:User>",
                        xml_escape(n)
                    )
                })
                .collect();
            format!("<tds:GetUsersResponse>{users}</tds:GetUsersResponse>")
        }
        "GetEndpointReference" => format!(
            "<tds:GetEndpointReferenceResponse><tds:GUID>urn:uuid:{}</tds:GUID>\
</tds:GetEndpointReferenceResponse>",
            cam.uuid
        ),
        "GetWsdlUrl" => {
            "<tds:GetWsdlUrlResponse><tds:WsdlUrl>http://www.onvif.org/onvif/ver10/device/wsdl/devicemgmt.wsdl</tds:WsdlUrl></tds:GetWsdlUrlResponse>".to_string()
        }
        "GetDot11Capabilities" => "<tds:GetDot11CapabilitiesResponse></tds:GetDot11CapabilitiesResponse>".to_string(),
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("Device action '{other}' not supported"),
            });
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
}

/// The device-level switches this bridge can drive, expressed as ONVIF relay
/// outputs.
///
/// Relay outputs are the only device-level actuator ONVIF has, and clients
/// render them generically: Home Assistant turns each one into a switch entity,
/// and most VMSes into a button. That makes them the cheapest way to expose the
/// floodlight, siren and LEDs to a client that knows nothing about Reolink.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RelayKind {
    Floodlight,
    Siren,
    /// The infrared illuminator. Tri-state on the camera (`auto` / `open` /
    /// `close`) but binary over ONVIF; see `drive_relay` for the mapping.
    IrLed,
    /// The little status light on the front of the camera.
    StatusLed,
}

pub(crate) struct RelayOutput {
    pub(crate) token: &'static str,
    pub(crate) kind: RelayKind,
    /// `Bistable` latches until told otherwise; `Monostable` springs back on
    /// its own after `delay`.
    mode: &'static str,
    delay: &'static str,
}

impl RelayOutput {
    /// Render as a `RelayOutput`, under whatever element name the enclosing
    /// response uses.
    fn render(&self, element: &str) -> String {
        format!(
            "<{element} token=\"{tok}\"><tt:Properties>\
<tt:Mode>{mode}</tt:Mode>\
<tt:DelayTime>{delay}</tt:DelayTime>\
<tt:IdleState>open</tt:IdleState>\
</tt:Properties></{element}>",
            tok = self.token,
            mode = self.mode,
            delay = self.delay,
        )
    }

    fn render_options(&self) -> String {
        format!(
            "<tds:RelayOutputOptions token=\"{tok}\">\
<tds:Mode>{mode}</tds:Mode>\
<tds:DelayTimes>{delay}</tds:DelayTimes>\
<tds:Discrete>false</tds:Discrete>\
</tds:RelayOutputOptions>",
            tok = self.token,
            mode = self.mode,
            delay = self.delay,
        )
    }
}

/// The relays this camera should advertise.
///
/// Each one is gated on the capability probe, for the same reason the PTZ
/// service is: a switch that cannot do anything is worse than a missing one,
/// because the client shows it as working right up until the user presses it.
pub(crate) fn relay_outputs(caps: &CameraCapabilities) -> Vec<RelayOutput> {
    let mut out = vec![];
    if caps.floodlight {
        out.push(RelayOutput {
            token: "relay_floodlight",
            kind: RelayKind::Floodlight,
            mode: "Bistable",
            delay: "PT0S",
        });
    }
    if caps.siren() {
        // The siren is a one-shot: the camera plays the alarm and stops on its
        // own, so it is Monostable and the client gets a momentary switch
        // rather than one it has to remember to turn off.
        out.push(RelayOutput {
            token: "relay_siren",
            kind: RelayKind::Siren,
            mode: "Monostable",
            delay: "PT5S",
        });
    }
    if caps.led_ctrl {
        out.push(RelayOutput {
            token: "relay_ir_illuminator",
            kind: RelayKind::IrLed,
            mode: "Bistable",
            delay: "PT0S",
        });
        out.push(RelayOutput {
            token: "relay_status_led",
            kind: RelayKind::StatusLed,
            mode: "Bistable",
            delay: "PT0S",
        });
    }
    out
}

async fn drive_relay(cam: &CameraEntry, kind: RelayKind, active: bool) -> Result<(), FaultBody> {
    match kind {
        RelayKind::Floodlight => {
            // The BC call wants a duration for the "on" case. 180s matches the
            // MQTT `control/floodlight` surface, so the two agree about what
            // "on" means on the same camera.
            cam.run(move |c: &BcCamera| {
                Box::pin(async move { Ok(c.set_floodlight_manual(active, 180).await?) })
            })
            .await
            .map_err(other_fault)?;
        }
        RelayKind::Siren => {
            // Monostable: only the active edge does anything. Releasing it is
            // the camera's job, so `inactive` is a no-op rather than a fault —
            // clients routinely send it right after the activation.
            if active {
                cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.siren().await?) }))
                    .await
                    .map_err(other_fault)?;
            }
        }
        RelayKind::IrLed => {
            // `active` is `auto`, not `open`. The IR illuminator is a
            // night-time device: leaving it forced on in daylight washes out
            // the picture, so "on" meaning "let the camera decide" is what a
            // user wants from a switch — and it is the same convention Home
            // Assistant's Reolink integration uses for its IR-lights switch.
            cam.run(move |c: &BcCamera| {
                // `LightState` is not `Clone`, and `run` may invoke this more
                // than once, so build a fresh one per attempt.
                let state = if active {
                    LightState::Auto
                } else {
                    LightState::Off
                };
                Box::pin(async move { Ok(c.irled_light_set(state).await?) })
            })
            .await
            .map_err(other_fault)?;
        }
        RelayKind::StatusLed => {
            cam.run(move |c: &BcCamera| {
                Box::pin(async move { Ok(c.led_light_set(active).await?) })
            })
            .await
            .map_err(other_fault)?;
        }
    }
    Ok(())
}

/// The camera's clock, as ONVIF needs to describe it.
#[derive(Clone, Copy)]
struct CameraClock {
    /// Wall-clock time on the camera, in UTC.
    utc: chrono::DateTime<chrono::Utc>,
    /// The camera's raw `timeZone` field: seconds *west* of UTC, i.e. already
    /// in the inverted sense both POSIX `TZ` strings and ONVIF use.
    posix_offset_seconds: i32,
}

/// How long a clock sample is reused before the camera is asked again.
///
/// The point is not to save work but to keep an unauthenticated operation from
/// generating camera traffic: `GetSystemDateAndTime` is on the ONVIF pre-auth
/// whitelist, so without this a client that never logs in could drive one BC
/// round-trip per request — which on a battery camera means keeping it awake.
/// Between samples the reading is advanced by the bridge's own elapsed time,
/// which is exact unless the camera's clock is drifting against ours, and a
/// minute of drift is far below what any client cares about here.
const CLOCK_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Cached clock sample. Lives on the `CameraEntry` alongside the capability
/// cache, so it survives config reloads with the connection it describes.
#[derive(Default)]
pub(crate) struct ClockCache {
    slot: tokio::sync::Mutex<Option<ClockSample>>,
}

struct ClockSample {
    at: std::time::Instant,
    clock: CameraClock,
}

/// Read the camera's own clock, reusing a recent sample.
///
/// Deliberately built from `get_general` rather than `BcCamera::get_time`:
/// `get_time` hands back an `OffsetDateTime` whose offset is the raw Reolink
/// field, which is the negation of the real UTC offset, so anything derived
/// from it is wrong by twice the offset outside UTC. Here the sign convention
/// is applied explicitly.
///
/// `None` means the camera didn't answer or didn't fill the fields in; the
/// caller falls back to the bridge's own clock.
async fn read_camera_clock(cam: &CameraEntry) -> Option<CameraClock> {
    let mut slot = cam.clock.slot.lock().await;
    if let Some(sample) = slot.as_ref() {
        let age = sample.at.elapsed();
        if age < CLOCK_CACHE_TTL {
            return Some(advance(sample.clock, age));
        }
    }
    let general = cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_general().await?) }))
        .await
        .ok()?;
    let clock = clock_from_general(&general)?;
    *slot = Some(ClockSample {
        at: std::time::Instant::now(),
        clock,
    });
    Some(clock)
}

/// Move a cached sample forward by how long ago it was taken.
fn advance(clock: CameraClock, by: std::time::Duration) -> CameraClock {
    CameraClock {
        utc: clock.utc + chrono::Duration::seconds(by.as_secs() as i64),
        ..clock
    }
}

/// Drop any cached sample, so the next read goes to the camera.
///
/// Called after writing the clock: the value we just set is what the camera
/// now holds, and continuing to extrapolate the pre-write sample would report
/// the old time for up to a minute after a successful `SetSystemDateAndTime`.
async fn invalidate_clock_cache(cam: &CameraEntry) {
    *cam.clock.slot.lock().await = None;
}

fn clock_from_general(general: &SystemGeneral) -> Option<CameraClock> {
    use chrono::{NaiveDate, TimeZone, Utc};
    let posix_offset_seconds = general.time_zone?;
    let local = NaiveDate::from_ymd_opt(general.year?, general.month? as u32, general.day? as u32)?
        .and_hms_opt(
            general.hour? as u32,
            general.minute? as u32,
            general.second? as u32,
        )?;
    // Reolink stores seconds *west* of UTC, so UTC is the local reading plus
    // that value rather than minus it.
    let utc =
        Utc.from_utc_datetime(&local) + chrono::Duration::seconds(posix_offset_seconds as i64);
    Some(CameraClock {
        utc,
        posix_offset_seconds,
    })
}

/// Push a UTC instant onto the camera, preserving its configured time zone.
async fn write_camera_clock(
    cam: &CameraEntry,
    utc: chrono::DateTime<chrono::Utc>,
) -> Result<(), FaultBody> {
    // Read-modify-write: `SystemGeneral` also carries the device name, language
    // and OSD format, and sending a partial structure back would be a silent
    // way to lose them.
    let mut general = cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_general().await?) }))
        .await
        .map_err(other_fault)?;

    let offset = general.time_zone.unwrap_or(0);
    let local = utc - chrono::Duration::seconds(offset as i64);
    let naive = local.naive_utc();
    general.year = Some(chrono::Datelike::year(&naive));
    general.month = Some(chrono::Datelike::month(&naive) as u8);
    general.day = Some(chrono::Datelike::day(&naive) as u8);
    general.hour = Some(chrono::Timelike::hour(&naive) as u8);
    general.minute = Some(chrono::Timelike::minute(&naive) as u8);
    general.second = Some(chrono::Timelike::second(&naive) as u8);
    general.time_zone = Some(offset);

    cam.run(move |c: &BcCamera| {
        // `SystemGeneral` is not `Clone`, so rebuild the parts we need for
        // each attempt the camera actor may make.
        let g = SystemGeneral {
            version: general.version.clone(),
            time_zone: general.time_zone,
            year: general.year,
            month: general.month,
            day: general.day,
            hour: general.hour,
            minute: general.minute,
            second: general.second,
            osd_format: general.osd_format.clone(),
            time_format: general.time_format,
            language: general.language.clone(),
            device_name: general.device_name.clone(),
        };
        Box::pin(async move { Ok(c.set_general(g).await?) })
    })
    .await
    .map_err(other_fault)?;
    invalidate_clock_cache(cam).await;
    Ok(())
}

fn render_system_date_and_time(clock: Option<CameraClock>) -> String {
    use chrono::{Datelike, Timelike};
    // A camera that won't tell us its clock still needs a well-formed answer —
    // `GetSystemDateAndTime` is the unauthenticated probe every client opens
    // with, so failing it looks like the whole device is down.
    let (utc, posix_offset) = match clock {
        Some(c) => (c.utc, c.posix_offset_seconds),
        None => (chrono::Utc::now(), 0),
    };
    let local = utc - chrono::Duration::seconds(posix_offset as i64);
    let local = local.naive_utc();
    format!(
        "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
<tt:DateTimeType>Manual</tt:DateTimeType>\
<tt:DaylightSavings>false</tt:DaylightSavings>\
<tt:TimeZone><tt:TZ>{tz}</tt:TZ></tt:TimeZone>\
<tt:UTCDateTime>\
<tt:Time><tt:Hour>{uh}</tt:Hour><tt:Minute>{umi}</tt:Minute><tt:Second>{us}</tt:Second></tt:Time>\
<tt:Date><tt:Year>{uy}</tt:Year><tt:Month>{umo}</tt:Month><tt:Day>{ud}</tt:Day></tt:Date>\
</tt:UTCDateTime>\
<tt:LocalDateTime>\
<tt:Time><tt:Hour>{lh}</tt:Hour><tt:Minute>{lmi}</tt:Minute><tt:Second>{ls}</tt:Second></tt:Time>\
<tt:Date><tt:Year>{ly}</tt:Year><tt:Month>{lmo}</tt:Month><tt:Day>{ld}</tt:Day></tt:Date>\
</tt:LocalDateTime>\
</tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
        tz = posix_tz(posix_offset),
        uy = utc.year(),
        umo = utc.month(),
        ud = utc.day(),
        uh = utc.hour(),
        umi = utc.minute(),
        us = utc.second(),
        ly = local.year(),
        lmo = local.month(),
        ld = local.day(),
        lh = local.hour(),
        lmi = local.minute(),
        ls = local.second(),
    )
}

/// Format a POSIX `TZ` string from seconds *west* of UTC.
///
/// POSIX inverts the usual sign — `UTC-07:00` is seven hours *ahead* of UTC —
/// which is the same convention Reolink's `timeZone` field uses, so the value
/// passes through unnegated.
fn posix_tz(seconds_west: i32) -> String {
    let sign = if seconds_west < 0 { '-' } else { '+' };
    let abs = seconds_west.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
}

/// Pull the UTC instant out of a `SetSystemDateAndTime` request.
///
/// `None` when there is no usable `UTCDateTime` — which is what a client asking
/// for `DateTimeType` `NTP` sends, and the bridge has no way to configure NTP
/// on the camera.
fn parse_set_system_date_and_time(body_xml: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, TimeZone, Utc};
    let num = |name: &str| -> Option<i64> {
        read_first_text_element(body_xml, name)?.trim().parse().ok()
    };
    let date = NaiveDate::from_ymd_opt(
        num("Year")? as i32,
        num("Month")? as u32,
        num("Day")? as u32,
    )?
    .and_hms_opt(
        num("Hour")? as u32,
        num("Minute")? as u32,
        num("Second")? as u32,
    )?;
    Some(Utc.from_utc_datetime(&date))
}

/// Describe the bridge's own network presence for this virtual device.
///
/// Every camera answers on the same host address, so the interesting field is
/// the MAC: Home Assistant keys its ONVIF device registry entry on it, and
/// reporting the host's real MAC for all of them would collapse every camera
/// into one device. Instead each gets a stable locally-administered address
/// derived from its own ONVIF UUID — unique per camera, unchanged across
/// restarts, and marked (bit 0x02) as not globally assigned so it can never be
/// mistaken for real hardware.
fn render_network_interfaces(cam: &CameraEntry, host: &str) -> String {
    let mac = synthetic_mac(&cam.uuid);
    format!(
        "<tds:GetNetworkInterfacesResponse><tds:NetworkInterfaces token=\"neolink0\">\
<tt:Enabled>true</tt:Enabled>\
<tt:Info><tt:Name>neolink0</tt:Name><tt:HwAddress>{mac}</tt:HwAddress><tt:MTU>1500</tt:MTU></tt:Info>\
<tt:IPv4><tt:Enabled>true</tt:Enabled>\
<tt:Config><tt:Manual><tt:Address>{host}</tt:Address><tt:PrefixLength>24</tt:PrefixLength></tt:Manual>\
<tt:DHCP>false</tt:DHCP></tt:Config>\
</tt:IPv4>\
</tds:NetworkInterfaces></tds:GetNetworkInterfacesResponse>",
        host = xml_escape(host),
    )
}

fn synthetic_mac(uuid: &uuid::Uuid) -> String {
    let b = uuid.as_bytes();
    // Clear the multicast bit and set the locally-administered bit.
    let first = (b[0] & 0xfe) | 0x02;
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        first, b[1], b[2], b[3], b[4], b[5]
    )
}

fn other_fault(e: anyhow::Error) -> FaultBody {
    FaultBody {
        code: FaultCode::Other,
        reason: e.to_string(),
    }
}

fn scope_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What handlers return when they need the dispatcher to render a SOAP fault.
pub(crate) struct FaultBody {
    pub(crate) code: FaultCode,
    pub(crate) reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onvif::capabilities::resolve;
    use crate::onvif::capabilities::Probe;

    fn caps_with(floodlight: bool, led_ctrl: bool) -> CameraCapabilities {
        resolve(&Probe {
            floodlight: Some(floodlight),
            support_led_ctrl: Some(led_ctrl),
            ..Default::default()
        })
    }

    /// A relay that cannot do anything is worse than a missing one: the client
    /// draws a working-looking switch that fails the moment it is pressed.
    #[test]
    fn relays_are_gated_on_the_capability_probe() {
        let bare = relay_outputs(&caps_with(false, false));
        let tokens: Vec<_> = bare.iter().map(|r| r.token).collect();
        assert_eq!(tokens, vec!["relay_siren"], "only the always-there siren");

        let full = relay_outputs(&caps_with(true, true));
        let tokens: Vec<_> = full.iter().map(|r| r.token).collect();
        assert_eq!(
            tokens,
            vec![
                "relay_floodlight",
                "relay_siren",
                "relay_ir_illuminator",
                "relay_status_led"
            ]
        );
    }

    /// The siren fires once and stops by itself, so it must not be advertised
    /// as a latching switch the user has to remember to turn off.
    #[test]
    fn the_siren_is_monostable_and_the_lights_are_not() {
        let all = relay_outputs(&caps_with(true, true));
        let mode = |token| {
            all.iter()
                .find(|r| r.token == token)
                .unwrap_or_else(|| panic!("{} should exist", token))
                .mode
        };
        assert_eq!(mode("relay_siren"), "Monostable");
        assert_eq!(mode("relay_floodlight"), "Bistable");
        assert_eq!(mode("relay_ir_illuminator"), "Bistable");
    }

    #[test]
    fn a_relay_renders_under_the_requested_element() {
        let all = relay_outputs(&caps_with(true, false));
        let xml = all[0].render("tds:RelayOutputs");
        assert!(
            xml.starts_with("<tds:RelayOutputs token=\"relay_floodlight\">"),
            "{}",
            xml
        );
        assert!(xml.contains("<tt:Mode>Bistable</tt:Mode>"), "{}", xml);
        assert!(xml.ends_with("</tds:RelayOutputs>"), "{}", xml);
    }

    /// POSIX inverts the sign of a UTC offset, and so does Reolink's
    /// `timeZone`, so the value passes straight through. Getting this backwards
    /// would put every camera's reported time zone on the wrong side of UTC.
    #[test]
    fn posix_tz_keeps_the_inverted_sign() {
        // Reolink reports -25200 for UTC+7.
        assert_eq!(posix_tz(-25200), "UTC-07:00");
        // ...and +18000 for UTC-5.
        assert_eq!(posix_tz(18000), "UTC+05:00");
        assert_eq!(posix_tz(0), "UTC+00:00");
        // Half-hour and quarter-hour zones exist (India, Nepal, Chatham).
        assert_eq!(posix_tz(-19800), "UTC-05:30");
    }

    fn general_at(time_zone: i32) -> SystemGeneral {
        SystemGeneral {
            time_zone: Some(time_zone),
            year: Some(2026),
            month: Some(8),
            day: Some(9),
            hour: Some(14),
            minute: Some(30),
            second: Some(0),
            ..Default::default()
        }
    }

    /// The camera reports local wall-clock time plus a west-of-UTC offset.
    /// `BcCamera::get_time` reads that offset with the wrong sign, which is why
    /// this path does the arithmetic itself.
    #[test]
    fn the_camera_clock_is_converted_to_real_utc() {
        use chrono::{Datelike, Timelike};
        // UTC+7: local 14:30 is 07:30 UTC.
        let clock = clock_from_general(&general_at(-25200)).expect("all fields present");
        assert_eq!(clock.utc.hour(), 7);
        assert_eq!(clock.utc.minute(), 30);
        assert_eq!(clock.utc.day(), 9);

        // UTC-5: local 14:30 is 19:30 UTC.
        let clock = clock_from_general(&general_at(18000)).expect("all fields present");
        assert_eq!(clock.utc.hour(), 19);
        assert_eq!(clock.utc.day(), 9);
    }

    /// Between samples the reading advances with the bridge's own clock, so a
    /// client polling every few seconds sees time move rather than a value
    /// frozen at the last camera read.
    #[test]
    fn a_cached_clock_keeps_ticking() {
        use chrono::Timelike;
        let clock = clock_from_general(&general_at(0)).expect("all fields present");
        // 14:30 plus ninety minutes, so the hour rolls over too.
        let later = advance(clock, std::time::Duration::from_secs(90 * 60));
        assert_eq!(later.utc.hour(), 16);
        assert_eq!(later.utc.minute(), 0);
        assert_eq!(
            later.posix_offset_seconds, clock.posix_offset_seconds,
            "the zone is a setting, not something that ticks"
        );
    }

    /// A camera on an older firmware may leave fields out; that is a fallback
    /// to the bridge clock, not a panic or a nonsense date.
    #[test]
    fn a_partial_general_block_yields_no_clock() {
        let mut g = general_at(0);
        g.hour = None;
        assert!(clock_from_general(&g).is_none());

        let mut g = general_at(0);
        g.time_zone = None;
        assert!(clock_from_general(&g).is_none());

        // A date the calendar doesn't have is not a clock either.
        let mut g = general_at(0);
        g.month = Some(13);
        assert!(clock_from_general(&g).is_none());
    }

    /// The response has to carry both readings, and they must be consistent
    /// with the zone it reports.
    #[test]
    fn the_date_and_time_response_carries_utc_and_local() {
        let clock = clock_from_general(&general_at(-25200));
        let xml = render_system_date_and_time(clock);
        assert!(xml.contains("<tt:TZ>UTC-07:00</tt:TZ>"), "{}", xml);
        // UTC 07:30 ...
        assert!(
            xml.contains("<tt:UTCDateTime><tt:Time><tt:Hour>7</tt:Hour>"),
            "{}",
            xml
        );
        // ... local 14:30.
        assert!(
            xml.contains("<tt:LocalDateTime><tt:Time><tt:Hour>14</tt:Hour>"),
            "{}",
            xml
        );
    }

    /// `GetSystemDateAndTime` is the unauthenticated probe every client opens
    /// with, so a silent camera still has to produce a well-formed answer.
    #[test]
    fn a_silent_camera_still_gets_a_valid_date_and_time() {
        let xml = render_system_date_and_time(None);
        assert!(xml.contains("<tt:TZ>UTC+00:00</tt:TZ>"), "{}", xml);
        assert!(xml.contains("<tt:UTCDateTime>"), "{}", xml);
        assert!(xml.contains("<tt:LocalDateTime>"), "{}", xml);
    }

    /// Home Assistant keys its device registry on the MAC, so two cameras
    /// behind the same bridge must not share one — and it must survive a
    /// restart, since it is derived from the same stable UUID the rest of the
    /// device identity uses.
    #[test]
    fn each_camera_gets_its_own_stable_locally_administered_mac() {
        let a = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"driveway");
        let b = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"shed");
        assert_ne!(synthetic_mac(&a), synthetic_mac(&b));
        assert_eq!(synthetic_mac(&a), synthetic_mac(&a), "stable");

        // Locally administered (0x02 set) and unicast (0x01 clear), so it can
        // never collide with a real vendor-assigned address.
        let first = u8::from_str_radix(&synthetic_mac(&a)[0..2], 16).expect("hex");
        assert_eq!(first & 0x02, 0x02, "locally administered");
        assert_eq!(first & 0x01, 0x00, "unicast");
    }

    #[test]
    fn set_system_date_and_time_reads_the_utc_block() {
        let body = "<tds:SetSystemDateAndTime><tds:DateTimeType>Manual</tds:DateTimeType>\
<tds:DaylightSavings>false</tds:DaylightSavings>\
<tds:UTCDateTime><tt:Time><tt:Hour>7</tt:Hour><tt:Minute>30</tt:Minute><tt:Second>5</tt:Second></tt:Time>\
<tt:Date><tt:Year>2026</tt:Year><tt:Month>8</tt:Month><tt:Day>9</tt:Day></tt:Date>\
</tds:UTCDateTime></tds:SetSystemDateAndTime>";
        let parsed = parse_set_system_date_and_time(body).expect("a manual time is usable");
        assert_eq!(parsed.to_rfc3339(), "2026-08-09T07:30:05+00:00");
    }

    /// A client asking for NTP sends no UTCDateTime, and the bridge cannot
    /// configure NTP on the camera — so this has to be a clear fault rather
    /// than a silently ignored request.
    #[test]
    fn an_ntp_request_has_no_time_to_apply() {
        let body = "<tds:SetSystemDateAndTime><tds:DateTimeType>NTP</tds:DateTimeType>\
</tds:SetSystemDateAndTime>";
        assert!(parse_set_system_date_and_time(body).is_none());
    }
}
