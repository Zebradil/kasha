{
  description = "kasha — net-local Nix binary cache (box read path)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: lib.genAttrs systems (system: f {
        inherit system;
        pkgs = nixpkgs.legacyPackages.${system};
      });
    in
    {
      nixosModules.box = import ./modules/box.nix;
      nixosModules.consumer = import ./modules/consumer.nix;
      nixosModules.default = self.nixosModules.box;

      devShells = forAllSystems ({ pkgs, ... }: {
        default = pkgs.mkShellNoCC {
          packages = [ pkgs.shellcheck pkgs.shfmt pkgs.actionlint pkgs.jq pkgs.awscli2 pkgs.nixpkgs-fmt ];
        };
      });

      packages = forAllSystems ({ system, pkgs }:
        lib.optionalAttrs (lib.hasSuffix "linux" system) (
          let
            mkScript = name: file: runtimeInputs: pkgs.writeShellApplication {
              inherit name runtimeInputs;
              text = builtins.readFile file;
            };
            checkStoreFs = mkScript "kasha-check-store-fs" ./scripts/check-store-fs.sh [ pkgs.coreutils ];
            mirrorDown = mkScript "kasha-mirror-down" ./scripts/mirror-down.sh [ pkgs.awscli2 pkgs.coreutils pkgs.gnused pkgs.jq pkgs.nix pkgs.util-linux ];
            mirrorUp = mkScript "kasha-mirror-up" ./scripts/mirror-up.sh [ pkgs.awscli2 pkgs.coreutils pkgs.gnused pkgs.jq pkgs.nix pkgs.util-linux ];
            ociEntrypoint = pkgs.writeShellApplication {
              name = "oci-entrypoint";
              runtimeInputs = [ pkgs.bash pkgs.coreutils pkgs.curl pkgs.gnused pkgs.harmonia pkgs.nix pkgs.openssh pkgs.shadow pkgs.util-linux checkStoreFs mirrorDown mirrorUp ];
              text = builtins.readFile ./scripts/oci-entrypoint.sh;
            };
          in
          {
            oci-image = pkgs.dockerTools.streamLayeredImage {
              name = "ghcr.io/zebradil/kasha-box";
              tag = "dev";
              contents = [
                pkgs.bashInteractive
                pkgs.cacert
                pkgs.coreutils
                pkgs.curl
                pkgs.gnused
                pkgs.harmonia
                pkgs.nix
                pkgs.openssh
                pkgs.shadow
                ociEntrypoint
                checkStoreFs
                mirrorDown
                mirrorUp
              ];
              config = {
                Entrypoint = [ "${ociEntrypoint}/bin/oci-entrypoint" ];
                ExposedPorts = {
                  "5000/tcp" = { };
                  "22/tcp" = { };
                };
                Env = [
                  "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
                  "NIX_CONFIG=experimental-features = nix-command flakes"
                ];
                Volumes = { "/kasha" = { }; };
              };
            };
          }
        ));

      checks = forAllSystems ({ system, pkgs }:
        {
          shellcheck = pkgs.runCommand "shellcheck"
            { nativeBuildInputs = [ pkgs.shellcheck ]; } ''
            shellcheck ${./scripts}/*.sh ${./tests}/run.sh
            touch $out
          '';

          actionlint = pkgs.runCommand "actionlint"
            { nativeBuildInputs = [ pkgs.actionlint pkgs.shellcheck ]; } ''
            actionlint -color ${./.github/workflows}/*.yml
            touch $out
          '';

          # The env-in -> stdout-out fixture pattern every reusable tool uses.
          fixtures = pkgs.runCommand "fixture-tests"
            { nativeBuildInputs = [ pkgs.bash pkgs.coreutils pkgs.jq ]; } ''
            # Copy writable so patchShebangs can fix fixture fakes' `/usr/bin/env`
            # shebang — it doesn't exist in the sandbox, and fakes are exec'd via PATH.
            mkdir src
            cp -r ${self}/tests ${self}/scripts src/
            chmod -R u+w src
            cd src
            patchShebangs tests/fixtures
            bash tests/run.sh
            touch $out
          '';
        }
        # Seed -> serve -> substitute -> verify round-trip. NixOS VM, Linux only.
        // lib.optionalAttrs (lib.hasSuffix "linux" system) {
          smoke = import ./tests/smoke.nix {
            inherit pkgs;
            boxModule = self.nixosModules.box;
          };

          # Reverse flow: ssh-ng push -> serve immediately (issue #4).
          push = import ./tests/push.nix {
            inherit pkgs;
            boxModule = self.nixosModules.box;
          };

          # Selection: on-LAN read from box, off-LAN fall back to remote (issue #5).
          selection = import ./tests/selection.nix {
            inherit pkgs;
            boxModule = self.nixosModules.box;
            consumerModule = self.nixosModules.consumer;
          };

          # Down replica: discover remote roots, pull closures into box (issue #6).
          mirror-down = import ./tests/mirror-down.nix {
            inherit pkgs;
            boxModule = self.nixosModules.box;
          };

          # Up replica: discover box-local roots, push closures to remote (issue #8).
          mirror-up = import ./tests/mirror-up.nix {
            inherit pkgs;
            boxModule = self.nixosModules.box;
          };
        });

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
