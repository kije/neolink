# Neolink on the Raspberry Pi 5 — compatibility & performance investigation

Status: investigation only. Nothing in this document has been applied to the
tree; each item lists the exact change it would need. Everything marked
**[verified]** was confirmed by reading the sources in this repo or in the
vendored crate sources under `~/.cargo/registry`. Everything marked
**[verify on device]** is a hardware/distro claim that should be confirmed on
an actual Pi 5 before acting on it.

---

## 0. Executive summary

The headline result is a negative one, and it should shape everything else:
**for its main job — `neolink rtsp` — neolink never touches a video codec.**
Frames arrive from the camera already H.264/H.265-encoded, are parsed,
re-timestamped and handed to `rtph264pay`/`rtph265pay`. There is no decode, no
encode, no scale, no colour conversion on the streaming path
(`src/rtsp/factory.rs:771-861`) **[verified]**. So the Pi 5's video block —
and the fact that it lost the H.264 hardware codec its predecessor had — is
*almost irrelevant* to neolink. Chasing "hardware acceleration" here would be
chasing a cost neolink does not pay.

What actually costs CPU on a Pi 5 is: per-packet allocation and memcpy,
AES-CFB decryption (some of it provably wasted), atomics/`Arc` traffic in the
tokio plumbing, and the fact that each RTSP client gets its own private
pipeline *and its own camera connection*.

Ranked by expected value:

| # | Item | Kind | Impact | Risk |
|---|------|------|--------|------|
| 1 | jemalloc built for 4 KB pages aborts on the Pi 5's stock 16 KB-page kernel | **Compatibility — hard startup failure** | Binary does not run at all | None; build-flag only |
| 2 | Wasted AES pass + double copy on every media packet (`bc/de.rs:180`) | Performance | Removes one alloc + one AES pass + one memcpy *per packet* | Low; pure refactor |
| 3 | `aes` crate falls back to the slow software backend on aarch64 | Performance | ~5-10× on AES throughput for `FullAes` cameras | None; runtime-detected |
| 4 | Generic `aarch64` codegen: no inline LSE atomics, no `+crypto`, no LTO | Performance | Broad, few % across the whole binary | Medium; needs a Pi-5-specific artifact |
| 5 | `set_shared(false)`: N RTSP clients ⇒ N camera connections | Scalability | Linear CPU + camera-bandwidth multiplier | Medium; changes semantics |
| 6 | HEVC hardware decode for snapshots only | Hardware accel | Only helps `neolink image` / ONVIF snapshot polling | Low |
| 7 | Non-vectorizable XOR loops in the legacy ciphers | Performance | Small — these are not on the media path | Low |

---

## 1. What a Raspberry Pi 5 actually is

BCM2712, and it differs from the Pi 4 in ways that matter here.

### 1.1 CPU **[verify on device]**

* 4 × **Cortex-A76** @ 2.4 GHz, **ARMv8.2-A**, out-of-order, 64-bit only.
* **NEON** — 128-bit ASIMD, baseline and always available on aarch64.
* **No SVE/SVE2.** The A76 predates it. Anything written against SVE is dead
  code on this machine.
* **Crypto extensions present**: `AES` (AESE/AESD/AESMC/AESIMC), `PMULL`,
  `SHA1`, `SHA2`, plus **`CRC32`**.
* **LSE atomics** (ARMv8.1 `CAS`/`LDADD`/`SWP`) — mandatory from 8.1, so
  present.
* Also: `RDM`, **`DotProd`** (8.2 SDOT/UDOT), **full FP16** arithmetic,
  `RCPC` (8.3 load-acquire), `SSBS`.
* Caches: 64 KB L1I + 64 KB L1D per core, 512 KB L2 per core, 2 MB shared L3.
  Much larger than the Pi 4's, which changes what "a big memcpy" costs.

Compare Pi 4 (Cortex-A72, ARMv8.0-A): crypto extensions and CRC32 yes, **LSE
no**, DotProd no, FP16 no. That asymmetry is why a Pi-5-tuned build cannot be
shipped as *the* aarch64 artifact — see §5.2.

### 1.2 Memory pages — the important one **[verify on device]**

Raspberry Pi OS (Bookworm and later, arm64) boots the Pi 5 with
`kernel_2712.img`, which is built **`CONFIG_ARM64_16K_PAGES`** — a **16 KB**
page size. The Pi 4 and everything before it use 4 KB pages. `getconf
PAGESIZE` on a stock Pi 5 returns `16384`.

This single fact is the root of finding #1 and is the only thing in this
document that can stop neolink from starting at all.

### 1.3 Video hardware **[verify on device]**

This is where the Pi 5 *regressed* relative to the Pi 4:

| Function | Pi 4 (BCM2711) | Pi 5 (BCM2712) |
|---|---|---|
| H.264 decode | hardware (1080p60) | **none — software only** |
| H.264 encode | hardware (1080p30) | **none** |
| HEVC/H.265 decode | none | **hardware, 4Kp60** (`rpivid`, V4L2 stateless) |
| HEVC encode | none | none |
| JPEG encode/decode | firmware (MMAL) | **none** |
| MMAL / OpenMAX / `bcm_host` firmware codec stack | present | **removed entirely** |

The Pi 5 has exactly one video accelerator: an HEVC **decoder**. There is no
video **encoder** of any kind. The A76 cores are expected to do H.264 decode
and all encoding in software — which is fine, because they are roughly 2-3×
an A72 per clock.

Practical consequences for neolink:

* `v4l2h264dec`, `omxh264dec`, `omxh264enc`, `v4l2h264enc` — **do not exist**
  on a Pi 5. Any advice on the internet telling you to use them is Pi-4 advice.
* The HEVC decoder is exposed as a **V4L2 stateless** decoder (the `rpivid`
  driver, typically `/dev/video19`). In GStreamer that means the `v4l2codecs`
  plugin's **`v4l2slh265dec`**, not the older `v4l2h265dec` M2M element.
  Confirm with `gst-inspect-1.0 v4l2slh265dec`.
* The RP1 southbridge carries GbE and USB3 over PCIe Gen2 ×4 — networking is
  genuinely full gigabit, unlike the Pi 3's USB-attached NIC.

---

## 2. Compatibility

### 2.1 Finding #1 — jemalloc aborts on the 16 KB-page kernel

**This is a hard startup failure, not a slowdown.**

`src/main.rs:24-30` installs jemalloc as the global allocator on every
non-MSVC target **[verified]**:

```rust
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;
```

jemalloc bakes its page size in **at compile time** as `LG_PAGE`. At startup
`pages_boot()` compares the real page size against it and, if the system page
is *larger*, gives up:

```c
if (os_page > PAGE) {
    malloc_write("<jemalloc>: Unsupported system page size\n");
    ...
}
```

A failed jemalloc init means every allocation returns NULL, and the process
dies immediately.

`tikv-jemalloc-sys 0.5.4`'s `build.rs` special-cases **only iOS** for this:

```rust
if target.contains("ios") {
    // newer iOS deviced have 16kb page sizes:
    cmd.arg("--with-lg-page=14");
}
```

**[verified]** — line 180 of the vendored `build.rs`. Every other target,
including `aarch64-unknown-linux-gnu`, gets whatever autoconf detects on the
*build host*. Our aarch64 artifacts are cross-compiled from an x86_64 GitHub
runner (`.github/workflows/build.yml`, the `cross` job) **[verified]**, so
`LG_PAGE` comes out as **12 (4 KB)**.

Result: **the published `linux/arm64` neolink binary and Docker image cannot
start on a stock Raspberry Pi 5.** The user-visible symptom is a one-line
`<jemalloc>: Unsupported system page size` and an immediate exit, which looks
nothing like a configuration problem and is very hard to self-diagnose.

Users who hit this today work around it by forcing the old 4 KB kernel in
`/boot/firmware/config.txt`:

```ini
kernel=kernel8.img
```

…which costs them the Pi 5's tuned kernel. We should not require that.

**Fix.** Build aarch64 with 16 KB pages. jemalloc explicitly supports the
`os_page < PAGE` direction, so a `--with-lg-page=14` build runs correctly on
both 16 KB (Pi 5) *and* 4 KB (Pi 4, generic servers) systems — the cost on 4 KB
systems is slightly coarser arena granularity, not a failure.

The knob is an environment variable read by the build script
(`JEMALLOC_SYS_WITH_LG_PAGE`, line 218 **[verified]**), so it has to be set
where the build is invoked. Note it cannot go in `.cargo/config.toml`'s
`[env]` table, because that table is not target-conditional.

`.github/workflows/build.yml`, in the `cross` job's build step:

```yaml
      - name: Build
        env:
          # jemalloc bakes the page size in at compile time and only
          # special-cases iOS for 16 KB pages. The Raspberry Pi 5 boots a
          # 16 KB-page kernel by default, so a 4 KB-page jemalloc aborts at
          # startup with "Unsupported system page size". 14 (= 16 KB) works
          # on both 16 KB and 4 KB systems; 12 only works on 4 KB.
          JEMALLOC_SYS_WITH_LG_PAGE: ${{ matrix.target == 'aarch64-unknown-linux-gnu' && '14' || '' }}
```

and the same for the `linux/arm64` leg of the Dockerfile's from-scratch build:

```dockerfile
    if [ "${TARGETPLATFORM}" = "linux/arm64" ]; then \
      export JEMALLOC_SYS_WITH_LG_PAGE=14; \
    fi; \
    cargo build --release;
```

An alternative worth considering on its own merits: **drop jemalloc on
Linux/aarch64 entirely.** glibc's malloc has closed much of the gap, neolink is
not allocation-throughput-bound in the way a database is, and this whole class
of bug disappears. That is a bigger decision than this document should make,
but it should be on the table — the jemalloc dependency is currently buying us
an unquantified benefit in exchange for a known hard failure.

**Verification:** on a Pi 5, `getconf PAGESIZE` should print `16384`; the
binary should then start and `neolink --version` should print normally.

### 2.2 Architecture and distro

* **Use the arm64 build.** The `armv7-unknown-linux-gnueabihf` artifact is for
  32-bit userlands; there is no reason to run one on a Pi 5 and it forfeits
  half the register file. **[verify on device]** whether current 32-bit
  Raspberry Pi OS even supports the Pi 5 — if it does not, we could consider
  documenting armv7 as Pi-4-and-older.
* GStreamer on Bookworm is 1.22, on Trixie 1.26. The crates pin the `v1_20`
  feature as a floor (`Cargo.toml`) **[verified]**, so both are fine.
* The Docker runtime image installs `gstreamer1.0-plugins-{base,good,bad}` and
  `gstreamer1.0-libav` **[verified]**, which covers `h264parse`/`h265parse`
  (bad), the RTP payloaders (good), and `avdec_aac` (libav). A bare-metal Pi
  install needs the same set — that is worth stating explicitly in
  `docs/unix_setup.md`.

### 2.3 Thermals and power — do not skip these **[verify on device]**

A Pi 5 under sustained multi-camera load *will* throttle without active
cooling; it soft-throttles around 80 °C and hard-throttles at 85 °C. It also
needs a 5 V/5 A USB-PD supply for the full peripheral budget — an undersized
supply throttles the CPU and looks exactly like a software performance
problem.

`vcgencmd get_throttled` returning anything other than `throttled=0x0` means
**every benchmark taken on that machine is invalid**. This belongs at the top
of any Pi 5 performance checklist, ours included.

---

## 3. Where neolink actually spends CPU

Traced by reading the path a single video frame takes, from socket to RTP.

```
TCP/UDP socket
  └─ BcCodex::decode                       crates/core/src/bc/codex.rs
       └─ bc_modern_msg                    crates/core/src/bc/de.rs:75
            ├─ decrypt(ext_buf)            → Vec alloc + AES/XOR
            ├─ decrypt(payload_buf)        → Vec alloc + AES   ← often WASTED (§4.1)
            └─ payload_buf.to_vec()        → Vec alloc + memcpy
  └─ BcMediaCodex::decode                  crates/core/src/bcmedia/codex.rs
       └─ bcmedia_iframe / _pframe         crates/core/src/bcmedia/de.rs:209,244
            └─ data_slice.to_vec()         → Vec alloc + memcpy
  └─ mpsc::channel(100)                    src/common/instance/gst.rs:198
  └─ mpsc → blocking frame-pump thread
  └─ send_to_appsrc                        src/rtsp/factory.rs:585
       └─ Buffer::from_mut_slice(data)     → ZERO copy (good — already fixed)
  └─ h264parse ! rtph264pay                → GStreamer, software, unavoidable
```

**[verified]** for every line. Observations:

1. **Three heap allocations and at least two full memcpys of the frame payload
   per packet**, before GStreamer sees it. The final hand-off is already
   zero-copy and correctly commented as such (`factory.rs:618-627`) — the
   remaining copies are upstream of it, in the protocol crate.
2. **No codec work at all.** Confirmed by reading `pipe_h264`/`pipe_h265`
   (`factory.rs:771`, `816`): `appsrc ! queue ! h264parse ! rtph264pay`. This
   is the point made in §0.
3. The only decode in the whole program is `decodebin` in the snapshot path
   (`src/image/gst.rs:185,195`) and the audio paths (`avdec_aac`/`faad` at
   `factory.rs:932-934`, `decodebin` for ADPCM at `factory.rs:1068`).
4. ADPCM→PCM is a hand-written nibble-at-a-time loop in Rust
   (`src/rtsp/adpcm.rs:135-240`). It is inherently serial (each sample depends
   on the previous predictor state) so it cannot be vectorized, but it is also
   tiny — 8 kHz mono. Not worth touching.

---

## 4. Concrete code-level findings

### 4.1 Finding #2 — a wasted AES pass and a wasted copy on every media packet

`crates/core/src/bc/de.rs:179-193` **[verified]**:

```rust
let processed_payload_buf =
    encryption_protocol.decrypt(header.channel_id as u32, payload_buf);
if context.in_bin_mode.contains(&(header.msg_num)) || in_binary {
    payload = match (context.get_encrypted(), encrypted_len) {
        (EncryptionProtocol::FullAes { .. }, Some(encrypted_len)) => {
            Some(BcPayloads::Binary(
                processed_payload_buf[0..(encrypted_len as usize)].to_vec(),
            ))
        }
        _ => Some(BcPayloads::Binary(payload_buf.to_vec())),   // ← discards it
    };
} else {
    ...BcXml::try_parse(processed_payload_buf.as_slice())...
}
```

The decrypt is **unconditional**, but the binary branch only *uses* its result
for `FullAes` cameras. For an ordinary `Aes` camera — the common case, where
control messages are encrypted but the media stream is not — every single
media packet is run through a full AES-CFB pass into a freshly allocated
`Vec`, and that `Vec` is then thrown away in favour of `payload_buf.to_vec()`.

And in the `FullAes` branch that *does* use it, the slice is copied **again**
into a second `Vec`.

So per media packet we currently pay:

* `Aes` camera: 1 wasted allocation + 1 wasted AES-CFB pass over the payload +
  1 real copy.
* `FullAes` camera: 1 allocation + 1 AES pass (needed) + 1 **extra** copy.

Making the decrypt lazy fixes both. Sketch:

```rust
// Only the FullAes binary case and the XML case actually consume the
// decrypted bytes; for every other binary case the plaintext is
// `payload_buf` itself, so decrypting it is pure waste on the hottest
// path in the program.
if context.in_bin_mode.contains(&(header.msg_num)) || in_binary {
    payload = match (context.get_encrypted(), encrypted_len) {
        (EncryptionProtocol::FullAes { .. }, Some(encrypted_len)) => {
            let mut decrypted =
                encryption_protocol.decrypt(header.channel_id as u32, payload_buf);
            decrypted.truncate(encrypted_len as usize);
            Some(BcPayloads::Binary(decrypted))          // no second copy
        }
        _ => Some(BcPayloads::Binary(payload_buf.to_vec())),
    };
} else {
    let processed_payload_buf =
        encryption_protocol.decrypt(header.channel_id as u32, payload_buf);
    ...
}
```

`truncate` replaces `[0..n].to_vec()`, killing the extra allocation and copy in
the `FullAes` path too.

This is a pure win on every platform; it just shows up hardest on a Pi because
the Pi has the least CPU to waste. It is also the change I would make first,
because it costs nothing and needs no new build configuration.

**Verification:** `cargo test -p neolink_core` covers the crypto round-trips
(`bc/crypto.rs:109-121`) and the `bc` deserializer samples; the behaviour of
both branches is unchanged by construction.

### 4.2 Finding #3 — the `aes` crate is running its *software* backend

`crates/core/Cargo.toml` uses `aes = "0.8.4"` + `cfb-mode` **[verified]**.

In `aes` 0.8.4, the ARMv8 crypto-extension backend is behind a **cfg flag that
is off by default** (`src/lib.rs:134`) **[verified]**:

```rust
if #[cfg(all(target_arch = "aarch64", aes_armv8, not(aes_force_soft)))] {
```

Without `--cfg aes_armv8`, aarch64 gets `soft.rs` — the constant-time
fixsliced implementation. That is a good portable fallback and a poor use of a
CPU that has AESE/AESD in hardware.

Crucially, the flag is **safe to enable unconditionally**: with it set, `aes`
uses `autodetect.rs`, which runtime-detects via the `cpufeatures` crate
(`cpufeatures::new!(aes_intrinsics, "aes")`, `autodetect.rs:19`) and falls back
to `soft` on CPUs without the extension. The intrinsics carry their own
`#[target_feature(enable = "aes")]` annotations (`armv8/encdec.rs:14`), so **no
global `-C target-feature=+aes` is required** and the resulting binary still
runs on any aarch64 machine. **[verified]** on all four points.

The change goes in `.cargo/config.toml` — **but note the trap**: Cargo does
*not* merge `target.<triple>.rustflags` with `build.rustflags`; the
target-specific table wins outright. The existing
`[target.x86_64-pc-windows-msvc]` entry already has to repeat `--cfg
tokio_unstable` for exactly this reason **[verified]**. So:

```toml
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
# `aes` 0.8 hides its ARMv8 crypto-extension backend behind this cfg and
# defaults to the (much slower) constant-time software implementation
# without it. The backend is runtime-detected via `cpufeatures` and carries
# its own `#[target_feature]`, so this stays safe on aarch64 CPUs that lack
# the extension.
#
# NOTE: target-specific rustflags REPLACE `build.rustflags` rather than
# merging with them, so `tokio_unstable` has to be repeated here.
rustflags = ["--cfg", "tokio_unstable", "--cfg", "aes_armv8"]
```

The same applies to `aarch64-apple-darwin` if we ever ship it.

**How much is this worth?** Be honest about the magnitude. AES-CFB *decrypt*
is parallelizable, so both backends get to batch: the fixsliced software path
lands somewhere around 8-15 cycles/byte and the hardware path around 1-2, so
call it 5-10×. But at a typical 8 Mbit/s camera stream that is ~1 MB/s, i.e.
moving from a fraction of a percent of one core to a smaller fraction. It only
becomes visible with `FullAes` cameras at high bitrate, several at once. It
costs nothing, so it is still worth doing — just do not expect it to be the
change anyone notices. Finding #2 is the bigger one.

### 4.3 Finding #7 — the legacy XOR ciphers cannot vectorize

Two hand-rolled stream ciphers use `Iterator::cycle()` over the key:

`crates/core/src/bc/crypto.rs:68-74` (`BCEncrypt`) **[verified]**:

```rust
let key_iter = XML_KEY.iter().cycle().skip(offset as usize % 8);
key_iter.zip(buf).map(|(key, i)| *i ^ key ^ (offset as u8)).collect()
```

`crates/core/src/bcudp/xml_crypto.rs:5-11` **[verified]**:

```rust
let key = XML_KEY.iter().flat_map(|i| (i + offset).to_le_bytes()).cycle();
buf.iter().zip(key).map(|(byte, key)| key ^ byte).collect()
```

LLVM cannot auto-vectorize a `cycle()`/`flat_map` chain — the loop-carried
iterator state defeats it — so these compile to byte-at-a-time scalar XOR.
Materializing the repeating key block once and XOR-ing in fixed-size chunks
lets LLVM emit NEON `EOR v0.16b`:

```rust
// Materialise one period of the keystream so the XOR is a flat slice-vs-slice
// loop that LLVM can turn into NEON `EOR`, instead of a `cycle()` chain whose
// loop-carried iterator state blocks vectorisation.
let mut key = [0u8; 8];
for (i, k) in key.iter_mut().enumerate() {
    *k = XML_KEY[(offset as usize + i) % 8] ^ (offset as u8);
}
buf.iter().enumerate().map(|(i, b)| b ^ key[i % 8]).collect()
```

(with the `% 8` hoisted by chunking at 8 bytes, or 64 for a wider inner loop).

**Worth being clear about the payoff: it is small.** `BCEncrypt` is a legacy
pre-2021 camera path, and `bcudp::xml_crypto` only covers UDP *discovery and
negotiation* XML — `udp_data`'s payload is passed through untouched
(`bcudp/de.rs:127-138`) **[verified]**, so it is not on the media path. This is
a tidy-up, not a fix. Listed for completeness because the question asked about
vectorization specifically.

### 4.4 The CFB state is cloned per call

`bc/crypto.rs:79` and `:99` **[verified]**:

```rust
dec.clone().decrypt(&mut decrypted);
```

Each call clones the full CFB decryptor (key schedule + IV) so that every
packet restarts from the fixed IV. That is required by the protocol — packets
are independently decryptable — so the clone is semantically necessary. But it
does mean re-deriving nothing (the schedule is copied, not recomputed), plus
`buf.to_vec()` before decrypting in place. An in-place API taking `&mut [u8]`
would remove the allocation:

```rust
pub fn decrypt_in_place(&self, offset: u32, buf: &mut [u8]) { ... }
```

Low priority on its own; becomes free to do at the same time as §4.1.

### 4.5 Finding #5 — every RTSP client opens its own camera connection

`src/rtsp/gst/factory.rs:38` **[verified]**:

```rust
factory.set_shared(false);
```

With `shared = false`, `RTSPMediaFactory` constructs a **new media (a whole new
pipeline) per client**. Following it through: each pipeline calls
`NeoInstance::stream()` (`src/common/instance/gst.rs:191-198`), which spawns
its own `start_video` against the camera **[verified]**.

So three consumers of one camera — say Frigate, Home Assistant, and a phone —
means **three** TCP sessions to the camera, three BC decrypt+parse pipelines,
three sets of the allocations in §3, and three lots of RTP payloading. On a
4-core Pi 5 serving several cameras this is the single largest scalability
factor in the program. It also multiplies load on the *camera*, which typically
caps concurrent streams.

This looks deliberate rather than accidental: the per-client `max_fps`
decimator is explicitly documented as per-client ("One instance lives in each
client's blocking frame-pump thread, so two clients on the same camera
decimate independently", `factory.rs:470-475`) **[verified]**, and that
behaviour depends on unshared media.

The right shape is probably a per-camera opt-in — `shared = true` for cameras
where `max_fps` is unset, unshared where it is set — rather than a global
flip. Flagged here as the highest-value *architectural* item for low-power
hosts; it needs its own design discussion, not a patch in this document.

---

## 5. Codegen: instructions and vectorization

### 5.1 What the default aarch64 target gives us today

```
$ rustc --print cfg --target aarch64-unknown-linux-gnu | grep target_feature
target_feature="neon"
$ rustc --print target-spec-json -Zunstable-options --target aarch64-unknown-linux-gnu
  "features": "+v8a,+outline-atomics",
```

**[verified]** on this machine. So the stock target is **baseline ARMv8.0-A +
NEON**. No `lse`, no `aes`, no `sha2`, no `crc`, no `dotprod`, no `fp16`, no
`rcpc` — none of the things the A76 actually has.

The one nuance is `+outline-atomics`, which is on by default. It means atomics
are *not* compiled to LL/SC loops; they are compiled to calls into libgcc
helpers that branch on a runtime `__aarch64_have_lse_atomics` flag and use the
LSE instruction when present. So we do already get LSE on a Pi 5 — at the cost
of **an indirect call and a branch on every atomic operation**.

That matters more than it sounds, because neolink is unusually
atomic-dense: `Arc` clone/drop on every frame, tokio `mpsc`/`watch` channels
throughout, `UseCounter`, and the reactor's shared state.

### 5.2 A Pi-5-tuned build

```
-C target-cpu=cortex-a76
```

LLVM's `cortex-a76` model enables `FeatureLSE`, `FeatureAES`, `FeatureSHA2`,
`FeatureCRC`, `FeatureRDM`, `FeatureDotProd`, `FeatureFullFP16`, `FeatureRCPC`
and `FeatureSSBS`, and switches to A76 scheduling. Concretely that means:

* atomics become **inline `CAS`/`LDADD`** — the indirect call and branch
  disappear from every `Arc` clone and every channel operation;
* `+aes`/`+sha2`/`+crc` are available for direct (non-runtime-detected) use;
* the instruction scheduler models the real pipeline instead of a generic one.

**The catch:** the resulting binary **does not run on a Pi 4**. Cortex-A72 is
ARMv8.0 and has no LSE; an inline `CAS` is an illegal instruction there. So
this cannot replace the generic `aarch64-unknown-linux-gnu` artifact — it has
to be an *additional* one:

```yaml
# .github/workflows/build.yml, cross job matrix
- arch: arm64-pi5
  target: aarch64-unknown-linux-gnu
  gcc: aarch64-linux-gnu
  pkgconfig: aarch64-linux-gnu
  rustflags: "-C target-cpu=cortex-a76"
```

published as something unmistakable like `neolink-linux-arm64-pi5`, with the
README stating plainly that it requires a Pi 5 (or another Cortex-A76+ SoC) and
that the plain `arm64` artifact is the safe choice.

A middle option that keeps one artifact: `-C target-feature=+aes,+sha2,+crc`
without `+lse`. That gets the crypto instructions while remaining A72-safe —
but it gives up the atomics win, which is probably the larger half. And since
§4.2's `--cfg aes_armv8` already gets us hardware AES *with runtime detection
and no compatibility cost*, the incremental value of `+aes` at the codegen
level is close to zero. **If we ship a Pi-5 artifact, `target-cpu=cortex-a76`
for the atomics is the reason to do it.**

Anyone building on the Pi itself gets the whole thing for free with:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

which is worth documenting for Pi users who compile locally — it is the
simplest way to get every one of these wins with no CI changes at all.

### 5.3 CRC32 — already correct, no action needed

`crc32fast 1.5.0` (used by `bcudp/crc.rs`) detects `crc` at **runtime** on
aarch64 via `std::arch::is_aarch64_feature_detected!("crc")` and uses the
`__crc32*` intrinsics when available (`src/specialized/aarch64.rs:20-27`), gated
on a `stable_arm_crc32_intrinsics` cfg its own `build.rs` sets for rustc ≥ 1.80
**[verified]**. Our MSRV is 1.88, so **this already uses the hardware CRC
instruction on a Pi 5 with no flags.** Noted so nobody spends time on it.

### 5.4 Link-time optimization

`Cargo.toml` has **no `[profile.release]` section** **[verified]**, so we build
with the defaults: `opt-level = 3`, `lto = false`, `codegen-units = 16`.

```toml
[profile.release]
lto = "thin"
codegen-units = 1
```

This matters more on this codebase than on most, because the hot path crosses
crate boundaries constantly — `nom` combinators, `bytes`, `tokio-util`'s codec
machinery, and our own `neolink_core` are all separate compilation units, and
`nom`'s parser combinators in particular are only fast when they inline.

Cost: CI build time goes up meaningfully (`codegen-units = 1` serializes
codegen). If that is unacceptable, `lto = "thin"` alone with the default
codegen-units captures most of the cross-crate inlining for much less.

Would need before/after measurement (§7) rather than being taken on faith.

### 5.5 Hand-written NEON — not recommended

For completeness, since the question asked about vectorization specifically:
there is **no good candidate for hand-written NEON intrinsics in this
codebase.** The XOR loops (§4.3) are cold and are better fixed by *letting*
LLVM vectorize them than by writing intrinsics; AES is better served by the
`aes` crate's existing, audited backend (§4.2); ADPCM is serial by
construction; and everything else is parsing and I/O. `std::arch::aarch64`
intrinsics would add `unsafe`, a second code path to test, and a maintenance
burden, for no measurable gain. I would not do it.

---

## 6. Hardware acceleration: what is actually available, and what it buys

| Path | Where | Today | Pi 5 hardware option | Verdict |
|---|---|---|---|---|
| RTSP video | `factory.rs:771-861` | passthrough, no codec | — | **Nothing to accelerate.** Already optimal. |
| Snapshot, H.265 camera | `image/gst.rs:195` | `decodebin` → software HEVC | **`v4l2slh265dec`** (rpivid, 4Kp60) | Real win *if* snapshots are polled |
| Snapshot, H.264 camera | `image/gst.rs:185` | `decodebin` → `avdec_h264` | none (Pi 5 has no H.264 decoder) | Software only. A76 handles it. |
| JPEG encode | `jpegenc`, `factory.rs:694` | libjpeg-turbo | none | Already NEON-accelerated inside libjpeg-turbo |
| AAC decode | `faad`/`avdec_aac` | software | none | Negligible cost |
| ADPCM decode | `adpcm.rs` | Rust, serial | none | Negligible cost |

The only genuine hardware-acceleration opportunity in the whole program is
**HEVC decode for snapshots**, and only for users who poll them (ONVIF snapshot
URLs hit `src/onvif/snapshot.rs:41` on every request — a Home Assistant or
Frigate setup polling once a second is decoding a keyframe once a second).

If pursued, it should be a *probe-and-fall-back*, matching how the codebase
already handles `faad` vs `avdec_aac` (`factory.rs:932-934`) **[verified]** —
never a hard requirement, since `v4l2slh265dec` is absent on every non-Pi
platform and possibly on some Pi images too:

```rust
// Prefer the Pi 5's stateless HEVC decoder when the v4l2codecs plugin has
// found one; fall back to decodebin's software choice everywhere else.
let decoder = if ElementFactory::find("v4l2slh265dec").is_some() {
    "h265parse ! v4l2slh265dec"
} else {
    "h265parse ! decodebin"
};
```

Before writing that, confirm on hardware that the element exists and
negotiates, and **measure** — for a once-per-snapshot keyframe decode, the
V4L2 buffer setup and teardown may well cost more than an A76 software decode
of a single frame. This is a "measure first" item, not a "obviously do it" one.

---

## 7. How to measure any of this

Nothing above should be merged on the strength of the reasoning alone. On a Pi
5, with the official Active Cooler, a 5 V/5 A supply, and
`vcgencmd get_throttled` reading `0x0`:

1. **Baseline.** `perf stat -a` and `pidstat -p $(pgrep neolink) 1` over a
   10-minute steady-state run with a representative camera set. Record
   `%CPU`, `RSS`, and context switches per second.
2. **Profile.** `perf record -g -p $(pgrep neolink)` → flamegraph. This will
   settle §4.1 vs §4.2 vs §5.2 empirically instead of by argument. My
   prediction, stated so it can be falsified: allocation/memcpy dominates,
   AES is visible only with `FullAes` cameras, and atomics show up as a broad
   flat band rather than a peak.
3. **Microbenchmarks** for the crypto claims — a `criterion` bench over
   `EncryptionProtocol::decrypt` with a 64 KB buffer, run with and without
   `--cfg aes_armv8`, is a 20-line file and turns §4.2's "5-10×" from an
   estimate into a number.
4. **Scale test** for §4.5: 1 vs 2 vs 4 RTSP clients on one camera, watching
   whether CPU scales linearly (it should today, and should not after a
   `shared` fix).
5. Re-run 1-2 with `-C target-cpu=cortex-a76` and with `lto`/`codegen-units`
   to size §5.2 and §5.4 separately.

---

## 8. Recommended order of work

1. **§2.1 jemalloc 16 KB pages.** Compatibility blocker; the binary does not
   start. Build-flag only, no code risk. Do this first regardless of anything
   else here.
2. **§4.1 lazy decrypt in `bc/de.rs`.** Biggest CPU win in our own code, pure
   refactor, helps every platform.
3. **§4.2 `--cfg aes_armv8`.** Free, safe, runtime-detected. Bundle with the
   `.cargo/config.toml` comment about the rustflags-override trap.
4. **§7 measurement harness.** Before spending anything on 5-7.
5. **§5.4 LTO** — measure, then decide whether the CI time is worth it.
6. **§5.2 Pi-5 artifact** with `target-cpu=cortex-a76`, plus documenting
   `RUSTFLAGS="-C target-cpu=native"` for local builders (which is free and
   could ship immediately, in `docs/unix_setup.md`).
7. **§4.5 shared media** — needs a design discussion about `max_fps`
   semantics first.
8. **§6 HEVC snapshot decode** — only after measuring that snapshot decode
   registers at all.

§4.3 (XOR vectorization) and §4.4 (in-place CFB) are worth folding into
whichever change touches that file next, and are not worth a PR of their own.
