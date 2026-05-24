//! ONVIF Device service. Returns metadata about the camera; never writes.

use anyhow::Result;
use neolink_core::bc_protocol::BcCamera;

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
        "GetSystemDateAndTime" => {
            let now = chrono::Utc::now();
            let utc_y = now.format("%Y").to_string();
            let utc_mo = now.format("%-m").to_string();
            let utc_d = now.format("%-d").to_string();
            let utc_h = now.format("%-H").to_string();
            let utc_mi = now.format("%-M").to_string();
            let utc_s = now.format("%-S").to_string();
            format!(
                "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime>\
<tt:DateTimeType>Manual</tt:DateTimeType>\
<tt:DaylightSavings>false</tt:DaylightSavings>\
<tt:TimeZone><tt:TZ>UTC0</tt:TZ></tt:TimeZone>\
<tt:UTCDateTime>\
<tt:Time><tt:Hour>{h}</tt:Hour><tt:Minute>{mi}</tt:Minute><tt:Second>{s}</tt:Second></tt:Time>\
<tt:Date><tt:Year>{y}</tt:Year><tt:Month>{mo}</tt:Month><tt:Day>{d}</tt:Day></tt:Date>\
</tt:UTCDateTime>\
</tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>",
                y = utc_y,
                mo = utc_mo,
                d = utc_d,
                h = utc_h,
                mi = utc_mi,
                s = utc_s,
            )
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
            if action == "GetCapabilities" {
                format!(
                    "<tds:GetCapabilitiesResponse><tds:Capabilities>\
<tt:Device><tt:XAddr>{dev}</tt:XAddr>\
<tt:Network><tt:IPFilter>false</tt:IPFilter><tt:ZeroConfiguration>false</tt:ZeroConfiguration><tt:IPVersion6>false</tt:IPVersion6><tt:DynDNS>false</tt:DynDNS></tt:Network>\
<tt:System><tt:DiscoveryResolve>false</tt:DiscoveryResolve><tt:DiscoveryBye>true</tt:DiscoveryBye><tt:RemoteDiscovery>false</tt:RemoteDiscovery><tt:SystemBackup>false</tt:SystemBackup><tt:SystemLogging>false</tt:SystemLogging><tt:FirmwareUpgrade>false</tt:FirmwareUpgrade></tt:System>\
</tt:Device>\
<tt:Media><tt:XAddr>{media}</tt:XAddr><tt:StreamingCapabilities><tt:RTPMulticast>false</tt:RTPMulticast><tt:RTP_TCP>true</tt:RTP_TCP><tt:RTP_RTSP_TCP>true</tt:RTP_RTSP_TCP></tt:StreamingCapabilities></tt:Media>\
<tt:PTZ><tt:XAddr>{ptz}</tt:XAddr></tt:PTZ>\
</tds:Capabilities></tds:GetCapabilitiesResponse>"
                )
            } else {
                format!(
                    "<tds:GetServicesResponse>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver10/device/wsdl</tds:Namespace><tds:XAddr>{dev}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace><tds:XAddr>{media}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
<tds:Service><tds:Namespace>http://www.onvif.org/ver20/ptz/wsdl</tds:Namespace><tds:XAddr>{ptz}</tds:XAddr><tds:Version><tt:Major>2</tt:Major><tt:Minor>5</tt:Minor></tds:Version></tds:Service>\
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
        "GetNetworkInterfaces" => "<tds:GetNetworkInterfacesResponse></tds:GetNetworkInterfacesResponse>".to_string(),
        "GetUsers" => "<tds:GetUsersResponse></tds:GetUsersResponse>".to_string(),
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
