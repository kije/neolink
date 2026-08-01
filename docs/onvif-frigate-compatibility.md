# ONVIF compatibility with Frigate NVR

An audit of `src/onvif/` against the ONVIF client Frigate actually ships, with
the concrete changes needed to make a neolink-bridged Reolink camera work as a
Frigate ONVIF device.

## How this was checked

Frigate's ONVIF client is [`onvif-zeep-async`][ozap], pinned to `4.0.*` in
`docker/main/requirements-wheels.txt`. It is a [zeep][zeep] SOAP client, so
every response is parsed against the official ONVIF WSDL/XSD rather than
string-matched. That matters: neolink builds its responses by string
concatenation (`src/onvif/soap.rs:6-9`), and zeep rejects anything whose
element order or namespace does not match the schema.

Rather than guess, each response neolink emits was replayed through
`zeep`'s `Binding.process_reply()` using the exact WSDLs bundled in
`onvif_zeep_async-4.0.4`, and Frigate's own capability-detection code
(`frigate/ptz/onvif.py`) was re-run against the parsed results. Findings below
marked *(verified)* are parser output, not inference.

The Frigate code referenced is the `dev` branch: `frigate/ptz/onvif.py`
(runtime PTZ), `frigate/api/camera.py` (the ONVIF probe wizard used during
camera onboarding), and `frigate/config/camera/onvif.py` (config model).

[ozap]: https://pypi.org/project/onvif-zeep-async/
[zeep]: https://docs.python-zeep.org/

## Summary

| # | Finding | Impact on Frigate | Effort |
|---|---------|-------------------|--------|
| F1 | Device service is not at `/onvif/device_service` | **Cannot connect at all** | medium |
| F2 | `tt:PTZSpaces` children out of schema order | Capability discovery lost; autotracking impossible | trivial |
| F3 | `PTZConfiguration` declares only the continuous spaces | Relative + absolute moves never used | trivial |
| F4 | `GetVideoSources` uses the `tt:` namespace | Focus/imaging probe disabled | trivial |
| F5 | No Imaging service advertised | No focus support | medium |
| F6 | `MoveStatus` reported as unsupported, `GetStatus` always `IDLE` | Autotracking refused | small |
| F7 | No FOV relative translation space | Autotracking refused | design call |
| F8 | `SupportedVersions` missing from `tt:SystemCapabilities` | None today; stricter clients | trivial |
| F9 | Frigate has no ONVIF event client | **Our events/detections are invisible to it** | n/a |
| F10 | `advertise_host` is used verbatim by the client | Silent failure behind Docker/NAT | docs |

F1 is the only thing standing between "nothing works" and "PTZ works". F2–F4
are what stand between "PTZ works" and "PTZ works well". F9 is the answer to
the events question and it is not fixable from the ONVIF side.

---

## F1 — Frigate cannot reach our device service (blocker)

`onvif-zeep-async` hardcodes the device service URL and only substitutes host
and port (`onvif/client.py`, `ONVIFCamera.get_definition`):

```python
if name == "devicemgmt":
    xaddr = "{}:{}/onvif/device_service".format(
        self.host
        if (self.host.startswith("http://") or self.host.startswith("https://"))
        else f"http://{self.host}",
        self.port,
    )
    return xaddr, wsdlpath, binding_name
```

`host` may carry a scheme, but not a path — and Frigate's onboarding wizard
independently rejects anything that is not a bare hostname
(`_is_valid_host` in `frigate/api/camera.py` matches `^[a-zA-Z0-9.-]+$`).

neolink routes only `/onvif/{camera}/device_service`
(`src/onvif/server.rs:37`). A request to `/onvif/device_service` has one path
segment too few, does not match, and 404s. `update_xaddrs()` therefore throws
and `_init_onvif()` returns `False` on its first statement — no PTZ, no
profiles, and the probe wizard reports the device as unreachable.

Everything *after* the device service is fine: `update_xaddrs()` reads the
`XAddr` values out of our `GetCapabilities` response and uses them verbatim
(`normalize_url` only strips a duplicated port), so media/PTZ/events can stay
on their current per-camera paths *(verified — the parsed xaddrs came back as
`http://host:8100/onvif/driveway/media_service` etc.)*.

Three ways to fix it, not mutually exclusive:

**(a) One listener port per camera — recommended.** Add a per-camera
`port` to `OnvifCameraConfig` (`src/config.rs:91`), or auto-assign
`onvif.bind_port + n`, and serve the full device tree at the root of that
port: `/onvif/device_service`, `/onvif/media_service`, and so on. This is what
every VMS assumes — one ONVIF device is one `host:port` — and it makes the
Frigate config the obvious `host: neolink, port: 8101`. The existing
`/onvif/{camera}/...` tree can stay for clients that already use it.

**(b) A root alias.** Route the bare `/onvif/device_service` (and siblings) to
a designated camera: a new `[onvif] default_camera`, defaulting to the single
enabled camera when there is exactly one. Cheapest possible fix and covers the
common single-camera install, but does not scale.

**(c) `Host`-header routing.** Map `Host: driveway.neolink.lan` to a camera so
one port serves all devices. Works, but needs DNS the user has to set up.

Suggested: ship (b) as an immediate unblock, then (a) as the real answer.

## F2 — `tt:PTZSpaces` children are out of schema order

*(verified)* Both `GetConfigurationOptions` and `GetNodes` fail to parse:

```
XMLParseError: Unexpected element '{http://www.onvif.org/ver10/schema}AbsoluteZoomPositionSpace'
```

`tt:PTZSpaces` is an `xs:sequence`, and the order is fixed:

```
AbsolutePanTiltPositionSpace, AbsoluteZoomPositionSpace,
RelativePanTiltTranslationSpace, RelativeZoomTranslationSpace,
ContinuousPanTiltVelocitySpace, ContinuousZoomVelocitySpace,
PanTiltSpeedSpace, ZoomSpeedSpace, Extension
```

`render_configuration_options()` (`src/onvif/services/ptz.rs:525`) and
`render_ptz_node()` (`src/onvif/services/ptz.rs:547`) both emit the continuous
spaces first and the absolute zoom space last.

Frigate wraps `GetConfigurationOptions` in a bare `except Exception`, so PTZ
still limps along — but `ptz_config` stays `None`, and with it goes every
capability keyed off `Spaces`: `relative_fov_range`, `absolute_zoom_range`,
`relative_zoom_range`. Autotracking is then impossible regardless of anything
else. Clients that do not swallow the exception (ONVIF Device Manager, Home
Assistant) lose the PTZ node outright.

Reordering the two renderers fixes it *(verified: both parse cleanly once
reordered)*.

## F3 — `PTZConfiguration` only declares the continuous spaces

Frigate does not derive PTZ features from what the service implements. It
reads them off `profile.PTZConfiguration` in the `GetProfiles` response
(`frigate/ptz/onvif.py`):

```python
if configs.DefaultContinuousPanTiltVelocitySpace:  supported_features.append("pt")
if configs.DefaultContinuousZoomVelocitySpace:     supported_features.append("zoom")
if configs.DefaultRelativePanTiltTranslationSpace: supported_features.append("pt-r")
if configs.DefaultRelativeZoomTranslationSpace:    supported_features.append("zoom-r")
if configs.DefaultAbsoluteZoomPositionSpace:       supported_features.append("zoom-a")
```

We emit only the two continuous entries (`src/onvif/services/media.rs:137` and
the duplicate at `src/onvif/services/ptz.rs:506`), so Frigate sees
`features = ["pt", "zoom"]`. Our PTZ service *does* implement `RelativeMove`
and zoom `AbsoluteMove` (`src/onvif/services/ptz.rs:327`, `:356`) — Frigate
simply never calls them, and both zoom modes of the autotracker are disabled
with "zoom range unavailable".

Add `DefaultAbsoluteZoomPositionSpace`, `DefaultRelativePanTiltTranslationSpace`
and `DefaultRelativeZoomTranslationSpace`, plus `ZoomLimits` (Frigate stores it
alongside `absolute_zoom_range`). They go **before** the continuous entries —
`tt:PTZConfiguration` is a sequence too:

```
Name, UseCount, NodeToken,
DefaultAbsolutePantTiltPositionSpace, DefaultAbsoluteZoomPositionSpace,
DefaultRelativePanTiltTranslationSpace, DefaultRelativeZoomTranslationSpace,
DefaultContinuousPanTiltVelocitySpace, DefaultContinuousZoomVelocitySpace,
DefaultPTZSpeed, DefaultPTZTimeout, PanTiltLimits, ZoomLimits, Extension
```

*(verified)* With F2 and F3 applied, replaying Frigate's detection code gives:

```
features: ['pt', 'zoom', 'pt-r', 'zoom-r', 'zoom-a', 'pt-r-fov']
absolute_zoom_range: {URI: .../ZoomSpaces/PositionGenericSpace, XRange: {Min: 0.0, Max: 1.0}}
```

While here: `render_ptz_configuration` in `media.rs` and
`render_ptz_configuration_xml` in `ptz.rs` are the same XML written twice and
have to stay byte-compatible. Worth collapsing into one renderer parameterised
by the wrapper tag.

## F4 — `GetVideoSources` uses the wrong namespace

*(verified)* `XMLParseError: Unexpected element '{http://www.onvif.org/ver10/schema}VideoSources'`.

`src/onvif/services/media.rs:278` emits `<tt:VideoSources>`. The media WSDL is
`elementFormDefault="qualified"`, so the response wrapper belongs to the
service namespace — `<trt:VideoSources>` — even though its *type* is
`tt:VideoSource`. (The `tt:Framerate` / `tt:Resolution` children are correct as
they are.)

Frigate catches this and sets `video_source_token = None`, which then disables
the imaging/focus probe. Swapping the prefix makes the call parse *(verified)*.

## F5 — no Imaging service

Frigate calls `create_imaging_service()` and then `GetImagingSettings` to
decide whether to expose focus control. Our `GetCapabilities`
(`src/onvif/services/device.rs:106`) advertises no `tt:Imaging` element, so
`create_imaging_service()` raises `ONVIFError` and focus is skipped.

The BC protocol does expose focus — `get_zoom()` returns a `PtzZoomFocus`
(`crates/core/src/bc_protocol/ptz.rs:306`) and there is a `StartZoomFocus`
message — so a minimal Imaging service (`GetImagingSettings`,
`GetMoveOptions`, `Move`, `Stop` with a `Focus.Continuous` block) is buildable.
Medium effort for one optional feature; sensible to defer behind F1–F4.

Note that adding `tt:Imaging` to `GetCapabilities` means inserting it between
`tt:Events` and `tt:Media` — `tt:Capabilities` is ordered
`Analytics, Device, Events, Imaging, Media, PTZ, Extension`.

## F6 — `MoveStatus` is declared unsupported

`src/onvif/services/ptz.rs:301` returns `MoveStatus="false"`, and `GetStatus`
(`:383`) hardcodes `IDLE` for both axes. Frigate treats `MoveStatus` as a hard
prerequisite for autotracking and polls it throughout a move to know when the
camera has settled.

We can report this honestly without any new camera queries: the bridge already
knows when it issued a `send_ptz`, when it issued `Direction::Stop`, when the
timed `RelativeMove` sleep is still running, and whether the simulated
continuous-zoom task (`spawn_zoom_task`, `:226`) is alive. Tracking that on
`CameraEntry` and deriving `MOVING`/`IDLE` from it would let us set
`MoveStatus="true"` truthfully.

Related: we currently claim `StatusPosition="true"` while reporting pan/tilt as
a hardcoded `x="0" y="0"`. Zoom position is real; pan/tilt is not. Either drop
the claim or scope it to zoom.

## F7 — FOV relative translation, and whether to fake it

Autotracking additionally needs a `RelativePanTiltTranslationSpace` whose URI
contains `TranslationSpaceFov`:

```python
fov_space_id = next((i for i, space in enumerate(
    ptz_config.Spaces.RelativePanTiltTranslationSpace)
    if "TranslationSpaceFov" in space["URI"]), None)
```

Reolink has no absolute pan/tilt and no FOV translation; our `RelativeMove`
already approximates one by treating the translation magnitude as a duration
and running a timed continuous move (`src/onvif/services/ptz.rs:327-350`).

We *could* advertise `TranslationSpaceFov` over that emulation, and Frigate's
autotracker would then start. It would also be uncalibrated — the autotracker
assumes a translation of 1.0 means "one full frame width" and cross-checks
against `GetStatus` positions we do not have. Recommendation: advertise the
honest set (`pt-r`, `zoom-r`, `zoom-a`) by default, and put FOV/autotracking
behind an explicit opt-in flag documented as experimental, rather than
advertising a capability we approximate with a stopwatch.

## F8 — `SupportedVersions` missing

`tt:SystemCapabilities` requires `SupportedVersions` (`minOccurs` 1). Our
`GetCapabilities` omits it (`src/onvif/services/device.rs:110`). zeep tolerates
the omission today, but it is free to add and stricter stacks may not.

## F9 — Frigate has no ONVIF event client (the events answer)

**Frigate does not consume ONVIF events at all.** Grepping the whole `frigate/`
Python tree on `dev` finds no `PullPoint`, no `CreatePullPointSubscription`,
no `GetEventProperties`, and no events-service construction. The only ONVIF
consumers are `frigate/ptz/onvif.py` (PTZ + imaging) and the
`frigate/api/camera.py` probe wizard. There is no WS-Discovery either.

So the event surface neolink already implements — `tns1:VideoSource/MotionAlarm`,
the `tns1:RuleEngine/MyRuleDetector/*` detectors, the smart-AI zone detector and
`tns1:AudioAnalytics/Audio/DetectedSound` (`src/onvif/events.rs:456-481`) — is
invisible to Frigate no matter what we do. For the record, that surface is
*correct*: the `PullMessages` envelope, the `ConcreteSet` topic dialect and the
`tt:Message` items all parse cleanly through the events WSDL *(verified)*, and
they are what Home Assistant, Blue Iris and Synology consume today. Nothing
here needs changing for Frigate's sake, because Frigate will never ask.

Upstream this is a long-standing request:
[#3485](https://github.com/blakeblackshear/frigate/issues/3485) (open),
[#22223](https://github.com/blakeblackshear/frigate/issues/22223) (closed as a
duplicate of it) and
[#4396](https://github.com/blakeblackshear/frigate/issues/4396).

### Getting Reolink detections into Frigate anyway

The supported external-event path is Frigate's HTTP API:

```
POST /api/events/{camera_name}/{label}/create
{"sub_label": null, "score": 0, "duration": 30,
 "include_recording": true, "draw": {}, "pre_capture": null}
```

(`frigate/api/event.py:1741`; requires an admin role, and `duration: null`
means the event must be ended explicitly via `/api/events/{id}/end`.)

A small optional bridge — reusing the same `NeoInstance::motion()` /
`NeoInstance::ai()` watches that already feed `EventsManager`
(`src/onvif/events.rs:295-416`) — could map the camera's AI types onto Frigate
labels (`people` → `person`, `vehicle` → `car`, `dog_cat` → `dog`/`cat`, …) and
open/close a Frigate event per detection. That would give Frigate users the
camera's on-device detections without running Frigate's own detector on the
stream, which is the actual thing people are asking for in #3485.

Points worth deciding before building it: it is a Frigate-specific integration
rather than a standards one; it needs a Frigate URL and API token in the
neolink config; and Frigate's manual events do not carry a bounding box, so
they will not participate in object tracking or autotracking. It is probably
better placed next to the MQTT surface than inside `src/onvif/`.

## F10 — `advertise_host` is taken literally

`normalize_url()` in the client only strips a duplicated port; it never
rewrites the host. The `XAddr` values in our `GetCapabilities` response are
dialled verbatim, so `advertise_host = "auto"`
(`src/onvif/state.rs:298-319`, which picks the outbound-interface IP) will hand
Frigate a container-internal address whenever neolink runs in Docker with a
bridge network. Same for the RTSP URI from `GetStreamUri`.

This is a documentation problem more than a code one, but a startup warning
when the auto-detected address is in a typical container range would save a lot
of confused issues.

---

## Suggested order of work

1. **F1** — root `/onvif/device_service` route, then per-camera ports.
   Nothing else matters until Frigate can connect.
2. **F2, F4, F8** — pure XML ordering/namespace fixes, no behaviour change.
3. **F3** — declare the relative/absolute spaces we already implement.
4. **F6** — real `MoveStatus` from move bookkeeping.
5. Optional: **F5** imaging/focus, **F7** opt-in FOV autotracking.
6. Separate track: **F9** Frigate external-events bridge for AI detections.

## Testing note

F2 and F4 are exactly the class of bug that hand-built XML plus hand-written
assertions cannot catch — every existing test in `src/onvif/` checks for
substrings, and a substring check passes happily on mis-ordered elements. A
schema-aware check is what found them. Worth adding a small conformance step
that replays the rendered responses against the ONVIF XSDs (either a Rust
validator over a vendored `onvif.xsd`, or a `zeep` script run in CI), so future
edits to the response builders cannot silently break strict clients again.
