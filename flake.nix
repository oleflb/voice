{
  description = "Peer-to-peer terminal voice chat";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      forLinux = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
      audioLibraries = pkgs: pkgs.lib.makeLibraryPath [ pkgs.alsa-lib pkgs.libpulseaudio ];
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
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postFixup = ''
            wrapProgram $out/bin/voice --prefix LD_LIBRARY_PATH : ${audioLibraries pkgs}
          '';
        };
      });
      devShells = forLinux (system: let pkgs = nixpkgs.legacyPackages.${system}; in {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy ];
          LD_LIBRARY_PATH = audioLibraries pkgs;
        };
      });
    };
}
