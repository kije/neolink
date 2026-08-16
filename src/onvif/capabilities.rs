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
//!
//! # When the answer is re-read
//!
//! Once per BC connection, not on a timer.
//!
//! These are properties of the hardware and of the logged-in session, and both
//! of those are established at connect+login. A firmware upgrade reboots the
//! camera, which drops the connection; a permission change only takes effect at
//! the next login — and the core caches the ability list at login
//! (`polulate_abilities`) and never refreshes it, so re-reading abilities more
//! often than that would only let this module disagree with the layer that
//! actually enforces them. The connection is therefore both the correct
//! invalidation signal and a strictly earlier one than any TTL.
//!
//! This used to be a 300-second TTL, which meant every camera paid a full probe
//! every five minutes — four sequential round-trips, one of them a deliberate
//! failure — with the cache mutex held throughout, so every ONVIF request for
//! every service queued behind it. That is the single biggest source of ONVIF
//! command latency in the bridge.

use std::sync::Weak;
use std::time::{Duration, Instant};

use neolink_core::bc::xml::{AbilityInfo, Support};
use neolink_core::bc_protocol::BcCamera;
use tokio::sync::{Mutex, RwLock};

use crate::onvif::state::{same_connection, CameraEntry};

/// How long to wait before re-probing a camera that told us *nothing*.
///
/// This is the camera-is-offline case. There is no connection to key the answer
/// to, so it can't be cached against one; back off instead of hammering a dead
/// socket on every SOAP request, and keep serving the permissive defaults in
/// the meantime.
const RETRY_UNKNOWN: Duration = Duration::from_secs(30);

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
    /// The camera has a microphone, so the RTSP stream carries audio and the
    /// media profiles should describe an audio source and encoder.
    pub(crate) audio: bool,
    /// The camera exposes LED control (IR illuminator + status light), so the
    /// LED relay outputs and the Imaging `IrCutFilter` control are real.
    pub(crate) led_ctrl: bool,
    /// The camera has a floodlight / spotlight that can be driven manually.
    pub(crate) floodlight: bool,
    /// The lens has a focus motor, so the Imaging service can offer focus.
    pub(crate) focus: bool,
    /// The camera exposes the general/OSD settings block.
    pub(crate) osd: bool,
}

impl CameraCapabilities {
    /// Should this camera have a PTZ service at all?
    pub(crate) fn ptz(&self) -> bool {
        self.pan_tilt || self.zoom || self.presets
    }

    /// Should this camera have an Imaging service at all?
    ///
    /// Imaging only carries two controls here — the IR cut filter and the focus
    /// motor — so a camera with neither gets no service address, the same way a
    /// motorless camera gets no PTZ address.
    pub(crate) fn imaging(&self) -> bool {
        self.led_ctrl || self.focus
    }

    /// Does this camera have any relay output worth advertising?
    ///
    /// The siren is always included (see `resolve`), so this is only ever false
    /// for a camera we have positive evidence has no siren — which today means
    /// never. Kept as a predicate so the Device service reads the same way the
    /// others do.
    pub(crate) fn relays(&self) -> bool {
        self.siren() || self.floodlight || self.led_ctrl
    }

    /// The siren is driven by a fire-and-forget `MSG_ID_PLAY_AUDIO` that every
    /// Reolink camera accepts (silently doing nothing if it has no speaker),
    /// and the `Support` table has no field we understand well enough to rule
    /// it out. So it is always offered, matching the MQTT surface.
    pub(crate) fn siren(&self) -> bool {
        true
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
    /// `GetZoomFocus`: the reported `(minPos, maxPos)` focus range.
    pub(crate) focus_range: Option<(u32, u32)>,
    /// `Support`: this channel has a microphone.
    pub(crate) support_audio: Option<bool>,
    /// `Support`: this channel exposes LED control.
    pub(crate) support_led_ctrl: Option<bool>,
    /// `Support`: this channel exposes the OSD settings block.
    pub(crate) support_osd: Option<bool>,
    /// The camera answered a floodlight-task read, so it has a floodlight.
    pub(crate) floodlight: Option<bool>,
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
            || self.focus_range.is_some()
            || self.support_audio.is_some()
            || self.support_led_ctrl.is_some()
            || self.support_osd.is_some()
            || self.floodlight.is_some()
    }
}

/// Turn raw observations into the capability set. Pure, so the reconciliation
/// rules can be tested without a camera.
pub(crate) fn resolve(p: &Probe) -> CameraCapabilities {
    // The non-PTZ capabilities are independent of the motor, so they are
    // resolved first and shared by both exits below. Each keeps the same
    // unknown-means-yes rule as the PTZ set.
    let audio = p.support_audio.unwrap_or(true);
    let led_ctrl = p.support_led_ctrl.unwrap_or(true);
    let osd = p.support_osd.unwrap_or(true);
    // Unlike the rest, the floodlight defaults to *absent*: this is a probe of
    // a specific accessory rather than a flag that a firmware might omit, and
    // advertising a relay for a light that isn't there puts a dead switch in
    // every client's UI.
    let floodlight = p.floodlight.unwrap_or(false);
    // A focus motor needs a real range, and it lives behind the same PTZ
    // `control` ability as the zoom.
    let focus = match p.focus_range {
        Some((min, max)) => max > min,
        None => false,
    };

    // Both of these default to "yes" when unknown, so a camera that answers
    // nothing keeps the pre-capability-detection behaviour.
    let controllable = p.ability_control.unwrap_or(true) && p.support_ptz_control.unwrap_or(true);
    if !controllable {
        return CameraCapabilities {
            pan_tilt: false,
            zoom: false,
            presets: false,
            preset_write: false,
            // A camera whose user cannot drive the motor cannot drive the
            // focus motor either — it is the same `control` ability.
            focus: false,
            audio,
            led_ctrl,
            floodlight,
            osd,
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
        focus,
        audio,
        led_ctrl,
        floodlight,
        osd,
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
    // `audioNum` is device-wide. A device that reports zero audio channels has
    // no microphone anywhere, which settles the question for this channel too;
    // a non-zero count only says *some* channel has audio, so it is left to the
    // per-channel `noAudio` flag below.
    if support.audio_num == Some(0) {
        p.support_audio = Some(false);
    }
    // `Support` is device-wide; on an NVR the per-channel `item` list is what
    // actually describes the camera behind this channel. Match it exactly —
    // borrowing another channel's flags would be worse than having none.
    let Some(item) = support.items.iter().find(|i| i.chn_id == channel_id as u32) else {
        return;
    };
    // Reolink states this one in the negative: `noAudio == 1` means the channel
    // has no microphone.
    //
    // Combined with, not substituted for, the device-wide `audioNum` above:
    // either negative is conclusive, and a firmware that reports zero audio
    // channels *and* `noAudio == 0` on a channel must not be able to talk us
    // back into advertising a microphone.
    if let Some(v) = item.no_audio {
        p.support_audio = Some(p.support_audio.unwrap_or(true) && v == 0);
    }
    if let Some(v) = item.led_ctrl {
        p.support_led_ctrl = Some(v != 0);
    }
    if let Some(v) = item.osd_cfg {
        p.support_osd = Some(v != 0);
    }
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
///
/// Reads go through `slot` (an `RwLock`) so a cached answer never waits on
/// anything; `probe_lock` exists only to stop N concurrent clients triggering N
/// probes of the same camera.
#[derive(Default)]
pub(crate) struct CapabilityCache {
    slot: RwLock<Option<CacheEntry>>,
    probe_lock: Mutex<()>,
}

#[derive(Clone)]
struct CacheEntry {
    /// The connection these capabilities were read over, or a null `Weak` when
    /// the probe learned nothing (nothing to key it to).
    conn: Weak<BcCamera>,
    /// When the probe ran. Only consulted for the learned-nothing case, where
    /// there is no connection to invalidate against.
    at: Instant,
    informative: bool,
    caps: CameraCapabilities,
}

impl CacheEntry {
    /// Is this entry still an answer about the camera as it is *now*?
    fn is_fresh(&self, conn: &Weak<BcCamera>) -> bool {
        if self.informative {
            same_connection(&self.conn, conn)
        } else {
            // Nothing was learned, so this is a placeholder rather than an
            // answer. Hold it briefly to avoid hammering an offline camera.
            self.at.elapsed() < RETRY_UNKNOWN
        }
    }
}

/// The capabilities of `cam` if they are already cached, without ever touching
/// the camera or waiting on an in-flight probe.
///
/// For callers that must not block. WS-Discovery answers Probe packets inline
/// on its receive loop, so a camera that has gone quiet must not be able to
/// stall the responder for every other camera. `None` means "no answer yet";
/// callers decide what to announce in the meantime.
pub(crate) fn cached(cam: &CameraEntry) -> Option<CameraCapabilities> {
    let slot = cam.capabilities.slot.try_read().ok()?;
    let entry = slot.as_ref()?;
    entry
        .is_fresh(&cam.connection_token())
        .then_some(entry.caps)
}

/// The capabilities of `cam`, probing the camera if what we have no longer
/// describes the current connection.
///
/// In the steady state this is a single `RwLock` read: the probe runs once per
/// connection, and [`probe_on_connect`] normally gets there first so no client
/// request ever pays for it.
pub(crate) async fn capabilities(cam: &CameraEntry) -> CameraCapabilities {
    let conn = cam.connection_token();
    if let Some(entry) = cam.capabilities.slot.read().await.as_ref() {
        if entry.is_fresh(&conn) {
            return entry.caps;
        }
    }
    refresh(cam).await
}

/// Probe and store, coalescing concurrent callers onto one probe.
async fn refresh(cam: &CameraEntry) -> CameraCapabilities {
    // Someone else is already probing. Rather than queue behind them, serve
    // whatever we last knew — a slightly stale capability set is a far better
    // answer to a VMS than a request that blocks for the probe's duration. Only
    // a camera we have never successfully probed waits.
    let _guard = match cam.capabilities.probe_lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            if let Some(entry) = cam.capabilities.slot.read().await.as_ref() {
                return entry.caps;
            }
            // Nothing to fall back on: wait for the in-flight probe, then take
            // its result.
            let guard = cam.capabilities.probe_lock.lock().await;
            if let Some(entry) = cam.capabilities.slot.read().await.as_ref() {
                return entry.caps;
            }
            guard
        }
    };

    // Re-check under the lock: we may have been the one queued behind a probe
    // that has just finished.
    let conn = cam.connection_token();
    if let Some(entry) = cam.capabilities.slot.read().await.as_ref() {
        if entry.is_fresh(&conn) {
            return entry.caps;
        }
    }

    let probe = run_probe(cam).await;
    let caps = resolve(&probe);
    let informative = probe.is_informative();

    let mut slot = cam.capabilities.slot.write().await;
    let changed = slot.as_ref().map(|e| e.caps) != Some(caps);
    if changed {
        log::debug!(
            "ONVIF: camera {} capabilities: pan/tilt={} zoom={} presets={} \
             preset_write={} focus={} audio={} led={} floodlight={} osd={} (from {probe:?})",
            cam.name,
            caps.pan_tilt,
            caps.zoom,
            caps.presets,
            caps.preset_write,
            caps.focus,
            caps.audio,
            caps.led_ctrl,
            caps.floodlight,
            caps.osd,
        );
    }
    *slot = Some(CacheEntry {
        // Keyed to the connection as it was *before* the probe: if it was
        // replaced mid-probe the entry no longer matches and the next caller
        // re-probes, which is the safe direction.
        conn: if informative { conn } else { Weak::new() },
        at: Instant::now(),
        informative,
        caps,
    });
    caps
}

/// Keep `cam`'s capabilities probed for as long as it has a connection.
///
/// Runs the probe as soon as a connection appears and again after every
/// reconnect, so the answer is already cached by the time any client asks.
/// Without this the first SOAP request after each reconnect pays for the probe.
///
/// Takes a `Weak` and upgrades it only for the duration of a probe, so a config
/// reload that drops the camera from the map is free to actually drop it.
pub(crate) async fn probe_on_connect(cam_weak: std::sync::Weak<CameraEntry>) {
    // The watch is cloned from the shared camera actor, so it stays usable
    // without keeping the `CameraEntry` alive.
    let mut watch = match cam_weak.upgrade() {
        Some(cam) => cam.instance.camera(),
        None => return,
    };
    loop {
        if cam_weak.strong_count() == 0 {
            return;
        }
        // Scoped tightly: the watch borrow is a synchronous guard and must not
        // be alive across the `.await`s below. `borrow_and_update` marks the
        // current value seen so `changed()` only fires on a real transition.
        let connected = { watch.borrow_and_update().strong_count() > 0 };
        if connected {
            let Some(cam) = cam_weak.upgrade() else {
                return;
            };
            let conn = cam.connection_token();
            let already_done = cam
                .capabilities
                .slot
                .read()
                .await
                .as_ref()
                .is_some_and(|e| e.is_fresh(&conn));
            if !already_done {
                log::trace!(
                    "ONVIF: camera {}: probing capabilities on connect",
                    cam.name
                );
                refresh(&cam).await;
            }
            drop(cam);
        }
        // Sleep until this connection goes away or is replaced.
        if watch.changed().await.is_err() {
            return;
        }
    }
}

async fn run_probe(cam: &CameraEntry) -> Probe {
    let channel_id = cam.channel_id;

    // Issued concurrently. They are independent reads over one multiplexed BC
    // connection, and running them in series made the probe cost the sum of
    // four timeouts on a camera that answers slowly (or not at all) instead of
    // the slowest single one.
    let (support, abilities, floodlight) = futures::join!(
        cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_support().await?) })),
        cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_abilityinfo().await?) })),
        // There is no "do you have a floodlight" flag in `Support`, so the read
        // itself is the test. This mirrors how the MQTT surface decides whether
        // to publish floodlight state.
        cam.run(|c: &BcCamera| Box::pin(async move { Ok(c.get_flightlight_tasks().await?) })),
    );

    let mut p = Probe::default();
    match support {
        Ok(support) => apply_support(&mut p, &support, channel_id),
        Err(e) => log::debug!("ONVIF: camera {}: no Support table ({e})", cam.name),
    }
    match abilities {
        Ok(info) => {
            p.ability_control = ptz_control_ability(&info, channel_id);
            p.ability_preset_write = ptz_preset_write_ability(&info, channel_id);
        }
        Err(e) => log::debug!("ONVIF: camera {}: no AbilityInfo ({e})", cam.name),
    }
    // A refusal is an answer ("no floodlight here"); a camera that never replied
    // is not, and must leave this unknown — otherwise `is_informative` below
    // reads an offline camera as a successfully probed one and pins the
    // permissive defaults for the whole connection.
    p.floodlight = match floodlight {
        Ok(_) => Some(true),
        Err(e) if is_refusal(&e) => Some(false),
        Err(e) => {
            log::debug!("ONVIF: camera {}: floodlight probe failed ({e})", cam.name);
            None
        }
    };

    // Only worth asking when something might move; on a camera we already know
    // has no PTZ control this is a guaranteed fault. Sequenced after the reads
    // above precisely so that check can be made.
    if p.ability_control != Some(false) && p.support_ptz_control != Some(false) {
        match cam
            .run(|c: &BcCamera| Box::pin(async move { Ok(c.get_zoom().await?) }))
            .await
        {
            Ok(zf) => {
                p.zoom_range = Some((zf.zoom.min_pos, zf.zoom.max_pos));
                p.focus_range = Some((zf.focus.min_pos, zf.focus.max_pos));
            }
            Err(e) => log::debug!("ONVIF: camera {}: no zoom range ({e})", cam.name),
        }
    }

    p
}

/// Did the camera answer and decline, as opposed to not answering at all?
///
/// `CameraServiceUnavailable` is the camera saying "I got your message and I do
/// not do that" — which for the floodlight probe is exactly the evidence we
/// want. A timeout or a dropped connection tells us nothing about the hardware.
fn is_refusal(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<neolink_core::Error>(),
        Some(neolink_core::Error::CameraServiceUnavailable { .. })
            | Some(neolink_core::Error::UnintelligibleReply { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use neolink_core::bc::xml::{AbilityInfoSubModule, AbilityInfoToken, SupportItem};

    fn probe() -> Probe {
        Probe::default()
    }

    /// An offline camera must not look like a successfully probed one.
    ///
    /// The floodlight probe used to be `Some(read.is_ok())`, which is `Some`
    /// whatever happens — so `is_informative` was unconditionally true, the
    /// "learned nothing" branch was unreachable, and a camera that answered
    /// nothing had the permissive defaults cached against it as though they
    /// were measured.
    #[test]
    fn a_camera_that_answered_nothing_is_not_informative() {
        let mut p = probe();
        // What `run_probe` now records when the floodlight read did not get an
        // answer, as opposed to getting a refusal.
        p.floodlight = None;
        assert!(
            !p.is_informative(),
            "a probe with no answers must not be treated as an answer"
        );
    }

    /// A refusal *is* an answer: the camera replied, and what it said is that
    /// it has no floodlight. That is exactly the evidence the probe is for.
    #[test]
    fn a_refused_floodlight_read_is_informative() {
        let mut p = probe();
        p.floodlight = Some(false);
        assert!(p.is_informative());
        assert!(!resolve(&p).floodlight);

        let mut p = probe();
        p.floodlight = Some(true);
        assert!(p.is_informative());
        assert!(resolve(&p).floodlight);
    }

    /// Any single answer is enough to key the result to the connection; the
    /// floodlight is just the one that is always attempted.
    #[test]
    fn one_answered_field_is_enough_to_be_informative() {
        for p in [
            Probe {
                support_ptz_control: Some(true),
                ..Default::default()
            },
            Probe {
                ability_control: Some(false),
                ..Default::default()
            },
            Probe {
                zoom_range: Some((0, 100)),
                ..Default::default()
            },
        ] {
            assert!(p.is_informative(), "{p:?}");
        }
        assert!(!Probe::default().is_informative());
    }

    /// However the floodlight read failed, the *resolved* capability is the
    /// same conservative "no floodlight" it always was — only the caching
    /// decision changes.
    #[test]
    fn an_unanswered_floodlight_still_resolves_to_absent() {
        let mut p = probe();
        p.floodlight = None;
        assert!(!resolve(&p).floodlight);
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
                audio: true,
                led_ctrl: true,
                osd: true,
                // The two exceptions, both deliberate: a floodlight is a
                // physical accessory we probe for rather than a flag a
                // firmware might omit, and a focus motor needs a measured
                // range before we claim it exists.
                floodlight: false,
                focus: false,
            }
        );
        assert!(caps.ptz());
    }

    #[test]
    fn audio_is_denied_by_either_the_device_count_or_the_channel_flag() {
        let mut p = probe();
        apply_support(
            &mut p,
            &Support {
                audio_num: Some(0),
                ..Default::default()
            },
            0,
        );
        assert_eq!(p.support_audio, Some(false));
        assert!(!resolve(&p).audio);

        // A device with audio somewhere still lets the per-channel flag speak
        // for this channel.
        let mut p = probe();
        apply_support(
            &mut p,
            &Support {
                audio_num: Some(4),
                items: vec![SupportItem {
                    chn_id: 2,
                    no_audio: Some(1),
                    ..Default::default()
                }],
                ..Default::default()
            },
            2,
        );
        assert_eq!(
            p.support_audio,
            Some(false),
            "noAudio=1 means no microphone"
        );

        let mut p = probe();
        apply_support(
            &mut p,
            &support_with(SupportItem {
                chn_id: 0,
                no_audio: Some(0),
                ..Default::default()
            }),
            0,
        );
        assert_eq!(p.support_audio, Some(true));
        assert!(resolve(&p).audio);
    }

    /// The two audio signals are combined, not overwritten. A firmware that
    /// reports zero audio channels device-wide *and* `noAudio == 0` on the
    /// channel is contradicting itself, and the negative has to win — the
    /// channel flag used to replace the device-wide evidence and talk us back
    /// into advertising a microphone that isn't there.
    #[test]
    fn a_contradictory_channel_flag_cannot_restore_audio() {
        let mut p = probe();
        apply_support(
            &mut p,
            &Support {
                audio_num: Some(0),
                items: vec![SupportItem {
                    chn_id: 0,
                    no_audio: Some(0),
                    ..Default::default()
                }],
                ..Default::default()
            },
            0,
        );
        assert_eq!(p.support_audio, Some(false));
        assert!(!resolve(&p).audio);
    }

    #[test]
    fn led_and_osd_flags_are_read_per_channel() {
        let mut p = probe();
        apply_support(
            &mut p,
            &support_with(SupportItem {
                chn_id: 0,
                led_ctrl: Some(0),
                osd_cfg: Some(1),
                ..Default::default()
            }),
            0,
        );
        let caps = resolve(&p);
        assert!(!caps.led_ctrl);
        assert!(caps.osd);
    }

    /// A floodlight is only advertised once the camera has answered a
    /// floodlight read: a dead switch in every client's UI is worse than a
    /// missing one.
    #[test]
    fn the_floodlight_relay_needs_positive_evidence() {
        assert!(!resolve(&probe()).floodlight);
        assert!(
            resolve(&Probe {
                floodlight: Some(true),
                ..probe()
            })
            .floodlight
        );
    }

    #[test]
    fn focus_needs_a_measured_range() {
        assert!(!resolve(&probe()).focus, "unknown is not a focus motor");
        assert!(
            !resolve(&Probe {
                focus_range: Some((100, 100)),
                ..probe()
            })
            .focus,
            "a zero-width range is a fixed lens"
        );
        let caps = resolve(&Probe {
            focus_range: Some((0, 4000)),
            ..probe()
        });
        assert!(caps.focus);
        assert!(caps.imaging());
    }

    /// Focus rides on the same `control` ability as the motor, so losing that
    /// ability has to take focus with it.
    #[test]
    fn no_control_ability_removes_focus_too() {
        let caps = resolve(&Probe {
            ability_control: Some(false),
            focus_range: Some((0, 4000)),
            ..probe()
        });
        assert!(!caps.focus);
        assert!(!caps.ptz());
    }

    /// The Imaging service only exists for the controls it can actually offer.
    #[test]
    fn imaging_needs_something_to_control() {
        let caps = resolve(&Probe {
            support_led_ctrl: Some(false),
            ..probe()
        });
        assert!(!caps.imaging(), "no IR cut filter and no focus motor");
        assert!(caps.relays(), "the siren is still there");
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
