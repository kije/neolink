# Neolink

![CI](https://github.com/QuantumEntangledAndy/neolink/workflows/CI/badge.svg)
[![dependency status](https://deps.rs/repo/github/QuantumEntangledAndy/neolink/status.svg)](https://deps.rs/repo/github/QuantumEntangledAndy/neolink)

Neolink is a small program that acts as a proxy between Reolink IP cameras and
normal RTSP clients.
Certain cameras, such as the Reolink B800, do not implement ONVIF or RTSP, but
instead use a proprietary "Baichuan" protocol only compatible with their apps
and NVRs (any camera that uses "port 9000" will likely be using this protocol).
Neolink allows you to use NVR software such as Shinobi or Blue Iris to receive
video from these cameras instead.
The Reolink NVR is not required, and the cameras are unmodified.
Your NVR software connects to Neolink, which forwards the video stream from the
camera.

The Neolink project is not affiliated with Reolink in any way; everything it
does has been reverse engineered.

## This Fork

This fork is an extension of
[thirtythreeforty's](https://github.com/thirtythreeforty/neolink) with additional
features not yet in upstream master.

**Major Features**:

- MQTT
- ONVIF (Profile S — device discovery, RTSP/snapshot URI hand-off, PTZ)
- Motion Detection
- Paused Streams (when no rtsp client or no motion detected)
- Save a still image to disk

**Minor Features**:

- Improved error messages when missing gstreamer plugins
- Protocol more closely follows official reolink format
  - Possibly can handle more simulatenous connections
- More ways to connect to the camera. Including Relaying through reolink
  servers
- Camera battery levels can be displayed in the log

## Installation

Download from the
[release page](https://github.com/QuantumEntangledAndy/neolink/releases)

Extract the zip

Install the latest [gstreamer](https://gstreamer.freedesktop.org/download/)
(1.20.5 as of writing this).

- **Windows**: ensure you install `full` when prompted in the MSI options.
- **Mac**: Install the dpkg version on the official gstreamer website over
  the brew version
- **Ubuntu/Debian**: These packages should work

```bash
sudo apt install \
  libgstrtspserver-1.0-0 \
  libgstreamer1.0-0 \
  libgstreamer-plugins-bad1.0-0 \
  gstreamer1.0-x \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  libssl
```

- **Windows**: You may also need to
  [install openssl](https://wiki.openssl.org/index.php/Binaries)
- **Macos**: You may also need to
  [install openssl](https://wiki.openssl.org/index.php/Binaries) or
  `brew install openssl@1.1`
- **Ubuntu/Debian**: Install the `libssl` package

Make a config file see below.

## Config/Usage

### RTSP

To use `neolink` you need a config file.

There's a more complete example
[here](https://github.com/QuantumEntangledAndy/neolink/blob/master/sample_config.toml),
but the following should work as a minimal example.

```toml
bind = "0.0.0.0"

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"

[[cameras]]
name = "Camera02"
username = "admin"
password = "password"
uid = "BCDEF0123456789A"
address = "192.168.1.10"
```

Create a text file called `neolink.toml` in the same folder as the
neolink binary. With your config options.

When ready start `neolink` with the following command
using the terminal in the same folder the neolink binary is in.

```bash
./neolink rtsp --config=neolink.toml
```

### Tuning for a consumer

neolink's defaults suit the clients it has historically been pointed at —
Blue Iris, VLC, ffmpeg, Home Assistant's generic camera. Those are lenient:
they retry, they wait, and they decode almost any RTP payload format.

go2rtc is neither lenient nor the end consumer. It terminates neolink's RTSP
once and republishes to browsers over WebRTC and MSE, which means a narrow
codec set, a fixed five-second timeout on every RTSP request, and reconnects
as a matter of routine rather than of failure. It wants close to the opposite
settings.

Rather than make everyone find each knob separately, `compat` picks a
coherent set of **defaults**:

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
compat = "go2rtc"          # "default" (what neolink has always done) or "go2rtc"
```

| setting | `"default"` | `"go2rtc"` | why |
| --- | --- | --- | --- |
| `audio_format` | `pcm` | `all` | go2rtc's MP4/HLS output wants the AAC untouched; its WebRTC output cannot decode AAC at all and needs the `L16` track |
| `buffer_duration` | 3000 ms | 250 ms | queue depth rides out congestion for a recorder; in front of a live viewer it is just delay |
| `use_splash` | on | **off** | the placeholder is MJPEG, which browsers cannot play, and it ends after ~20s — go2rtc then reconnects, forever |
| RTSP transports | any | TCP only | go2rtc dials TCP anyway, so this costs it nothing and takes UDP packet loss off the table |
| media on disconnect | held until the session times out | torn down at once | go2rtc drops the connection without a TEARDOWN when its read deadline fires; holding the pipeline open just stacks up camera subscriptions |

**It only moves defaults.** Anything you set explicitly still wins, so
`compat = "go2rtc"` with `buffer_duration = 1000` gives you the go2rtc
profile with a one-second queue. `profile` and `tuned_for` are accepted as
aliases for the key, and `webrtc` and `frigate` for the value.

The profile also warns, once per stream, when the camera is sending H265 —
browsers are far pickier about it than about H264, and neolink cannot change
the camera's encoder. See the note at the end of this section.

`docs/go2rtc-compatibility.md` has the reasoning, and what is still open.

### Piping a camera straight into go2rtc

Everything above tunes the RTSP server so that go2rtc can live with it. The
`stream` subcommand takes the other route and removes the server from the
picture: it writes one camera to standard output as MPEG-TS, and go2rtc runs
it as an `exec:` source.

```yaml
streams:
  front: exec:neolink stream --config=/etc/neolink.toml Front
  drive: exec:neolink stream --config=/etc/neolink.toml Drive --stream sub
```

That is the whole configuration. There is no port to bind, no mount point to
name, and no RTSP tuning to get right, because the things that make go2rtc
awkward in front of an RTSP server are not present in a pipe:

| what goes wrong over RTSP | over a pipe |
| --- | --- |
| go2rtc gives every RTSP request five seconds; a battery camera waking up can miss it and the DESCRIBE fails | go2rtc waits on the pipe indefinitely, so a slow camera costs startup time rather than the connection |
| a media connection is dropped after five seconds of silence | there is no idle deadline on a pipe |
| the media factory is unshared, so every RTSP client opens its own camera subscription, and a client that vanishes without a TEARDOWN holds it for another 30 s | go2rtc starts the process for the first viewer and stops it after the last, so the camera is connected on demand and exactly once |
| whether the SDP has an audio track depends on what arrived during the learning window, and a reconnect that changes the track set breaks live viewers | the track list is settled before the first byte and cannot change under a viewer |
| the "Stream not Ready" splash is MJPEG, which browsers cannot play, and it stops after ~20 s | nothing is written until there is a real keyframe to write |

Output always starts on a keyframe, so the parameter sets a decoder needs are
in front of the first frame a consumer sees.

#### Options

| flag | default | notes |
| --- | --- | --- |
| `-o`, `--output` | `-` | `-` is stdout. Anything else is a path; an existing FIFO works, and opening it blocks until a reader arrives |
| `--stream` | `main` | `main`, `sub` or `extern` |
| `--format` | `mpegts` | or `annexb` for a bare video elementary stream with no audio, for piping into ffmpeg |
| `--audio` | `auto` | `auto`, `aac`, `pcma` or `none` |
| `--audio-probe` | `2` | seconds to wait for an audio frame before deciding a camera is video only. Only the first frames are delayed; `--audio none` skips the wait |

#### Audio

| the camera sends | `--audio auto` writes | what go2rtc does with it |
| --- | --- | --- |
| AAC | the camera's AAC, untouched | named `MPEG4-GENERIC`, so MP4, HLS and recordings get it with no re-encode. WebRTC cannot use AAC in any framing, so a WebRTC viewer gets no sound |
| ADPCM | G.711 A-law, decoded and re-encoded | WebRTC takes A-law with no transcode — **but only on a go2rtc newer than 1.9.14**, whose MPEG-TS reader does not look for it and will show the stream as video only |

Both cases are cheap: AAC is copied, and A-law costs a table lookup per
sample. Neither path decodes video.

If you need audio in a browser from an AAC camera today, the RTSP server is
still the way — `compat = "go2rtc"` gives go2rtc an `L16` track it can
resample for WebRTC alongside the AAC it mixes into MP4.

#### Why a pipe rather than a FIFO or a virtual camera

A named FIFO is the same bytes with the good part removed. `--output` will
write to one, but nothing then starts neolink when a viewer appears or stops
it when they leave, a FIFO carries exactly one reader, and both ends have to
be restarted in the right order after either dies. The `exec:` pipe is a FIFO
that go2rtc also manages the lifetime of.

A virtual video device (`v4l2loopback`) is worse again: it needs an
out-of-tree kernel module and root, it carries raw frames, so the H264 or
H265 the camera already produced has to be decoded and re-encoded, and V4L2
has nowhere to put the audio.

#### Notes

- The `pause` settings do not apply. There is nothing to pause when nothing
  runs unless someone is watching.
- Logs go to stderr, so they land in go2rtc's log and never in the stream.
  `RUST_LOG=debug` works as everywhere else.
- This path does not use GStreamer, so it is available in builds without the
  `gstreamer` feature.
- H265 works, and is passed through as-is. The browser support caveat is
  unchanged: no desktop Firefox, and WebRTC needs Chrome 136+ or Safari 18+.
  `--stream sub` is usually H264.

### Neolink on the host, Frigate in a container

`exec:` has one requirement that is easy to miss: go2rtc runs the command
*inside its own container*, so neolink, its config and a route to the
cameras all have to be there. If neolink runs on the host and Frigate or
go2rtc runs in Docker, that does not hold.

`neolink pipe` crosses that boundary with a path instead. It creates one
endpoint per camera in a directory you bind-mount, and streams into
whichever of them something opens:

```bash
neolink pipe --config=/etc/neolink.toml --dir=/run/neolink
```

```yaml
# docker-compose.yml
services:
  frigate:
    volumes:
      - /run/neolink:/pipes
```

A camera is connected only while something is reading its endpoint, and
released when the last reader goes — the same on-demand behaviour `exec:`
gets from starting and stopping the process, without needing the process
to be startable.

#### Running it

It stays in the foreground and does not fork, write a PID file, or detach.
Supervise it the way you would anything else — systemd on the host, or a
container of its own:

```ini
# /etc/systemd/system/neolink-pipe.service
[Unit]
Description=Neolink camera pipes
After=network-online.target

[Service]
ExecStart=/usr/local/bin/neolink pipe --config=/etc/neolink.toml --dir=/run/neolink
Restart=on-failure
RuntimeDirectory=neolink

[Install]
WantedBy=multi-user.target
```

One process serves **every enabled camera in the config** — `enabled = false`
cameras are skipped — and `--camera Front --camera Drive` narrows it to the
ones you name. Endpoints are named after the camera, so a camera called
`Front` becomes `Front.ts`.

`--endpoint` decides which kind gets made, and it makes only that kind:
`fifo` (the default) creates `Front.ts` alone, `socket` creates
`Front.sock` alone, and `both` creates each. Endpoints left over from a
previous run are reused rather than replaced, so a reader that is already
attached to one is not disturbed by a restart.

**Read each endpoint once.** A FIFO delivers each byte to exactly one
reader, so two processes on the same one get a share each and neither a
whole stream. That is not a limitation to work around so much as the thing
to design for: have one reader take the pipe and republish.

For **plain go2rtc**, that one reader is `cat`:

```yaml
streams:
  front: exec:cat /pipes/Front.ts
```

Everything else — WebRTC, MSE, the RTSP restream — is served by go2rtc
from that single connection.

**Frigate** embeds go2rtc, so the same applies with one extra step: let its
go2rtc read the pipe, and point the camera's ffmpeg at go2rtc's restream
rather than at the pipe a second time.

```yaml
go2rtc:
  streams:
    front: exec:cat /pipes/Front.ts

cameras:
  front:
    ffmpeg:
      # Not `/pipes/Front.ts` — go2rtc already has it, and a FIFO cannot
      # be read twice. This is Frigate's usual restream arrangement.
      inputs:
        - path: rtsp://127.0.0.1:8554/front
          input_args: preset-rtsp-restream
          roles: [detect, record]
```

If you would rather each consumer open the camera for itself, use
`--endpoint socket`, which takes as many readers as turn up:

```yaml
go2rtc:
  streams:
    front: ffmpeg:unix:///pipes/Front.sock#video=copy#audio=copy
```

#### Which endpoint

`--endpoint` picks what gets created. Both carry the same MPEG-TS.

| | `fifo` (default) | `socket` |
| --- | --- | --- |
| path | `<dir>/<Camera>.ts` | `<dir>/<Camera>.sock` |
| readers | one at a time — two processes on one FIFO would each get a share of the bytes and neither a whole stream | as many as you like, each with a complete stream of its own |
| go2rtc | `exec:cat /pipes/Front.ts` | `ffmpeg:unix:///pipes/Front.sock#video=copy#audio=copy` |
| Frigate | via its embedded go2rtc, then `rtsp://127.0.0.1:8554/front` | `path: unix:///pipes/Front.sock`, or via go2rtc |
| a reader dying | noticed at the next write | noticed at once |

Use `fifo` unless you want several consumers each opening the camera
independently; `--endpoint both` creates each. Note that "more readers"
is rarely what you need — go2rtc already fans one connection out to
every viewer, which is the cheaper arrangement for the camera too.

#### Joining a stream already in progress

A reader that connects part-way through a GOP would otherwise see nothing
until the next keyframe — on a camera with a four second keyframe interval,
four seconds of black. The GOP in progress is therefore kept and handed to
a reader the moment it attaches, so it has a picture straight away and
still starts on a keyframe. The catch-up burst is exempt from the backlog
limit below, since it is old by construction and would otherwise look like
a reader that had already fallen behind.

#### A stalled reader never becomes a backlog

If a reader crashes, hangs, or is simply slower than the camera, queuing
what it missed is the wrong answer — it would come back and play the gap
out, permanently that far behind. Past `--max-backlog` seconds of queued
video, that reader's queue is dropped and it resumes at the next keyframe,
so a recovered reader sees what the camera is doing *now*.

Each reader is measured separately, so a slow one cannot hold up a fast
one, and neither can hold up the camera. `--max-backlog 0` turns it off,
which is what you want when writing a file you intend to keep and not
what you want in front of anything live.

The same applies to `neolink stream`, which has the flag too.

#### Why not shared memory or a virtual camera

Both come up, and neither is better here.

A **shared-memory ring buffer** (`/dev/shm`, mmap) would be the fastest
transport and would give the "always the newest data" behaviour for free.
Nothing can read it: go2rtc has no shared-memory source and ffmpeg has no
demuxer for one, so it would need a shim process to copy the ring into a
pipe — and then the pipe is the interface and the shared memory bought
nothing. It is also solving a cost that is not being paid: a 4 Mbps camera
is 500 KB/s, which is a memcpy and a syscall per frame.

A **virtual video device** (`v4l2loopback`) needs an out-of-tree kernel
module and root on the host, carries raw frames — so the H264 or H265 the
camera already produced has to be decoded and re-encoded — and V4L2 has
nowhere to put the audio.

### Audio and Latency

Reolink cameras send audio either as AAC or as DVI4 ADPCM.

By default neolink decodes the audio and sends raw `L16` samples (RFC 3551).
Every RTSP client understands that, so the default works everywhere without
anyone having to know what their client supports.

For AAC cameras there are faster options. Passthrough sends the compressed
audio straight through: nothing is decoded, resampled or re-encoded, so the
audio adds no codec latency and costs the camera's own bitrate (typically
16-32kbps) instead of the ~256kbps raw 16kHz mono samples need. Two RTP payload
formats carry the same frames, differing only in how RTP frames them:

| `audio_format` | SDP `rtpmap` | RFC | understood by |
| --- | --- | --- | --- |
| `"mpeg4-generic"` | `MPEG4-GENERIC` | 3640, `mode=AAC-hbr` | ffmpeg/ffprobe, VLC, **go2rtc**, Blue Iris |
| `"latm"` | `MP4A-LATM` | 6416 | ffmpeg/ffprobe, VLC, Blue Iris |

Ask for one per camera:

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
audio_format = "mpeg4-generic"   # "pcm" (default), "mpeg4-generic", "latm" or "all"
```

`MPEG4-GENERIC` is the passthrough to reach for: it is what native RTSP
cameras almost universally emit for AAC, so client support is the broadest.
In particular **go2rtc does not understand `MP4A-LATM`** — it identifies AAC
solely by the `MPEG4-GENERIC` rtpmap name, so a LATM track is parsed as an
unknown codec and dropped without an error. If go2rtc (and so Home Assistant
or Frigate) is downstream, use `"mpeg4-generic"` or `"all"`, never `"latm"`.

Neither is the default, because neither is understood by *everything*.

ADPCM cameras have no RTP passthrough format available and are always decoded
to `L16`; `audio_format` has no effect on them.

#### Per-client selection

RTSP has no codec negotiation: the server must commit to the formats in the SDP
before the client has said anything about what it can decode. Two things partly
make up for that.

The first is that a client can ask for a different *resource*, which RTSP does
support. Add `?audio=` to the URL and that client alone gets that format:

```text
rtsp://neolink:8554/Camera01/mainStream                        # the configured format
rtsp://neolink:8554/Camera01/mainStream?audio=mpeg4-generic   # RFC 3640 passthrough, this client only
rtsp://neolink:8554/Camera01/mainStream?audio=latm            # RFC 6416 passthrough, this client only
rtsp://neolink:8554/Camera01/mainStream?audio=pcm             # decoded L16, this client only
rtsp://neolink:8554/Camera01/mainStream?audio=all             # offer every format, this client only
```

It takes the same spellings as the config file, and an unrecognised one is
logged and ignored rather than guessed at. Each client gets its own pipeline,
so two clients can hold different formats on the same camera at the same time —
one NVR on `?audio=mpeg4-generic` and one legacy viewer on the default, say —
and neither needs the config changed.

The second is that a client can set up only the tracks it wants. That is what
`audio_format = "all"` is for: it advertises a `MPEG4-GENERIC` track, a
`MP4A-LATM` track and an `L16` track, in that order, and a client that
negotiates sets up one and ignores the rest.

go2rtc does negotiate — it issues `SETUP` per track, on demand — and it is the
case `all` most clearly helps, because its two outputs want different things
from the same camera: what it muxes into MP4/HLS takes the AAC untouched,
while its WebRTC output cannot decode AAC in any framing and needs the `L16`
track. One mount serves both.

`all` is opt-in because it is only safe for clients that do negotiate. Clients
that do not — `rtspsrc` and others like it — set up every track in the SDP and
so receive all the audio streams at once. Two further costs apply even to a
well-behaved client:

- every branch runs server-side regardless of what is subscribed, so the AAC
  decode that passthrough exists to avoid is paid anyway;
- the decode branch shares the fate of the mount. If the decoder fails on this
  camera's audio it takes the passthrough tracks and the video down with it,
  which a passthrough format on its own would not.

So: leave it alone and everything works; name a single format when you know
your clients; set `all` when one camera serves a mix and the negotiating client
is the one whose latency you care about.

Passthrough is offered only where it can work. It needs MPEG-4 AAC in ADTS
framing, so before serving a stream neolink checks that this camera's own audio
really does reach the payloader; when it does not — a camera sending MPEG-2
AAC, for instance — that track is dropped and the remaining ones are renumbered
so the SDP has no gap, with a warning in the log saying why. This holds however
the format was asked for, in the config or on the URL. The check matters
because an audio format the pipeline cannot negotiate does not merely mute the
stream: it stops the whole RTSP media from being described, taking the video
with it.

The other latency control is `buffer_duration`, which caps how much media the
server-side queues may hold, in milliseconds:

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
buffer_duration = 3000   # default
```

Lowering it reduces how far behind live a client can drift when it briefly
stops keeping up, at the cost of dropping frames sooner on a congested
network. Raising it does the opposite. Values below 50ms are treated as 50ms.

### Limiting the frame rate

`max_fps` caps how many video frames per second neolink forwards to its RTSP
clients. Excess frames are dropped on the way in, before GStreamer sees them,
which reduces both CPU use and outgoing bandwidth. Audio is never throttled.

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
max_fps = 5   # or fps_limit = 5
```

The option is strictly opt-in: omitting it, or setting it to `0`, leaves the
frame path exactly as it is without it — no limiter is constructed and no
per-frame decision is made. Each connected client is decimated independently,
and the frames that do get through keep the camera's own capture timestamps, so
the media timeline stays correct.

#### What the output looks like

neolink drops frames rather than re-encoding the stream, and H.264/H.265
P-frames are predicted from the frames before them. Dropping a frame from the
middle of a group of pictures would leave the frames after it referencing data
that never arrived, so neolink only ever drops a *tail* of each GOP: once a
frame is withheld, the rest are withheld too until the next keyframe restarts
the prediction chain. Everything the client receives is decodable.

Two consequences follow from that:

- **Output is bursty.** Frames arrive as a run at the camera's native rate
  followed by a gap, rather than evenly spaced. Averaged over a keyframe
  interval the rate is `max_fps`; instant to instant it is not. Expect visible
  stutter on a live view.
- **The keyframe rate is a floor.** Keyframes are never dropped, so if your
  camera sends one every 2 s you cannot get below 0.5 fps, and asking for less
  simply yields the keyframe rate. Dropping keyframes would blank the stream
  rather than thin it.

Smoothly spaced output at an arbitrary rate would require transcoding, which
costs far more CPU than this option is meant to save. So `max_fps` is a good
fit for bandwidth ceilings, periodic still grabs and motion snapshots, and a
poor fit for a live view you intend to watch — leave it unset for that.

### Discovery

To connect to a camera using a UID we need to find the IP address of the camera
with that UID

The IP is discovered with four methods

1. Local discovery: Here we send a broadcast on all visible networks asking
   the local network if there is a camera with this UID. This only works if
   the network supports broadcasts

   If you know the ip address you can put it into the `address` field of the
   config and attempt a direct connection without broadcasts. This requires a
   route from neolink to the camera.

2. Remote discovery: Here we ask the reolink servers what the IP address is.
   This requires that we contact reolink and provide some basic information
   like the UID. Once we have this information we connect directly to the
   local IP address. This requires a route from neolink to the camera and
   for the camera to be able to contact reolink.

3. Map discovery: In this case we register our IP address with reolink and ask
   the camera to connect to us. Once the camera either polls/recieves a connect
   request from the reolink servers the camera will initiate a connection
   to neolink. This requires that our IP and reolink are reachable from
   the camera.

4. Relay: In this case we request that reolink relay our connection. Neolink
   nor the camera need to be able to direcly contact each other. But both
   neolink and the camera need to be able to contact reolink.

This can be controlled with the config

```toml
discovery = "local"
```

In the `[[cameras]]` section of the toml.

Possible values are `local`, `remote`, `map`, `relay` later values implictly
enable prior methods.

#### Cellular

Cellular cameras should select `"cellular"` which only enables `map` and
`relay` since `local` and `remote` will always fail

```toml
discovery = "cellular"
```

See the sample config file for more details.

### MQTT

To use mqtt you will need to adjust your config file as such:

```toml
bind = "0.0.0.0"

[mqtt]
broker_addr = "127.0.0.1" # Address of the mqtt server
port = 1883 # mqtt servers port
credentials = ["username", "password"] # mqtt server login details

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
```

Then to start the mqtt+rtsp connection run the following:

```bash
./neolink mqtt-rtsp --config=neolink.toml
```

(If you also set `[onvif].enabled = true` in your config, `mqtt-rtsp` will
spawn the ONVIF bridge alongside MQTT and RTSP automatically. See
[ONVIF](#onvif) below.)

OR for only mqtt

```bash
./neolink mqtt --config=neolink.toml
```

Neolink will publish these messages:

Messages that are prefixed with `neolink/`

- `/status` Tracks the connection of neolink, `connected` for ready `offline`
  for not ready this is a LastWill message
- `/config` The configuration file used to start neolink, you can publish to
  this to **temporarily** alter the live configuration
- `/config/status` If you publish to `/config` then any errors from your
  publish config will show here, or `Ok(())` if no errors and finished loading

Messages that are prefixed with `neolink/{CAMERANAME}`

Control messages:

- `/control/led [on|off]` Turns status LED on/off
- `/control/ir [on|off|auto]` Turn IR lights on/off or automatically via light
  detection
- `/control/reboot` Reboot the camera
- `/control/ptz [up|down|left|right|in|out] (amount)` Control the PTZ
  movements, amount defaults to 32.0
- `/control/ptz/preset [id]` Move the camera to a PTZ preset
- `/control/ptz/assign [id] [name]` Set the current PTZ position to a preset ID
  and name
- `/control/zoom (amount)` Zoom the camera to the specified amount. Example: 1.0
  for normal and 3.5 for 3.5x zoom factor. This only works on cameras that support
  zoom
- `/control/pir [on|off]`
- `/control/floodlight [on|off]` Turns floodlight (if equipped) on/off
- `/control/floodlight_tasks [on|off]` Turns floodlight (if equipped) tasks on/off
  This is the automatic tasks such as on motion and night triggers
- `/control/wakeup (mins)` For cameras that are using `idle_disconnect` this will
  force a wakeup for at least the given minutes
- `/control/siren on` Signal the siren, the message is always "on" as there is no
  "off" signal for the siren

Status Messages:

- `/status disconnected` Sent when the camera goes offline
- `/status/battery` Sent in reply to a `/query/battery` an XML encoded version
  of the battery status
- `/status/battery_level` A simple % value of current battery level, only
  published when `enable_battery` is true in the config
- `/status/pir` Sent in reply to a `/query/pir` an XML encoded version of the
  pir status
- `/status/motion` Contains the motion detection alarm status. `on` for motion
  and `off` for still, only published when `enable_moton` is true in the config
- `/status/ai` A comma separated list of the AI types currently detected, or
  `none`. Only published when `enable_ai` is true in the config
- `/status/ai/{type}` One topic per AI type (`people`, `vehicle`, `dog_cat`,
  `face`, `visitor`, `package`, `cry`), `on` while that type is detected and
  `off` otherwise. Types the camera cannot detect simply stay `unknown`.
  Requires an AI capable camera
- `/status/ptz/preset` Sent in reply to a `/query/ptz/preset` an XML encoded
  version of the PTZ presets
- `/status/preview` a base64 encoded camera image updated every 2s. Not
  every camera supports the snapshot command needed for this. In such cases
  there will be no `/status/preview` message. Only published when
  `enable_preview` is true in the config
- `/status/floodlight_tasks` The current status of the floodlight tasks
  used updated every 2s by default

Query Messages:

- `/query/battery` Request that the camera reports its battery level
- `/query/pir` Request that the camera reports its pir status
- `/query/ptz/preset` Request that the camera reports its PTZ presets
- `/query/preview` Request that the camera post a base64 encoded jpeg
  of the stream to `/status/preview` now, ignoring the timer

### Controlling RTSP from MQTT

If neolink is started with `mqtt-rtsp` then the `/neolink/config` can be used
to control the RTSP

Changes made to the config by publishing to `/neolink/config` should be
reflected in the rtsp

These include changing the:

- Available users

```toml
[[users]]
  name = "me"
  pass = "mepass"
```

- Permitted users on a camera

```toml
[[cameras]]
  permitted_users = [ "me" ]
```

- Available streams

```toml
[[cameras]]
  stream = "Main"
```

Setting a value of `None` will disable the stream

- Disable the entire camera (mqtt updates and all)

```toml
[[cameras]]
  enabled = false
```

### MQTT Disable Features

Certain features like preview and motion detection may not be desired
you can disable them with the following config options.
Disabling these may help to conserve battery

```toml
bind = "0.0.0.0"

[mqtt]
broker_addr = "127.0.0.1" # Address of the mqtt server
port = 1883 # mqtt servers port
credentials = ["username", "password"] # mqtt server login details

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
[cameras.mqtt]
enable_motion = false        # motion detection
                             # (limited battery drain since it
                             # is a passive listening connection)
                             #
enable_ai = false            # per AI type detections in `/status/ai/*`
                             # (no extra battery drain: these arrive on the
                             # same connection as motion detection)
                             #
enable_light = false         # flood lights only available on some camera
                             # (limited battery drain since it
                             # is a passive listening connection)
                             #
enable_battery = false       # battery updates in `/status/battery_level`
                             #
enable_preview = false       # preview image in `/status/preview`
                             #
enable_floodlight = false    # preview image in `/status/floodlight_tasks`
                             #
battery_update = 2000        # Number of ms between `/status/battery_level` updates
                             #
preview_update = 2000        # Number of ms between `/status/preview` updates
                             #
floodlight_update = 2000     # Number of ms between `/status/floodlight_tasks` updates
```

#### MQTT Discovery

[MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery)
is partially supported. Currently, discovery is opt-in and camera features
must be manually specified.

```toml
[cameras.mqtt]
  # <see above>
  [cameras.mqtt.discovery]
  topic = "homeassistant"
  features = ["floodlight"]
```

Available features are:

- `floodlight`: This adds a light control to home assistant
- `camera`: This adds a camera preview to home assistant. It is only updated
  every 0.5s and cannot be much more than that since it is updated over mqtt
  not over RTSP. Not every camera supports the snapshot command needed for
  this. In such cases there will be no `/status/preview` message.
- `led`: This adds a switch to chage the LED status light on/off to home
  assistant
- `ir`: This adds a selection switch to chage the IR light on/off/auto to home
  assistant
- `motion`: This adds a motion detection binary sensor to home assistant
- `ai`: This adds one binary sensor per AI type (person, vehicle, pet, face,
  visitor, package, baby cry) to home assistant. Only meaningful on cameras
  with AI detection; sensors for types the camera cannot detect stay unknown
- `reboot`: This adds a reboot button to home assistant
- `pt`: This adds a selection of buttons to control the pan and tilt of the
  camera
- `battery`: This adds a battery level sensor to home assistant
- `siren`: Adds a siren button to home assistant

### Extra Camera Settings

Listed below are extra camera settings:

```toml
[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
debug = false # Displays Debug XML messages from camera
enabled = true # Enable or Disable the camera
update_time = false # When camera connects, force the setting of the camera date/time to now. The default is false
print_format = "None"  # Type of format that logs are displayed in (None, Human, Xml). The default is None
```

- **Debug:** Will dump the various XMLs from the camera as they are recieved
and decrypted. Leave this off unless asked for it to fix an issue.

- **Enabled:** Useful if you want to remove a camera from rtsp without deleting
it from the config

- **update_time:** Used to FORCE an update on the camera time. Usually it checks
if it is needed but this
will force it regardless. (Mostly this was introduced to address a specific
ssue a user had)

- **print_format:** Used for adjusting printing of some values mostly, battery
messages

### Pause

To use the pause feature you will need to adjust your config file as such:

```toml
bind = "0.0.0.0"

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
  [cameras.pause]
  on_motion = true # Should pause when no motion
  on_client = true # Should pause when no rtsp client
  timeout = 2.1 # How long to wait after motion stops before pausing
```

Then start the rtsp server as usual:

```bash
./neolink rtsp --config=neolink.toml
```

### Idle Disconnects

To really save battery we need to disconnect the camera when it is idle.

To acheieve this you can add `idle_disconnect = true` to the `[[cameras]]`
section

```toml
bind = "0.0.0.0"

[[cameras]]
name = "Camera01"
username = "admin"
password = "password"
uid = "ABCDEF0123456789"
idle_disconnect = true
[cameras.pause]
  on_client = true # Should pause when no rtsp client
  timeout = 2.1 # How long to wait after motion stops before pausing
```

When `idle_disconnect = true` neolink will disconnect from the camera 30s
after it stops being used.

Neolink considers it as being used if there is an active stream running, or
if there is motion being detected or an mqtt command being run

[Because google remove the api for the push notifications we cannot
reliably use push notifications to wake up, so motion won't wake
up neolink anymore]

You can make neolink stop active streams when there are no rtsp clients using

```toml
[cameras.pause]
  on_client = true # Should pause when no rtsp client
```

Once in the disconnected state. Neolink will stay disconnected until there is a
new requested activation such as a client connecting or an mqtt command

~Neolink will also wake up on push notifications from the camera. These are usually
sent by the camera on motion or PIR alarms. To disable this you can set
`push_notifications = false` in the `[[cameras]]` config~

[Google removed the apis we were using for push notifications]

### Docker

[Docker](https://hub.docker.com/r/quantumentangledandy/neolink) builds are also
provided in multiple architectures. The latest tag tracks master while each
branch gets it's own tag.

```bash
docker pull quantumentangledandy/neolink

# Add `-e "RUST_LOG=debug"` to run with debug logs
#
# --network host is only needed if you require to connect
# via local broadcasts. If you can connect via any other
# method then normal bridge mode should work fine
# and you can ommit this option. Not all OSes support
# network=host, notably macos lacks this option.
docker run --network host --volume=$PWD/config.toml:/etc/neolink.toml quantumentangledandy/neolink
```

#### Environmental Variables

There are currently 2 environmental variables available as part of the container:

- `NEO_LINK_MODE`: defaults to `"rtsp"` if not set, other options are "mqtt" or "mqtt-rtsp".
- `NEO_LINK_PORT`: defaults to `8554`, set this to your required port value.

### ONVIF

Neolink can expose each configured Reolink camera as a virtual ONVIF Profile S
device. Standard VMS clients (Home Assistant, Frigate, BlueIris, Synology
Surveillance Station, Agent DVR, ONVIF Device Manager, ...) can then:

- discover the camera automatically over WS-Discovery (UDP multicast)
- read its RTSP and JPEG-snapshot URLs without manual configuration
- drive PTZ (continuous and relative pan/tilt, absolute & continuous zoom, presets)

Each enabled camera becomes its own ONVIF device under
`http://<host>:<onvif_port>/onvif/<camera-name>/...`. PTZ commands are
translated to the same Reolink calls the CLI and MQTT surfaces already use,
so no extra protocol code runs against the camera.

To turn it on, add an `[onvif]` block to your config:

```toml
[onvif]
enabled = true
bind = "0.0.0.0"
bind_port = 8000              # SOAP + snapshot HTTP server
discovery = true              # answer multicast WS-Discovery probes
advertise_host = "auto"       # "auto" picks the first non-loopback IPv4 NIC;
                              # set to your LAN IP if auto-detection picks wrong
```

Then either run ONVIF on its own:

```bash
./neolink onvif --config=neolink.toml
```

…or, more commonly, alongside the other services with the combined launcher
(which now spawns RTSP, MQTT, **and** ONVIF in one process):

```bash
./neolink mqtt-rtsp --config=neolink.toml
```

#### Per-camera ONVIF options

Each camera can opt out, or pin a stable UUID:

```toml
[[cameras]]
name = "driveway"
# ... usual camera fields
  [cameras.onvif]
  enabled = true            # default true when the global [onvif] is enabled
  uuid    = "auto"          # "auto" → UUIDv5 derived from name + UID
                            # or supply a fixed UUID for explicit identity
```

#### What's exposed

| ONVIF service | Operations |
|---|---|
| Device | `GetDeviceInformation`, `GetSystemDateAndTime`, `GetCapabilities`, `GetServices`, `GetServiceCapabilities`, `GetHostname`, `GetScopes` |
| Media  | `GetProfiles`, `GetProfile`, `GetStreamUri`, `GetSnapshotUri`, `GetVideoSources`, `GetVideoEncoderConfigurations` |
| PTZ    | `GetNodes`, `GetConfigurations`, `GetConfigurationOptions`, `ContinuousMove`, `RelativeMove`, `AbsoluteMove` (zoom only), `Stop`, `GetStatus`, `GetPresets`, `GotoPreset`, `SetPreset`, `GotoHomePosition`, `SetHomePosition` |
| Events | `GetEventProperties`, `CreatePullPointSubscription`, `Subscribe`, `PullMessages`, `Renew`, `Unsubscribe` |

The Events service publishes the same topics a Reolink camera publishes
natively, so a VMS sees the bridge as it would see the camera:

| Topic | Meaning |
|---|---|
| `tns1:VideoSource/MotionAlarm` | Motion |
| `tns1:RuleEngine/MyRuleDetector/PeopleDetect` | Person detected |
| `tns1:RuleEngine/MyRuleDetector/VehicleDetect` | Vehicle detected |
| `tns1:RuleEngine/MyRuleDetector/DogCatDetect` | Pet detected |
| `tns1:RuleEngine/MyRuleDetector/FaceDetect` | Face detected |
| `tns1:RuleEngine/MyRuleDetector/Visitor` | Doorbell press |
| `tns1:RuleEngine/MyRuleDetector/Package` | Package detected |
| `tns1:RuleEngine/FieldDetector/ObjectsInside` | A smart-AI zone triggered; the `Rule` item names the detector (`crossline`, `intrusion`, `loitering`, `legacy`, `loss`) |
| `tns1:AudioAnalytics/Audio/DetectedSound` | Baby cry detected |

These are the topics Home Assistant's ONVIF integration parses, so AI
detections show up as binary sensors without any extra configuration.

#### Capabilities match the camera

What the bridge advertises is probed from the camera, not assumed. On first
use (and every five minutes after) each camera is asked for its `Support`
table, its ability list and its zoom range, and the answer decides what every
description-shaped response says:

| If the camera has… | …then |
|---|---|
| no PTZ at all (fixed mount, no zoom, or an account whose PTZ `control` permission is missing or read-only) | `GetCapabilities`/`GetServices` list no PTZ address, `GetScopes` and WS-Discovery drop `onvif://www.onvif.org/type/ptz`, and media profiles carry no `PTZConfiguration` |
| pan/tilt but no optical zoom (E1 Pro, most pan/tilt models) | the PTZ node advertises only the continuous pan/tilt space; zoom moves return `ter:NoContinuousZoomSpace` / `ter:NoRelativeZoomSpace` / `ter:NoAbsoluteZoomSpace` |
| optical zoom but no pan/tilt | the node advertises only the zoom spaces; pan/tilt moves return `ter:NoContinuousPanTiltSpace` / `ter:NoRelativePanTiltSpace` |
| no preset support | `MaximumNumberOfPresets` is `0`, `HomeSupported` is `false`, and `GotoPreset`/`SetPreset`/`GotoHomePosition`/`SetHomePosition` return `ter:ActionNotSupported` |
| presets the logged-in account may recall but not store (a `preset_ro` grant) | presets and home still work for *going to* a position; the node reports `FixedHomePosition="true"`, `SetPreset` returns `ter:ActionNotSupported` and `SetHomePosition` returns `ter:CannotOverwriteHome` |

The home position is preset 0 — the Reolink protocol has no separate home slot —
so `SetHomePosition` writes that preset, keeping whatever name the slot already
had. `SetPreset` without a token allocates from preset 1 upwards, so saving a
new preset never silently moves where the home button goes; slot 0 is still
handed out once every other slot is full, and `SetPreset` reports
`ter:TooManyPresets` when the table is genuinely full.

`GotoHomePosition` before anything has been saved to slot 0 returns
`ter:NoHomePosition` rather than a bare protocol error, so a client can tell
"no home saved yet" apart from "the camera refused".

The point is that a client only offers controls that work: Home Assistant
draws its PTZ pad from the profile's `PTZConfiguration`, and Frigate decides
whether to show presets from `MaximumNumberOfPresets`.

Detection errs towards keeping a capability. A camera that is offline, or a
firmware that omits one of these fields, leaves that signal *unknown*, and
unknown means "advertise it" — the same behaviour as before this existed. A
capability is only withdrawn on positive evidence that the camera lacks it.
Run with `RUST_LOG=neolink::onvif=debug` to see what was probed and decided.

`GetSnapshotUri` returns `http://<host>:<onvif_port>/onvif/<camera>/snapshot/<stream>`;
that URL serves a JPEG produced by Reolink's `SNAP` command (HTTP Basic auth,
using the same `[[users]]` table).

`GetStreamUri` returns the existing neolink RTSP URL, so the ONVIF client
ends up streaming through the regular RTSP server.

#### Authentication

ONVIF SOAP uses **WS-UsernameToken** (`PasswordText` or `PasswordDigest`); the
snapshot HTTP endpoint uses **HTTP Basic**. Both consult the same global
`[[users]]` table that RTSP uses, and honour each camera's `permitted_users`
list. A handful of operations are reachable without auth as required by the
ONVIF spec: `GetSystemDateAndTime`, `GetCapabilities`, `GetServices`,
`GetServiceCapabilities`.

#### Limitations

- **No absolute pan/tilt**: the underlying Reolink protocol doesn't expose
  absolute PT coordinates, so `AbsoluteMove` for pan/tilt returns the
  standard `ter:NoAbsolutePTZSpace` fault. Profile S explicitly allows this
  on continuous-only devices; `RelativeMove` and `ContinuousMove` work.
- **PullPoint events only.** Motion and AI detections are delivered through
  PullPoint subscriptions, which every VMS we target supports. Push
  (`NotificationConsumer`) subscriptions, an optional part of Profile T, are
  not implemented. The client-supplied topic `Filter` is ignored: like a real
  camera, every subscription receives every topic.
- **No HTTPS yet** on the ONVIF port. Run behind a TLS-terminating reverse
  proxy if you need it.

#### Deployment — file descriptor limit

Neolink keeps an open TCP connection per active RTSP client and may serve
many short-lived ONVIF HTTP requests under load. The default per-process
file-descriptor limit on most distros (1024) can be exhausted, after which
`accept error: Too many open files` floods the log and the bridge stops
serving new connections.

For systemd deployments, raise it with a drop-in:

```ini
# /etc/systemd/system/neolink.service.d/limits.conf
[Service]
LimitNOFILE=65536
```

For Docker / Kubernetes set `--ulimit nofile=65536:65536` (or the
equivalent `securityContext`).

### Image

You can write an image from the stream to disk using:

```bash
neolink image --config=config.toml --file-path=filepath CameraName
```

Where filepath is the path to save the image to and CameraName is the name of
the camera from the config to save the image from.

File is always jpeg and the extension given in filepath will be added or changed
to reflect this.

Some cameras do not support the SNAP command that is used to generate the image
on the camera. If this is the case with your camera you can try the
`--use-stream` option which will instead create a jpeg by transcoding the video
stream.

### Battery Levels

You can get the battery level and status using

```bash
neolink battery --config=config.toml CameraName
```

This will produce an xml formatted battery status on stdout for processing

### AI Detection

Second generation Reolink cameras (Duo 3, TrackMix, CX series, Argus Eco
Ultra, ...) expose their AI detectors over the Baichuan protocol. The `ai`
subcommand reads and configures them.

```bash
# Dump the configured smart-AI zones (all five detectors, or name one)
neolink ai --config=config.toml CameraName zones
neolink ai --config=config.toml CameraName zones intrusion

# Read, then change, the alarm settings of one AI type
neolink ai --config=config.toml CameraName alarm people
neolink ai --config=config.toml CameraName alarm people --sensitivity 60

# Read and set the baby-cry detection sensitivity
neolink ai --config=config.toml CameraName cry
neolink ai --config=config.toml CameraName cry 50

# Dump the AiCfg block (auto tracking plus cry detection)
neolink ai --config=config.toml CameraName cfg

# Follow detections live
neolink ai --config=config.toml CameraName watch
```

Detections themselves do not need this subcommand: they are published to
MQTT under `/status/ai/*` and over ONVIF as the `tns1:RuleEngine/...` topics,
both described above.

The detection *zones* are read-only for now. Reolink expects the whole zone
container to be written back, and the zone geometry it contains has not been
captured from a real camera yet — writing a partially understood container
could erase a configured detection line or area.

### PIR

You can control pir using

```bash
neolink pir --config=config.toml CameraName [on|off]
```

This will turn the PIR on or off

### Reboot

You can reboot a camera using

```bash
neolink reboot --config=config.toml CameraName
```

### Status LED

You can control the status LED using

```bash
neolink status-light --config=config.toml CameraName [on|off]
```

### Talk

You can talk over the camera using

```bash
neolink talk --config=config.toml --adpcm-file=data.adpc\
               --sample-rate=16000 --block-size=512 CameraName
```

Where the sounds is ADPCM encoded

or

```bash
neolink talk --config=config.toml --microphone  CameraName
```

Which uses the default microphone which depends on
[gstreamer](https://gstreamer.freedesktop.org/documentation/autodetect/autoaudiosrc.html?gi-language=c#autoaudiosrc-page)

### PTZ

You can control the PTZ using

```bash
neolink ptz --config=config.toml CameraName control 32 [left|right|up|down|in|out]
```

Where 32 is the speed. Not all cameras support speed

Some cameras also support preset positions

```bash
# Print the list of preset positions
neolink ptz --config=config.toml CameraName preset
# Move the camera to preset ID 0
neolink ptz --config=config.toml CameraName preset 0
# Save the current position as preset ID 0 with name PresetName
neolink ptz --config=config.toml CameraName assign 0 PresetName
```

To change the zoom level use the following:

```bash
# Zoom the camera to 2.5x
neolink ptz --config=config.toml CameraName zoom 2.5
```

With 1.0 being normal and 2.5 being 2.5x zoom

## License

Neolink is free software, released under the GNU Affero General Public License
v3.

This means that if you incorporate it into a piece of software available over
the network, you must offer that software's source code to your users.

## Donations

If you find this code helpful please consider supporting development.

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/G2G5HOYIZ)
