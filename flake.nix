{
  description = "kasha — net-local Nix binary cache";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems =
        f:
        lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
          }
        );
      mkKasha =
        rustPlatform:
        rustPlatform.buildRustPackage {
          pname = "kasha";
          version = (lib.importTOML ./Cargo.toml).package.version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
        };
    in
    {
      nixosModules = {
        consumer = import ./modules/consumer.nix;
        default = self.nixosModules.consumer;
      };

      packages = forAllSystems (
        { system, pkgs }:
        {
          kasha = mkKasha pkgs.rustPlatform;
          default = self.packages.${system}.kasha;
        }
        // lib.optionalAttrs (lib.hasSuffix "linux" system) rec {
          # Static musl build: the only thing the OCI image ships.
          kasha-static = mkKasha pkgs.pkgsStatic.rustPlatform;
          oci-image = pkgs.dockerTools.streamLayeredImage {
            name = "ghcr.io/zebradil/kasha-box";
            tag = "dev";
            contents = [
              kasha-static
              pkgs.cacert
            ];
            config = {
              Entrypoint = [ "/bin/kasha" ];
              Cmd = [ "serve" ];
              ExposedPorts."5000/tcp" = { };
              Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
              Volumes."/kasha" = { };
            };
          };
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = pkgs.mkShellNoCC {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.actionlint
              pkgs.nixpkgs-fmt
            ];
          };
        }
      );

      checks = forAllSystems (
        { system, pkgs }:
        {
          # cargo test runs in the package's checkPhase.
          build = self.packages.${system}.kasha;

          actionlint =
            pkgs.runCommand "actionlint"
              {
                nativeBuildInputs = [
                  pkgs.actionlint
                  pkgs.shellcheck
                ];
              }
              ''
                actionlint -color ${./.github/workflows}/*.yml
                touch $out
              '';
        }
        # Real nix client against the server: signed push (nix copy --to),
        # manifest emit, substitute pull with sig gate. NixOS VM, Linux only.
        // lib.optionalAttrs (lib.hasSuffix "linux" system) {
          integration = import ./tests/v2.nix {
            inherit pkgs;
            kasha = self.packages.${system}.kasha;
          };
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
