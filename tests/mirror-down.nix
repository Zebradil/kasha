# Smoke check for the eager down replica (issue #6): remote root manifest -> box closure.
{ pkgs, boxModule }:
let
  port = 5001;
  flake = "znix";
  gen = "20260705-demo";
  signPublicKey = "kasha-push-test-1:GjobTMnaEc8bB0ccSdA/vvPLLKMMFAHuq/siqLDVVuM=";
  signSecretKey = "kasha-push-test-1:orF8lRzcbfzQ2ueM6V9/Ij1wB6mDRnkenklqs602JxAaOhtMydoRzxsHRxxJ0D++88ssowwUAe6r+yKosNVW4w==";
  remoteDep = pkgs.runCommand "kasha-down-dep" { } "echo down-dep > $out";
  remoteSeed = pkgs.runCommand "kasha-down-seed" { } ''
    mkdir -p $out
    ln -s ${remoteDep} $out/dep
    echo down-mirror > $out/file
  '';
in
pkgs.testers.runNixOSTest {
  name = "kasha-mirror-down";

  nodes = {
    remote = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = { enable = true; inherit port; };
      nix.settings.experimental-features = [ "nix-command" ];
      virtualisation.additionalPaths = [ remoteSeed ];
    };

    box = { pkgs, ... }: {
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

      # The real script lists S3 roots. For the VM demo, serve the same seam with
      # static listing + manifest so we test copy behavior without fake S3 service.
      environment.etc."kasha-test/manifests/${gen}.json".text = builtins.toJSON {
        inherit flake gen;
        timestamp = "2026-07-05T00:00:00Z";
        roots = [ (builtins.unsafeDiscardStringContext "${remoteSeed}") ];
      };
      systemd.services."kasha-mirror-down-${flake}" = {
        path = [ pkgs.coreutils ];
        environment = {
          KASHA_AWS = "${pkgs.writeShellScript "kasha-test-aws" ''
            set -euo pipefail
            if [[ "$1 $2 $3" == "s3 ls --recursive" ]]; then
              printf '2026-07-05 00:00:00        100 roots/${flake}/${gen}.json\n'
            elif [[ "$1 $2 $3" == "s3 cp s3://kasha-test-cache/roots/${flake}/${gen}.json" ]]; then
              cat /etc/kasha-test/manifests/${gen}.json
            else
              exit 1
            fi
          ''}";
          KASHA_COPY = "${pkgs.writeShellScript "kasha-test-copy" ''
            set -euo pipefail
            if [[ -e /var/lib/kasha/mirror-down/fail-copy ]]; then
              exit 42
            fi
            mkdir -p /var/lib/kasha/mirror-down
            printf '%s %s\n' "$1" "$2" >> /var/lib/kasha/mirror-down/copies
            exec ${pkgs.nix}/bin/nix copy --from http://remote:${toString port} "$2"
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
    remote.succeed("nix store sign --key-file /root/sk ${remoteSeed} ${remoteDep}")

    path = "${remoteSeed}"
    dep = "${remoteDep}"
    box.fail(f"nix-store --check-validity {path}")
    box.fail(f"nix-store --check-validity {dep}")

    # Crash/failure safety: copy failure must not publish the generation as seen.
    box.succeed("mkdir -p /var/lib/kasha/mirror-down && touch /var/lib/kasha/mirror-down/fail-copy")
    box.fail("systemctl start kasha-mirror-down-${flake}.service")
    box.fail("grep -q '${gen}' /var/lib/kasha/mirror-down/${flake}.seen")
    box.fail(f"nix-store --check-validity {path}")
    box.succeed("rm /var/lib/kasha/mirror-down/fail-copy")

    box.succeed("systemctl start kasha-mirror-down-${flake}.service")
    box.succeed(f"nix-store --check-validity {path}")
    box.succeed(f"nix-store --check-validity {dep}")
    box.succeed(f"grep -q down-mirror {path}/file")

    # Idempotent: second run sees same generation and does no copy work.
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-down/copies) = 1")
    box.succeed("systemctl start kasha-mirror-down-${flake}.service")
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-down/copies) = 1")
    box.succeed("grep -q '${gen}' /var/lib/kasha/mirror-down/${flake}.seen")
  '';
}
