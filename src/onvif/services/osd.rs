//! The OSD half of the ONVIF Media service.
//!
//! Reolink's `SystemGeneral` message carries exactly two things that show up as
//! an on-screen overlay: the camera's display name and the format its date
//! stamp is drawn in. So this exposes exactly two OSDs — a `Text` one for the
//! name and a `DateAndTime` one for the stamp — and nothing else.
//!
//! In particular there is no position, colour or font control: the Baichuan
//! protocol has no message for any of it, and inventing values a client could
//! then try to change would produce settings that silently do nothing. Both
//! OSDs report a fixed `UpperLeft` position, which is where Reolink draws them.

use anyhow::Result;
use neolink_core::bc::xml::SystemGeneral;
use neolink_core::bc_protocol::BcCamera;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::onvif::capabilities::CameraCapabilities;
use crate::onvif::services::device::FaultBody;
use crate::onvif::services::media::read_first_text_element;
use crate::onvif::soap::{wrap_envelope, xml_escape, FaultCode, NS_ALL};
use crate::onvif::state::CameraEntry;

/// Token for the OSD that draws the camera's name.
const TOKEN_NAME: &str = "osd_name";
/// Token for the OSD that draws the date and time.
const TOKEN_DATETIME: &str = "osd_datetime";

/// The ONVIF `DateFormat` strings we map Reolink's `osdFormat` onto.
///
/// Reolink names the field order (`DMY`, `MDY`, `YMD`); ONVIF wants a strftime
/// -ish pattern. These three are the only values Reolink emits.
const DATE_FORMATS: &[(&str, &str)] = &[
    ("DMY", "dd/MM/yyyy"),
    ("MDY", "MM/dd/yyyy"),
    ("YMD", "yyyy/MM/dd"),
];

fn reolink_to_onvif_date_format(reolink: &str) -> &'static str {
    DATE_FORMATS
        .iter()
        .find(|(r, _)| r.eq_ignore_ascii_case(reolink.trim()))
        .map(|(_, o)| *o)
        // A format we don't recognise is still a date being drawn; reporting
        // the most common layout beats reporting none at all.
        .unwrap_or("dd/MM/yyyy")
}

fn onvif_to_reolink_date_format(onvif: &str) -> Option<&'static str> {
    DATE_FORMATS
        .iter()
        .find(|(_, o)| o.eq_ignore_ascii_case(onvif.trim()))
        .map(|(r, _)| *r)
}

pub(crate) async fn dispatch(
    cam: &CameraEntry,
    caps: &CameraCapabilities,
    action: &str,
    body_xml: &str,
) -> Result<String, FaultBody> {
    if !caps.osd {
        // The camera told us it has no OSD block. Empty lists rather than
        // faults, for the same reason the audio getters do it: "there are none"
        // is a real answer.
        let body = match action {
            "GetOSDs" => "<trt:GetOSDsResponse/>".to_string(),
            "GetOSDOptions" => "<trt:GetOSDOptionsResponse/>".to_string(),
            _ => {
                return Err(FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "This camera has no OSD configuration".to_string(),
                })
            }
        };
        return Ok(wrap_envelope(&body, NS_ALL));
    }

    let general = read_general(cam).await;
    let body = match action {
        "GetOSDs" => {
            let osds = format!(
                "{}{}",
                render_name_osd(cam, general.as_ref(), "trt:OSDs"),
                render_datetime_osd(cam, general.as_ref(), "trt:OSDs"),
            );
            format!("<trt:GetOSDsResponse>{osds}</trt:GetOSDsResponse>")
        }
        "GetOSD" => {
            let token = read_first_text_element(body_xml, "OSDToken").ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: "Missing OSDToken".to_string(),
            })?;
            let osd = match token.as_str() {
                TOKEN_NAME => render_name_osd(cam, general.as_ref(), "trt:OSD"),
                TOKEN_DATETIME => render_datetime_osd(cam, general.as_ref(), "trt:OSD"),
                other => {
                    return Err(FaultBody {
                        code: FaultCode::InvalidArgs,
                        reason: format!("Unknown OSD token '{other}'"),
                    })
                }
            };
            format!("<trt:GetOSDResponse>{osd}</trt:GetOSDResponse>")
        }
        "GetOSDOptions" => render_osd_options(),
        "SetOSD" => {
            let token = read_osd_token(body_xml).ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: "SetOSD needs an OSD element carrying a token attribute".to_string(),
            })?;
            apply_set_osd(cam, &token, body_xml).await?;
            "<trt:SetOSDResponse/>".to_string()
        }
        other => {
            return Err(FaultBody {
                code: FaultCode::ActionNotSupported,
                reason: format!("OSD action '{other}' not supported"),
            })
        }
    };
    Ok(wrap_envelope(&body, NS_ALL))
}

/// Read the OSD token out of a `SetOSD` request.
///
/// The two operations spell it differently, and only `GetOSD` uses an element:
///
/// ```xml
/// <trt:GetOSD><trt:OSDToken>osd_name</trt:OSDToken></trt:GetOSD>
/// <trt:SetOSD><trt:OSD token="osd_name">...</trt:OSD></trt:SetOSD>
/// ```
///
/// `SetOSD` carries a whole `tt:OSDConfiguration`, whose token is the `token`
/// attribute it inherits from `tt:DeviceEntity` — there is no `OSDToken`
/// element anywhere in the request. Looking for one rejected every conforming
/// `SetOSD` as missing its token.
fn read_osd_token(body_xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(body_xml);
    reader.config_mut().trim_text(true);
    loop {
        let event = reader.read_event();
        let e = match event {
            Err(_) | Ok(Event::Eof) => return None,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => e,
            _ => continue,
        };
        let name = e.name();
        let raw = name.into_inner();
        let s = std::str::from_utf8(raw).unwrap_or("");
        if s.rsplit(':').next().unwrap_or(s) != "OSD" {
            continue;
        }
        for attr in e.attributes().flatten() {
            let key = std::str::from_utf8(attr.key.into_inner()).unwrap_or("");
            if key.rsplit(':').next().unwrap_or(key) == "token" {
                return Some(attr.unescape_value().ok()?.to_string());
            }
        }
    }
}

async fn read_general(cam: &CameraEntry) -> Option<SystemGeneral> {
    cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_general().await?) }))
        .await
        .ok()
}

/// The camera's configured display name, falling back to the neolink camera
/// name when the camera has not answered.
fn display_name(cam: &CameraEntry, general: Option<&SystemGeneral>) -> String {
    general
        .and_then(|g| g.device_name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cam.name.clone())
}

fn render_name_osd(cam: &CameraEntry, general: Option<&SystemGeneral>, element: &str) -> String {
    format!(
        "<{element} token=\"{TOKEN_NAME}\">\
<tt:VideoSourceConfigurationToken>vs_{cam_name}</tt:VideoSourceConfigurationToken>\
<tt:Type>Text</tt:Type>\
<tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>\
<tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>{text}</tt:PlainText></tt:TextString>\
</{element}>",
        cam_name = xml_escape(&cam.name),
        text = xml_escape(&display_name(cam, general)),
    )
}

fn render_datetime_osd(
    cam: &CameraEntry,
    general: Option<&SystemGeneral>,
    element: &str,
) -> String {
    let format = general
        .and_then(|g| g.osd_format.as_deref())
        .map(reolink_to_onvif_date_format)
        .unwrap_or("dd/MM/yyyy");
    format!(
        "<{element} token=\"{TOKEN_DATETIME}\">\
<tt:VideoSourceConfigurationToken>vs_{cam_name}</tt:VideoSourceConfigurationToken>\
<tt:Type>Text</tt:Type>\
<tt:Position><tt:Type>UpperLeft</tt:Type></tt:Position>\
<tt:TextString><tt:Type>DateAndTime</tt:Type>\
<tt:DateFormat>{format}</tt:DateFormat><tt:TimeFormat>HH:mm:ss</tt:TimeFormat>\
</tt:TextString>\
</{element}>",
        cam_name = xml_escape(&cam.name),
    )
}

fn render_osd_options() -> String {
    let formats: String = DATE_FORMATS
        .iter()
        .map(|(_, o)| format!("<tt:DateFormat>{o}</tt:DateFormat>"))
        .collect();
    // MaximumNumberOfOSDs is two because there are exactly two settable things
    // behind this — the name and the date format — not because of any limit in
    // the camera.
    format!(
        "<trt:GetOSDOptionsResponse><trt:OSDOptions>\
<tt:MaximumNumberOfOSDs Total=\"2\" Image=\"0\" PlainText=\"1\" Date=\"0\" Time=\"0\" DateAndTime=\"1\"/>\
<tt:Type>Text</tt:Type>\
<tt:PositionOption>UpperLeft</tt:PositionOption>\
<tt:TextOption><tt:Type>Plain</tt:Type><tt:Type>DateAndTime</tt:Type>\
{formats}<tt:TimeFormat>HH:mm:ss</tt:TimeFormat></tt:TextOption>\
</trt:OSDOptions></trt:GetOSDOptionsResponse>"
    )
}

async fn apply_set_osd(cam: &CameraEntry, token: &str, body_xml: &str) -> Result<(), FaultBody> {
    // Read-modify-write, so setting the name cannot clobber the clock and vice
    // versa: `SystemGeneral` carries both. The lock is what actually makes that
    // true — it is shared with `SetSystemDateAndTime`, and without it two
    // concurrent writers read the same snapshot and the second write reverts
    // the first one's field.
    let _guard = cam.general_lock.lock().await;
    let mut general = cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_general().await?) }))
        .await
        .map_err(|e| FaultBody {
            code: FaultCode::Other,
            reason: e.to_string(),
        })?;

    match token {
        TOKEN_NAME => {
            let text = read_first_text_element(body_xml, "PlainText").ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: "SetOSD on the name OSD needs a PlainText value".to_string(),
            })?;
            if text.trim().is_empty() {
                return Err(FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "The camera name cannot be empty".to_string(),
                });
            }
            general.device_name = Some(text);
        }
        TOKEN_DATETIME => {
            let format =
                read_first_text_element(body_xml, "DateFormat").ok_or_else(|| FaultBody {
                    code: FaultCode::InvalidArgs,
                    reason: "SetOSD on the date OSD needs a DateFormat value".to_string(),
                })?;
            let reolink = onvif_to_reolink_date_format(&format).ok_or_else(|| FaultBody {
                code: FaultCode::InvalidArgs,
                reason: format!(
                    "Unsupported DateFormat '{format}'; the camera only orders the \
                     fields, so it accepts dd/MM/yyyy, MM/dd/yyyy or yyyy/MM/dd"
                ),
            })?;
            general.osd_format = Some(reolink.to_string());
        }
        other => {
            return Err(FaultBody {
                code: FaultCode::InvalidArgs,
                reason: format!("Unknown OSD token '{other}'"),
            })
        }
    }

    cam.run(move |c: &BcCamera| {
        // `SystemGeneral` is not `Clone` and `run` may retry, so rebuild it.
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
    .map_err(|e| FaultBody {
        code: FaultCode::Other,
        reason: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SetOSD` carries a whole `tt:OSDConfiguration`, whose token is an
    /// attribute — there is no `OSDToken` element in the request at all.
    /// Looking for one rejected every conforming `SetOSD` as untokened.
    #[test]
    fn the_set_osd_token_is_read_from_the_attribute() {
        let body = "<trt:SetOSD><trt:OSD token=\"osd_name\">\
<tt:VideoSourceConfigurationToken>vs_cam</tt:VideoSourceConfigurationToken>\
<tt:Type>Text</tt:Type>\
<tt:TextString><tt:Type>Plain</tt:Type><tt:PlainText>Driveway</tt:PlainText></tt:TextString>\
</trt:OSD></trt:SetOSD>";
        assert_eq!(read_osd_token(body).as_deref(), Some("osd_name"));
    }

    /// Namespace prefixes vary between clients; the token does not move.
    #[test]
    fn the_set_osd_token_survives_any_prefix() {
        assert_eq!(
            read_osd_token("<SetOSD><OSD token=\"osd_datetime\"/></SetOSD>").as_deref(),
            Some("osd_datetime")
        );
        assert_eq!(
            read_osd_token("<x:SetOSD><x:OSD x:token=\"osd_name\"/></x:SetOSD>").as_deref(),
            Some("osd_name")
        );
    }

    /// A request with no OSD element, or an OSD element with no token, is a
    /// genuine client error and has to stay one.
    #[test]
    fn a_set_osd_without_a_token_is_rejected() {
        assert!(read_osd_token("<trt:SetOSD></trt:SetOSD>").is_none());
        assert!(read_osd_token("<trt:SetOSD><trt:OSD/></trt:SetOSD>").is_none());
        // The `GetOSD` shape is not a valid `SetOSD`, and must not be
        // mistaken for one.
        assert!(
            read_osd_token("<trt:GetOSD><trt:OSDToken>osd_name</trt:OSDToken></trt:GetOSD>")
                .is_none()
        );
    }

    #[test]
    fn date_formats_round_trip() {
        for (reolink, onvif) in DATE_FORMATS {
            assert_eq!(reolink_to_onvif_date_format(reolink), *onvif);
            assert_eq!(onvif_to_reolink_date_format(onvif), Some(*reolink));
        }
    }

    #[test]
    fn an_unknown_camera_format_still_reports_a_date() {
        // The stamp is on screen either way, so reporting no format would be
        // less true than reporting the common one.
        assert_eq!(reolink_to_onvif_date_format("WAT"), "dd/MM/yyyy");
    }

    /// The camera can only reorder the fields, so a client asking for a layout
    /// it cannot draw has to be told, not silently given something else.
    #[test]
    fn an_unsupported_client_format_is_rejected() {
        assert_eq!(onvif_to_reolink_date_format("MM.dd.yy"), None);
        assert_eq!(
            onvif_to_reolink_date_format(" YYYY/MM/DD "),
            Some("YMD"),
            "case and padding are the client's business, not the camera's"
        );
    }
}
