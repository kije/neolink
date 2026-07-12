# Baichuan device-lifecycle commands: discovery checklist

This document tracks the work needed to fully implement the two
device-lifecycle commands scaffolded under issue
[#14](https://github.com/kije/neolink/issues/14): **firmware upgrade** and
**factory reset**.

## Why this document exists

Unlike most other Baichuan messages implemented in `neolink_core`, the
exact wire format for firmware upgrade and factory reset is **not**
documented in any of the reference implementations the project's
maintainer surveyed. Sending the wrong `cmd_id` to the camera in the
wrong context can — at worst — brick the device.

So this PR ships only the scaffolding:

- Placeholder MSG_ID constants in `crates/core/src/bc/model.rs`
  (`MSG_ID_UPGRADE_BEGIN`, `MSG_ID_UPGRADE_DATA`, `MSG_ID_UPGRADE_COMMIT`,
  `MSG_ID_FACTORY_RESET`) — all currently `= 0`.
- Placeholder XML structs in `crates/core/src/bc/xml.rs`
  (`UpgradeReq`, `UpgradeData`, `FactoryReset`).
- An `Error::NotImplemented` variant returned by both lifecycle calls
  until the placeholders are filled in.
- CLI subcommands `neolink upgrade` and `neolink factory-reset`, both
  gated behind `--yes-i-am-sure` and both refusing to transmit until
  the discovery work below is complete.

Once the discovery work below is done, the scaffolding should require
only constant / XML-shape edits to become functional. The Rust API
surface (`BcCamera::upgrade_firmware`, `BcCamera::factory_reset`),
CLI flags, and error variants should not need to change.

## Reference material on hand

- `dissector/baichuan.lua` — Wireshark plugin shipped with this repo.
  Labels (treat as hints, not facts):
  - cmd 65 → `<ConfigFileInfo>` (Export)
  - cmd 66 → `<ConfigFileInfo>` (Import)
  - **cmd 67 → `<ConfigFileInfo>` (FW Upgrade)** — likeliest candidate
    for the firmware-upgrade session-open frame.
  - **cmd 99 → `<Restore>` (factory default)** — likeliest candidate
    for the factory-reset frame.
  - cmd 195 → `<AutoUpdate>` — possibly involved in the upgrade flow.
- `apocaliss92/nodelink-js`, file `src/protocol/constants.ts`:
  `BC_CLASS_FILE_DOWNLOAD = 0x6482` — the message-class also used for
  upload. Reflected here as `MSG_CLASS_FILE_DOWNLOAD` in `bc/model.rs`.
- `crates/core/src/bc_protocol/reboot.rs` — the simplest comparable
  lifecycle command; the eventual upgrade-commit ack will likely look
  similar.

## Discovery checklist

Items must be completed in order. Each item that produces a fact
should be back-filled into a comment in the relevant source file, and
the corresponding `TODO: confirm via capture` comment removed.

### 1. Capture setup

- [ ] On the LAN running the test camera, install the
  `dissector/baichuan.lua` plugin into Wireshark
  (`~/.local/lib/wireshark/plugins/` or equivalent).
- [ ] Verify the dissector decodes a known-good message (e.g. a reboot,
  cmd_id 23) — this is the baseline that proves the capture is
  working before any destructive operations are attempted.
- [ ] Run a `tcpdump -w trace.pcap port 9000` (or whatever TCP port the
  camera responds on) for the duration of each test.

### 2. Factory reset (do this first — smaller surface area)

- [ ] On a **disposable** test camera, trigger a factory reset from the
  Reolink mobile app.
- [ ] In the capture, locate the request frame addressed to the camera
  immediately before it reboots / drops the connection.
- [ ] Record:
  - The `cmd_id` → fill into `MSG_ID_FACTORY_RESET`.
  - The `class` field (16-bit, in the header).
  - The XML payload (if any) → adjust the `FactoryReset` struct in
    `bc/xml.rs` to match. The current placeholder assumes a single
    optional `keepNetwork` field; the real frame may have no payload
    or a completely different shape.
- [ ] Document whether `--keep-network` is a real, meaningful option
  the camera honours, or whether the app implements it client-side
  (in which case drop the CLI flag).
- [ ] Note the response `cmd_id` and `response_code` the camera sends
  back, if any, before the reboot.

### 3. Firmware upgrade (do this on a camera you can recover via
       Reolink's recovery procedure)

- [ ] Obtain a known-good `.pak` for the test camera's exact model and
  hardware revision. **Do not** test with a `.pak` from a different
  model; that is the most likely path to a brick.
- [ ] Trigger a firmware upgrade from the Reolink mobile app.
- [ ] In the capture, identify the message sequence between client and
  camera. There are likely three distinct cmd_ids:
  - [ ] Session open / metadata frame
    → fill into `MSG_ID_UPGRADE_BEGIN` and adjust the `UpgradeReq`
    struct in `bc/xml.rs`. Confirm:
    - Field name for total length (`fileSize`? `length`? something
      else?).
    - Hash type and field name (`sha256`? `md5`? `crc32`?).
  - [ ] Per-chunk frame(s)
    → fill into `MSG_ID_UPGRADE_DATA`. Confirm:
    - The message `class` (`0x6482` is the hypothesis from
      `nodelink-js`).
    - Whether each chunk has an XML extension or is pure binary.
    - The chunk size used by the app (so `UPGRADE_CHUNK_SIZE` in
      `lifecycle.rs` can be set to a known-working value).
  - [ ] Commit / finalize frame
    → fill into `MSG_ID_UPGRADE_COMMIT`. Confirm:
    - Whether it carries an XML payload.
    - Whether the camera replies with an ack before rebooting, or
      simply drops the connection (cf. `reboot.rs`).
- [ ] Note any **progress / push** frames the camera sends mid-upload
  so that the implementation can either consume them or explicitly
  ignore them.

### 4. Implementation pass

After steps 2 and 3 are documented:

- [ ] Replace the `= 0` placeholders in `crates/core/src/bc/model.rs`.
- [ ] Adjust the placeholder XML structs in `crates/core/src/bc/xml.rs`
  to match the captured shape (rename, drop, or add fields as
  needed — `serde(skip_serializing_if = "Option::is_none")` is
  already in place so optional-only diffs are forwards-compatible).
- [ ] Replace the `Err(Error::NotImplemented { ... })` returns in
  `crates/core/src/bc_protocol/lifecycle.rs` with the real send /
  receive loop, modelled on `reboot.rs` for the simple commands and
  on `talk.rs` for the chunked streaming.
- [ ] Add a round-trip test in `bc/xml.rs` for each new XML payload
  against a sample captured from the real device.
- [ ] Tighten the ability-name checks in `lifecycle.rs` once the real
  ability strings are known (currently best-effort `"upgrade"` and
  `"restore"` with `warn!` on failure).

### 5. Validation pass

- [ ] Run `neolink upgrade --yes-i-am-sure --dry-run` on a `.pak` —
  confirms the pre-flight is sound.
- [ ] Run `neolink upgrade --yes-i-am-sure` on a **disposable** test
  device with a known-good `.pak`. Confirm the camera comes back up
  on the new firmware version (cross-check via `neolink ... version`).
- [ ] Run `neolink factory-reset --yes-i-am-sure` on a disposable test
  device. Confirm the camera comes back up at its default IP /
  username / password.
- [ ] Repeat on at least one other firmware version if available.

### 6. Promotion

- [ ] Convert PR #25 from draft to ready-for-review and remove the
  `[scaffold]` title prefix.
- [ ] Update issue #14 with the captured wire-format facts.
- [ ] Tick the acceptance-criteria boxes on the issue.

## Capture template

When recording each fact above, append it under a per-camera heading
here so future maintainers have a quick reference (and so different
firmware versions can be cross-checked):

```
### <camera model> firmware <version>
- factory_reset: cmd_id=___, class=0x____, payload=<paste XML or "none">
- upgrade_begin: cmd_id=___, class=0x____, payload=<paste XML>
- upgrade_data:  cmd_id=___, class=0x____, chunk_size=___
- upgrade_commit:cmd_id=___, class=0x____, payload=<paste XML>
- abilities advertised: <copy from <Support>>
```
