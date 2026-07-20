# Smoke check for the box push path (reverse flow): push -> serve immediately.
#
# Proves, end to end, that an authorized client can `nix copy --to ssh-ng://box`
# a signed path into the box store and it is served from the box's HTTP endpoint
# at once — with no up-mirror in the picture. Also proves the require-sigs gate
# (an unsigned push is rejected) and that the box holds no signing key
# (ADR-0004). Linux-only (NixOS VM).
{ pkgs, boxModule }:
let
  port = 5000;
  sshKeys = import (pkgs.path + "/nixos/tests/ssh-keys.nix") pkgs;

  # A throwaway binary-cache keypair standing in for the existing remote-cache
  # signing key. Public half is trusted by the box; the client signs with the
  # secret half out-of-band. NOT a security issue — test-only, like the ssh keys.
  signPublicKey = "kasha-push-test-1:GjobTMnaEc8bB0ccSdA/vvPLLKMMFAHuq/siqLDVVuM=";
  signSecretKey = "kasha-push-test-1:orF8lRzcbfzQ2ueM6V9/Ij1wB6mDRnkenklqs602JxAaOhtMydoRzxsHRxxJ0D++88ssowwUAe6r+yKosNVW4w==";

  # INPUT-addressed seeds (not `nix-store --add`, which is content-addressed and
  # self-verifies by hash — making the require-sigs gate vacuous, the exact trap
  # the read-path smoke hit). Two paths: one gets signed and pushed, one stays
  # unsigned to prove the gate rejects it.
  signedSeed = pkgs.runCommand "kasha-push-signed" { } "echo push-signed > $out";
  unsignedSeed = pkgs.runCommand "kasha-push-unsigned" { } "echo push-unsigned > $out";
in
pkgs.testers.runNixOSTest {
  name = "kasha-box-push-path";

  nodes = {
    box = { ... }: {
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
      nix.settings.experimental-features = [ "nix-command" ];
    };

    client = _: {
      nix.settings.experimental-features = [ "nix-command" ];
      # The seeds live only on the client to start; the box gets them via push.
      virtualisation.additionalPaths = [
        signedSeed
        unsignedSeed
      ];
    };
  };

  testScript = ''
    start_all()

    box.wait_for_unit("harmonia.socket")
    box.wait_for_unit("sshd.service")
    box.wait_for_open_port(${toString port})

    # The box loads NO signing key (ADR-0004): harmonia has no sign-key credential.
    box.fail("systemctl show harmonia -p LoadCredential --value | grep -q sign-key")

    # Client's ssh identity (the snakeoil key the box authorized) + trust the host.
    client.succeed("mkdir -p /root/.ssh")
    client.succeed("cp ${sshKeys.snakeOilEd25519PrivateKey} /root/.ssh/id_ed25519")
    client.succeed("chmod 600 /root/.ssh/id_ed25519")
    client.succeed("printf 'StrictHostKeyChecking no\n' > /root/.ssh/config")

    signed = "${signedSeed}"
    unsigned = "${unsignedSeed}"

    # Sign only the signed seed, with the trusted key, out-of-band (ADR-0004: the
    # box never signs — pushes arrive already signed by the existing key).
    client.succeed("printf '%s' '${signSecretKey}' > /root/sk && chmod 600 /root/sk")
    client.succeed(f"nix store sign --key-file /root/sk {signed}")

    # Box does not have either path yet.
    box.fail(f"nix-store --check-validity {signed}")
    box.fail(f"nix-store --check-validity {unsigned}")

    # require-sigs gate: an unsigned push is rejected and lands nothing.
    client.fail(f"nix copy --to ssh-ng://kasha-push@box {unsigned}")
    box.fail(f"nix-store --check-validity {unsigned}")

    # Signed path pushes into the box store over ssh-ng.
    client.succeed(f"nix copy --to ssh-ng://kasha-push@box {signed}")
    box.succeed(f"nix-store --check-validity {signed}")

    # Served immediately over the box HTTP endpoint — no up-mirror in the loop.
    # Drop it from the client store, then substitute the NAR back from the box and
    # verify under the good key: proves real serving, not just a narinfo answer.
    client.succeed(f"nix-store --delete --ignore-liveness {signed}")
    client.fail(f"nix-store --check-validity {signed}")
    client.succeed(
        f"nix copy --from http://box:${toString port} "
        f"--option trusted-public-keys '${signPublicKey}' "
        f"--option require-sigs true {signed}"
    )
    client.succeed(f"nix-store --check-validity {signed}")
  '';
}
