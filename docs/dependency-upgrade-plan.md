# Dependency upgrade plan

Audit date: 2026-07-29. Workspace at `neolink` 0.6.3-rc.3.

This document covers every dependency in the workspace: what has already been
upgraded, what is left, what each remaining upgrade actually costs, and in what
order to do them. It exists because "is it a major version behind?" turned out
to be a poor predictor of effort — several major bumps needed no code at all,
while one of the cleanest-compiling upgrades in the whole set crashes the
process at startup.

## How the claims here were established

Every effort estimate below is measured, not guessed. Two independent methods
were used and they caught different things, which is the main lesson of this
audit:

1. **Compile probes.** Each candidate upgrade was applied in isolation to a
   clean tree, compiled with `cargo check --workspace --all-targets
   --all-features`, and reverted before the next. Error counts and file
   locations in this document come from those runs.
2. **Changelog and source reading.** For each crate the upstream CHANGELOG,
   release notes and migration guide were read, and the claimed breaking
   changes were checked against the actual unpacked crate sources in
   `~/.cargo/registry/src/`.

Neither method is sufficient alone:

- `axum` 0.7 → 0.8 compiles with **zero errors** and then panics on startup.
  Only the changelog reveals it.
- `quick-xml` 0.36 → 0.41 reports 5 errors in `neolink_core`, which stops the
  build before the root crate is even attempted — hiding 4 more errors in
  `src/onvif/`. Only reading the changelog surfaced the full blast radius, plus
  a silent behavioural change the compiler cannot see at all.

Where a number below is "errors", it means distinct `error[EXXXX]:` lines from a
real compile, with the two pre-existing `mismatched_lifetime_syntaxes` warnings
in `bcconn.rs` and `mqtt/mqttc.rs` discounted.

## Baseline

`cargo test --workspace --all-features` passes 129 tests today (63 in the root
crate, 64 in `neolink_core`, 2 doc-tests). That number is the safety net for
everything below, and §"Prerequisite test coverage" explains where it has
holes.

## Already landed on this branch

Two commits, both verified green on all four feature combinations
(`--all-features`, `--no-default-features`, gstreamer-only, pushnoti-only),
129 tests passing, clippy clean, `cargo fmt --check` clean, and building under
the CI-pinned toolchain.

### 1. Every semver-compatible update, and the MSRV move to 1.88

`cargo update` moved 363 lockfile entries and each manifest's version floor was
raised to the release actually tested. No source changes were required.

The MSRV had to move from 1.82 to 1.88, and the CI pin moved with it. This was
forced:

- `time` 0.3.54 — a direct dependency of `neolink_core` — declares
  `rust-version = "1.88.0"`.
- `home` 0.5.12 declares `1.88`, reached through
  `which` → `prost-build` → `fcm-push-listener`'s build script.
- 25 crates now in the tree are edition 2024 (`clap` 4.6, `toml` 1.1,
  `cpufeatures` 0.3, `getrandom` 0.4, `hashbrown` 0.17, `indexmap` 2.14,
  `zeroize` 1.9, …). Cargo 1.82 cannot even parse their manifests — it fails
  with ``feature `edition2024` is required`` on `cpufeatures` 0.3.0.

1.88 is the exact floor, not an overshoot: `cargo +1.87.0 check` fails with
`rustc 1.87.0 is not supported by the following packages`, and
`cargo +1.88.0 check --workspace --all-features` succeeds.

Every workspace manifest now declares `rust-version = "1.88"`, so Cargo reports
a clear MSRV error rather than a confusing manifest-parse failure, and the
constraint lives next to the dependencies that impose it instead of only in a
workflow file.

Hygiene fixed at the same time:

- `env_logger = "*"` in `crates/decoder` and in `crates/core`'s dev-dependencies
  is replaced with a real requirement. A wildcard cannot be published to
  crates.io and makes builds unreproducible.
- `crates/mailnoti` and `crates/pushnoti` were pinned to env_logger 0.10 while
  the root crate used 0.11, so the workspace built two copies. All are now
  0.11.11; the `Builder::from_env(Env::default().default_filter_or(..))` call
  sites compile unchanged. This dropped `humantime`, `is-terminal`, `termcolor`
  and `winapi-util` from the tree.

### 2. Eight major bumps that needed no code changes

| Crate | From | To | Where |
|---|---|---|---|
| `thiserror` | 1.0.69 | 2.0.19 | `crates/core` |
| `delegate` | 0.12.0 | 0.13.5 | `crates/core` |
| `md5` | 0.7.0 | 0.8.1 | root, `crates/core`, `crates/decoder` |
| `sha1` | 0.10.7 | 0.11.0 | root |
| `socket2` | 0.5.10 | 0.6.5 | root |
| `base64` | 0.22.1 | 0.23.0 | root |
| `dirs` | 5.0.1 | 6.0.0 | root |
| `requestty` | 0.5.0 | 0.6.3 | `crates/decoder` |

Three of these touch code where a silent change would be worse than a compile
error, so each was checked beyond "it builds":

- **`md5`** feeds the camera AES key derivation at
  `crates/core/src/bc_protocol/credentials.rs:49`,
  `crates/core/src/bc_protocol.rs:465` and `crates/decoder/src/main.rs:27`, all
  through `format!("{:X}\0", md5::compute(..))`. A change in hex formatting
  would break camera login without failing to compile. Both versions declare
  `pub struct Digest(pub [u8; 16])` and generate hex from the same macro with
  the same format strings — `implement!(LowerHex, "{:02x}")` /
  `implement!(UpperHex, "{:02X}")` at `md5-0.7.0/src/lib.rs:90-91` and
  `md5-0.8.1/src/lib.rs:82-83` — so `{:X}` is byte-identical. The only 0.7→0.8
  break is `Context::compute` → `Context::finalize`, and this repo only ever
  calls the free `md5::compute`.
- **`sha1`** computes the ONVIF WS-Security password digest in
  `src/onvif/soap.rs`. Every use is `B64.encode(hasher.finalize())` over raw
  bytes, with no hex or `Debug` formatting. `digest` 0.11 changes the output
  container (`GenericArray` → `hybrid_array::Array`) but not the bytes, and
  both impl `AsRef<[u8]>`. The existing tests at `src/onvif/soap.rs:455` and
  `:489` cover that path end to end.
- **`dirs::config_dir()`** locates the saved push-notification token at
  `src/common/pushnoti.rs:77`. A changed path would orphan users' existing
  tokens. The Linux implementation is character-for-character identical between
  `dirs-5.0.1/src/lin.rs:9` and `dirs-6.0.0/src/lin.rs:9`.

`thiserror` 1.0.69, `socket2` 0.5.10 and `base64` 0.21/0.13 remain in the tree
as transitive duplicates pulled by other crates; only our direct requirements
moved.

## Prerequisite test coverage — do this before the risky phases

Three of the remaining upgrades change code that has no test covering it. These
tests are cheap, they are useful independently of any upgrade, and without them
the corresponding migration cannot be verified at all. **They are the highest
value work in this document.**

### P1. AES-CFB round trip (blocks `aes`/`cfb-mode` 0.9)

`crates/core/src/bc/crypto.rs` has exactly two `#[test]` functions, at lines 106
and 115, and **both exercise only `EncryptionProtocol::BCEncrypt`** — the simple
XOR path. The `Aes` and `FullAes` variants, which handle traffic from every
modern Reolink camera, have **no test at all**. The only non-test constructors
are at `crates/core/src/bc/codex.rs:121` and `:124`.

This matters because the `aes`/`cfb-mode` 0.9 migration deletes the `Clone` impl
the current design depends on (see §`aes` + `cfb-mode` below), forcing a
refactor of exactly this code. Add, before that upgrade:

- a round-trip test: `EncryptionProtocol::aes(key)` encrypt-then-decrypt returns
  the input, for a payload that is *not* a multiple of 16 bytes so the trailing
  partial-block path is covered;
- the same for `full_aes`;
- ideally a known-answer test: a hard-coded key and plaintext with the expected
  ciphertext bytes captured from the *current* implementation, so the refactor
  is proven byte-identical rather than merely self-consistent.

### P2. ONVIF entity-reference handling (blocks `quick-xml` 0.41)

Since quick-xml 0.38 an entity reference is delivered as its own
`Event::GeneralRef` event rather than being folded into `Event::Text`. All four
of our reader loops end in a catch-all that would silently discard it:

- `src/onvif/soap.rs:165` (WS-Security `Username`/`Password`/`Nonce`/`Created`)
- `src/onvif/discovery.rs:116` (WS-Discovery `MessageID`)
- `src/onvif/services/media.rs:343`
- `src/onvif/services/events.rs:518`

A password of `p&w` would arrive as `Text("p")`, `GeneralRef("amp")`,
`Text("w")`, and the `_ => {}` arm drops the middle event, yielding `pw`. The
build would be green and ONVIF authentication would fail for any credential
containing `&`, `<`, `>`, `"` or `'`.

Add a WS-Security authentication test using a password containing those five
characters. It will pass on quick-xml 0.36 and fail the moment 0.38+ lands
without the corresponding loop fix — which is exactly what a regression test
should do.

### P3. ONVIF router construction (blocks `axum` 0.8)

Nothing currently constructs the router built at `src/onvif/server.rs:36-49`.
Since axum 0.8's rejection of the old path syntax is a runtime panic in
`Router::route`, the failure mode today is "panics on first ONVIF start" with no
test to catch it. Add a test that builds the router and asserts the routes
resolve.

## Phase 0 — `fcm-push-listener` 2.0.3 → 3.0.0: already broken in production

**This is not a "keep current" upgrade. Push-notification registration is dead
on 2.0.3 today, and has been since 2024-06-20.** It is listed first because it
is the only item in this document that fixes a live user-facing failure, and
because it is cheap.

`fcm-push-listener` 2.0.3's `register()` posts to
`https://fcm.googleapis.com/fcm/connect/subscribe`
(`fcm-push-listener-2.0.3/src/fcm.rs:40`). Google shut that endpoint down on
2024-06-20 as part of the FCM-23 deprecation. The crate author says so in the
first line of his own README:

> # IMPORTANT
> [An endpoint that this library is dependent upon will be shut down on June 20, 2024](https://firebase.google.com/support/faq#fcm-23-deprecation).
> I don't plan on trying to get this working again as it looks like it would be extremely difficult.

3.0.0 (published 2024-01-13, ahead of the shutdown) moves to the replacement
APIs — `https://fcmregistrations.googleapis.com/v1/projects/{id}/registrations`
and `https://firebaseinstallations.googleapis.com/v1/projects/{id}/installations`.

**Blast radius: existing installs are fine, new ones are not.** The token file is
loaded first and `register()` only runs when there is no saved token
(`src/common/pushnoti.rs:110`, `crates/pushnoti/src/main.rs:57`). So anyone who
registered before June 2024 still works; anyone setting up fresh, or who loses
their token, cannot register at all.

**Effort: low — and most of the work is already written.** Someone clearly
started this migration and left it commented out:

- `crates/pushnoti/src/main.rs:64-70` contains the correct 4-argument call,
  commented out, with the Firebase constants at `:44-47`.
- `src/common/pushnoti.rs:70-76` has the same constants commented out.

3.0.0's signature is exactly what those comments assume:

```rust
// fcm-push-listener-3.0.0/src/register.rs:31
pub async fn register(firebase_app_id: &str, firebase_project_id: &str,
                      firebase_api_key: &str, vapid_key: &str)
    -> Result<Registration, Error>
// versus 2.0.3/src/register.rs:30
pub async fn register(sender_id: &str) -> Result<Registration, Error>
```

**The token file format does not change**, which is what makes this safe.
`Registration { gcm: GcmRegistration, fcm_token: String, keys: WebPushKeys }` is
identical between `fcm-push-listener-2.0.3/src/register.rs:24-28` and
`3.0.0/src/register.rs:25-29`. `FcmPushListener`, `FcmMessage`, `WebPushKeys` and
every `Error` variant matched at `src/common/pushnoti.rs:199-217` are also
unchanged, so the listener loop and error handling need no edits at all. No user
migration, no re-registration prompt, no MSRV movement, no CI change.

Steps:

1. Bump `Cargo.toml:26` and `crates/pushnoti/Cargo.toml:13` to `"3.0.0"`.
2. `src/common/pushnoti.rs`: uncomment the constants at `:72-75`, change the
   `register(sender_id)` call to the 4-argument form, drop the now-unused
   `sender_id`.
3. `crates/pushnoti/src/main.rs`: uncomment `:64-70` and `:44-47`, delete the
   `sender_id` binding at `:49` and the 1-argument call.

**One genuine open question: the VAPID key.** The commented-out constant is
`let vapid_key = "";` — an empty string. That needs confirming against a real
camera before this is called done, because it is the one input we do not appear
to have a known-good value for.

**Bug worth fixing in the same pass:** `src/common/pushnoti.rs:67` combined with
the `continue` at `:135` retries registration every 3 seconds with no backoff.
Right now that means hammering a dead Google endpoint every 3 seconds forever.
Add backoff while you are in the file.

## Phase 1 — low cost, do these next

### `gstreamer`, `gstreamer-app`, `gstreamer-rtsp`, `gstreamer-rtsp-server`: 0.23 → 0.24.5

**Effort: one line.** This is the best value in the entire plan.

Measured: with all four crates moved to 0.24.5, the whole workspace produces
**exactly one error**:

```
error[E0061]: this method takes 1 argument but 2 arguments were supplied
    --> src/rtsp/factory.rs:616:16
616 |     if payload.has_property("aggregate-mode", None) {
    |                ^^^^^^^^^^^^                   ---- unexpected argument #2
```

glib 0.21 split `ObjectExt::has_property(name, Option<Type>)` into
`has_property(name)` and `has_property_with_type(name, Type)`. Since the call
passes `None`, the fix is:

```rust
if payload.has_property("aggregate-mode") {
```

Nothing else changes. The two hand-rolled GObject subclasses in
`src/rtsp/gst/server.rs` and `src/rtsp/gst/factory.rs` already use the
post-rework idioms (`self.obj()`, `self.imp()`, `#[object_subclass]`,
`glib::wrapper!` with `@extends`), so the 0.24 subclassing trait reshuffle does
not touch them.

Two things that are *not* problems, both worth stating because they are the
obvious fears:

- **The GStreamer C library floor does not move.** 0.24.5 and 0.25.3 both still
  document "at least GStreamer 1.14 and gst-plugins-base 1.14", and the `v1_20`
  feature still maps to pkg-config `gstreamer-1.0 >= 1.20` in the 0.25 sys
  crates. Ubuntu 22.04 (1.20.x), macOS (1.20.4) and Windows (1.24.2) all remain
  valid. **Do not raise the feature to `v1_22`+** as part of this — that
  *would* break the Ubuntu and macOS runners.
- **MSRV is fine.** gstreamer-rs 0.24.x requires Rust 1.83, comfortably under
  our 1.88.

One caveat found while probing: bumping these crates pulls a newer
`system-deps`, which pulls `kstring` 2.0.4 — and `kstring` 2.0.3+ declares
`rust-version = 1.96.0`, above even current stable. Until that settles, the
lockfile needs `cargo update -p kstring --precise 2.0.2` (2.0.2 requires only
1.73). `kstring` is a build-dependency of a build script, so this pin has no
effect on the shipped binary.

**0.25.x is deliberately not recommended** — see Phase 4.

### `axum`: 0.7.9 → 0.8.9

**Effort: seven route strings. Compiles clean either way, so this needs care.**

Measured: `cargo check --workspace --all-targets --all-features` with axum 0.8.9
is a **clean compile, zero errors**. That is the trap. axum 0.8 upgraded
`matchit` 0.7 → 0.8, which changed path-parameter syntax from `:name` to
`{name}`, and rejects the old form at **runtime**:

- `axum-0.8.9/src/routing/path_router.rs:53-73` — `validate_v07_paths` returns
  `Err("Path segments must not start with `:`. For capture groups, use
  `{capture}`. …")`
- reached from `validate_path` (`path_router.rs:39`, called at `:88` and `:146`)
- and `Router::route` wraps that in `panic_on_err!`
  (`axum-0.8.9/src/routing/mod.rs:180`)

So the ONVIF server would panic the first time it builds its router. Rewrite all
seven patterns in `src/onvif/server.rs`:

| Line | Before | After |
|---|---|---|
| 37 | `/onvif/:camera/device_service` | `/onvif/{camera}/device_service` |
| 38 | `/onvif/:camera/media_service` | `/onvif/{camera}/media_service` |
| 39 | `/onvif/:camera/ptz_service` | `/onvif/{camera}/ptz_service` |
| 40 | `/onvif/:camera/events_service` | `/onvif/{camera}/events_service` |
| 42 | `/onvif/:camera/subscription/:sub_id` | `/onvif/{camera}/subscription/{sub_id}` |
| 46 | `/onvif/:camera/snapshot/:stream` | `/onvif/{camera}/snapshot/{stream}` |
| 47 | `/onvif/:camera` | `/onvif/{camera}` |

`"/"` at line 48 is unaffected. There are no `*` wildcard routes and no literal
braces needing `{{`/`}}` escaping.

Do **not** reach for `Router::without_v07_checks()`. That escape hatch exists
for routers that must literally match a segment beginning with `:`, which is not
our case.

Notes on the rest of axum 0.8, all verified as not applying:

- The rewritten set is conflict-free under matchit 0.8: the first-position param
  is named `camera` in every route (matchit 0.8 rejects differently-named params
  at the same position), and every param spans a whole segment (matchit 0.8 has
  no dynamic prefix/suffix support, so `/{id}.json` would be rejected — we have
  none).
- `Sync` is now required for handlers. All ours are plain `async fn` items, and
  `OnvifState` is already `Clone + Send + Sync + 'static` via
  `Arc<OnvifStateInner>` (`src/onvif/state.rs:116-119`), which `with_state`
  already demanded in 0.7.
- Tuple `Path` extractors now check arity exactly. Both of ours already match:
  `Path<(String, String)>` at `src/onvif/server.rs:103` against the 2-param
  route at `:42`, and at `src/onvif/snapshot.rs:22` against `:46`. Worth a smoke
  test anyway, since a mismatch is a 400 at runtime rather than a compile error.
- The `#[async_trait]` removal from `FromRequest`/`FromRequestParts` does not
  apply — we implement no extractors.
- `tower` needs no move: axum 0.8.9 requires `tower ^0.5.2` and the lockfile
  already has 0.5.3. We have no tower middleware of our own.

**Avoid axum 0.8.2 — it is yanked.** MSRV of 0.8.9 is 1.80, under our 1.88.

### `rand`: 0.8.7 → 0.10.2

**Effort: ~6 sites. Go straight to 0.10.2 — 0.9 is a dead end.**

This upgrade *shrinks* the tree. `rand` 0.10.2 is **already in `Cargo.lock`**,
pulled by `uuid` 1.24.0, and `rand` 0.8.7's only reverse dependency is
`neolink_core` itself. So moving `crates/core` to 0.10.2 deletes `rand` 0.8.7,
`rand_chacha` 0.3.1, `rand_core` 0.6.4 and `ppv-lite86` 0.2.21 from the lockfile
and adds nothing. Stopping at 0.9 would instead *add* `rand_core` 0.9 and
`getrandom` 0.3 as extra majors and still leave `rand` duplicated.

**Version safety.** RUSTSEC-2026-0097 ("Rand is unsound with a custom logger
using `rand::rng()`") affects 0.7.0–0.8.5, 0.9.0–0.9.2 and 0.10.0. Our current
0.8.7 is **not** affected, and neither is 0.10.2 — but do not land on 0.9.0–0.9.2
or 0.10.0.

**The measured error count understates the work, and this is worth
understanding.** A probe of `crates/core` on rand 0.10 reports exactly two
errors, both `error[E0432]: unresolved import `rand::thread_rng`` at
`crates/core/src/bc_protocol/connection/discovery.rs:18` and
`crates/core/src/bc_protocol/connection/udpsource.rs:13`. That is misleading:
with the import unresolved, every `let mut rng = thread_rng();` binds `rng` to
the error type and rustc suppresses all downstream method-resolution
diagnostics. Fix the two imports and a second wave of four
`error[E0599]: no method named 'gen' found` appears at `discovery.rs:1268`,
`discovery.rs:1273`, `udpsource.rs:385` and `udpsource.rs:446`.

A third subtlety compounds it: **`use rand::Rng` keeps resolving under 0.10**,
so nothing flags it — but `rand::Rng` is now a re-export of `rand_core::Rng`
(the trait formerly called `RngCore`, `rand-0.10.2/src/lib.rs:59`), while the
trait carrying `.random()` and `.random_range()` is now `rand::RngExt`
(`rand-0.10.2/src/rng.rs:56`, methods at `:93` and `:163`). The breakage
surfaces as missing methods, not as a failed import.

Renames to apply:

| rand 0.8 | rand 0.10 |
|---|---|
| `rand::thread_rng()` | `rand::rng()` |
| `use rand::Rng` (for `gen`/`gen_range`) | `use rand::RngExt` |
| `Rng::gen()` | `RngExt::random()` |
| `Rng::gen_range(a..b)` | `RngExt::random_range(a..b)` |
| `rand::seq::SliceRandom` (for `choose`) | `rand::seq::IndexedRandom` |

Affected sites: `discovery.rs:18` (import), `:1268`, `:1273`, `:1279`;
`udpsource.rs:13` (import), `:385`, `:446`, `:766`, `:783`.

**Value ranges are unchanged**, which matters because these pick protocol client
IDs and UDP ports. `gen::<u8>()` → `random::<u8>()` resolves to the same
`rng.next_u32() as u8` in both versions (`rand-0.8.7/src/distributions/integer.rs:16-21`
vs `rand-0.10.2/src/distr/integer.rs:28-33`), so no behavioural drift.

rand 0.10.2 is edition 2024 with MSRV 1.85, under our 1.88.

### `quick-xml`: 0.36.2 → 0.41.0

**Effort: 9 sites, of which 4 need thought rather than a mechanical swap.**

MSRV 1.79, comfortably under 1.88. Do this **after P2** above.

Measured in `neolink_core`: 5 errors, in two files. The root crate is never
reached because the core build stops first, which hides 4 more errors — the
`unescape()` sites listed in P2.

**Step 1 — error-type split (mechanical).** quick-xml 0.37 split `DeError` into
`DeError` (deserialization) and `SeError` (serialization). Change the return type
from `Result<W, quick_xml::de::DeError>` to `Result<W, quick_xml::se::SeError>`
at `crates/core/src/bc/xml.rs:179` (`BcXml::serialize`),
`crates/core/src/bc/xml.rs:195` (`Extension::serialize`) and
`crates/core/src/bcudp/xml.rs:80` (`UdpXml::serialize`). Leave the three
`try_parse` functions on `DeError`.

No caller changes are needed: there is no `From<DeError>` impl anywhere in the
workspace, and every caller of these three uses `.unwrap()`
(`crates/core/src/bc/ser.rs:80`, `:91`, `crates/core/src/bcudp/ser.rs:13`)
rather than `?`.

**Step 2 — the `<P2P>` wrapper (needs restructuring).**
`crates/core/src/bcudp/xml.rs:86-92` uses
`create_element("P2P").write_inner_content::<_, quick_xml::de::DeError>(..)`.
In 0.37 that method lost its generic error parameter and its closure must now
return `io::Result<()>`, which `write_serializable` (returning `SeError`) cannot
satisfy. Replace the closure form with three explicit calls —
`write_event(Event::Start(BytesStart::new("P2P")))?`,
`write_serializable("", &self)?`,
`write_event(Event::End(BytesEnd::new("P2P")))?` — inside the now-`SeError`
function from step 1. This works because `SeError: From<io::Error>` exists even
though the reverse does not. The empty root name `""` still behaves identically,
so the enum-variant-as-tag trick survives.

**Step 3 — the four reader loops (compile error *and* silent behaviour change).**
`BytesText::unescape()` is gone from 0.41 (only `decode()` remains), so
`src/onvif/soap.rs:165`, `src/onvif/discovery.rs:116`,
`src/onvif/services/media.rs:343` and `src/onvif/services/events.rs:518` all
fail to compile. **Do not just swap in `decode()`.** Each loop must also handle
`Event::GeneralRef` and append the resolved entity to the accumulating text,
instead of dropping it in `_ => {}`. See P2 for why, and add that test first.

Existing XML coverage that will help: round-trip and golden tests in
`crates/core/src/bc/xml.rs` (7 tests), `crates/core/src/bcudp/ser.rs` (6),
`crates/core/src/bcudp/de.rs` (7), `crates/core/src/bc/de.rs` (9),
`crates/core/src/bcmedia/de.rs` (7), `crates/core/src/bc/ser.rs` (2),
`crates/core/src/bcudp/xml.rs` (1). The Baichuan side is therefore well
protected; the ONVIF side is where the gap is.

## Phase 2 — real work, one dependency per commit

### `aes` 0.8.4 → 0.9.2 and `cfb-mode` 0.8.2 → 0.9.1

**Effort: 29 errors, but 23 of them are in one 120-line file.** Do this
**after P1**.

These two must move **together, in one commit**, in both
`crates/core/Cargo.toml` and `crates/decoder/Cargo.toml`, because
`cfb_mode::Encryptor<Aes128>` requires `aes`'s cipher-0.5 traits — aes 0.9 with
cfb-mode 0.8 does not typecheck, and neither does the reverse.

**Pin `aes >= 0.9.1`: 0.9.0 is yanked** (AArch64 build warnings and a wrong
minimal `zeroize` requirement; 0.9.2 additionally fixes an x86 performance
regression). MSRV 1.85, under our 1.88.

Measured error distribution: `crates/core/src/bc/crypto.rs` 23,
`crates/decoder/src/main.rs` 2 (plus duplicates from the lib-test target).
Three distinct causes:

1. **`Clone` was removed** from `cfb_mode::Encryptor`/`Decryptor` (0.9.0
   CHANGELOG: "Removed the `std` feature and `Clone` implementation"). This
   breaks `#[derive(Debug, Clone)]` on `EncryptionProtocol`
   (`crypto.rs:16`) and the two per-packet `.clone()` calls at `crypto.rs:79`
   and `:99`.

   The fix is to **store the key and build a fresh cipher per call**. This is
   behaviour-preserving, and it is worth understanding why: `decrypt(&self, ..)`
   and `encrypt(&self, ..)` take `&self` (`crypto.rs:65`, `:86`), so the stored
   `enc`/`dec` are never mutated — they always sit at their initial IV state,
   which is precisely why the code clones them per packet in the first place.
   AES-128-CFB128 with the fixed IV `b"0123456789abcdef"` produces the same
   keystream whether you clone a never-mutated `Decryptor` or construct a new
   one. The refactor also makes `Debug`/`Clone` on `EncryptionProtocol`
   trivial again.

2. **`AsyncStreamCipher` was removed** from the `cipher` crate entirely in
   cipher 0.5. Drop it from the import lists; the method calls are unchanged:

   ```rust
   // crates/core/src/bc/crypto.rs:1-4, and the same shape in crates/decoder/src/main.rs
   -use aes::{cipher::{AsyncStreamCipher, KeyIvInit}, Aes128};
   +use aes::{cipher::KeyIvInit, Aes128};
   ```

3. **`generic-array` → `hybrid-array`.** `KeyIvInit::new` now takes
   `&Key<Self>`/`&Iv<Self>` as `hybrid_array::Array`, which has no infallible
   `From<&[u8]>`. Hence the 17 `error[E0277]: the trait bound
   `&aes::cipher::Array<u8, …>: From<&[u8]>` is not satisfied` at
   `crypto.rs:52-53` and `:59-60`. Retyping the IV constant so its length is in
   the type fixes most of it:

   ```rust
   -const IV: &[u8] = b"0123456789abcdef";
   +const IV: &[u8; 16] = b"0123456789abcdef";
   ```

   (`b"..."` is already `&[u8; 16]`, so this only makes the existing type
   visible.)

Neither crate has a RUSTSEC advisory; both are actively maintained by
RustCrypto.

### `nom`: 7.1.3 → 8.0.0 — recommended to DEFER

**Effort: 42 errors across three files, ~95% mechanical by volume — but the
mechanical fix is also where the semantic landmine is. Recommend staying on
7.1.3 for now.**

Measured error distribution in `neolink_core`:
`crates/core/src/bcudp/de.rs` 34, `crates/core/src/bc/de.rs` 21,
`crates/core/src/bcmedia/de.rs` 13, `crates/core/src/lib.rs` 1.

**The reason to defer is not the volume — it is this.** In nom 8,
streaming-versus-complete stopped being a module choice (`nom::bytes::streaming`
vs `nom::bytes::complete`) and became **a mode selected per call**:

```rust
// nom-8.0.0/src/internal.rs:412-421
fn parse(&mut self, input: Input) -> IResult<..> {
    self.process::<OutputM<Emit, Emit, Streaming>>(input)
}
fn parse_complete(&mut self, input: Input) -> IResult<..> {
    self.process::<OutputM<Emit, Emit, Complete>>(input)
}
```

Which method you write decides whether a short buffer yields `Err::Incomplete` or
`Err::Error`. This codebase drives `tokio_util` codecs off exactly that
distinction (`crates/core/src/bc/codex.rs`,
`crates/core/src/bcmedia/codex.rs`, `crates/core/src/bcudp/codex.rs`). So the
"mechanical" work of inserting `.parse(` at ~34 sites is simultaneously ~34
semantic decisions about frame boundaries — and getting one wrong produces a
stalled or mis-framed camera stream, not a compile error. That combination
(bulk edit + invisible failure mode) is what makes it a bad fit for a batch.

For when it is attempted, the 42 errors reduce to five root causes:

1. **`VerboseError` is gone** — it moved to a separate `nom-language` crate. In
   nom 8 it survives in `nom-8.0.0/src/error.rs` only inside a `/*` block
   starting at line 592. Four sites reference it: `crates/core/src/lib.rs:76`
   (`NomErrorType`) and the `type IResult<..., E = VerboseError<I>>` aliases at
   `bc/de.rs:10`, `bcmedia/de.rs:6`, `bcudp/de.rs:12`. Because every signature in
   the three `de.rs` files is written in terms of those aliases, **fixing four
   lines collapses a large share of the 70 error lines at once.**
2. **~34 call sites need `.parse(` inserted** (combinators now return
   `impl Parser`, not closures): 29 `context`/`error_context`, 3 `consumed`,
   2 `hex32()`.
3. **Two return-type changes**, `hex32` (`bc/de.rs:62`) and `take4`
   (`bcmedia/de.rs:159`), from `impl FnMut(..) -> IResult<..>` to
   `impl Parser<..>`.
4. **One deprecation**: `tuple((le_u16, le_u16))` → `(le_u16, le_u16)` at
   `bc/de.rs:235`.
5. **The one genuinely non-mechanical edit**: the hand-written
   `impl Parser for BcParser` at `bc/de.rs:27-35` cannot be ported
   field-for-field. nom 8's trait moved from `Parser<I, O, E>` with `fn parse`
   to `Parser<Input>` with associated `Output`/`Error` and a required generic
   `fn process<OM: OutputMode>`.

**Why deferring is safe:** nom 7.1.3 is not yanked, has no RUSTSEC advisory,
declares MSRV 1.48 (so it can never block a future toolchain bump), and pulls
only `memchr`. nom 8.0.0 shipped 2025-01-25 and is still the only 8.x release
eighteen months later, with upstream quiet since 2025-08-26. The upgrade buys
code-size and monomorphisation wins this project does not need, adds a
`nom-language` dependency, and concentrates its residual risk exactly where the
tests are thinnest. Blast radius is one crate — nom is a direct dependency of
`crates/core` only.

The safety net if it is attempted: 64 tests in `neolink_core`, many
round-tripping captured protocol samples via `include_bytes!`. Note the gap
though — **`crates/core/src/bcmedia/ser.rs` has zero tests**, while
`bcudp/ser.rs` has 6 and `bc/ser.rs` has 2.

`cookie-factory` needs no action: it is already at the latest 0.3.3, and its
manifest has no `nom` dependency edge at all (only an optional `futures`), so
the two are fully independent.

## Phase 3 — compiled clean, behaviour investigated

Every item in this phase produced **zero compile errors** in the probes, so the
compiler has nothing to tell us and the whole question is behavioural. Two of the
four have since been investigated and cleared; two have not.

- `toml` and `validator` — **investigated and verified safe.** Effectively
  trivial; promote them to Phase 1 whenever convenient.
- `rumqttc` — **still unverified.** Treat the checks below as required work.
- `tikv-jemallocator` — investigated: the upgrade is a net *improvement* for
  aarch64, and it surfaced a latent Docker bug that should be fixed today,
  independent of any version change.

### `toml` 0.8.23 → 1.1.4 and `validator` 0.18.1 → 0.21.0 — both verified safe

**Both are effectively trivial. They were the biggest unknowns in this audit and
they came back clean, measured rather than inferred.** They can land together in
one commit; they are independent of each other and need no source edits, no
feature changes and no CI changes.

The worry was real: toml 0.9 was a substantial rewrite onto
`toml_parser`/`toml_writer`, and `toml::to_string` output is user-visible in
three places — `src/mqtt/mod.rs:199` and `:206` serialise the live `Config` and
**publish it over MQTT** (so any formatting change reaches Home Assistant), and
`src/common/pushnoti.rs:118` / `crates/pushnoti/src/main.rs:71` write the saved
FCM token file that is read back with `toml::from_str`.

**`toml::to_string` output is byte-identical between 0.8.23 and 1.1.4** for every
shape this repo serialises. This was measured by compiling a harness replicating
`Config` (`src/config.rs:20-56`, `202-305`) and the FCM `Registration` twice
against the two toml rlibs and diffing the emitted text — identical md5 under
both versions. Identical across all the places a rewrite could plausibly have
drifted: root scalars emitted before `[[cameras]]` despite `cameras` being
declared first, standard sub-table headers, the blank line before each table
header, inline arrays, `skip_serializing` omission, `Option` skipping,
`motion_timeout = 1.0` whole-float form, and string quoting/escaping (quote →
single-quoted literal, backslash → literal string, newline → multi-line basic,
tab → `\t`, unicode passthrough). The zero-camera case emits `cameras = []`
inline at the declaration position under both. So the MQTT payload does not
change and token files round-trip in both directions.

**`validator` 0.21.0 changes no validation behaviour either.** Verified with a
differential harness covering every `#[validate(...)]` attribute in
`src/config.rs` plus all three custom validator functions
(`validate_mqtt_server` at `src/config.rs:376`, `validate_username` at `:648`,
`validate_camera_config` at `:658`) across 13 scenarios — valid config, regex
failure on `tls_client_auth` (`:41`), regex failure on `max_encryption` inside a
nested `Vec` element (`:239`), range failures, and the custom-function paths.
Same outcomes and same error structure under 0.18.1 and 0.21.0.

MSRV: toml 1.1.4 declares 1.85 (fits). **validator 0.21.0 declares exactly
1.88** — zero headroom, caused by let-chains in `validator_derive-0.20.1`. Worth
noting that validator 0.21 would independently have forced the same 1.88 bump
that `time` and `home` already forced, so the MSRV move buys this for free.
Neither crate is yanked; both are the latest release.

Steps: bump `toml` to `"1.1.4"` and `validator` to `"0.21.0"` (keeping
`features = ["derive"]`) in all three manifests — root, `crates/mailnoti`,
`crates/pushnoti` — then `cargo update -p toml -p validator`.

### `rumqttc`: 0.24.0 → 0.25.1

Compiles clean. The behavioural surface is larger than average: reconnect and
backoff timing, whether `EventLoop::poll` returns different errors on
disconnect, QoS/retain handling, and last-will behaviour. Any change shows up as
flapping Home Assistant entities, not a build failure. The root manifest
declares `rumqttc = "0.24.0"` with **no explicit features**, so also confirm
0.25 did not change the default feature set — a changed TLS default could
silently disable TLS MQTT or pull a much larger tree (rustls major, ring vs
aws-lc-rs, and possibly a cmake/nasm build requirement).

Relevant code: `src/mqtt/mqttc.rs`, `src/mqtt/mod.rs`, `src/mqtt/discovery.rs`.

### `tikv-jemallocator`: 0.5.4 → 0.7.0 — and a latent Docker bug to fix first

Compiles clean with no new warnings, but only for the host target — jemalloc is a
C library built by a build script and is the most fragile thing in a cross build.
CI cross-compiles four Linux targets and neolink commonly runs on Raspberry Pi.

**Fix this regardless of whether you take the upgrade.** `Dockerfile:44`'s
from-scratch branch runs a bare `cargo build --release` with no `JEMALLOC_*`
environment. CI's cross artifacts *are* protected — `build.yml:127` and `:201`
both set `JEMALLOC_SYS_WITH_LG_PAGE=16` — but the Docker path is not. So an
arm64 image built from scratch (buildx/QEMU on a 4K-page host) compiles jemalloc
with `LG_PAGE=12`, and jemalloc's page size must be at least the system's, so
that binary **aborts at startup on a 16K-page arm64 kernel — a Raspberry Pi 5.**
This is a live bug in the current tree; it has gone unnoticed because the
released artifacts come from the protected CI path. Add
`JEMALLOC_SYS_WITH_LG_PAGE=16` to that Dockerfile branch.

**The 0.7 upgrade is a net improvement on exactly this point**, which reverses
the obvious assumption that it is pure risk. jemalloc 5.3.1's configure adds:

```
aarch64-unknown-linux-*)  if test "x$LG_PAGE" = "xdetect"; then LG_PAGE=16 ; fi
```

5.3.0 has no such case and falls back to `lg_page=12` for cross builds. So 0.7
makes the safe value the *default* for aarch64-linux rather than something the
build environment has to remember to set.

**Pin carefully: `tikv-jemalloc-sys` 0.7.0 is yanked** (published 2026-05-25
17:11, yanked, and 0.7.1 published 13 minutes later). `tikv-jemallocator` 0.7.0
requires `^0.7.0` so a fresh resolve picks 0.7.1+5.3.1 on its own — but assert it
in the lockfile rather than trusting a stale one. MSRV is 1.71.0, well under our
1.88.

No Rust source change: `tikv_jemallocator::Jemalloc` is unchanged, so
`src/main.rs` is untouched.

**The verification that substitutes for hardware you may not have:** 0.7's build
script prints `CC=`, `CFLAGS=`, `LDFLAGS=`, `CPPFLAGS=`, and dumps the full
`config.log` on failure when `CI` is set. Run the cross job and read the log for
each of the four targets, confirming (a) `CC` is the prefixed cross compiler
(`arm-linux-gnueabihf-gcc`, `aarch64-linux-gnu-gcc`, `i686-linux-gnu-gcc`) and
not the host `cc`, and (b) configure reports `LG_PAGE : 16`. Those two lines
retire most of the residual risk.

Then, before any release: run the CI arm64 artifact on a 16K-page Pi 5 (check
with `getconf PAGESIZE` → 16384). `neolink --version` alone proves the allocator
initialised, since that happens before `main`. Follow with a soak test sampling
RSS against a 0.5.4 baseline on the same hardware — this is a memory-behaviour
change, and a startup check cannot detect RSS creep.

Land it as its own PR, not batched.

### `heck` — already current, but never bump it casually

`heck` 0.5.0 is the latest, so there is nothing to do. Recording it here because
it is **on a wire-visible path** and that is not obvious: `to_title_case()`
builds Home Assistant MQTT discovery payloads at `src/mqtt/discovery.rs:274`
(the device `friendly_name`) and `:501`. A future change to heck's title-casing
would compile cleanly and silently **rename entities in users' Home Assistant
installs**, breaking their dashboards and automations. If heck ever does move,
diff the generated discovery JSON before and after.

## Phase 4 — recommended against, for now

### `gstreamer` 0.25.x

The API cost is the same single `has_property` line as 0.24 — measured, the
0.25 probe produces the identical one error. The blocker is **MSRV 1.92**, a
ten-minor jump from our 1.88 and very recent. Since 0.25 offers nothing this
project uses over 0.24.5, take 0.24.5 now and revisit 0.25 when 1.92 is
comfortably old. Note the version skew if you do: `gstreamer-rtsp` has only
0.25.0 while the others are at 0.25.2/0.25.3, so a single `"0.25"` requirement
is the way to express it.

### `fcm-push-listener` 4.1.1 — take 3.0.0 instead, and treat it as urgent

**See the dedicated section above.** In short: 4.1.1 is a genuine rewrite
(17 error lines, `reqwest` 0.13 added, token file incompatible, listener loop
rebuilt, `cmake`/AWS-LC needed in CI for the cross targets), and 3.0.0 fixes the
actual problem for almost nothing. Only reach for 4.x if something specific
requires it.

## Phase 5 — unmaintained dependencies and hygiene

These are not version upgrades; they are dependencies that should be replaced or
dropped. None is urgent, all are low-risk, and each shrinks the tree.

### `get_if_addrs` 0.5.3 — last published 2018

Used in `crates/core` and `crates/mailnoti`. Call sites:
`crates/core/src/bc_protocol/connection/discovery.rs:1234-1250` (finds the first
non-loopback IPv4 interface, and separately iterates IPv4 interfaces for
broadcast/netmask), plus `crates/mailnoti`. Candidate replacements are
`if-addrs`, `local-ip-address` and `network-interface`. Any replacement must keep
working on Linux, macOS and Windows, since all three are CI targets, and must
preserve the broadcast-address computation.

### `async-std` 1.13.2 — discontinued upstream

Used only by `crates/mailnoti`, and likely forced by `mailin-embedded` rather
than chosen. `mailnoti` already depends on tokio with `features = ["full"]`, so
if `mailin-embedded` has a tokio-compatible mode the dependency can simply go.
If it does not, the question becomes whether to replace `mailin-embedded`.

### `lazy_static` 1.5.0 → `std::sync::LazyLock`

Used in `crates/core`, `crates/mailnoti` and `crates/pushnoti` (one known block
at `crates/core/src/bc_protocol/connection/discovery.rs:62`). `LazyLock`
stabilised in Rust 1.80 and our MSRV is now 1.88, so std covers this and the
dependency can be dropped outright.

### `once_cell` 1.21.4 → `std::sync::OnceLock`/`LazyLock`

Same argument. Check each use — once_cell's `Lazy` and std's `LazyLock` differ
slightly, and once_cell's non-thread-safe `OnceCell` has no exact std
equivalent — but most uses are likely replaceable.

### `hex-string` 0.1.0 — last published 2019

Used only in `crates/decoder/src/main.rs`. Replace with `hex`, `const-hex`, or
plain std formatting.

### CI repair (independent of any dependency)

Found while validating this work: five CI jobs fail for reasons unrelated to
dependencies, all in setup steps that run before `cargo`.

1. **macOS GStreamer download is a dead URL.** `build.yml` hardcodes
   `https://gstreamer.freedesktop.org/data/pkg/osx/1.20.4/gstreamer-1.0-devel-1.20.4-universal.pkg`,
   which returns HTTP 404 with a 272-byte error page that `installer` rejects.
   Upstream no longer has 1.20.4; **1.22.12 still returns 200** and satisfies
   the `v1_20` feature.
2. **`macos-12` is a retired runner image** — that job never starts. Move to
   `macos-13`/`macos-14`.
3. **Windows GStreamer path detection** fails at
   `dir "$env:GSTREAMER_1_0_ROOT_MSVC_X86_64"` with
   `Cannot find path 'D:\gstreamer\1.0\msvc_x86_64\'`. Also `openssl` is pinned
   to `1.1.1.2100`, EOL since 2023.
4. **`dockerprune.yml`'s secret guard does not work.**
   `if: ${{ steps.vars.outputs.HAS_SECRET_TOKEN }}` is truthy for the *string*
   `"false"`, so the step runs without a token and fails with
   `jq: error … Cannot iterate over null`. It needs
   `== 'true'`. Its job is also confusingly named `native`, colliding with
   `build.yml`.
5. **`Build Docker image`** fails at `docker/login-action@v3` with
   `Password required` — an unset fork secret.

Also worth considering: `cargo deny` has a fully populated `deny.toml` but does
not appear to run in any workflow. A dependency-heavy project with a
configured-but-unrun `deny.toml` gets no benefit from it.

## Inventory: current state of every direct dependency

`current` reflects this branch after the two landed commits.

| Crate | Current | Latest | Recommendation | Effort |
|---|---|---|---|---|
| anyhow | 1.0.104 | 1.0.104 | current | — |
| assert_matches | 1.5.0 | 1.5.0 | current | — |
| axum | 0.7.9 | 0.8.9 | Phase 1 | 7 strings |
| base64 | 0.23.0 | 0.23.0 | landed | — |
| byte-slice-cast | 1.2.3 | 1.2.3 | current | — |
| bytes | 1.12.1 | 1.12.1 | current | — |
| cfb-mode | 0.8.2 | 0.9.1 | Phase 2, with aes | medium |
| chrono | 0.4.45 | 0.4.45 | current | — |
| clap | 4.6.4 | 4.6.4 | current | — |
| cookie-factory | 0.3.3 | 0.3.3 | current | — |
| crc32fast | 1.5.0 | 1.5.0 | current | — |
| crossbeam-channel | 0.5.16 | 0.5.16 | current | — |
| aes | 0.8.4 | 0.9.2 | Phase 2, with cfb-mode | 29 errors |
| delegate | 0.13.5 | 0.13.5 | landed | — |
| dirs | 6.0.0 | 6.0.0 | landed | — |
| env_logger | 0.11.11 | 0.11.11 | current, now uniform | — |
| fcm-push-listener | 2.0.3 | 4.1.1 | **Phase 0 — go to 3.0.0** | low |
| futures | 0.3.33 | 0.3.33 | current | — |
| get_if_addrs | 0.5.3 | 0.5.3 | Phase 5 — replace | low |
| gstreamer\* (×4) | 0.23.5/0.23.7 | 0.25.x | Phase 1 → 0.24.5 | 1 line |
| heck | 0.5.0 | 0.5.0 | current — wire-visible, never batch | — |
| hex-string | 0.1.0 | 0.1.0 | Phase 5 — replace | low |
| indoc | 2.0.7 | 2.0.7 | current | — |
| lazy_static | 1.5.0 | 1.5.0 | Phase 5 — drop for std | low |
| log | 0.4.33 | 0.4.33 | current | — |
| mailin-embedded | 0.8.3 | 0.8.3 | current; see async-std | — |
| md5 | 0.8.1 | 0.8.1 | landed | — |
| nom | 7.1.3 | 8.0.0 | Phase 2 — recommend defer | 42 errors |
| once_cell | 1.21.4 | 1.21.4 | Phase 5 — drop for std | low |
| percent-encoding | 2.3.2 | 2.3.2 | current | — |
| quick-xml | 0.36.2 | 0.41.0 | Phase 1, after P2 | 9 sites |
| rand | 0.8.7 | 0.10.2 | Phase 1 — also dedupes | ~6 sites |
| regex | 1.13.1 | 1.13.1 | current | — |
| requestty | 0.6.3 | 0.6.3 | landed | — |
| rumqttc | 0.24.0 | 0.25.1 | Phase 3 | behavioural |
| serde | 1.0.229 | 1.0.229 | current | — |
| serde_json | 1.0.151 | 1.0.151 | current | — |
| sha1 | 0.11.0 | 0.11.0 | landed | — |
| socket2 | 0.6.5 | 0.6.5 | landed | — |
| thiserror | 2.0.19 | 2.0.19 | landed | — |
| tikv-jemallocator | 0.5.4 | 0.7.0 | Phase 3 — fixes a Pi 5 bug | medium |
| time | 0.3.54 | 0.3.54 | current; sets MSRV 1.88 | — |
| tokio | 1.53.1 | 1.53.1 | current | — |
| tokio-stream | 0.1.19 | 0.1.19 | current | — |
| tokio-util | 0.7.19 | 0.7.19 | current | — |
| toml | 0.8.23 | 1.1.4 | Phase 3 — verified safe | trivial |
| uuid | 1.24.0 | 1.24.0 | current | — |
| validator | 0.18.1 | 0.21.0 | Phase 3 — verified safe | trivial |
| async-std | 1.13.2 | 1.13.2 | Phase 5 — discontinued | medium |

## Suggested order

1. **`fcm-push-listener` → 3.0.0** — the only item that fixes something already
   broken for users. Cheap, and most of the code is written. Confirm the VAPID
   key against a real camera.
2. **P1, P2, P3** — the three prerequisite tests. Cheap, independently useful,
   and they are what make the rest verifiable.
3. **gstreamer → 0.24.5** — one line, biggest version gap closed.
4. **`toml` → 1.1.4 and `validator` → 0.21.0** — one commit, no source edits,
   both verified behaviourally identical.
5. **axum → 0.8.9** — seven strings; needs P3 to be meaningful.
6. **rand → 0.10.2** — ~6 sites, and it shrinks the lockfile.
7. **quick-xml → 0.41** — needs P2; do not shortcut step 3.
8. **aes + cfb-mode → 0.9** — needs P1; single atomic commit, pin ≥ 0.9.1.
9. **CI repair** — independent of everything, can go any time.
10. **Phase 5 hygiene** — `lazy_static`/`once_cell` → std first (easiest),
    then `get_if_addrs`, then `hex-string`.
11. **`rumqttc` → 0.25.1** — the one Phase 3 item still needing a behavioural
    check.
12. Leave **`nom` 8**, **`fcm-push-listener` 4.x** (3.0.0 is step 1; 4.x is a
    separate, much larger job), **`gstreamer` 0.25** and
    **`tikv-jemallocator` 0.7** alone until there is a reason. Each carries a
    failure mode the compiler will not catch. If nom 8 is eventually attempted,
    give it a dedicated session and treat every `.parse(` insertion as a
    frame-boundary decision, not a rename.
