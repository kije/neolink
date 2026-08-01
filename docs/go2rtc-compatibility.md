# Optimising neolink for go2rtc / WebRTC / MSE

*Investigation and design proposal for a compatibility mode.*

Status: **proposal** — no code changes yet. Everything below is grounded in
the current tree and in go2rtc's source at `AlexxIT/go2rtc@master`.

---

## 1. Why a mode at all

neolink's RTSP defaults are tuned for the consumers it has historically been
asked about: Blue Iris, VLC, ffmpeg, Home Assistant's generic camera. Those
clients are lenient — they will retry a 404, they will sit on a stalled
socket for a minute, they will decode almost any RTP payload format, and they
re-DESCRIBE freely.

go2rtc is not lenient, and it is not the end consumer. It is a *republisher*:
it terminates neolink's RTSP once and then feeds browsers over WebRTC and
MSE. That changes three things at once.

1. **The codec set is narrow.** Whatever go2rtc cannot name, it drops. It has
   no fallback path to "just decode it somehow" unless the user wires up
   ffmpeg.
2. **The timeouts are short and fixed.** Five seconds, everywhere. There is no
   knob in neolink's control that widens them.
3. **The negotiation is cached.** Downstream WebRTC/MSE sessions are pinned to
   the track list go2rtc learned at connect time. An SDP that changes shape
   between reconnects breaks live sessions, not just the next one.

Several of neolink's current defaults are actively wrong under those rules —
one of them (`audio_format = "latm"`) silently discards audio entirely. The
right shape for the fix is a **profile that changes defaults**, not a pile of
individually-documented knobs that every go2rtc user has to discover the hard
way.

---

## 2. Findings

### 2.1 The default audio format produces no audio in go2rtc at all

This is the highest-impact finding and it is unambiguous.

go2rtc identifies AAC in an SDP **solely by the rtpmap encoding name
`MPEG4-GENERIC`**:

```go
// go2rtc pkg/core/core.go
CodecAAC  = "MPEG4-GENERIC"
```

`UnmarshalCodec` (`pkg/core/codec.go`) takes the rtpmap name verbatim,
uppercases it, and stores it as `Codec.Name`. There is no aliasing step. So an
`a=rtpmap:97 MP4A-LATM/16000` line yields `Codec.Name == "MP4A-LATM"`, which
matches no constant, and:

```go
// go2rtc pkg/core/media.go
func GetKind(name string) string {
	switch name {
	case CodecH264, CodecH265, ...:      return KindVideo
	case CodecPCMU, CodecPCMA, CodecAAC, CodecOpus, ...: return KindAudio
	}
	return ""                                    // <- MP4A-LATM lands here
}
```

neolink's default is `audio_format = "latm"` ([`src/config.rs:560`][cfg-latm]),
which builds `rtpmp4apay` ([`src/rtsp/factory.rs:1023`][f-latm]) and therefore
emits exactly that rtpmap. **Every go2rtc user on default settings is getting
silent streams**, and the `audio_format` docs currently claim LATM is
"Understood by … go2rtc (and therefore Home Assistant and Frigate)"
([`src/config.rs:558`][cfg-claim], [`sample_config.toml:117`][sc-claim]). That
claim is wrong and should be corrected regardless of whether the mode lands.

The workaround users find is `audio_format = "pcm"` → `rtpL16pay` → `L16`,
which go2rtc *does* understand (`CodecPCM = "L16"`). That works, at the cost
of an AAC decode and ~256 kbps of raw samples on the wire.

#### The two consumers want different things

Neither AAC nor L16 is right for both of go2rtc's outputs:

| go2rtc output | video | audio it can use | what it does with ours |
|---|---|---|---|
| **MSE / MP4 / HLS / recording**<br>(`pkg/mp4/consumer.go`) | H264, H265 | `MPEG4-GENERIC` (passthrough), Opus, MP3; PCMA/PCMU/L16/PCML → **re-encoded to FLAC** | `MP4A-LATM` → `handler.Handler = nil`, dropped. `L16` → FLAC-in-MP4, which is not universally playable. |
| **WebRTC**<br>(`pkg/webrtc/consumer.go`) | H264, H265 | PCMA, PCMU, L16, PCML only — via `WithResampling` in `pkg/webrtc/helpers.go`, which appends wildcard `PCMA/0`/`PCMU/0` codecs so any clock rate matches and gets transcoded to G.711 | `MP4A-LATM` → no match, dropped. **AAC in any framing → no match, dropped.** `L16` → transcoded to PCMA/8000. Works. |

So: MSE wants AAC as `MPEG4-GENERIC`; WebRTC cannot use AAC at all and wants
L16. Without ffmpeg transcoding configured in go2rtc, there is no single audio
codec that serves both.

**Fixes, in order of value:**

- **(a) Add `audio_format = "mpeg4-generic"` using `rtpmp4gpay`.** This is
  nearly free. `rtpmp4gpay` takes exactly the caps the existing LATM pipeline
  already produces — `audio/mpeg, mpegversion=4, stream-format=raw`, see
  [`pipe_aac_latm` at `src/rtsp/factory.rs:876`][f-pipeaac] — and emits
  `encoding-name=MPEG4-GENERIC` (RFC 3640 AAC-hbr). It is a one-element swap
  in `build_aac`, same passthrough, same zero decode cost. This is what
  `"latm"` should probably have been in the first place.
- **(b) Offer both audio tracks.** gst-rtsp-server collects `pay0`, `pay1`,
  `pay2`… into successive `m=` lines. Adding a second audio track (`pay1` =
  `MPEG4-GENERIC`, `pay2` = `L16`) lets each go2rtc consumer pick the codec it
  can actually use — passthrough AAC for MSE and recording, L16→PCMA for
  WebRTC — from one RTSP connection. Cost: one AAC decode + `audioconvert`
  running alongside the passthrough branch, and some risk with third-party
  clients that assume a single audio track. Worth gating behind the mode
  rather than making it a global default.

### 2.2 go2rtc's 5-second deadlines vs. neolink's blocking DESCRIBE

go2rtc applies a fixed 5-second deadline to **every** RTSP request/response
round trip:

```go
// go2rtc pkg/rtsp/client.go
var Timeout = time.Second * 5
```

used as `SetWriteDeadline`/`SetReadDeadline` around each request in
`pkg/rtsp/conn.go`. There is no per-source override for it (`source.Timeout`
in the config only affects the *media* read deadline and the dial timeout).

neolink builds the whole pipeline **inside the DESCRIBE**. `create_element`
([`src/rtsp/gst/factory.rs:141`][gf-create]) calls into
`make_factory`'s callback ([`src/rtsp/factory.rs:373`][f-cb]), which blocks on
a oneshot while the per-client task connects the camera, starts the video
subscription, and then spends **up to ten seconds** learning the stream type:

```rust
// src/rtsp/factory.rs:236
let _ = tokio::time::timeout(Duration::from_secs(10), async {
    while let Some(media) = media_rx.recv().await {
        stream_config.update_from_media(&media);
        ...
```

For a camera that is already streaming this returns in well under a second.
For a battery camera waking up, a camera reached over a Reolink relay, or any
camera during its own reconnect backoff, it does not — and go2rtc's DESCRIBE
fails at 5 s. go2rtc then retries, and because the factory is **not shared**
([`src/rtsp/gst/factory.rs:38`][gf-shared]) each retry runs the whole thing
again from scratch, opening another `start_video` subscription
([`src/common/instance/gst.rs:208`][ig-start]) on the same camera. That is a
reconnect storm that gets worse the slower the camera is.

**Fixes:** cache the learned `StreamConfig` per `(camera, stream)` so the
second and subsequent DESCRIBEs are instant; shorten the learning window in
the mode and build from the cached profile when it expires; keep the media
prepared between clients (see §2.6) so DESCRIBE never has to touch the camera
at all.

### 2.3 The stream must never go silent for more than 5 seconds

Once playing, go2rtc as an active producer sets a **5-second read deadline on
the media connection**, refreshed only by inbound data:

```go
// go2rtc pkg/rtsp/conn.go, core.ModeActiveProducer
if c.Timeout == 0 {
    timeout = time.Second * 5
    if len(c.Receivers) == 0 || c.Transport == "udp" {
        timeout += keepaliveDT
    }
}
```

The `+= keepaliveDT` relief applies only when go2rtc has *no* receivers (audio
backchannel only) or is on UDP. A normal neolink stream over TCP gets the flat
5 seconds. Five seconds of no RTP → connection dropped → reconnect.

neolink has three ways to go quiet for longer than that:

- **`pause.on_motion = true`.** The pause implementation simply stops pulling
  frames ([`src/common/instance/gst.rs:21`][ig-pause]) — nothing is
  substituted. Note also that `pause.mode`, `pause.on_disconnect` and
  `pause.motion_timeout` are documented in `sample_config.toml` and
  `src/rtsp/mod.rs` but are **read nowhere in the tree**; only `on_motion` has
  any effect. So the documented `mode = "none"` behaviour ("resends the last
  iframe") does not exist, and the modes that would keep frames flowing
  (`black`/`still`/`test`) are not wired up either.
- **Battery/idle cameras** that stop sending between events.
- **`max_fps = 1`** on a low-framerate substream.

Relying on RTCP to hold the socket open is not safe: GStreamer's `rtpsession`
randomises the RTCP interval around a 5 s minimum (roughly 2.5–7.5 s), so it
straddles go2rtc's deadline rather than clearing it.

**Fixes:** in the mode, warn (or refuse) on `pause.on_motion`; and/or
implement the long-documented `mode = "none"` as an actual keep-alive that
re-pushes the last I-frame on a timer well inside 5 s. The latter is the
better answer — it makes pausing usable with go2rtc instead of merely
forbidden.

### 2.4 The "Stream not Ready" splash is actively harmful here

While the camera is being set up, and whenever pipeline construction fails,
neolink serves a placeholder built from `videotestsrc ! textoverlay ! jpegenc
! rtpjpegpay` ([`build_unknown`, `src/rtsp/factory.rs:678`][f-unknown]),
mounted at every path up front ([`src/rtsp/mod.rs:357`][m-mount]) and used as
the fallback when `create_element` cannot build a real pipeline
([`src/rtsp/gst/factory.rs:155`][gf-fallback]).

That exists for good reason — the comments are explicit that Blue Iris gives
up permanently on a 404. For go2rtc it is the wrong trade:

- The SDP advertises **MJPEG video and no audio**. go2rtc caches the
  producer's media list; WebRTC and MSE can use neither.
- `num-buffers = 500` at 25 fps means the splash **ends after ~20 seconds**
  ([`src/rtsp/factory.rs:686`][f-numbuf]). With `eos_shutdown(false)` the
  session is not torn down, the stream simply stops — and go2rtc's 5 s read
  deadline fires. Reconnect, get another 20 s of JPEG, repeat.

go2rtc, unlike Blue Iris, retries a failed DESCRIBE with backoff perfectly
happily. **In the mode, `use_splash` should default to off** and DESCRIBE
should fail cleanly instead.

### 2.5 The SDP shape must be identical across reconnects

Whether the SDP has an audio track depends on whether an audio frame happened
to arrive inside the 10-second learning window
([`src/rtsp/factory.rs:236–279`][f-learn]): `aud_src` stays `None` and no
`pay1` is added if none did. Video type is learned the same way, and falls
back to the MJPEG splash when unknown.

So the same camera can present *video-only* on one connection and
*video+audio* on the next. go2rtc pins its downstream WebRTC/MSE sessions to
the media list it learned at connect time; a reconnect that changes the track
set breaks live viewers, not just the next one.

**Fix:** in the mode, always emit the same track set. The scaffolding is
already in the tree — `pipe_silence`
([`src/rtsp/factory.rs:1106`][f-silence]) is a complete silence-source builder
currently marked `#[allow(dead_code)]`. Combined with the cached
`StreamConfig` from §2.2, the track list becomes a property of the camera
rather than of the particular moment a client connected.

### 2.6 Session lifecycle: one client's churn shouldn't cost N camera sessions

Current factory settings ([`src/rtsp/gst/factory.rs:38–44`][gf-shared]):

```rust
factory.set_shared(false);
factory.set_eos_shutdown(false);
factory.set_stop_on_disconnect(false);
factory.set_suspend_mode(RTSPSuspendMode::Reset);
```

plus a 30-second session timeout ([`src/rtsp/gst/server.rs:68`][gs-timeout]).

`shared = false` means every RTSP client gets its own pipeline *and its own
camera video subscription*. `stop_on_disconnect = false` means a client that
drops its TCP connection without TEARDOWN — which is exactly what go2rtc does
when its read deadline fires — leaves that pipeline and subscription alive for
up to 30 seconds. Under the reconnect storm from §2.2/§2.3 those stack:
several concurrent `start_video` subscriptions against a camera that has a
small connection limit.

**Fixes:** `set_shared(true)` in the mode — one pipeline serving go2rtc,
snapshots and any second consumer, which is strictly better when the camera is
the scarce resource. Where sharing is not wanted, `set_stop_on_disconnect(true)`
at least releases the camera promptly. Both are per-factory, so they can be
mode-gated without touching the default path.

### 2.7 Start on a keyframe

The frames buffered during stream-type learning are replayed into the appsrc
verbatim ([`src/rtsp/factory.rs:308`][f-replay]), so a client can begin
mid-GOP. Buffer flags are already correct — `DELTA_UNIT` is set on P-frames
([`src/rtsp/factory.rs:635`][f-delta]) — and the payloaders already carry
parameter sets in-band via `config-interval=-1`
([`tune_video_payloader`, `src/rtsp/factory.rs:759`][f-tune]), so this is not
broken so much as untidy: go2rtc's H264 depayloader will discard the partial
access units, and time-to-first-frame is whatever the camera's GOP length
happens to be.

Dropping everything before the first I-frame in the frame pump makes the start
deterministic and guarantees the SDP's `sprop-parameter-sets` are populated
before the first client byte.

Worth noting what *won't* work: `RTSPMediaFactory::set_ensure_keyunit_on_start()`
exists in the bindings, but it needs GStreamer 1.24 (`v1_24` feature; we build
against `v1_20`) **and** it works by sending a force-key-unit event upstream —
which our appsrc cannot honour, because the Baichuan protocol has no
request-I-frame command. There is no such call anywhere in
`crates/core/src/bc_protocol`. Gating in neolink's own pump is the portable
answer.

### 2.8 Latency

The payloader tuning is already right. What is left:

- **`buffer_duration` defaults to 3000 ms** ([`src/config.rs:643`][cfg-buf]),
  and it is the dominant term in neolink's own contribution to glass-to-glass
  delay ([`make_queue`, `src/rtsp/factory.rs:1276`][f-queue]). Three seconds is
  a sensible ride-out-congestion default for a recorder; it is far too much in
  front of a WebRTC consumer. ~250 ms is the right order for the mode.
- **Transport.** go2rtc dials RTSP over **TCP interleaved by default**
  (`pkg/rtsp/client.go` sets `Protocol = "rtsp+tcp"` unless `?transport=udp`),
  so `set_protocols(RTSPLowerTrans::TCP)` costs go2rtc nothing and removes UDP
  packet loss as a failure mode for everyone else on that mount.
- **`max_fps` must be off.** It decimates frames without re-encoding, breaking
  the inter-frame prediction chain — the README already says as much. Under
  WebRTC/MSE that is continuous visible corruption, not a bandwidth saving.
  The mode should force it off and say so.

### 2.9 H265 is a real limitation, and not one neolink can fix

go2rtc's own compatibility table: H265 over WebRTC needs Chrome 136+ or
Safari 18+, and desktop Firefox does not support it for WebRTC or MSE at all.
Many current Reolink models default their main stream to H265.

neolink cannot change this. There is no encoder-configuration setter in the
Baichuan client — the full public API in `crates/core/src/bc_protocol` has
`get_stream_info()` but no corresponding set, and no I-frame request. The best
the mode can do is **detect and say so**: when the learned `vid_type` is H265,
log a prominent one-line warning naming the camera and suggesting either the
substream (usually H264) or go2rtc-side transcoding. Silent
"it works in Chrome but not Firefox" is the worst outcome.

---

## 3. Proposed configuration surface

A single per-camera profile key that changes **defaults only**, so every
individual setting stays overridable and nothing about the existing default
path changes:

```toml
[[cameras]]
name = "Front"
compat = "go2rtc"          # "default" (current behaviour) | "go2rtc"
buffer_duration = 400      # still wins over the profile default
```

Optionally the same key at top level as a default for all cameras.

`compat` is deliberately a *profile*, not a boolean — the same mechanism can
later carry a `"blueiris"` profile that pins today's lenient defaults
(splash on, long buffer, unshared) so those users are insulated if the global
defaults ever move.

### What the profile sets

| setting | today | `compat = "go2rtc"` | §  |
|---|---|---|---|
| `audio_format` | `latm` (**broken for go2rtc**) | `both` — `MPEG4-GENERIC` on `pay1`, `L16` on `pay2` | 2.1 |
| `buffer_duration` | 3000 ms | 250 ms | 2.8 |
| `use_splash` | true | **false** | 2.4 |
| `max_fps` | unset | forced off, warn if set | 2.8 |
| `pause.on_motion` | false | warn if set (until keep-alive exists) | 2.3 |
| *new* `shared` | false | **true** | 2.6 |
| *new* `stable_sdp` | false | **true** (silence fallback, fixed track set) | 2.5 |
| *new* `start_on_keyframe` | false | **true** | 2.7 |
| *new* `rtsp_protocols` | any | `tcp` | 2.8 |
| *new* `stream_learn_timeout` | 10 s | 2 s, backed by a cached per-stream profile | 2.2 |

### New `audio_format` variants

```
latm            → rtpmp4apay   MP4A-LATM       (kept; not usable by go2rtc)
mpeg4-generic   → rtpmp4gpay   MPEG4-GENERIC   (new — MSE/HLS/recording)
pcm             → rtpL16pay    L16             (kept — WebRTC)
both            → pay1 MPEG4-GENERIC + pay2 L16 (new — go2rtc profile default)
```

---

## 4. Suggested sequencing

The findings are mostly independent; they do not have to land as one change.

**Stage 1 — correctness, no mode needed.** These are bug fixes and should go
in on their own regardless of whether the profile is adopted.

1. Add `audio_format = "mpeg4-generic"` (`rtpmp4gpay`). One element swap in
   `build_aac`; the existing LATM pipeline already produces the right caps.
2. Correct the `audio_format` documentation in `src/config.rs` and
   `sample_config.toml` — LATM is *not* understood by go2rtc.
3. Either implement `pause.mode` / `pause.on_disconnect` /
   `pause.motion_timeout` or delete them from the docs. Documented-but-dead
   config is worse than no config.

**Stage 2 — the profile.** Introduce `compat`, wire it to the existing keys
(`audio_format`, `buffer_duration`, `use_splash`, `max_fps`) plus the factory
flags (`shared`, `stop_on_disconnect`, `protocols`), and add the H265 warning.
This is the smallest change that makes a default go2rtc install work well.

**Stage 3 — the structural work.** Cached per-stream `StreamConfig` and a
prepared/pre-warmed media so DESCRIBE never blocks on the camera (§2.2);
deterministic SDP with the silence fallback (§2.5); keyframe gating in the
frame pump (§2.7); the dual-audio-track pipeline (§2.1b); pause keep-alive by
I-frame re-push (§2.3). Each is independently testable.

## 5. Testing notes

`src/rtsp/factory.rs` already has pipeline-level tests that build the real
bins and assert on negotiated caps (`aac_latm_pipeline_negotiates_mp4a_latm`,
`video_payloaders_are_tuned_for_low_latency`), each guarded by a `require()`
helper that skips when a plugin is missing. The same pattern covers the new
work directly: assert `encoding-name == "MPEG4-GENERIC"` for the `rtpmp4gpay`
path, assert two audio `pay` elements exist for `both`, assert the factory
flags per profile, and unit-test keyframe gating on the pump.

Note that this checkout has no GStreamer installed, so those tests skip here;
they need to be run somewhere with `gst-plugins-good`/`-bad` present.

<!-- refs -->
[cfg-latm]: ../src/config.rs#L560
[cfg-claim]: ../src/config.rs#L558
[cfg-buf]: ../src/config.rs#L643
[sc-claim]: ../sample_config.toml#L117
[f-latm]: ../src/rtsp/factory.rs#L1023
[f-pipeaac]: ../src/rtsp/factory.rs#L876
[f-cb]: ../src/rtsp/factory.rs#L373
[f-learn]: ../src/rtsp/factory.rs#L236
[f-unknown]: ../src/rtsp/factory.rs#L678
[f-numbuf]: ../src/rtsp/factory.rs#L686
[f-silence]: ../src/rtsp/factory.rs#L1106
[f-replay]: ../src/rtsp/factory.rs#L308
[f-delta]: ../src/rtsp/factory.rs#L635
[f-tune]: ../src/rtsp/factory.rs#L759
[f-queue]: ../src/rtsp/factory.rs#L1276
[gf-create]: ../src/rtsp/gst/factory.rs#L141
[gf-shared]: ../src/rtsp/gst/factory.rs#L38
[gf-fallback]: ../src/rtsp/gst/factory.rs#L155
[gs-timeout]: ../src/rtsp/gst/server.rs#L68
[ig-pause]: ../src/common/instance/gst.rs#L21
[ig-start]: ../src/common/instance/gst.rs#L208
[m-mount]: ../src/rtsp/mod.rs#L357
