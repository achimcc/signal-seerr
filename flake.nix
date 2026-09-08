{
  description = "Request movies and series through Seerr from a Signal chat";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "signal-seerr";
          version = "0.1.0";
          src = self;
          # No hash to keep in step with the sources: the lock file IS the input.
          cargoLock.lockFile = ./Cargo.lock;
          meta = {
            description = "Request movies and series through Seerr from a Signal chat";
            license = pkgs.lib.licenses.agpl3Only;
            mainProgram = "signal-seerr";
          };
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy signal-cli ];
        };
      });

      nixosModules.default = ./nix/module.nix;

      checks = forAll (pkgs: {
        package = self.packages.${pkgs.system}.default;
        clippy = self.packages.${pkgs.system}.default.overrideAttrs (old: {
          pname = "signal-seerr-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --all-targets -- -D warnings";
          installPhase = "touch $out";
        });
      });
    };
}
