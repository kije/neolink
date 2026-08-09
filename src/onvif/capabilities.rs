//! What a camera can actually *do*, as opposed to what the bridge would like
//! to claim it can do.
//!
//! Every ONVIF surface that describes the device — `GetCapabilities`,
//! `GetServices`, `GetScopes`, the media profiles, the PTZ node and its
//! configuration options, and the WS-Discovery scopes — used to be rendered
//! from constants. A fixed-lens doorbell therefore advertised a full
//! continuous-zoom space, and a camera with no motor at all still got a PTZ
//! service address. Clients believe that: Home Assistant draws the PTZ pad,
//! Frigate offers presets, and every button then fails at the Reolink layer.
//!
//! This module asks the camera once and caches the answer, so all of those
//! surfaces agree with each other and with the hardware.
//!
//! # Evidence
//!
//! Three independent sources, in decreasing order of trust:
//!
//! 1. `GetZoomFocus` — the zoom range. If `maxPos == minPos` there is no
//!    optical zoom, full stop. This is measured, not declared, so it wins.
//! 2. `Support` — the camera's own feature table: `ptzMode` (`"pt"`, `"ptz"`,
//!    ...) plus the per-channel `ptzControl` / `ptzType` / `ptzPreset` flags.
//! 3. `AbilityInfo` — which PTZ abilities the *logged-in user* holds, and at
//!    what access level. Every `BcCamera` PTZ call gates on `control` being
//!    read/write, so without that nothing we advertise could work regardless
//!    of the hardware. The `preset` ability is granted separately, and an
//!    account that holds it read-only may recall stored positions but not
//!    redefine them — including the home position, which is a preset slot.
//!
//! # Being wrong in the safe direction
//!
//! A camera that doesn't answer (offline, or an older firmware that omits a
//! field) leaves the corresponding signal *unknown*, and unknown always falls
//! back to the previous always-on behaviour. Removing a capability needs
//! positive evidence that it is absent; that way a flaky camera loses no
//! function it used to have.

use std::time::{Duration, Instant};

use neolink_core::bc::xml::{AbilityInfo, Support};
use neolink_core::bc_protocol::BcCamera;
use tokio::sync::Mutex;

use crate::onvif::state::CameraEntry;

/// How long a probe that learned something stays good for. Capabilities are
/// physical properties of the camera; they change when the hardware is
/// swapped, not while it is running. Long enough that an ONVIF client polling
/// `GetProfiles` costs nothing, short enough to pick up a firmware upgrade or
/// a permission change without a neolink restart.
const CACHE_TTL_KNOWN: Duration = Duration::from_secs(300);

/// How long a probe that learned *nothing* stays good for. This is the
/// camera-is-offline case: keep answering (with the permissive defaults)
/// rather than stalling every SOAP request on a dead socket, but retry soon.
const CACHE_TTL_UNKNOWN: Duration = Duration::from_secs(30);

/// The resolved capability set handed to the ONVIF handlers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CameraCapabilities {
    /// The camera has a pan and/or tilt motor.
    pub(crate) pan_tilt: bool,
    /// The camera has an optical zoom.
    pub(crate) zoom: bool,
    /// The camera can recall stored PTZ presets.
    pub(crate) presets: bool,
    /// The logged-in user may also *write* the preset table — `SetPreset` and
    /// `SetHomePosition`. Reolink hands out the preset ability separately from
    /// the movement one, so an account can be allowed to drive the camera and
    /// recall stored positions while being unable to redefine them.
    pub(crate) preset_write: bool,
}

impl CameraCapabilities {
    /// Should this camera have a PTZ service at all?
    pub(crate) fn ptz(&self) -> bool {
        self.pan_tilt || self.zoom || self.presets
    }
}

/// The letters in Reolink's `ptzMode` string (`"pt"`, `"ptz"`, `"p"`, ...).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PtzMode {
    pub(crate) pan: bool,
    pub(crate) tilt: bool,
    pub(crate) zoom: bool,
}

/// Raw observations, before they are reconciled. `None` on a field means the
/// camera did not tell us — never "no".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    /// `Support`: this channel has PTZ hardware wired up at all.
    pub(crate) support_ptz_control: Option<bool>,
    /// `Support.ptzMode`, when it is a string we recognise.
    pub(crate) support_mode: Option<PtzMode>,
    /// `Support`: this channel supports stored presets.
    pub(crate) support_presets: Option<bool>,
    /// `AbilityInfo`: the logged-in user holds the PTZ `control` ability.
    pub(crate) ability_control: Option<bool>,
    /// `AbilityInfo`: the user's `preset` ability is read/write rather than
    /// read-only.
    pub(crate) ability_preset_write: Option<bool>,
    /// `GetZoomFocus`: the reported `(minPos, maxPos)` zoom range.
    pub(crate) zoom_range: Option<(u32, u32)>,
}

impl Probe {
    /// Did the camera tell us anything at all? Drives the cache TTL: a probe
    /// that learned nothing is an offline camera, not a featureless one.
    fn is_informative(&self) -> bool {
        self.support_ptz_control.is_some()
            || self.support_mode.is_some()
            || self.support_presets.is_some()
            || self.ability_control.is_some()
            || self.ability_preset_write.is_some()
            || self.zoom_range.is_some()
    }
}

/// Turn raw observations into the capability set. Pure, so the reconciliation
/// rules can be tested without a camera.
pub(crate) fn resolve(p: &Probe) -> CameraCapabilities {
    // Both of these default to "yes" when unknown, so a camera that answers
    // nothing keeps the pre-capability-detection behaviour.
    let controllable = p.ability_control.unwrap_or(true) && p.support_ptz_control.unwrap_or(true);
    if !controllable {
        return CameraCapabilities {
            pan_tilt: false,
            zoom: false,
            presets: false,
            preset_write: false,
        };
    }

    // A measured zoom range beats whatever `ptzMode` claims: the RLC-823A
    // family reports `"pt"` on some firmwares despite having a real zoom, and
    // conversely a `"ptz"` camera with the lens motor disabled reports
    // `minPos == maxPos`.
    let zoom = match p.zoom_range {
        Some((min, max)) => max > min,
        None => p.support_mode.map(|m| m.zoom).unwrap_or(true),
    };
    let pan_tilt = p.support_mode.map(|m| m.pan || m.tilt).unwrap_or(true);
    // A preset is a stored motor position, so it is only meaningful if some
    // motor exists — a camera that cannot move cannot recall a position.
    let presets = (pan_tilt || zoom) && p.support_presets.unwrap_or(true);
    // Storing a preset is a separate permission from recalling one, and there
    // is nothing to store into on a camera with no preset table at all.
    let preset_write = presets && p.ability_preset_write.unwrap_or(true);

    CameraCapabilities {
        pan_tilt,
        zoom,
        presets,
        preset_write,
    }
}

/// `ptzMode` is a set of axis letters. Anything outside that vocabulary is a
/// value we have never seen, and guessing at it would be worse than admitting
/// we don't know.
pub(crate) fn parse_ptz_mode(s: &str) -> Option<PtzMode> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || !s.chars().all(|c| matches!(c, 'p' | 't' | 'z')) {
        return None;
    }
    Some(PtzMode {
        pan: s.contains('p'),
        tilt: s.contains('t'),
        zoom: s.contains('z'),
    })
}

/// Read the PTZ-relevant flags out of the camera's `Support` table.
pub(crate) fn apply_support(p: &mut Probe, support: &Support, channel_id: u8) {
    if let Some(mode) = support.ptz_mode.as_deref().and_then(parse_ptz_mode) {
        p.support_mode = Some(mode);
    }
    // `Support` is device-wide; on an NVR the per-channel `item` list is what
    // actually describes the camera behind this channel. Match it exactly —
    // borrowing another channel's flags would be worse than having none.
    let Some(item) = support.items.iter().find(|i| i.chn_id == channel_id as u32) else {
        return;
    };
    if let Some(v) = item.ptz_control {
        p.support_ptz_control = Some(v != 0);
    } else if item.ptz_type == Some(0) {
        // `ptzType == 0` is Reolink's "no PTZ hardware on this channel". Only
        // consulted when the explicit `ptzControl` flag is missing, so a
        // firmware that sets both never has them fight.
        p.support_ptz_control = Some(false);
    }
    if let Some(v) = item.ptz_preset {
        p.support_presets = Some(v != 0);
    }
}

/// How much of an ability the logged-in user holds. Reolink writes this as the
/// suffix on each entry: `control_rw`, `preset_ro`.
///
/// Ordered, so two entries naming the same ability resolve to the more
/// permissive one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Access {
    /// The ability was not in the list at all.
    #[default]
    Absent,
    /// `_ro`: may be read, may not be changed.
    ReadOnly,
    /// `_rw`: full access.
    ReadWrite,
}

/// The PTZ abilities the logged-in user holds on one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PtzAbilities {
    /// Driving the motors. `BcCamera`'s PTZ calls all require this read/write.
    pub(crate) control: Access,
    /// The preset table. Recalling a preset needs it at all; storing one needs
    /// it read/write.
    pub(crate) preset: Access,
}

/// Read the PTZ ability list for `channel_id` out of the camera's answer.
///
/// `None` means the camera told us nothing we could parse — never "the user
/// holds nothing".
pub(crate) fn ptz_abilities(info: &AbilityInfo, channel_id: u8) -> Option<PtzAbilities> {
    let Some(token) = info.ptz.as_ref() else {
        // The camera answered and listed no PTZ module at all.
        return Some(PtzAbilities::default());
    };
    let mut any = false;
    let mut out = PtzAbilities::default();
    for sub in token
        .sub_module
        .iter()
        .filter(|s| s.channel_id.map(|c| c == channel_id).unwrap_or(true))
    {
        for entry in sub.ability_value.split(',') {
            // Entries look like `control_rw` / `preset_ro`.
            let mut parts = entry.trim().split('_');
            let name = parts.next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            any = true;
            let access = match parts.next() {
                Some("rw") => Access::ReadWrite,
                // A suffix we don't recognise still proves the ability exists.
                // Read the weaker of the two out of it rather than inventing a
                // write permission the camera may reject.
                _ => Access::ReadOnly,
            };
            match name {
                "control" => out.control = out.control.max(access),
                "preset" => out.preset = out.preset.max(access),
                _ => {}
            }
        }
    }
    // An empty ability list is a firmware we don't understand, not a camera
    // without a motor.
    if !any {
        return None;
    }
    Some(out)
}

/// Can the logged-in user drive the motors? Every `BcCamera` PTZ call — moves,
/// zoom, and both preset calls — goes through `has_ability_rw("control")`, so
/// anything short of read/write here means every PTZ button in the client would
/// return a fault.
pub(crate) fn ptz_control_ability(info: &AbilityInfo, channel_id: u8) -> Option<bool> {
    Some(ptz_abilities(info, channel_id)?.control == Access::ReadWrite)
}

/// May the logged-in user *store* presets, as opposed to only recalling them?
///
/// A firmware that enumerates PTZ abilities without naming `preset` at all is
/// not telling us presets are read-only, so that stays unknown.
pub(crate) fn ptz_preset_write_ability(info: &AbilityInfo, channel_id: u8) -> Option<bool> {
    match ptz_abilities(info, channel_id)?.preset {
        Access::Absent => None,
        Access::ReadOnly => Some(false),
        Access::ReadWrite => Some(true),
    }
}

/// Per-camera cache. Lives on the `CameraEntry` so it survives config reloads
/// along with the connection it describes.
#[derive(Default)]
pub(crate) struct CapabilityCache {
    slot: Mutex<Option<CacheEntry>>,
}

struct CacheEntry {
    at: Instant,
    ttl: Duration,
    caps: CameraCapabilities,
}

/// The capabilities of `cam` if they are already cached, without ever touching
/// the camera or waiting on an in-flight probe.
///
/// For callers that must not block. WS-Discovery answers Probe packets inline
/// on its receive loop, so a camera that has gone quiet must not be able to
/// stall the responder for every other camera. `None` means "no answer yet";
/// callers decide what to announce in the meantime.
pub(crate) fn cached(cam: &CameraEntry) -> Option<CameraCapabilities> {
    let slot = cam.capabilities.slot.try_lock().ok()?;
    let entry = slot.as_ref()?;
    (entry.at.elapsed() < entry.ttl).then_some(entry.caps)
}

/// The capabilities of `cam`, probing the camera if the cached answer has
/// expired.
///
/// The lock is deliberately held across the probe: several ONVIF clients
/// polling at once should cost the camera one round of queries, not one per
/// request. The probe is bounded by `CameraEntry::run`'s hard timeout, so a
/// dead camera delays callers by that much and no more.
pub(crate) async fn capabilities(cam: &CameraEntry) -> CameraCapabilities {
    let mut slot = cam.capabilities.slot.lock().await;
    if let Some(entry) = slot.as_ref() {
        if entry.at.elapsed() < entry.ttl {
            return entry.caps;
        }
    }

    let probe = run_probe(cam).await;
    let caps = resolve(&probe);
    let ttl = if probe.is_informative() {
        CACHE_TTL_KNOWN
    } else {
        CACHE_TTL_UNKNOWN
    };
    let changed = slot.as_ref().map(|e| e.caps) != Some(caps);
    if changed {
        log::debug!(
            "ONVIF: camera {} capabilities: pan/tilt={} zoom={} presets={} preset_write={} (from {probe:?})",
            cam.name,
            caps.pan_tilt,
            caps.zoom,
            caps.presets,
            caps.preset_write,
        );
    }
    *slot = Some(CacheEntry {
        at: Instant::now(),
        ttl,
        caps,
    });
    caps
}

async fn run_probe(cam: &CameraEntry) -> Probe {
    let mut p = Probe::default();
    let channel_id = cam.channel_id;

    match cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_support().await?) }))
        .await
    {
        Ok(support) => apply_support(&mut p, &support, channel_id),
        Err(e) => log::debug!("ONVIF: camera {}: no Support table ({e})", cam.name),
    }

    match cam
        .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_abilityinfo().await?) }))
        .await
    {
        Ok(info) => {
            p.ability_control = ptz_control_ability(&info, channel_id);
            p.ability_preset_write = ptz_preset_write_ability(&info, channel_id);
        }
        Err(e) => log::debug!("ONVIF: camera {}: no AbilityInfo ({e})", cam.name),
    }

    // Only worth asking when something might move; on a camera we already know
    // has no PTZ control this is a guaranteed fault.
    if p.ability_control != Some(false) && p.support_ptz_control != Some(false) {
        match cam
            .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_zoom().await?) }))
            .await
        {
            Ok(zf) => p.zoom_range = Some((zf.zoom.min_pos, zf.zoom.max_pos)),
            Err(e) => log::debug!("ONVIF: camera {}: no zoom range ({e})", cam.name),
        }
    }

    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core::bc::xml::{AbilityInfoSubModule, AbilityInfoToken, SupportItem};

    fn probe() -> Probe {
        Probe::default()
    }

    /// The whole point of the fallbacks: a camera that says nothing keeps
    /// everything it had before capability detection existed.
    #[test]
    fn a_silent_camera_keeps_every_capability() {
        let caps = resolve(&probe());
        assert_eq!(
            caps,
            CameraCapabilities {
                pan_tilt: true,
                zoom: true,
                presets: true,
                preset_write: true,
            }
        );
        assert!(caps.ptz());
    }

    #[test]
    fn a_measured_zero_width_zoom_range_removes_zoom() {
        let caps = resolve(&Probe {
            zoom_range: Some((1000, 1000)),
            ..probe()
        });
        assert!(!caps.zoom);
        assert!(caps.pan_tilt, "pan/tilt is a separate question");
        assert!(caps.ptz());
    }

    /// A real range beats `ptzMode`, which some firmwares under-report.
    #[test]
    fn a_measured_range_overrides_the_mode_string() {
        let caps = resolve(&Probe {
            support_mode: parse_ptz_mode("pt"),
            zoom_range: Some((1000, 3000)),
            ..probe()
        });
        assert!(caps.zoom);

        let caps = resolve(&Probe {
            support_mode: parse_ptz_mode("ptz"),
            zoom_range: Some((0, 0)),
            ..probe()
        });
        assert!(!caps.zoom);
    }

    #[test]
    fn the_mode_string_decides_zoom_when_the_range_is_unknown() {
        let caps = resolve(&Probe {
            support_mode: parse_ptz_mode("pt"),
            ..probe()
        });
        assert!(!caps.zoom);
        assert!(caps.pan_tilt);
    }

    /// A zoom-only camera (fixed mount, motorised lens) must not advertise a
    /// pan/tilt space.
    #[test]
    fn a_zoom_only_camera_has_no_pan_tilt() {
        let caps = resolve(&Probe {
            support_mode: parse_ptz_mode("z"),
            zoom_range: Some((1000, 3000)),
            ..probe()
        });
        assert!(!caps.pan_tilt);
        assert!(caps.zoom);
        assert!(caps.ptz());
    }

    /// Without the `control` ability every PTZ call faults, so advertising any
    /// of it is a lie regardless of the hardware.
    #[test]
    fn no_control_ability_removes_all_ptz() {
        let caps = resolve(&Probe {
            ability_control: Some(false),
            support_mode: parse_ptz_mode("ptz"),
            zoom_range: Some((1000, 3000)),
            ..probe()
        });
        assert!(!caps.ptz());
    }

    #[test]
    fn support_can_deny_ptz_outright() {
        let caps = resolve(&Probe {
            support_ptz_control: Some(false),
            ..probe()
        });
        assert!(!caps.ptz());
    }

    #[test]
    fn presets_need_a_motor_to_be_meaningful() {
        let caps = resolve(&Probe {
            support_mode: Some(PtzMode::default()),
            zoom_range: Some((0, 0)),
            ..probe()
        });
        assert!(!caps.presets);
        assert!(!caps.ptz());
    }

    #[test]
    fn presets_can_be_denied_on_a_camera_that_moves() {
        let caps = resolve(&Probe {
            support_presets: Some(false),
            ..probe()
        });
        assert!(!caps.presets);
        assert!(!caps.preset_write, "nothing to write into");
        assert!(caps.pan_tilt);
        assert!(caps.ptz(), "the PTZ service is still worth having");
    }

    /// A `preset_ro` account can jump to stored positions but not redefine
    /// them, and the two must be advertised separately.
    #[test]
    fn recalling_and_storing_presets_are_separate_permissions() {
        let caps = resolve(&Probe {
            ability_preset_write: Some(false),
            ..probe()
        });
        assert!(caps.presets);
        assert!(!caps.preset_write);
    }

    #[test]
    fn presets_are_writable_when_nothing_says_otherwise() {
        assert!(resolve(&probe()).preset_write);
        assert!(
            resolve(&Probe {
                ability_preset_write: Some(true),
                ..probe()
            })
            .preset_write
        );
    }

    /// A camera that cannot move at all cannot have a writable preset table
    /// either, whatever the ability list says.
    #[test]
    fn preset_writes_need_a_preset_table() {
        let caps = resolve(&Probe {
            support_presets: Some(false),
            ability_preset_write: Some(true),
            ..probe()
        });
        assert!(!caps.preset_write);
    }

    #[test]
    fn ptz_mode_vocabulary() {
        assert_eq!(
            parse_ptz_mode("pt"),
            Some(PtzMode {
                pan: true,
                tilt: true,
                zoom: false
            })
        );
        assert_eq!(
            parse_ptz_mode("PTZ"),
            Some(PtzMode {
                pan: true,
                tilt: true,
                zoom: true
            })
        );
        assert_eq!(
            parse_ptz_mode(" p "),
            Some(PtzMode {
                pan: true,
                tilt: false,
                zoom: false
            })
        );
        // Unknown vocabulary is unknown, not empty: guessing here would strip
        // capabilities from a camera that has them.
        assert_eq!(parse_ptz_mode(""), None);
        assert_eq!(parse_ptz_mode("3d"), None);
        assert_eq!(parse_ptz_mode("basic"), None);
    }

    fn support_with(item: SupportItem) -> Support {
        Support {
            items: vec![item],
            ..Default::default()
        }
    }

    #[test]
    fn support_flags_are_read_from_the_matching_channel() {
        let mut p = probe();
        apply_support(
            &mut p,
            &Support {
                ptz_mode: Some("pt".to_string()),
                items: vec![
                    SupportItem {
                        chn_id: 0,
                        ptz_control: Some(1),
                        ptz_preset: Some(1),
                        ..Default::default()
                    },
                    SupportItem {
                        chn_id: 1,
                        ptz_control: Some(0),
                        ptz_preset: Some(0),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            1,
        );
        assert_eq!(p.support_ptz_control, Some(false));
        assert_eq!(p.support_presets, Some(false));
        assert_eq!(p.support_mode, parse_ptz_mode("pt"));
    }

    /// An NVR channel we have no `item` for must not inherit another
    /// channel's flags.
    #[test]
    fn an_unlisted_channel_learns_nothing_channel_specific() {
        let mut p = probe();
        apply_support(
            &mut p,
            &support_with(SupportItem {
                chn_id: 0,
                ptz_control: Some(1),
                ..Default::default()
            }),
            3,
        );
        assert_eq!(p.support_ptz_control, None);
    }

    #[test]
    fn ptz_type_zero_means_no_ptz_hardware() {
        let mut p = probe();
        apply_support(
            &mut p,
            &support_with(SupportItem {
                chn_id: 0,
                ptz_type: Some(0),
                ..Default::default()
            }),
            0,
        );
        assert_eq!(p.support_ptz_control, Some(false));
    }

    /// `ptzControl` is the explicit flag; `ptzType` is only a fallback, so the
    /// two can never contradict each other into a false negative.
    #[test]
    fn an_explicit_control_flag_wins_over_ptz_type() {
        let mut p = probe();
        apply_support(
            &mut p,
            &support_with(SupportItem {
                chn_id: 0,
                ptz_control: Some(1),
                ptz_type: Some(0),
                ..Default::default()
            }),
            0,
        );
        assert_eq!(p.support_ptz_control, Some(true));
    }

    fn ability(channel_id: Option<u8>, values: &str) -> AbilityInfo {
        AbilityInfo {
            ptz: Some(AbilityInfoToken {
                sub_module: vec![AbilityInfoSubModule {
                    channel_id,
                    ability_value: values.to_string(),
                }],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn control_ability_is_found_in_the_ptz_token() {
        assert_eq!(
            ptz_control_ability(&ability(Some(0), "control_rw, preset_rw"), 0),
            Some(true)
        );
        assert_eq!(
            ptz_control_ability(&ability(Some(0), "preset_ro"), 0),
            Some(false)
        );
    }

    /// Every `BcCamera` PTZ call demands `control` read/write, so a read-only
    /// `control` grant moves nothing — advertising PTZ for it would be a lie.
    #[test]
    fn read_only_control_is_not_control() {
        assert_eq!(
            ptz_control_ability(&ability(Some(0), "control_ro, preset_rw"), 0),
            Some(false)
        );
        assert!(!resolve(&Probe {
            ability_control: Some(false),
            ..probe()
        })
        .ptz());
    }

    #[test]
    fn the_preset_ability_carries_its_own_read_write_kind() {
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "control_rw, preset_rw"), 0),
            Some(true)
        );
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "control_rw, preset_ro"), 0),
            Some(false)
        );
    }

    /// A firmware that enumerates PTZ abilities without naming `preset` is not
    /// telling us presets are read-only.
    #[test]
    fn an_unlisted_preset_ability_is_unknown_not_read_only() {
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "control_rw"), 0),
            None
        );
        assert_eq!(ptz_preset_write_ability(&ability(Some(0), ""), 0), None);
    }

    /// A camera that lists no PTZ module at all holds no preset ability
    /// either, but that is already covered by losing `control`, so the write
    /// flag stays unknown rather than pretending to be evidence.
    #[test]
    fn a_missing_ptz_token_leaves_the_preset_kind_unknown() {
        assert_eq!(
            ptz_preset_write_ability(&AbilityInfo::default(), 0),
            None,
            "no PTZ module means no PTZ at all, decided by `control`"
        );
    }

    /// An unrecognised suffix proves the ability exists without proving it is
    /// writable.
    #[test]
    fn an_unknown_access_suffix_reads_as_read_only() {
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "preset_wtf"), 0),
            Some(false)
        );
    }

    /// Duplicated entries resolve to the most permissive one rather than to
    /// whichever happened to come last.
    #[test]
    fn the_strongest_grant_for_an_ability_wins() {
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "preset_ro, preset_rw"), 0),
            Some(true)
        );
        assert_eq!(
            ptz_preset_write_ability(&ability(Some(0), "preset_rw, preset_ro"), 0),
            Some(true)
        );
    }

    #[test]
    fn a_missing_ptz_token_means_no_ptz() {
        assert_eq!(ptz_control_ability(&AbilityInfo::default(), 0), Some(false));
    }

    /// An ability list we can't parse is not evidence of absence.
    #[test]
    fn an_empty_ability_list_is_unknown() {
        assert_eq!(ptz_control_ability(&ability(Some(0), ""), 0), None);
    }

    #[test]
    fn abilities_for_another_channel_are_ignored() {
        assert_eq!(
            ptz_control_ability(&ability(Some(2), "control_rw"), 0),
            None
        );
    }

    /// Firmware that omits the channel on a submodule is answering about the
    /// channel we asked for.
    #[test]
    fn a_channelless_submodule_applies_to_us() {
        assert_eq!(
            ptz_control_ability(&ability(None, "control_rw"), 4),
            Some(true)
        );
    }

    #[test]
    fn a_probe_that_learned_nothing_is_not_informative() {
        assert!(!probe().is_informative());
        assert!(Probe {
            zoom_range: Some((0, 0)),
            ..probe()
        }
        .is_informative());
    }
}
