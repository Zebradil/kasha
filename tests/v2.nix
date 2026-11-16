# End-to-end against a real nix client: sign -> nix copy --to (netrc basic
# auth) -> kasha emit manifest -> substitute pull gated by the trusted key.
# The box runs only the kasha binary — no nix, no signing key (ADR-0004).
# Then pull-through (ADR-0011): a second box with nothing stored and the first
# as its only upstream serves the same path to a fresh client. The path is
# pushed as xz, so the first box serves it as xz; the second stores it as zstd
# (ADR-0012).
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
  # Both boxes run through the NixOS module (modules/box.nix), so this test is
  # also its test.
  boxDefaults = {
    enable = true;
    package = kasha;
    listen = "0.0.0.0:${toString port}";
    trustedKeys = [ pk ];
    openFirewall = true;
  };
in
pkgs.testers.runNixOSTest {
  name = "kasha-v2";

  nodes = {
    box = _: {
      imports = [ ../modules/box.nix ];
      services.kasha-box = boxDefaults // {
        environmentFile = pkgs.writeText "kasha.env" "KASHA_TOKEN=${token}";
      };
    };

    # Pull-through box: no remote cache, no write token, one upstream.
    edge = _: {
      imports = [ ../modules/box.nix ];
      services.kasha-box = boxDefaults // {
        environment = {
          KASHA_PULL_THROUGH = "true";
          KASHA_UPSTREAMS = "http://box:${toString port}";
        };
      };
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

    # Stands in for a short-lived consumer whose only substituter is the edge
    # box.
    job = _: {
      nix.settings = {
        experimental-features = [ "nix-command" ];
        substituters = pkgs.lib.mkForce [ "http://edge:${toString port}" ];
        trusted-public-keys = pkgs.lib.mkForce [ pk ];
      };
      environment.systemPackages = [ pkgs.curl ];
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

    # Sign locally (box never signs), push via plain nix copy + netrc. xz
    # named explicitly: the pull-through check below needs an xz source,
    # whatever Nix's default compression is.
    pusher.succeed(f"nix store sign --key-file ${sk} {path}")
    pusher.succeed(f"nix copy --to '{base}?compression=xz' {path}")

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

    # Pull-through: the edge box holds nothing and fetches from `box` on a
    # narinfo miss.
    edge.wait_for_unit("kasha.service")
    edge.wait_for_open_port(${toString port})
    job.wait_for_unit("multi-user.target")
    edge_base = "http://edge:${toString port}"
    store_hash = path.split("/")[-1][:32]
    narinfo = f"/var/lib/kasha/{store_hash}.narinfo"
    # Substitution, as a build does it: a GET for the narinfo. `nix copy
    # --from` would not do: it first asks the source whether the path is
    # valid with a HEAD, which only reports what the edge box already stores.
    substitute = f"nix-store --realise {path}"

    # HEAD reports the store and never starts a fetch.
    code = job.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -I {edge_base}/{store_hash}.narinfo"
    ).strip()
    assert code == "404", f"expected 404, got {code}"
    edge.fail(f"test -e {narinfo}")

    job.succeed(substitute)
    job.succeed(f"nix-store --check-validity {path}")
    job.succeed(f"grep -q hello-kasha-v2 {path}")

    # The edge box kept the path: a narinfo describing a NAR it stores.
    edge.wait_until_succeeds(f"test -f {narinfo}")
    edge.succeed(f"grep -q '^FileHash: ' {narinfo}")
    box.succeed(f"grep -qx 'Compression: xz' /var/lib/kasha/{store_hash}.narinfo")
    edge.succeed(f"grep -qx 'Compression: zstd' {narinfo}")
    edge.succeed(f"test -f /var/lib/kasha/$(sed -n 's/^URL: //p' {narinfo})")

    # With the upstream gone the edge box still serves it.
    box.succeed("systemctl stop kasha.service")
    job.succeed(f"nix-store --delete {path}")
    job.succeed(substitute)
    job.succeed(f"grep -q hello-kasha-v2 {path}")
  '';
}
