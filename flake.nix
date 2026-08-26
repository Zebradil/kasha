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

          # The resolve -> sign -> push core every producer runs. Shipped here
          # so a fix reaches producers through the same flake input that pins
          # `kasha emit`, which this script calls to publish the manifest.
          #
          # KASHA_BIN is deliberately not baked in: producers pass their own
          # emitter path, and hard-wiring one would make this script depend on
          # the Rust build it is often used to publish.
          kasha-cache-push = pkgs.writeShellApplication {
            name = "kasha-cache-push";
            runtimeInputs = with pkgs; [
              git
              coreutils
              gnugrep
              gnused
              findutils
            ];
            text = builtins.readFile ./scripts/cache-push.sh;
          };
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
          default = pkgs.mkShell {
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
        let
          # Linux checks run against the musl static build, because that is the
          # binary the OCI image ships — musl's stub resolver and lack of NSS
          # are exactly the differences a glibc build would hide. Darwin has no
          # static build and tests the native one, which is the producer path.
          tested = self.packages.${system}.kasha-static or self.packages.${system}.kasha;
        in
        {
          # cargo test runs in the package's checkPhase.
          build = tested;

          # writeShellApplication shellchecks the script in its build.
          kasha-cache-push = self.packages.${system}.kasha-cache-push;

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
            kasha = tested;
          };
        }
      );

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
