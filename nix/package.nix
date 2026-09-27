# Neolink and its workspace helper binaries, built with nixpkgs' Rust tooling.
#
# Written for `callPackage`, so it works from this repo's flake (packages,
# overlay) and can be tweaked by consumers via `.override { ... }`:
#
#   neolink.override { withPushNotifications = true; }
{
  lib,
  rustPlatform,
  pkg-config,
  makeBinaryWrapper,
  protobuf,
  openssl,
  gst_all_1,

  # Build the `neolink` bridge (default), or the workspace's helper crates
  # (`neolink_decoder`, `mailnoti`, `pushnoti`) instead.
  tools ? false,
  # Cargo feature `gstreamer`: RTSP server, image and talk support.
  withGstreamer ? true,
  # Cargo feature `pushnoti`: FCM push notifications (pulls in openssl, protoc).
  withPushNotifications ? false,
  # Commit reported by `neolink --version`; the flake passes its own rev.
  rev ? null,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;

  # pushnoti is a workspace member, so the tools build always needs its deps.
  needsPushDeps = tools || withPushNotifications;
  needsGstreamer = !tools && withGstreamer;

  cargoPackages =
    if tools then
      [
        "--package=neolink_decoder"
        "--package=mailnoti"
        "--package=pushnoti"
      ]
    else
      [ "--package=neolink" ];

  # Elements neolink instantiates at runtime (appsrc, rtph26xpay, h26xparse,
  # avdec_h26x, x264enc, jpegenc, ...). Everything else is linked directly.
  gstPlugins = with gst_all_1; [
    gstreamer
    gst-plugins-base
    gst-plugins-good
    gst-plugins-bad
    gst-plugins-ugly
    gst-libav
  ];
in
rustPlatform.buildRustPackage {
  pname = if tools then "neolink-tools" else "neolink";
  inherit (cargoToml.package) version;

  # Only what cargo needs, so docs/CI/nix edits do not trigger rebuilds.
  # `.cargo/config.toml` is left out on purpose: its hard-coded cross linkers
  # (e.g. `aarch64-linux-gnu-gcc`) break native builds on those targets; the
  # one setting we need from it, `--cfg tokio_unstable`, is set below.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  buildNoDefaultFeatures = true;
  buildFeatures =
    lib.optional needsGstreamer "gstreamer"
    ++ lib.optional (!tools && withPushNotifications) "pushnoti";

  cargoBuildFlags = cargoPackages;
  cargoTestFlags = cargoPackages;

  nativeBuildInputs = [
    pkg-config
  ]
  ++ lib.optional needsPushDeps protobuf
  ++ lib.optional needsGstreamer makeBinaryWrapper;

  buildInputs =
    lib.optionals needsGstreamer (
      with gst_all_1;
      [
        gstreamer
        gst-plugins-base
        gst-rtsp-server
      ]
    )
    ++ lib.optional needsPushDeps openssl;

  env = {
    RUSTFLAGS = "--cfg tokio_unstable";
  }
  # build.rs uses $GITHUB_SHA verbatim as the version string when set, and
  # otherwise shells out to `git describe`, which cannot work in the sandbox.
  // lib.optionalAttrs (rev != null) {
    GITHUB_SHA = "${cargoToml.package.version} (${rev})";
  };

  # Point GStreamer at the plugins neolink needs, independent of the host.
  postFixup = lib.optionalString needsGstreamer ''
    wrapProgram $out/bin/neolink \
      --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${
        lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins
      }"
  '';

  passthru = { inherit gstPlugins; };

  meta = {
    description =
      if tools then
        "Neolink workspace helpers: AES packet decoder and notification test clients"
      else
        cargoToml.package.description;
    homepage = "https://github.com/QuantumEntangledAndy/neolink";
    license = lib.licenses.agpl3Plus;
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  }
  // lib.optionalAttrs (!tools) { mainProgram = "neolink"; };
}
