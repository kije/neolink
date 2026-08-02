# Optimising neolink for go2rtc / WebRTC / MSE

*Investigation and design proposal for a compatibility mode.*

Status: **mostly implemented.** Stages 0, 1 and 2 of §5 have landed on this
branch, along with the caching half of §3.2 — the misleading documentation is
corrected, `audio_format = "mpeg4-generic"` exists so `all` offers a track
go2rtc can use, and a learned stream profile now keeps the camera off the
DESCRIBE path. §2.1, §2.2 and §3.1 are marked resolved below and kept for the
record, §3.2 is reduced to its remaining half, and §3.4/§3.8 are carried by
the `compat = "go2rtc"` profile. What is left is §3.3 (silence), §3.5
(shared media), §3.6 (deterministic SDP) and §3.7 (keyframe start).

**§7 adds a second route.** `neolink stream` writes MPEG-TS to a pipe for
go2rtc's `exec:` source instead of serving RTSP. Most of what is left open
above is a property of the RTSP server rather than of neolink, and simply
does not arise over a pipe: §3.2, §3.3, §3.5, §3.6 and §3.7 are all closed
structurally there. It is an addition, not a replacement — the RTSP server
remains the only path with an `L16` track, which is still the only way to get
audio to a WebRTC viewer from an AAC camera.

Revised against `master` at `a09634b` (PRs #34 and #35), which landed a large
rework of the audio path and the FPS limiter while this was being written.
§1 records what that changed. Findings about go2rtc are cited against
`AlexxIT/go2rtc@master` source rather than its docs; findings about neolink
are cited by `file:line` against `a09634b`.

---

## 0. Why a mode at all

neolink's RTSP defaults are tuned for the consumers it has historically been
asked about: Blue Iris, VLC, ffmpeg, Home Assistant's generic camera. Those
clients are lenient — they retry a 404, they sit on a stalled socket for a
minute, they decode almost any RTP payload format.

go2rtc is not lenient, and it is not the end consumer. It is a *republisher*:
it terminates neolink's RTSP once and then feeds browsers over WebRTC and
MSE. That changes three things at once.

1. **The codec set is narrow.** Whatever go2rtc cannot name, it drops. There
   is no "decode it somehow" fallback unless the user wires up ffmpeg.
2. **The timeouts are short and fixed.** Five seconds, everywhere, with no
   knob in neolink's reach.
3. **Reconnects are routine, not exceptional.** go2rtc tears down and
   re-DESCRIBEs whenever a consumer needs a track it has not set up yet
   (§3.2). Anything expensive on the connect path is paid repeatedly.

The right shape for the fix is a **profile that changes defaults**, not a pile
of individually-documented knobs each go2rtc user has to discover the hard
way.

---

## 1. What master already fixed

Three of the original findings are now closed or materially reduced.

### 1.1 The default no longer silently drops audio — **resolved**

The original headline finding was that `audio_format` defaulted to `latm`,
which go2rtc cannot name, so **every go2rtc user on default settings got
silent streams**. `06364df` changed the default back to `pcm` (L16). go2rtc
understands `L16` (`CodecPCM`), so out of the box audio now reaches both
outputs: WebRTC via the G.711 resample path, MSE via FLAC.

This is the right default for go2rtc. The remaining audio problem — that the
*good* path was unreachable — is §3.1, closed on this branch.

### 1.2 `max_fps` no longer corrupts the stream — **resolved**

The old limiter forwarded every Nth frame regardless of type, orphaning
P-frames and producing continuous artefacts — which made it unusable in front
of WebRTC/MSE. `32a113a` replaced it with `GopLimiter`, which only ever drops
a *suffix* of a GOP and never drops a keyframe
(`src/rtsp/factory.rs:640-671`). Everything reaching the client is decodable.

One residual interaction is noted in §3.3.

### 1.3 An un-payloadable camera no longer kills the whole stream — **resolved**

`ab987c6` fixed a case where a camera sending MPEG-2 AAC made `aacparse`
refuse to negotiate, failed `gst_rtsp_media_prepare`, and turned DESCRIBE into
a `503` — taking the *video* with it. Framing is now learned from the ADTS
header, and a throwaway probe pipeline tries the real LATM chain before the
serving pipeline commits to it.

This is a genuine robustness win and exactly the right shape of fix. It does
add to the connect-path budget, which matters — see §3.2.

### 1.4 Confirmed sound: go2rtc really does negotiate per-track

`5b4d5c8`/`367b9aa` added `audio_format = "all"` (two audio tracks in the
SDP) and a per-client `?audio=` query parameter, on the premise that a
negotiating client sets up only the track it wants. **That premise checks
out.** go2rtc's RTSP producer sets up media lazily:

```go
// go2rtc pkg/rtsp/producer.go
func (c *Conn) GetTrack(media *core.Media, codec *core.Codec) (*core.Receiver, error) {
	...
	channel, err = c.SetupMedia(media)
```

so it issues `SETUP` only for tracks a consumer actually asked for. The
multi-track design works. The `?audio=` escape hatch is also a good fit for
go2rtc specifically — go2rtc passes an RTSP source URL through verbatim, so
`rtsp://neolink:8554/Cam/mainStream?audio=…` can go straight in its config
with no neolink-side change.

Both features were, however, pointed at a track go2rtc cannot use. §3.1 gives
them one it can.

---

## 2. What master introduced that needs attention

### 2.1 The documentation actively misled go2rtc users — **fixed on this branch**

The default moved, but the claim that motivated the old default did not. Three
places told a go2rtc user to reach for a format that would mute them:

- `src/config.rs:560` — LATM is "Understood by ffmpeg/ffprobe, VLC, **go2rtc**
  (and therefore Home Assistant and Frigate) and Blue Iris."
- `README.md:139` — "`MP4A-LATM` is understood by ffmpeg/ffprobe, VLC,
  **go2rtc** (and so Home Assistant and Frigate)…"
- `README.md:169` and the `AudioFormat::All` doc comment — `all` exists so
  that "a client that negotiates (**go2rtc**, and so Home Assistant and
  Frigate)" can pick the track it wants.

None of that is true of `MP4A-LATM`. go2rtc identifies AAC **solely** by the
rtpmap encoding name `MPEG4-GENERIC`:

```go
// go2rtc pkg/core/core.go
CodecAAC  = "MPEG4-GENERIC"
```

`UnmarshalCodec` (`pkg/core/codec.go`) stores the rtpmap name verbatim with no
aliasing step, so `a=rtpmap:97 MP4A-LATM/16000` yields `Codec.Name ==
"MP4A-LATM"`, which matches no constant, and:

```go
// go2rtc pkg/core/media.go
func GetKind(name string) string {
	switch name {
	case CodecH264, CodecH265, ...:                       return KindVideo
	case CodecPCMU, CodecPCMA, CodecAAC, CodecOpus, ...:  return KindAudio
	}
	return ""                                    // <- MP4A-LATM lands here
}
```

The practical consequence: `audio_format = "all"` gives go2rtc **nothing over
`pcm`**. go2rtc will negotiate, look at the two tracks, be unable to name the
LATM one, and set up L16 — while neolink runs and pays for a passthrough
branch nobody will ever subscribe to. Worse, a user who reads the README and
sets `latm` for their go2rtc camera gets silence and no error.

All three now say what is true, and `AudioFormat::Latm`'s own doc comment
points at `mpeg4-generic` instead.

### 2.2 The passthrough probe lands on the most contended path — **fixed on this branch**

`decide_audio_tracks_off_thread` is called at `src/rtsp/factory.rs:396`, which
is **before** `reply.send(element)` at line 429 — i.e. inside the window where
the client's DESCRIBE is blocked. Each probe is bounded by
`LATM_PROBE_TIMEOUT` (2 s, `src/rtsp/factory.rs:1268`).

That is a sound design in isolation, and it only runs for the passthrough
formats. But it stacks on top of the pre-existing 10-second learning window on
a budget that is already over-spent (§3.2) — and `all` now probes two formats,
so the worst case is camera connect + 10 s + 4 s against go2rtc's fixed 5 s.

Fixed by the §3.2 caching: verdicts are now recorded per `(stream, payload)`
in the shared profile and consulted before any probe runs, so each format is
probed at most once per stream per TTL rather than once per client. The
profile keeps a few of the camera's own AAC frames so that a client arriving
on a warm cache can still probe a format nobody has asked for yet, rather than
guessing.

---

## 3. What remains open

### 3.1 There was no AAC format go2rtc could use — **fixed on this branch**

Neither AAC nor L16 serves both go2rtc outputs, and neolink offered no AAC
framing go2rtc could name at all:

| go2rtc output | video | audio it can use | what it does with ours |
|---|---|---|---|
| **MSE / MP4 / HLS / recording**<br>(`pkg/mp4/consumer.go`) | H264, H265 | `MPEG4-GENERIC` (passthrough), Opus, MP3; PCMA/PCMU/L16/PCML → **re-encoded to FLAC** | `MP4A-LATM` → `handler.Handler = nil`, dropped. `L16` → FLAC-in-MP4, not universally playable. |
| **WebRTC**<br>(`pkg/webrtc/consumer.go`) | H264, H265 | PCMA, PCMU, L16, PCML — via `WithResampling` in `pkg/webrtc/helpers.go`, which appends wildcard `PCMA/0`/`PCMU/0` codecs so any clock rate matches and is transcoded to G.711 | `MP4A-LATM` → no match. **AAC in any framing → no match.** `L16` → transcoded to PCMA/8000. Works. |

So MSE wants AAC as `MPEG4-GENERIC`; WebRTC cannot use AAC at all and wants
L16. Today's `pcm` default is the best available compromise — WebRTC is
correct, MSE pays an FLAC re-encode, and recording never gets the camera's
own AAC.

**What landed.** `5b4d5c8` had already built the multi-track machinery, so
the fix was small:

- `audio_format = "mpeg4-generic"` payloads with `rtpmp4gpay`, which accepts
  exactly the caps the existing passthrough chain already produced —
  `audio/mpeg, mpegversion=4, stream-format=raw` — and emits
  `encoding-name=MPEG4-GENERIC` (RFC 3640, `mode=AAC-hbr`). Same passthrough,
  same zero decode cost. The framing guards and the negotiation probe are now
  parameterised by payload format and apply to it unchanged.
- `all` offers `MPEG4-GENERIC`, then `MP4A-LATM`, then `L16`. go2rtc finally
  has a real choice from one connection: passthrough AAC for what it muxes
  into MP4/HLS, `L16` for the WebRTC output that cannot take AAC at all.
  `AudioTracks` became an ordered list so the payloader indices stay
  contiguous however many formats a given camera is ruled out of.

Verified against a real gst-rtsp-server: a four-payloader bin describes as
four `m=` lines, the AAC one as

```text
a=rtpmap:96 MPEG4-GENERIC/16000/1
a=fmtp:96 streamtype=5;profile-level-id=2;mode=AAC-hbr;config=140856e500;sizelength=13;indexlength=3;indexdeltalength=3
```

which is the shape go2rtc's `pkg/aac` depayloader documents as its supported
case (`sizelength=13;indexlength=3` gives the 2-byte AU headers it requires).

### 3.2 go2rtc's 5-second deadlines vs. neolink's blocking DESCRIBE

go2rtc applies a fixed 5-second deadline to **every** RTSP request/response
round trip:

```go
// go2rtc pkg/rtsp/client.go
var Timeout = time.Second * 5
```

used as `SetWriteDeadline`/`SetReadDeadline` around each request in
`pkg/rtsp/conn.go`. The `source.Timeout` config option does not widen it — it
affects only the dial and the media read deadline.

neolink builds the whole pipeline **inside the DESCRIBE**. `create_element`
(`src/rtsp/gst/factory.rs:141`) calls `make_factory`'s callback, which blocks
on a oneshot while the per-client task connects the camera, starts the video
subscription, spends **up to ten seconds** learning the stream type
(`src/rtsp/factory.rs:303`), and now optionally runs the 2-second LATM probe
(§2.2).

For an already-streaming camera this returns quickly. For a battery camera
waking up, a camera reached over a Reolink relay, or one in its own reconnect
backoff, it does not — and go2rtc's DESCRIBE fails at 5 s.

**This is worse than it first appears, because go2rtc re-DESCRIBEs during
normal operation.** From `pkg/rtsp/producer.go`:

```go
func (c *Conn) GetTrack(media *core.Media, codec *core.Codec) (*core.Receiver, error) {
	...
	case core.ModeActiveProducer:
		if c.state == StatePlay {
			if err := c.Reconnect(); err != nil {
```

Any consumer attaching after playback has started and needing a track go2rtc
has not yet set up — a WebRTC viewer opening while MSE is already running,
Frigate starting a recording alongside a live view — triggers a **full
teardown, DESCRIBE, SETUP, PLAY cycle**. So the cold-start cost is not paid
once at startup; it is paid every time the consumer mix changes. And because
the factory is not shared (`src/rtsp/gst/factory.rs:39`), each cycle runs the
learning window again and opens another `start_video` subscription
(`src/common/instance/gst.rs:208`) on a camera with a small connection limit.

**Half fixed on this branch.** `ProfileCache` holds one `StreamProfile` per
`(camera, stream)`, shared by every client of it: the learned `StreamConfig`
(so no `get_stream_info` round trip and no learning window), a few AAC frames,
and the per-payload probe verdicts. A cold client learns as before and stores
what it found; every client after it — including every one of go2rtc's routine
re-DESCRIBEs — is served from the profile. A five-minute TTL means changing
the camera's encoder settings still takes effect without restarting neolink,
and a learn that found no video type is deliberately *not* stored, so a camera
that was simply not ready gets retried rather than pinned to the splash.

**Still open:** the first frame. gst-rtsp-server prerolls the media before it
can describe it, so even a warm DESCRIBE waits for the camera to deliver one
buffer. That is far inside go2rtc's 5 s budget where the camera is already
streaming, but a battery camera waking from idle can still exceed it. Keeping
the media prepared between clients (§3.5) is what removes the camera from the
DESCRIBE path completely.

### 3.3 The stream must never go silent for more than 5 seconds

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
5 seconds.

neolink can still go quiet for longer than that:

- **`pause.on_motion = true`.** The pause implementation simply stops pulling
  frames (`src/common/instance/gst.rs:21`); nothing is substituted. Note also
  that `pause.mode`, `pause.on_disconnect` and `pause.motion_timeout` are
  documented in `sample_config.toml` and `src/rtsp/mod.rs` but are **read
  nowhere in the tree** — only `on_motion` has any effect. The documented
  `mode = "none"` behaviour ("resends the last iframe") does not exist, and
  the modes that would keep frames flowing are not wired up either.
- **Battery/idle cameras** that stop sending between events.
- **`max_fps` on a long-GOP camera — new.** `GopLimiter` never drops a
  keyframe, so the worst-case gap between forwarded frames is one GOP. For the
  typical Reolink 2–4 s GOP that stays inside the deadline; a camera
  configured with a longer keyframe interval would not. This is a caution to
  document, not a bug in the limiter.

Relying on RTCP to hold the socket open is not safe: GStreamer's `rtpsession`
randomises the RTCP interval around a 5 s minimum (roughly 2.5–7.5 s), so it
straddles go2rtc's deadline rather than clearing it.

**Fixes:** in the mode, warn (or refuse) on `pause.on_motion`; and/or
implement the long-documented `mode = "none"` as an actual keep-alive that
re-pushes the last I-frame on a timer well inside 5 s. The latter is the
better answer — it makes pausing usable with go2rtc instead of merely
forbidden.

### 3.4 The "Stream not Ready" splash is actively harmful here — **carried by the profile**

While the camera is being set up, and whenever pipeline
construction fails, neolink serves a placeholder built from `videotestsrc !
textoverlay ! jpegenc ! rtpjpegpay` (`build_unknown`,
`src/rtsp/factory.rs:895`), mounted at every path up front
(`src/rtsp/mod.rs:357`) and used as the fallback when `create_element` cannot
build a real pipeline (`src/rtsp/gst/factory.rs:174`).

That exists for good reason — the comments are explicit that Blue Iris gives
up permanently on a 404. For go2rtc it is the wrong trade:

- The SDP advertises **MJPEG video and no audio**. go2rtc caches the
  producer's media list; WebRTC and MSE can use neither.
- `num-buffers = 500` at 25 fps means the splash **ends after ~20 seconds**
  (`src/rtsp/factory.rs:903`). With `eos_shutdown(false)` the session is not
  torn down, the stream simply stops — and go2rtc's 5 s read deadline fires.
  Reconnect, get another 20 s of JPEG, repeat.

go2rtc, unlike Blue Iris, retries a failed DESCRIBE with backoff perfectly
happily. `compat = "go2rtc"` therefore defaults `use_splash` to off, and a
DESCRIBE that cannot be served fails cleanly instead. Anyone who wants the
placeholder back can still set `use_splash = true` alongside the profile.

### 3.5 Session lifecycle: one client's churn shouldn't cost N camera sessions

Unchanged (`src/rtsp/gst/factory.rs:38-44`):

```rust
factory.set_shared(false);
factory.set_eos_shutdown(false);
factory.set_stop_on_disconnect(false);
factory.set_suspend_mode(RTSPSuspendMode::Reset);
```

plus a 30-second session timeout (`src/rtsp/gst/server.rs:68`).

`shared = false` means every RTSP client gets its own pipeline *and its own
camera video subscription*. `stop_on_disconnect = false` means a client that
drops its TCP connection without TEARDOWN — exactly what go2rtc does when its
read deadline fires — leaves that pipeline and subscription alive for up to 30
seconds. Given how routine go2rtc's reconnects turn out to be (§3.2), those
stack.

There is a tension to resolve here: `367b9aa` relies on the factory *not*
being shared, so that two clients can hold different `?audio=` formats on the
same camera at once. Sharing would have to be keyed on the resolved format
(one shared media per distinct format) rather than switched on wholesale.

**Half done.** `compat = "go2rtc"` now sets `set_stop_on_disconnect(true)`,
which releases the camera as soon as the client goes rather than at the
session timeout — the cheap half, and it needs no design change.

**Still open:** `set_shared(true)`, keyed per resolved audio format, so one
pipeline serves go2rtc, snapshots and any second consumer. That is strictly
better when the camera is the scarce resource, but it reworks the per-client
model `367b9aa` just landed, so it wants deciding on its own rather than
riding in on a profile.

### 3.6 The SDP shape must be identical across reconnects

Whether the SDP has an audio track still depends on whether an audio frame
happened to arrive inside the learning window
(`src/rtsp/factory.rs:303-361`): `aud_src` stays `None` and no `pay1` is added
if none did. Video type is learned the same way and falls back to the MJPEG
splash when unknown.

`ab987c6` adds a second source of shape variance: when the LATM probe fails,
`all` collapses from two audio tracks to one. That decision is derived from the
camera's own frames so it should be stable in practice — but it is derived
per-connection, from whichever frames that connection happened to buffer.

go2rtc pins downstream WebRTC/MSE sessions to the media list it learned at
connect time, so a reconnect that changes the track set breaks live viewers,
not just the next one.

**Fix:** in the mode, always emit the same track set, and derive it from the
cached per-stream profile (§3.2) rather than from the current connection's
buffer. The scaffolding is in the tree — `pipe_silence`
(`src/rtsp/factory.rs:1559`) is a complete silence-source builder still marked
`#[allow(dead_code)]`.

### 3.7 Start on a keyframe

Unchanged. The frames buffered during stream-type learning are replayed into
the appsrc verbatim (`src/rtsp/factory.rs:401`), so a client can begin
mid-GOP. Buffer flags are already correct — `DELTA_UNIT` is set on P-frames
(`src/rtsp/factory.rs:853`) — and the payloaders already carry parameter sets
in-band via `config-interval=-1` (`tune_video_payloader`,
`src/rtsp/factory.rs:976`), so this is untidy rather than broken: go2rtc's
H264 depayloader discards the partial access units, and time-to-first-frame is
whatever the camera's GOP length happens to be.

Dropping everything before the first I-frame in the frame pump makes the start
deterministic and guarantees the SDP's `sprop-parameter-sets` are populated
before the first client byte. `GopLimiter` already tracks keyframe boundaries,
so the state to do this is present.

Worth noting what *won't* work:
`RTSPMediaFactory::set_ensure_keyunit_on_start()` exists in the bindings, but
it needs GStreamer 1.24 (`v1_24` feature; we build against `v1_20`) **and** it
works by sending a force-key-unit event upstream — which our appsrc cannot
honour, because the Baichuan protocol has no request-I-frame command. There is
no such call anywhere in `crates/core/src/bc_protocol`. Gating in neolink's own
pump is the portable answer.

### 3.8 Latency

The payloader tuning is right and `max_fps` is now safe (§1.2). What is left:

Both now carried by the profile:

- **`buffer_duration`** is the dominant term in neolink's own contribution to
  glass-to-glass delay (`make_queue`). Three seconds is a sensible
  ride-out-congestion default for a recorder; it is far too much in front of a
  WebRTC consumer. `compat = "go2rtc"` defaults it to 250 ms.
- **Transport.** go2rtc dials RTSP over **TCP interleaved by default**
  (`pkg/rtsp/client.go` sets `Protocol = "rtsp+tcp"` unless `?transport=udp`),
  so restricting the factory to `RTSPLowerTrans::TCP` costs go2rtc nothing and
  removes UDP packet loss as a failure mode for everyone else on that mount.

### 3.9 H265 is a real limitation, and not one neolink can fix

go2rtc's own compatibility table: H265 over WebRTC needs Chrome 136+ or
Safari 18+, and desktop Firefox does not support it for WebRTC or MSE at all.
Many current Reolink models default their main stream to H265.

neolink cannot change this. There is no encoder-configuration setter in the
Baichuan client — the public API in `crates/core/src/bc_protocol` has
`get_stream_info()` but no corresponding set, and no I-frame request. The best
the mode can do is **detect and say so**: when the learned `vid_type` is H265,
log a prominent one-line warning naming the camera and suggesting either the
substream (usually H264) or go2rtc-side transcoding. Silent "it works in
Chrome but not Firefox" is the worst outcome.

---

## 4. Proposed configuration surface

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
later carry a `"blueiris"` profile pinning today's lenient defaults (splash
on, long buffer, unshared) so those users are insulated if the global defaults
move again.

| setting | today | `compat = "go2rtc"` | § |
|---|---|---|---|
| `audio_format` | `pcm` | `all` (done) | 3.1 |
| `buffer_duration` | 3000 ms | 250 ms (done) | 3.8 |
| `use_splash` | true | **false** (done) | 3.4 |
| `pause.on_motion` | false | warn if set (until keep-alive exists) | 3.3 |
| `max_fps` | unset | warn if the camera's GOP approaches 5 s | 3.3 |
| *new* `shared` | false | **true**, keyed per resolved audio format (open) | 3.5 |
| *new* `stable_sdp` | false | **true** (silence fallback, fixed track set) | 3.6 |
| *new* `start_on_keyframe` | false | **true** | 3.7 |
| *new* `rtsp_protocols` | any | `tcp` (done, not a config key — the profile sets it) | 3.8 |
| *new* `stream_learn_timeout` | 10 s | 2 s (the cached profile makes this a cold-start-only cost) | 3.2 |

### New `audio_format` variant

```
mpeg4-generic   → rtpmp4gpay   MPEG4-GENERIC   (MSE/HLS/recording passthrough)
latm            → rtpmp4apay   MP4A-LATM       (not usable by go2rtc)
pcm             → rtpL16pay    L16             (default; WebRTC)
all             → all of the above as separate tracks, contiguous payN
```

---

## 5. Suggested sequencing

**Stage 0 — documentation. Done.** The places that told go2rtc users to reach
for `MP4A-LATM` now say what is true (§2.1).

**Stage 1 — the missing codec. Done.** `audio_format = "mpeg4-generic"`
(`rtpmp4gpay`), included in `all`, which is what makes `all` and `?audio=`
worth anything to a go2rtc user (§3.1).

**Stage 2 — the profile. Done**, except for shared media. `compat` moves the
defaults of `audio_format`, `buffer_duration` and `use_splash`, sets
`stop_on_disconnect` and TCP-only transports on the factory, and warns on
H265. Anything set explicitly still wins. Shared media was deliberately left
out — see §3.5.

**Stage 3 — the structural work.** The cached per-stream profile is **done**
(§3.2). Remaining: keeping the media prepared between clients so DESCRIBE need
not touch the camera at all (§3.2/§3.5); deterministic SDP with the silence
fallback (§3.6); keyframe gating in the frame pump (§3.7); pause keep-alive by
I-frame re-push (§3.3). Each is independently testable.

Also worth resolving on its own: either implement `pause.mode`,
`pause.on_disconnect` and `pause.motion_timeout` or delete them from the docs.
Documented-but-dead config is worse than no config.

## 6. Testing notes

`src/rtsp/factory.rs` has pipeline-level tests that build the real bins and
assert on negotiated caps (`aac_latm_pipeline_negotiates_mp4a_latm`,
`video_payloaders_are_tuned_for_low_latency`, and the newer multi-track and
probe tests), each guarded by a `require()` helper that skips when a plugin is
missing. The same pattern covers the new work directly: assert
`encoding-name == "MPEG4-GENERIC"` for the `rtpmp4gpay` path, assert the
payloader indices stay contiguous when a track is ruled out, assert the
factory flags per profile, and unit-test keyframe gating on the pump.

The Stage 0/1 work was developed with GStreamer 1.24 installed, so those
tests ran rather than skipping — including the new
`each_passthrough_format_negotiates_its_own_encoding_name`, which asserts the
`encoding-name` each payloader actually negotiates. On a machine without
`gst-plugins-good`/`-bad` they skip silently, so a green run there proves
less than it appears to.


---

## 7. The `exec:` pipe

Everything above treats go2rtc as an RTSP client and tries to make neolink a
better RTSP server for it. go2rtc has a second way in: `exec:` runs a command
and reads its stdout, autodetecting the container
(`internal/exec/exec.go`, `pkg/magic/producer.go`).

```yaml
streams:
  front: exec:neolink stream --config=/etc/neolink.toml Front
```

### 7.1 Why this closes so much of §3 at once

The pipe path is not a workaround; it removes the machinery the findings are
about.

| finding | over RTSP | over a pipe |
|---|---|---|
| §3.2 five second DESCRIBE deadline | `Timeout = 5s` on every request/response round trip, and go2rtc re-DESCRIBEs whenever the consumer mix changes | `handlePipe` applies **no** timeout at all. `starttimeout` is read but used only by the RTSP branch. A cold camera costs startup time, not the connection |
| §3.3 five second read deadline | flat 5 s on the media connection, refreshed only by inbound data | a pipe has no read deadline. A silent camera stalls the picture; it does not kill the source |
| §3.5 one camera session per client | `set_shared(false)`, plus a 30 s session timeout for clients that vanish without a TEARDOWN | go2rtc starts the process for the first consumer and closes it after the last, so the camera is connected on demand and exactly once. `set_shared(true)` becomes unnecessary rather than unfinished |
| §3.6 SDP shape must be identical across reconnects | the track set depends on what arrived inside the learning window | the PMT is written once, ahead of all output, from a completed probe. It cannot change mid-stream because there is no reconnect to change it on |
| §3.7 start on a keyframe | buffered frames are replayed verbatim, so a client can begin mid-GOP | the pump drops everything before the newest keyframe. It has to: go2rtc derives the H264/H265 codec from the first access unit it sees (`h264.AVCCToCodec`), so `sprop-parameter-sets` would otherwise be empty |
| §3.4 splash | mounted up front, MJPEG, ends after ~20 s | nothing is written until there is a real keyframe |
| §3.8 latency | `buffer_duration` queue, plus payloader tuning | no queue. The muxer writes and flushes per frame |

### 7.2 What the format has to be

`magic.Open` peeks four bytes and dispatches on them. Of everything it
accepts — Annex-B, WAV, Y4M, FLV, MJPEG, ADTS, multipart, MPEG-TS — only
**MPEG-TS** carries video and audio together, and it is also the only one
that carries H265.

go2rtc's TS demuxer (`pkg/mpegts/producer.go`) creates a media for these
stream types, and silently ignores every other one:

| stream type | value | notes |
|---|---|---|
| H264 | `0x1B` | payload must be Annex-B; the demuxer converts to AVCC itself |
| H265 | `0x24` | as above |
| AAC | `0x0F` | payload must be ADTS framed, which is exactly what the camera sends |
| Opus | `0xEB` | private type, keyed off a registration descriptor |
| A-law | `0x90` | private type adopted from Tapo. **See §7.4** |

Two of those are free. The camera's H264/H265 is already Annex-B and its AAC
is already ADTS, so the video and the AAC are copied rather than converted —
`src/mpegts` does not decode anything, and does not link GStreamer, which is
why `neolink stream` works in a build without the `gstreamer` feature.

### 7.3 The AAC track go2rtc actually wanted

§3.1 went to some trouble to give go2rtc an AAC track it could name, because
it identifies AAC solely by the rtpmap encoding name `MPEG4-GENERIC`. Over
MPEG-TS that problem does not exist: the codec comes from the PMT stream
type, and go2rtc constructs the codec as `CodecAAC` — which *is*
`MPEG4-GENERIC` — from the ADTS header (`aac.RTPToCodec`).

Confirmed against go2rtc 1.9.14, feeding it a program muxed by
`src/mpegts`:

```text
"format_name": "mpegts", "protocol": "pipe",
"medias": ["video, recvonly, H264", "audio, recvonly, MPEG4-GENERIC/16000/1"]
```

and an MP4 consumer attached to it took both tracks straight through, with
the senders' codecs unchanged from the receivers' — no FLAC re-encode, which
is what an `L16` track would have cost (§3.1).

### 7.4 A-law works, but not on the released go2rtc

The A-law stream type is the one thing here that is version dependent, and
the difference was verified by running both builds against the same file:

| go2rtc | what it reports |
|---|---|
| **1.9.14** (current release) | `medias: ["video, recvonly, H264"]` — the audio is absent |
| **master** (`1.9.14+dev.c245815`) | `medias: ["video, recvonly, H264", "audio, recvonly, PCMA/8000"]` |

The cause is not the muxing. `Producer.probe` only creates a media for the
stream types named in its `switch`, and 1.9.14 lists H264, H265, AAC and
Opus but not `StreamTypePCMATapo`; master adds it. The constant exists in
both, for the Tapo-specific producer.

The consequence is narrow but worth stating plainly: an ADPCM camera piped
into a released go2rtc has no audio. An unlisted stream type is skipped
rather than waited for, so emitting it costs nothing on an old version and
starts working on an upgrade — but it is not something to rely on today.

For an AAC camera — which is most of them — none of this applies, and the
RTSP server with `compat = "go2rtc"` remains the answer for anyone who needs
WebRTC audio from one.

### 7.5 What is still on the RTSP side

The pipe does not make §4's profile redundant.

* **WebRTC audio from an AAC camera.** go2rtc cannot use AAC for WebRTC in
  any framing, and MPEG-TS has no `L16` stream type, so the pipe cannot offer
  the resample-friendly track that `audio_format = "pcm"` does. Closing this
  needs an encoder in neolink: AAC decoded to A-law, or to Opus, which
  `0xEB` would carry and which both of go2rtc's outputs take natively.
* **Anything that wants RTSP.** Blue Iris, ZoneMinder and the rest are
  unaffected by all of this and keep the server.
* **§3.3's keep-alive.** `pause.on_motion` still stops frames, and while a
  pipe will not be killed for it, a frozen picture is not much better. The
  I-frame re-push idea stands on its own.
