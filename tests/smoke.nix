# Coarse smoke check for the box read path: seed -> serve -> substitute -> verify.
#
# Proves, end to end, that a manually-seeded, upstream-signed path served by the
# box is substitutable by a client and verifies under the existing public key —
# and that the box itself holds no signing key (ADR-0004). Linux-only (NixOS VM).
{ pkgs, boxModule }:
let
  port = 5000;
in
pkgs.testers.runNixOSTest {
  name = "kasha-box-read-path";

  nodes = {
    box = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        inherit port;
      };
      nix.settings.experimental-features = [ "nix-command" ];
    };

    client = { ... }: {
      nix.settings.experimental-features = [ "nix-command" ];
      # No local builders should be needed; the path must come from the box.
      nix.settings.substituters = pkgs.lib.mkForce [ ];
    };
  };

  testScript = ''
    start_all()

    box.wait_for_unit("harmonia.socket")
    box.wait_for_open_port(${toString port})

    # The box loads NO signing key (ADR-0004): harmonia has no sign-key credential.
    box.fail("systemctl show harmonia -p LoadCredential --value | grep -q sign-key")

    # A throwaway key stands in for the existing remote-cache signing key, plus a
    # second unrelated key to prove verification actually gates.
    box.succeed("nix-store --generate-binary-cache-key kasha-test-1 /root/sk /root/pk")
    box.succeed("nix-store --generate-binary-cache-key wrong-1 /root/wsk /root/wpk")
    good_key = box.succeed("cat /root/pk").strip()
    wrong_key = box.succeed("cat /root/wpk").strip()

    # Seed a path into the box store and sign it out-of-band (simulating a
    # pre-signed upstream path). The box only serves; it never signs.
    box.succeed("echo hello-kasha > /root/seed")
    path = box.succeed("nix-store --add /root/seed").strip()
    box.succeed(f"nix store sign --key-file /root/sk {path}")

    # Client does not have it yet.
    client.fail(f"nix-store --check-validity {path}")

    # Wrong key -> signature verification must fail (proves the check is real).
    client.fail(
        f"nix copy --from http://box:${toString port} "
        f"--option trusted-public-keys '{wrong_key}' "
        f"--option require-sigs true {path}"
    )
    client.fail(f"nix-store --check-validity {path}")

    # Correct key -> substitutes at LAN speed and verifies under the public key.
    client.succeed(
        f"nix copy --from http://box:${toString port} "
        f"--option trusted-public-keys '{good_key}' "
        f"--option require-sigs true {path}"
    )
    client.succeed(f"nix-store --check-validity {path}")
    client.succeed(f"grep -q hello-kasha {path}")
  '';
}
