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
          xcodeXcrun = pkgs.writeShellScriptBin "xcrun" ''
            unset DEVELOPER_DIR
            exec /usr/bin/xcrun "$@"
          '';
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
                name = "grimoire";
                desktopName = "Grimoire";
                genericName = "Peer-to-peer chat";
                comment = "Private peer-to-peer text, file, and voice communities";
                exec = "grimoire";
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
            pname = "grimoire";
            strictDeps = true;
            version = "0.0.1";
            meta.mainProgram = "grimoire";
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.pkg-config
              pkgs.protobuf
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.makeWrapper ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ xcodeXcrun ];
            buildInputs = [
              pkgs.libopus
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.alsa-lib ]
            ++ desktopRuntimeLibraries
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.apple-sdk ];
            postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              if [ -x "$out/bin/grimoire" ]; then
                wrapProgram "$out/bin/grimoire" \
                  --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath desktopRuntimeLibraries}
              fi
              mkdir -p "$out/share/applications"
              cp ${desktopItem}/share/applications/grimoire.desktop "$out/share/applications/"
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
              pname = "grimoire";
              version = "0.0.1";
            };
            test = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoNextestExtraArgs = "--workspace";
              }
            );
          };

          devShell = craneLib.devShell {
            LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.isLinux (
              pkgs.lib.makeLibraryPath desktopRuntimeLibraries
            );
            packages = [
              pkgs.cmake
              pkgs.cargo-nextest
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
              export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-''${XDG_CACHE_HOME:-$HOME/.cache}/grimoire/target}"
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
