# Smoke check for the eager down replica (issue #6): remote root manifest -> box.
#
# New flow (ADR-0003 v2): the box never builds a top-level (cross-system unsafe,
# and no cache holds it). For each {outPath, drvPath} root it copies the .drv
# recipe from the remote, then substitutes the drv's input output-closure minus
# the top output. So here the box must end up with the recipe + the input output
# (dep) valid, and the top output NEVER valid.
{ pkgs, boxModule }:
let
  port = 5001;
  flake = "znix";
  gen = "20260705-demo";
  signPublicKey = "kasha-push-test-1:GjobTMnaEc8bB0ccSdA/vvPLLKMMFAHuq/siqLDVVuM=";
  signSecretKey = "kasha-push-test-1:orF8lRzcbfzQ2ueM6V9/Ij1wB6mDRnkenklqs602JxAaOhtMydoRzxsHRxxJ0D++88ssowwUAe6r+yKosNVW4w==";

  system = pkgs.stdenv.hostPlatform.system;
  bash = "${pkgs.bash}/bin/bash";

  # Minimal derivations (no stdenv): the only build-input outputs are bash's
  # closure — already valid in any NixOS store — plus `dep`. So realising the top
  # drv's input output-closure substitutes exactly `dep` from the remote and
  # leaves everything else untouched, keeping the test's substitution surface tiny.
  remoteDep = derivation {
    name = "kasha-down-dep";
    inherit system;
    builder = bash;
    args = [ "-c" "echo down-dep > $out" ];
  };
  remoteTop = derivation {
    name = "kasha-down-top";
    inherit system;
    builder = bash;
    args = [ "-c" "mkdir -p $out; echo down-mirror > $out/file; ln -s ${remoteDep} $out/dep" ];
  };

  topOut = builtins.unsafeDiscardStringContext remoteTop.outPath;
  topDrv = builtins.unsafeDiscardStringContext remoteTop.drvPath;
  depOut = builtins.unsafeDiscardStringContext remoteDep.outPath;
  depDrv = builtins.unsafeDiscardStringContext remoteDep.drvPath;
  # The .drv as a *source* input (not a to-be-built derivation), so seeding it
  # into the remote store keeps the .drv file itself in the export closure —
  # a bare context-stripped string is not a real input and fails closure export.
  topDrvDep = builtins.unsafeDiscardOutputDependency remoteTop.drvPath;
in
pkgs.testers.runNixOSTest {
  name = "kasha-mirror-down";

  nodes = {
    remote = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = { enable = true; inherit port; };
      nix.settings.experimental-features = [ "nix-command" ];
      # Seed the recipe (.drv closure, so the box can copy it) and the input
      # output (dep, so the box can substitute it). The top output is deliberately
      # NOT seeded: no cache holds it, and the box must never need it.
      virtualisation.additionalPaths = [ topDrvDep remoteDep ];
    };

    box = { pkgs, lib, ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        trustedPublicKeys = [ signPublicKey ];
        mirrorDown = {
          enable = true;
          remoteCache = "s3://kasha-test-cache";
          flakes = [ flake ];
          interval = "1h";
        };
      };
      nix.settings.experimental-features = [ "nix-command" ];
      # The realise step substitutes from the box's substituters; point them at
      # the remote harmonia (mkForce over the module's s3 + FlakeHub defaults,
      # which are unreachable in the VM).
      nix.settings.substituters = lib.mkForce [ "http://remote:${toString port}" ];

      # The real script lists S3 roots. For the VM demo, serve the same seam with
      # static listing + manifest so we test copy behavior without a fake S3 service.
      # Written as a raw string, not builtins.toJSON: toJSON collapses an attrset
      # with an `outPath` key to just its store-path string (Nix path coercion),
      # which would destroy the {outPath, drvPath} object shape.
      environment.etc."kasha-test/manifests/${gen}.json".text =
        ''{"version":2,"flake":"${flake}","gen":"${gen}","timestamp":"2026-07-05T00:00:00Z",''
        + ''"roots":[{"outPath":"${topOut}","drvPath":"${topDrv}"}]}'';
      systemd.services."kasha-mirror-down-${flake}" = {
        path = [ pkgs.coreutils ];
        environment = {
          KASHA_AWS = "${pkgs.writeShellScript "kasha-test-aws" ''
            set -euo pipefail
            if [[ "$1 $2" == "s3api list-objects-v2" ]]; then
              printf '{"Contents":[{"Key":"roots/${flake}/${gen}.json"}]}\n'
            elif [[ "$1 $2" == "s3 cp" ]]; then
              cat /etc/kasha-test/manifests/${gen}.json
            else
              exit 1
            fi
          ''}";
          # Copy the recipe (a .drv, passed as $2) from the remote harmonia.
          KASHA_COPY = "${pkgs.writeShellScript "kasha-test-copy" ''
            set -euo pipefail
            if [[ -e /var/lib/kasha/mirror-down/fail-copy ]]; then
              exit 42
            fi
            mkdir -p /var/lib/kasha/mirror-down
            printf '%s %s\n' "$1" "$2" >> /var/lib/kasha/mirror-down/copies
            exec ${pkgs.nix}/bin/nix copy --from http://remote:${toString port} "$2"
          ''}";
          # Realise seam: the script hands us the recipe's input drvs. Record
          # them, then substitute only dep from the remote. ponytail: the other
          # input (bash's build toolchain) is substitutable from upstream in
          # production but not seeded in this VM, so we don't realise it here —
          # the assertions below prove dep.drv was in the set and the top drv
          # never is.
          KASHA_REALISE = "${pkgs.writeShellScript "kasha-test-realise" ''
            set -euo pipefail
            mkdir -p /var/lib/kasha/mirror-down
            printf '%s\n' "$@" > /var/lib/kasha/mirror-down/realise-args
            exec ${pkgs.nix}/bin/nix-store --realise ${depDrv}
          ''}";
        };
      };
    };
  };

  testScript = ''
    start_all()

    remote.wait_for_unit("harmonia.socket")
    remote.wait_for_open_port(${toString port})
    remote.succeed("printf '%s' '${signSecretKey}' > /root/sk && chmod 600 /root/sk")
    # Sign the recipe closure (copied to the box) and the dep output (substituted).
    remote.succeed("nix store sign --key-file /root/sk --recursive ${topDrv} ${depOut}")

    box.fail("nix-store --check-validity ${topDrv}")
    box.fail("nix-store --check-validity ${depOut}")
    box.fail("nix-store --check-validity ${topOut}")

    # Crash/failure safety: a copy failure must not publish the gen as seen,
    # and must not leave the dep realised.
    box.succeed("mkdir -p /var/lib/kasha/mirror-down && touch /var/lib/kasha/mirror-down/fail-copy")
    box.fail("systemctl start kasha-mirror-down-${flake}.service")
    box.fail("grep -q '${gen}' /var/lib/kasha/mirror-down/${flake}.seen")
    box.fail("nix-store --check-validity ${depOut}")
    box.succeed("rm /var/lib/kasha/mirror-down/fail-copy")

    box.succeed("systemctl start kasha-mirror-down-${flake}.service")
    # Recipe copied, input output substituted; the top-level output is NEVER
    # built or fetched (cross-system safe — the consumer assembles it at deploy).
    box.succeed("nix-store --check-validity ${topDrv}")
    box.succeed("nix-store --check-validity ${depOut}")
    box.fail("nix-store --check-validity ${topOut}")
    box.succeed("grep -q down-dep ${depOut}")
    # The script realises the recipe's input derivations (dep.drv among them),
    # never the top-level drv or output.
    box.succeed("grep -qxF ${depDrv} /var/lib/kasha/mirror-down/realise-args")
    box.fail("grep -qxF ${topDrv} /var/lib/kasha/mirror-down/realise-args")

    # Idempotent: second run sees the same generation and does no copy work.
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-down/copies) = 1")
    box.succeed("systemctl start kasha-mirror-down-${flake}.service")
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-down/copies) = 1")
    box.succeed("grep -q '${gen}' /var/lib/kasha/mirror-down/${flake}.seen")
  '';
}
