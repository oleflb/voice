{
  description = "Peer-to-peer terminal voice chat";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forLinux = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
      audioLibraries = pkgs: pkgs.lib.makeLibraryPath [ pkgs.alsa-lib pkgs.libpulseaudio ];
      videoTools = pkgs: [ pkgs.mpv-unwrapped pkgs.gst_all_1.gstreamer ];
      videoPlugins = pkgs: pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (with pkgs.gst_all_1; [
        gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad pkgs.pipewire
      ]);
    in {
      packages = forLinux (system: let pkgs = nixpkgs.legacyPackages.${system}; in {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "voice";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./crates ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
          buildInputs = [ pkgs.aws-lc ];
          postFixup = ''
            wrapProgram $out/bin/voice --prefix LD_LIBRARY_PATH : ${audioLibraries pkgs} \
              --prefix PATH : ${pkgs.lib.makeBinPath (videoTools pkgs)} \
              --prefix GST_PLUGIN_PATH_1_0 : ${videoPlugins pkgs}
          '';
        };
      });
      devShells = forLinux (system: let pkgs = nixpkgs.legacyPackages.${system}; in {
        default = pkgs.mkShell {
          packages = (with pkgs; [ cargo rustc rustfmt clippy cmake lld trunk ]) ++ videoTools pkgs;
          CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
          LD_LIBRARY_PATH = audioLibraries pkgs;
          GST_PLUGIN_PATH_1_0 = videoPlugins pkgs;
        };
      });
    };
}
