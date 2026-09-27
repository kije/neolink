{
  description = "Neolink: a standards-compliant bridge to Reolink IP cameras";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      rev = self.shortRev or self.dirtyShortRev or null;
    in
    {
      # For consumers who want neolink built against their own nixpkgs:
      #   nixpkgs.overlays = [ neolink.overlays.default ];
      overlays.default = final: _prev: {
        neolink = final.callPackage ./nix/package.nix { inherit rev; };
        neolink-tools = final.callPackage ./nix/package.nix {
          inherit rev;
          tools = true;
        };
      };

      packages = forAllSystems (
        pkgs:
        let
          neolinkPkgs = self.overlays.default pkgs pkgs;
        in
        neolinkPkgs // { default = neolinkPkgs.neolink; }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.neolink;
          meta.description = "Run the neolink bridge";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust toolchain. nixpkgs tracks stable, which is >= the MSRV in Cargo.toml.
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-deny

            # Native deps for every feature combination of the workspace.
            pkg-config
            protobuf
            openssl
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            gst_all_1.gst-rtsp-server

            pre-commit
          ];

          # `cargo run` finds the same GStreamer plugins as the packaged binary.
          GST_PLUGIN_SYSTEM_PATH_1_0 =
            lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0"
              self.packages.${pkgs.stdenv.hostPlatform.system}.neolink.passthru.gstPlugins;
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          # Debug builds compile tikv-jemalloc-sys at -O0, where the fortify
          # warning trips jemalloc's configure probes (-Werror) and breaks the build.
          hardeningDisable = [ "fortify" ];

          # .cargo/config.toml sets cross linkers such as `aarch64-linux-gnu-gcc`,
          # which also apply to native builds on that triple. Use the shell's cc.
          "CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_LINKER" = "cc";
        };
      });

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) neolink neolink-tools;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
