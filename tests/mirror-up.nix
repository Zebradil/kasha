# Smoke check for the eager up replica (issue #8): box-local root manifest -> remote closure.
{ pkgs, boxModule }:
let
  port = 5002;
  flake = "znix";
  gen = "20260705-local";
  sshKeys = import (pkgs.path + "/nixos/tests/ssh-keys.nix") pkgs;
  signPublicKey = "kasha-push-test-1:GjobTMnaEc8bB0ccSdA/vvPLLKMMFAHuq/siqLDVVuM=";
  signSecretKey = "kasha-push-test-1:orF8lRzcbfzQ2ueM6V9/Ij1wB6mDRnkenklqs602JxAaOhtMydoRzxsHRxxJ0D++88ssowwUAe6r+yKosNVW4w==";
  localDep = pkgs.runCommand "kasha-up-dep" { } "echo up-dep > $out";
  localSeed = pkgs.runCommand "kasha-up-seed" { } ''
    mkdir -p $out
    ln -s ${localDep} $out/dep
    echo up-mirror > $out/file
  '';
  manifest = builtins.toJSON {
    inherit flake gen;
    timestamp = "2026-07-05T00:00:00Z";
    roots = [ (builtins.unsafeDiscardStringContext "${localSeed}") ];
  };
in
pkgs.testers.runNixOSTest {
  name = "kasha-mirror-up";

  nodes = {
    remote = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        inherit port;
        trustedPublicKeys = [ signPublicKey ];
        push = {
          enable = true;
          authorizedKeys = [ sshKeys.snakeOilEd25519PublicKey ];
        };
      };
      services.openssh.enable = true;
      users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilEd25519PublicKey ];
      nix.settings.experimental-features = [ "nix-command" ];
    };

    box = { pkgs, ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        trustedPublicKeys = [ signPublicKey ];
        push = {
          enable = true;
          authorizedKeys = [ sshKeys.snakeOilEd25519PublicKey ];
        };
        mirrorUp = {
          enable = true;
          remoteCache = "s3://kasha-test-cache";
          flakes = [ flake ];
          interval = "1h";
        };
      };
      nix.settings.experimental-features = [ "nix-command" ];

      environment.etc."kasha-test/manifests/${gen}.json".text = manifest;
      systemd.services."kasha-mirror-up-${flake}" = {
        path = [
          pkgs.coreutils
          pkgs.openssh
        ];
        environment = {
          KASHA_AWS = "${pkgs.writeShellScript "kasha-test-aws-up" ''
            set -euo pipefail
            if [[ "$1 $2" == "s3api list-objects-v2" ]]; then
              # Emit s3api-style JSON Contents from the remote's published manifests.
              printf '{"Contents":['
              first=1
              for p in $(${pkgs.openssh}/bin/ssh root@remote 'ls /var/lib/kasha-test/roots/${flake}/*.json 2>/dev/null || true'); do
                [ "$first" = 1 ] || printf ','
                printf '{"Key":"roots/${flake}/%s"}' "$(basename "$p")"
                first=0
              done
              printf ']}\n'
            elif [[ "$1 $2 $3" == "s3 cp -" ]]; then
              key="''${4#s3://kasha-test-cache/roots/${flake}/}"
              body="$(mktemp)"
              cat > "$body"
              ${pkgs.openssh}/bin/ssh root@remote "mkdir -p /var/lib/kasha-test/roots/${flake}"
              ${pkgs.openssh}/bin/scp "$body" "root@remote:/var/lib/kasha-test/roots/${flake}/$key" >/dev/null
              rm -f "$body"
            else
              exit 1
            fi
          ''}";
          KASHA_COPY = "${pkgs.writeShellScript "kasha-test-copy-up" ''
            set -euo pipefail
            if [[ -e /var/lib/kasha/mirror-up/fail-copy ]]; then
              exit 42
            fi
            mkdir -p /var/lib/kasha/mirror-up
            printf '%s %s\n' "$1" "$2" >> /var/lib/kasha/mirror-up/copies
            exec ${pkgs.nix}/bin/nix copy --to ssh-ng://kasha-push@remote "$2"
          ''}";
        };
      };
    };

    client = _: {
      nix.settings.experimental-features = [ "nix-command" ];
      virtualisation.additionalPaths = [
        localSeed
        localDep
      ];
    };
  };

  testScript = ''
    start_all()

    box.wait_for_unit("harmonia.socket")
    box.wait_for_unit("sshd.service")
    remote.wait_for_unit("sshd.service")

    for machine in [client, box]:
        machine.succeed("mkdir -p /root/.ssh")
        machine.succeed("cp ${sshKeys.snakeOilEd25519PrivateKey} /root/.ssh/id_ed25519")
        machine.succeed("chmod 600 /root/.ssh/id_ed25519")
        machine.succeed("printf 'StrictHostKeyChecking no\n' > /root/.ssh/config")

    path = "${localSeed}"
    dep = "${localDep}"

    client.succeed("printf '%s' '${signSecretKey}' > /root/sk && chmod 600 /root/sk")
    client.succeed(f"nix store sign --key-file /root/sk {path} {dep}")

    box.fail(f"nix-store --check-validity {path}")
    remote.fail(f"nix-store --check-validity {path}")
    client.succeed(f"nix copy --to ssh-ng://kasha-push@box {path}")
    box.succeed(f"nix-store --check-validity {path}")
    remote.fail(f"nix-store --check-validity {path}")

    box.succeed("mkdir -p /var/lib/kasha/roots/${flake}")
    box.succeed("cp /etc/kasha-test/manifests/${gen}.json /var/lib/kasha/roots/${flake}/${gen}.json")

    # Crash/failure safety: copy failure must not publish the generation as mirrored.
    box.succeed("mkdir -p /var/lib/kasha/mirror-up && touch /var/lib/kasha/mirror-up/fail-copy")
    box.fail("systemctl start kasha-mirror-up-${flake}.service")
    box.fail("grep -q '${gen}' /var/lib/kasha/mirror-up/${flake}.seen")
    remote.fail(f"nix-store --check-validity {path}")
    remote.fail("test -f /var/lib/kasha-test/roots/${flake}/${gen}.json")
    box.succeed("rm /var/lib/kasha/mirror-up/fail-copy")

    box.succeed("systemctl start kasha-mirror-up-${flake}.service")
    remote.succeed(f"nix-store --check-validity {path}")
    remote.succeed(f"nix-store --check-validity {dep}")
    remote.succeed(f"grep -q up-mirror {path}/file")
    remote.succeed("grep -q '${gen}' /var/lib/kasha-test/roots/${flake}/${gen}.json")

    # Idempotent: second run sees same generation and does no copy work.
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-up/copies) = 1")
    box.succeed("systemctl start kasha-mirror-up-${flake}.service")
    box.succeed("test $(wc -l < /var/lib/kasha/mirror-up/copies) = 1")
    box.succeed("grep -q '${gen}' /var/lib/kasha/mirror-up/${flake}.seen")
  '';
}
