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

## Phase 1 — low cost, do these first

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

**Effort: two import lines, plus the call sites they feed.**

Measured, `crates/core`:

- **rand 0.9** compiles with **zero errors** but emits deprecation warnings —
  the old names still exist as deprecated aliases. Landing 0.9 alone would be
  green-but-indebted, and CI's nightly clippy would start reporting it.
- **rand 0.10** gives exactly **two errors**, both the same thing:
  `error[E0432]: unresolved import `rand::thread_rng`` at
  `crates/core/src/bc_protocol/connection/discovery.rs:18` and
  `crates/core/src/bc_protocol/connection/udpsource.rs:13`.

Because the whole delta is two imports and their dependent calls, go straight to
**0.10.2** in one step rather than staging through 0.9 and living with
deprecations. The renames to apply:

| rand 0.8 | rand 0.10 |
|---|---|
| `rand::thread_rng()` | `rand::rng()` |
| `Rng::gen()` | `Rng::random()` |
| `Rng::gen_range(a..b)` | `Rng::random_range(a..b)` |
| `rand::seq::SliceRandom` (for `choose`) | `rand::seq::IndexedRandom` |

Affected sites: `discovery.rs:18` (import), `:1267`, `:1272-1273`, `:1279`;
`udpsource.rs:13` (import), `:384`, `:445`, `:766`, `:783`. These pick protocol
client IDs and UDP ports/sequence numbers, so **confirm the value ranges are
unchanged** while editing — a silently different range would break camera
discovery in a way no current test would catch.

rand 0.10 is edition 2024 with MSRV 1.85, under our 1.88.

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

### `nom`: 7.1.3 → 8.0.0 — the largest single migration

**Effort: 42 errors across three files. Large, but the measurement shows it is
overwhelmingly mechanical.**

Measured error distribution in `neolink_core`:
`crates/core/src/bcudp/de.rs` 34, `crates/core/src/bc/de.rs` 21,
`crates/core/src/bcmedia/de.rs` 13, `crates/core/src/lib.rs` 1.

The useful finding is the *shape* of those errors, not the count. Roughly 50 of
them are one pattern:

```
error[E0618]: expected function, found `Context<fn(_) -> Result<(_, u32), Err<_>> {le_u32::<_, _>}>`
```

In nom 8 the combinators return `impl Parser` rather than a callable closure, so
every `parser(input)` call site becomes `parser.parse(input)`. That is a
find-and-adjust job, not a redesign. The remainder:

- `error[E0425]: cannot find type `VerboseError` in module `nom::error`` ×5 —
  `VerboseError` was removed; `NomErrorType` in `crates/core/src/lib.rs` and
  the error plumbing in `crates/core/src/bc_protocol/errors.rs` need a
  replacement error type.
- `error[E0282]: type annotations needed` ×7 — fallout from the `Parser` trait
  change; resolves as the call sites are fixed.
- one `error[E0107]: trait takes 1 generic argument but 3 generic arguments were
  supplied` — the `Parser` trait's generics were reduced to the input type with
  `Output`/`Error` as associated types.

The genuinely risky part is not the renames: this codebase drives
`tokio_util` codecs off `Err::Incomplete` from **streaming** parsers
(`crates/core/src/bc/codex.rs`, `crates/core/src/bcmedia/codex.rs`,
`crates/core/src/bcudp/codex.rs`). Any change in incomplete-input semantics
would show up as a stalled or mis-framed camera stream rather than a compile
error. Budget time for that specifically.

The safety net here is good: 64 tests in `neolink_core`, many of them
round-tripping captured protocol samples via `include_bytes!`. Run them
obsessively during this migration.

**This is the one upgrade where "do nothing" is a legitimate answer.** nom 7 has
no advisory and still works. If it is deferred, the cost is being one major
behind on a dependency that is otherwise invisible to users. Do it when there is
appetite for a focused session, not as part of a batch.

## Phase 3 — compiles clean, but the behaviour needs checking

Every item in this phase produced **zero compile errors** in the probes. That
means the compiler has nothing to tell us and the entire question is behavioural
— which is why they are not in Phase 1 despite looking cheap.

### `toml`: 0.8.23 → 1.1.4

Compiles clean across all three crates that use it. The concern is that
`toml::to_string` output is user-visible in three places:

- `src/mqtt/mod.rs:199` and `:206` — serialises the live `Config` and
  **publishes it over MQTT**, so a formatting change is visible to Home
  Assistant and any other subscriber.
- `src/common/pushnoti.rs:118` and `crates/pushnoti/src/main.rs:71` — writes the
  saved FCM `Registration` token file, read back with `toml::from_str` at
  `src/common/pushnoti.rs:110` and `crates/pushnoti/src/main.rs:57`.

toml 0.9 was a substantial rewrite onto `toml_parser`/`toml_writer`. Before
landing this, diff the emitted text for both shapes and confirm a token file
written by 0.8 still parses under 1.x.

### `validator`: 0.18.1 → 0.21.0

Compiles clean at 0.19 and at 0.21, across all three crates. The question is
whether the **validation error text or structure users see** changes, since
config validation failures are surfaced to the user at startup. Review the
derives in `src/config.rs` and `crates/mailnoti/src/config.rs` and compare the
rendered error for a deliberately invalid config before and after.

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

### `tikv-jemallocator`: 0.5.4 → 0.7.0

Compiles clean with no new warnings — **but only for the host target**, and that
is the whole problem. jemalloc is a C library built by a build script and is
historically the most fragile thing in a cross build. CI cross-compiles four
Linux targets (`x86_64`, `armv7-gnueabihf`, `aarch64`, `i686`), and neolink is
commonly run on Raspberry Pi and similar boards.

Before landing, confirm: that jemalloc-sys as vendored by 0.6/0.7 still builds
for armv7/aarch64/i686; the aarch64 **page-size** issue (jemalloc built on a
4K-page host can fail at runtime on a 16K/64K-page aarch64 kernel, controlled by
`--with-lg-page`); and whether 0.7 needs a newer C toolchain than the cross
containers provide. Smoke-test the resulting binary on real ARM hardware, not
just in the cross container.

Given that the only benefit is being current on an allocator, this is a
reasonable one to defer — or to reconsider whether the dependency is needed at
all.

## Phase 4 — recommended against, for now

### `gstreamer` 0.25.x

The API cost is the same single `has_property` line as 0.24 — measured, the
0.25 probe produces the identical one error. The blocker is **MSRV 1.92**, a
ten-minor jump from our 1.88 and very recent. Since 0.25 offers nothing this
project uses over 0.24.5, take 0.24.5 now and revisit 0.25 when 1.92 is
comfortably old. Note the version skew if you do: `gstreamer-rtsp` has only
0.25.0 while the others are at 0.25.2/0.25.3, so a single `"0.25"` requirement
is the way to express it.

### `fcm-push-listener` 2.0.3 → 4.1.1

**Effort: high, and the benefit is unclear.** Measured: 17 error lines. This is
a genuine API rewrite, not a rename:

- `fcm_push_listener::register(sender_id)` at `crates/pushnoti/src/main.rs:63`
  now "takes 5 arguments but 1 argument was supplied"
- the type `FcmPushListener` (`crates/pushnoti/src/main.rs:84`) no longer exists
- the type `FcmMessage` (`:86`) no longer exists
- the `Error` variants matched at `src/common/pushnoti.rs:201`
  (`MissingMessagePayload`, `MissingCryptoMetadata`, `ProtobufDecode`,
  `Base64Decode`) no longer all exist

The commented-out constants at `src/common/pushnoti.rs:70-76`
(`firebase_app_id`, `firebase_project_id`, `firebase_api_key`, `vapid_key`)
strongly suggest the newer API wants the full Firebase credential set, which
raises the question of whether Reolink's actual credentials are even usable with
the new registration flow.

Two further considerations before anyone attempts this:

- **The persisted credential format may change.** The `Registration` struct is
  written to disk as TOML and read back on startup. If its shape changed in
  3.x/4.x, every existing user's saved token becomes unreadable and each camera
  must re-register. That needs an explicit migration path or a re-registration
  prompt, not a silent failure.
- **It currently costs us our MSRV.** `fcm-push-listener` 2.0.3's build script
  pulls `prost-build` → `which` → `home` 0.5.12, and `home` is one of the two
  crates forcing MSRV 1.88. If 4.x drops `prost-build`, upgrading would
  *lower* our MSRV floor — which is an argument in favour, if the credential
  question can be answered.

Recommendation: leave on 2.0.3. Revisit if Google deprecates the registration
endpoint 2.0.3 uses, which is the failure that would force the issue.

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
| fcm-push-listener | 2.0.3 | 4.1.1 | Phase 4 — hold | high |
| futures | 0.3.33 | 0.3.33 | current | — |
| get_if_addrs | 0.5.3 | 0.5.3 | Phase 5 — replace | low |
| gstreamer\* (×4) | 0.23.5/0.23.7 | 0.25.x | Phase 1 → 0.24.5 | 1 line |
| heck | 0.5.0 | 0.5.0 | current | — |
| hex-string | 0.1.0 | 0.1.0 | Phase 5 — replace | low |
| indoc | 2.0.7 | 2.0.7 | current | — |
| lazy_static | 1.5.0 | 1.5.0 | Phase 5 — drop for std | low |
| log | 0.4.33 | 0.4.33 | current | — |
| mailin-embedded | 0.8.3 | 0.8.3 | current; see async-std | — |
| md5 | 0.8.1 | 0.8.1 | landed | — |
| nom | 7.1.3 | 8.0.0 | Phase 2 — or defer | 42 errors |
| once_cell | 1.21.4 | 1.21.4 | Phase 5 — drop for std | low |
| percent-encoding | 2.3.2 | 2.3.2 | current | — |
| quick-xml | 0.36.2 | 0.41.0 | Phase 1, after P2 | 9 sites |
| rand | 0.8.7 | 0.10.2 | Phase 1 | 2 imports |
| regex | 1.13.1 | 1.13.1 | current | — |
| requestty | 0.6.3 | 0.6.3 | landed | — |
| rumqttc | 0.24.0 | 0.25.1 | Phase 3 | behavioural |
| serde | 1.0.229 | 1.0.229 | current | — |
| serde_json | 1.0.151 | 1.0.151 | current | — |
| sha1 | 0.11.0 | 0.11.0 | landed | — |
| socket2 | 0.6.5 | 0.6.5 | landed | — |
| thiserror | 2.0.19 | 2.0.19 | landed | — |
| tikv-jemallocator | 0.5.4 | 0.7.0 | Phase 3 | cross risk |
| time | 0.3.54 | 0.3.54 | current; sets MSRV 1.88 | — |
| tokio | 1.53.1 | 1.53.1 | current | — |
| tokio-stream | 0.1.19 | 0.1.19 | current | — |
| tokio-util | 0.7.19 | 0.7.19 | current | — |
| toml | 0.8.23 | 1.1.4 | Phase 3 | behavioural |
| uuid | 1.24.0 | 1.24.0 | current | — |
| validator | 0.18.1 | 0.21.0 | Phase 3 | behavioural |
| async-std | 1.13.2 | 1.13.2 | Phase 5 — discontinued | medium |

## Suggested order

1. **P1, P2, P3** — the three prerequisite tests. Cheap, independently useful,
   and they are what make the rest verifiable.
2. **gstreamer → 0.24.5** — one line, biggest gap closed.
3. **axum → 0.8.9** — seven strings; needs P3 to be meaningful.
4. **rand → 0.10.2** — two imports.
5. **quick-xml → 0.41** — needs P2; do not shortcut step 3.
6. **aes + cfb-mode → 0.9** — needs P1; single atomic commit.
7. **CI repair** — independent of everything, can go any time.
8. **Phase 5 hygiene** — `lazy_static`/`once_cell` → std first (easiest),
   then `get_if_addrs`, then `hex-string`.
9. **Phase 3 behavioural trio** — `toml`, `validator`, `rumqttc`, one at a time
   with a manual check each.
10. **nom → 8** — its own focused session.
11. Leave `fcm-push-listener`, `gstreamer` 0.25 and `tikv-jemallocator` until
    there is a reason.
