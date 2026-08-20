# End-to-end against a real nix client: sign -> nix copy --to (netrc basic
# auth) -> kasha emit manifest -> substitute pull gated by the trusted key.
# The box runs only the kasha binary — no nix, no signing key (ADR-0004).
{ pkgs, kasha }:
let
  port = 5000;
  token = "test-token";
  # Throwaway fixture keypair (nix-store --generate-binary-cache-key); stands
  # in for the real remote-cache signing key. Not a secret.
  pk = "kasha-test-1:hGP48HKnk2Gxo63MZ2/shZSOvTqEsLjgjvQX3RZRlHM=";
  sk = pkgs.writeText "kasha-test-sk" "kasha-test-1:OcIl7QOIQAGBt1Z+v0MocT/Ga6WCQQkba94mSpgeE9eEY/jwcqeTYbGjrcxnb+yFlI69OoSwuOCO9BfdFlGUcw==";
  netrc = pkgs.writeText "netrc" "machine box login nix password ${token}";
  # Input-addressed path: substitution genuinely requires a trusted signature
  # (a content-addressed path would self-verify and make the sig gate vacuous).
  seed = pkgs.runCommand "kasha-seed" { } "echo hello-kasha-v2 > $out";
in
pkgs.testers.runNixOSTest {
  name = "kasha-v2";

  nodes = {
    box = _: {
      systemd.services.kasha = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${kasha}/bin/kasha serve --data /var/lib/kasha --listen 0.0.0.0:${toString port} --trusted-keys ${pk}";
          Environment = [ "KASHA_TOKEN=${token}" ];
          StateDirectory = "kasha";
        };
      };
      networking.firewall.allowedTCPPorts = [ port ];
    };

    pusher = _: {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        netrc-file = "${netrc}";
        substituters = pkgs.lib.mkForce [ ];
      };
      virtualisation.additionalPaths = [ seed ];
      environment.systemPackages = [
        kasha
        pkgs.curl
      ];
    };

    puller = _: {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = pkgs.lib.mkForce [ ];
      };
    };
  };

  testScript = ''
    start_all()
    box.wait_for_unit("kasha.service")
    box.wait_for_open_port(${toString port})

    path = "${seed}"
    base = "http://box:${toString port}"

    # Unauthenticated writes refused.
    pusher.wait_for_unit("multi-user.target")
    code = pusher.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -X PUT --data-binary x {base}/nar/x.nar.xz"
    ).strip()
    assert code == "401", f"expected 401, got {code}"

    # Sign locally (box never signs), push via plain nix copy + netrc.
    pusher.succeed(f"nix store sign --key-file ${sk} {path}")
    pusher.succeed(f"nix copy --to '{base}' {path}")

    # Emit + publish the v3 manifest through the authed push API.
    pusher.succeed(
        f"echo {path} | KASHA_TOKEN=${token} kasha emit"
        " --flake test --gen main-1-sys --branch main --attr sys"
        f" --to {base}"
    )
    pusher.succeed(f"curl -sf {base}/roots/test/main-1-sys.json | grep -q main-1-sys")
    pusher.succeed(f"curl -sf {base}/status | grep -q '\"objects\":1'")

    # Wrong key -> substitution must fail (sig verification is real).
    puller.succeed("nix-store --generate-binary-cache-key wrong-1 /root/wsk /root/wpk")
    wrong_key = puller.succeed("cat /root/wpk").strip()
    puller.fail(
        f"nix copy --from {base} "
        f"--option trusted-public-keys '{wrong_key}' "
        f"--option require-sigs true {path}"
    )
    puller.fail(f"nix-store --check-validity {path}")

    # Trusted key -> substitutes and verifies.
    puller.succeed(
        f"nix copy --from {base} "
        "--option trusted-public-keys '${pk}' "
        f"--option require-sigs true {path}"
    )
    puller.succeed(f"nix-store --check-validity {path}")
    puller.succeed(f"grep -q hello-kasha-v2 {path}")
  '';
}
