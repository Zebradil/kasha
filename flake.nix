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
      nixosModules.default = self.nixosModules.box;

      devShells = forAllSystems ({ pkgs, ... }: {
        default = pkgs.mkShellNoCC {
          packages = [ pkgs.shellcheck pkgs.shfmt pkgs.actionlint pkgs.jq pkgs.awscli2 pkgs.nixpkgs-fmt ];
        };
      });

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
        });

      formatter = forAllSystems ({ pkgs, ... }: pkgs.nixpkgs-fmt);
    };
}
