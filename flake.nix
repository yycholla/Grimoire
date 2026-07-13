{
  description = "Grimoire, a Rust-native peer-to-peer community";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      projects = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          desktopRuntimeLibraries = pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxi
            pkgs.libxkbcommon
            pkgs.libxrandr
            pkgs.wayland
            pkgs.libxcb
            pkgs.vulkan-loader
          ];
          desktopItem = pkgs.lib.optionalString pkgs.stdenv.isLinux (
            toString (
              pkgs.makeDesktopItem {
                name = "peer-community";
                desktopName = "Peer Community";
                genericName = "Peer-to-peer chat";
                comment = "Private peer-to-peer text, file, and voice communities";
                exec = "peer-gpui";
                icon = "internet-chat";
                categories = [
                  "Network"
                  "Chat"
                ];
                terminal = false;
              }
            )
          );
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: craneLib.filterCargoSources path type || pkgs.lib.hasSuffix ".proto" path;
          };
          commonArgs = {
            inherit src;
            pname = "peer-community";
            strictDeps = true;
            version = "0.1.0";
            meta.mainProgram = "peer-gpui";
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.pkg-config
              pkgs.protobuf
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.makeWrapper ];
            buildInputs = [
              pkgs.libopus
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ]
            ++ desktopRuntimeLibraries
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk ];
            postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              if [ -x "$out/bin/peer-gpui" ]; then
                wrapProgram "$out/bin/peer-gpui" \
                  --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath desktopRuntimeLibraries}
              fi
              mkdir -p "$out/share/applications"
              cp ${desktopItem}/share/applications/peer-community.desktop "$out/share/applications/"
            '';
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        rec {
          package = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );

          checks = {
            build = package;
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );
            fmt = craneLib.cargoFmt {
              inherit src;
              pname = "peer-community";
              version = "0.1.0";
            };
            test = craneLib.cargoTest (
              commonArgs
              // {
                inherit cargoArtifacts;
              }
            );
          };

          devShell = craneLib.devShell {
            LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.isLinux (
              pkgs.lib.makeLibraryPath desktopRuntimeLibraries
            );
            packages = [
              pkgs.cmake
              pkgs.libopus
              pkgs.pkg-config
              pkgs.protobuf
              pkgs.rust-analyzer
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ]
            ++ desktopRuntimeLibraries
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk ];
            shellHook = ''
              unset RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER
              export CARGO_TARGET_DIR="''${XDG_CACHE_HOME:-$HOME/.cache}/peer-community/target"
            '';
          };
        }
      );
    in
    {
      packages = forAllSystems (system: {
        default = projects.${system}.package;
      });
      checks = forAllSystems (system: projects.${system}.checks);
      devShells = forAllSystems (system: {
        default = projects.${system}.devShell;
      });
    };
}
