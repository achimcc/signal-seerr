{
  description = "Request movies and series through Seerr from a Signal chat";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  # The RustSec advisory database, pinned like any other input. The `audit`
  # check reads it offline; `nix flake update advisory-db` brings news in.
  inputs.advisory-db = {
    url = "github:rustsec/advisory-db";
    flake = false;
  };

  outputs =
    { self, nixpkgs, advisory-db }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in
    {
      packages = forAll (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "signal-seerr";
          # Read out of Cargo.toml rather than written down a second time.
          # The 0.1.1 bump changed Cargo.toml alone and this line kept
          # saying 0.1.0, so the store path disagreed with the crate about
          # what it was -- and nothing compared the two, because nothing
          # could. A derived value cannot drift; a guard against drift can
          # itself be forgotten.
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;
          src = self;
          # No hash to keep in step with the sources: the lock file IS the input.
          cargoLock.lockFile = ./Cargo.lock;
          # aws-lc-sys, pulled in by reqwest's `rustls` feature, builds a C
          # library with cmake. reqwest 0.13 has no ring-backed alternative
          # feature, so this is the price of having TLS available at all.
          nativeBuildInputs = [ pkgs.cmake ];
          # reqwest's `rustls` feature pulls rustls-platform-verifier, which
          # reads the system certificate store the moment a Client is built --
          # so even a test that only talks plain HTTP to a local mock trips it.
          # The sandbox has no such store. reqwest 0.13 offers no bundled-roots
          # feature to sidestep this (checked against its full feature list),
          # so the store is handed to the build instead.
          SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
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
        # Known advisories against Cargo.lock, read offline from the pinned
        # database. RUSTSEC-2026-0285 (rustls) sat in two deployed binaries of
        # sibling projects for four days before an audit found it by hand.
        audit = pkgs.runCommand "signal-seerr-audit" { nativeBuildInputs = [ pkgs.cargo-audit ]; } ''
          HOME=$TMPDIR cargo-audit audit --no-fetch --db ${advisory-db} --file ${./Cargo.lock}
          touch $out
        '';
        clippy = self.packages.${pkgs.system}.default.overrideAttrs (old: {
          pname = "signal-seerr-clippy";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
          buildPhase = "cargo clippy --all-targets -- -D warnings";
          installPhase = "touch $out";
        });
        fmt = self.packages.${pkgs.system}.default.overrideAttrs (old: {
          pname = "signal-seerr-fmt";
          nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.rustfmt ];
          buildPhase = "cargo fmt --check";
          installPhase = "touch $out";
        });
        vm = import ./nix/test.nix {
          inherit pkgs;
          module = self.nixosModules.default;
          package = self.packages.${pkgs.system}.default;
        };
      });
    };
}
