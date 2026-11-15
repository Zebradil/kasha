{
  description = "kasha — net-local Nix binary cache";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs =
    { self
    , nixpkgs
    , crane
    ,
    }:
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
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./src
        ];
      };
      # Dependencies build in their own derivation from crane's dummy source.
      # crane keeps kasha's own version in that source, so a release bump would
      # still rebuild every dependency; it is stripped from the dummy Cargo.toml
      # and Cargo.lock (both then read 0.0.0) so only kasha recompiles. Cargo.lock
      # is left out of `depsSrc` because crane copies it from there verbatim.
      version = (lib.importTOML ./Cargo.toml).package.version;
      lockVersionLine = v: "name = \"kasha\"\nversion = \"${v}\"\n";
      lock = builtins.readFile ./Cargo.lock;
      depsLock = builtins.replaceStrings [ (lockVersionLine version) ] [ (lockVersionLine "0.0.0") ] lock;
      depsSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./src
        ];
      };
      mkCrane =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          cargoVendorDir = craneLib.vendorCargoDeps { inherit src; };
          cargoArtifacts =
            assert lib.assertMsg (depsLock != lock) "kasha's version not found in Cargo.lock";
            craneLib.buildDepsOnly {
              pname = "kasha";
              version = "0.0.0";
              inherit cargoVendorDir;
              dummySrc = craneLib.mkDummySrc {
                src = depsSrc;
                cargoLock = builtins.toFile "Cargo.lock" depsLock;
                cleanCargoTomlFilter = path: craneLib.filters.cargoTomlDefault path && path != [ "package" "version" ];
              };
            };
        in
        {
          inherit craneLib cargoArtifacts;
          kasha = craneLib.buildPackage { inherit src cargoArtifacts cargoVendorDir; };
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
          kasha = (mkCrane pkgs).kasha;
          default = self.packages.${system}.kasha;

          # The binary this flake publishes to its cache for `system`, and the
          # one consumers should substitute rather than rebuild. Linux resolves
          # to the musl static build, because that is what the OCI image ships —
          # musl's stub resolver and lack of NSS are exactly the differences a
          # glibc build would hide. Darwin has no static build and uses the
          # native one, which is the producer path there anyway.
          #
          # One attr name that is correct on every system, so a consumer (the
          # emit-manifest action below) can ask for it without knowing which
          # flavour its runner needs. Named here rather than inlined at each use
          # so CI pushes and consumers resolve the same derivation by
          # construction: a second copy of this expression could drift, and the
          # symptom would be a silent full rebuild on every consumer run.
          kasha-bin = self.packages.${system}.kasha-static or self.packages.${system}.kasha;

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
          kasha-static = (mkCrane pkgs.pkgsStatic).kasha;
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
          # Test exactly the artefact the cache serves, never a sibling build of
          # the same source.
          tested = self.packages.${system}.kasha-bin;
          inherit (mkCrane pkgs) craneLib cargoArtifacts;
        in
        {
          # cargo test runs in the package's checkPhase.
          build = tested;

          # writeShellApplication shellchecks the script in its build.
          kasha-cache-push = self.packages.${system}.kasha-cache-push;

          fmt = craneLib.cargoFmt { inherit src; };
          clippy = craneLib.cargoClippy {
            inherit src cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
          };

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
