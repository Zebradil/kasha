# Smoke check for consumer selection (issue #5, ADR-0005): on-LAN read from the
# box, off-LAN fall back to the remote cache — bounded by connect-timeout, with
# NO reconfiguration between the two.
#
# Proves, end to end, that a host wired only with `services.kasha-consumer`:
#   * substitutes from the box while the box is reachable (on-LAN), and
#   * with the box made unreachable (off-LAN), the SAME config substitutes the
#     next path from the remote cache instead — within `connect-timeout`.
# The remote cache is stood up as a second kasha-box instance. Linux-only (VM).
{
  pkgs,
  boxModule,
  consumerModule,
}:
let
  boxPort = 5000;
  remotePort = 5001;
  connectTimeout = 2;

  # Throwaway binary-cache keypair standing in for the existing remote-cache
  # signing key — the consumer module trusts the public half. Test-only, like the
  # snakeoil ssh keys nixpkgs ships.
  signPublicKey = "kasha-push-test-1:GjobTMnaEc8bB0ccSdA/vvPLLKMMFAHuq/siqLDVVuM=";
  signSecretKey = "kasha-push-test-1:orF8lRzcbfzQ2ueM6V9/Ij1wB6mDRnkenklqs602JxAaOhtMydoRzxsHRxxJ0D++88ssowwUAe6r+yKosNVW4w==";

  # INPUT-addressed seeds (not content-addressed, which self-verify by hash and
  # make require-sigs vacuous). One lives only on the box, one only on the remote
  # — so which server answered is unambiguous.
  boxOnlySeed = pkgs.runCommand "kasha-sel-box-only" { } "echo from-the-box > $out";
  remoteOnlySeed = pkgs.runCommand "kasha-sel-remote-only" { } "echo from-the-remote > $out";
in
pkgs.testers.runNixOSTest {
  name = "kasha-consumer-selection";

  nodes = {
    box = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        port = boxPort;
      };
      nix.settings.experimental-features = [ "nix-command" ];
      # Seed lives only on the box.
      virtualisation.additionalPaths = [ boxOnlySeed ];
    };

    # The remote cache, stood up as a second box instance.
    remote = { ... }: {
      imports = [ boxModule ];
      services.kasha-box = {
        enable = true;
        port = remotePort;
      };
      nix.settings.experimental-features = [ "nix-command" ];
      # Seed lives only on the remote.
      virtualisation.additionalPaths = [ remoteOnlySeed ];
    };

    client = { ... }: {
      imports = [ consumerModule ];
      nix.settings.experimental-features = [ "nix-command" ];
      services.kasha-consumer = {
        enable = true;
        boxEndpoint = "http://box:${toString boxPort}";
        remoteCache = "http://remote:${toString remotePort}";
        inherit connectTimeout;
        trustedPublicKeys = [ signPublicKey ];
      };
    };
  };

  testScript = ''
    start_all()

    box.wait_for_unit("harmonia.socket")
    box.wait_for_open_port(${toString boxPort})
    remote.wait_for_unit("harmonia.socket")
    remote.wait_for_open_port(${toString remotePort})

    # Sign each seed in its server's store with the trusted key (out-of-band; the
    # box never signs — ADR-0004). The client trusts the public half via the module.
    box.succeed("printf '%s' '${signSecretKey}' > /root/sk && chmod 600 /root/sk")
    box.succeed("nix store sign --key-file /root/sk ${boxOnlySeed}")
    remote.succeed("printf '%s' '${signSecretKey}' > /root/sk && chmod 600 /root/sk")
    remote.succeed("nix store sign --key-file /root/sk ${remoteOnlySeed}")

    # The module wired selection into THIS host: box-first substituter list, the
    # low connect-timeout, and the trusted key. (Host-scoped: it's in the client's
    # nix.conf because the client imported the module — nothing global.)
    client.succeed("grep -qF 'substituters = http://box:${toString boxPort} http://remote:${toString remotePort}' /etc/nix/nix.conf")
    client.succeed("grep -qF 'connect-timeout = ${toString connectTimeout}' /etc/nix/nix.conf")
    client.succeed("grep -qF '${signPublicKey}' /etc/nix/nix.conf")

    # On-LAN: the box is reachable, so its path substitutes (and verifies under
    # the trusted key). Lives only on the box, so this proves the box answered.
    client.fail("nix-store --check-validity ${boxOnlySeed}")
    client.succeed("nix-store --realise ${boxOnlySeed}")
    client.succeed("nix-store --check-validity ${boxOnlySeed}")
    client.succeed("grep -q from-the-box ${boxOnlySeed}")

    # Take the box off-LAN: drop packets to it so the connection genuinely times
    # out (SYN blackholed), the real off-network condition — not a fast refusal.
    client.succeed("iptables -A OUTPUT -p tcp --dport ${toString boxPort} -d box -j DROP")

    # Off-LAN, SAME config, no reconfiguration: the next path substitutes from the
    # remote instead, bounded by connect-timeout. A blackholed host's TCP connect
    # takes ~127s to fail on its own (kernel SYN retransmits), so completing well
    # under that proves connect-timeout (2s) is what bounds the box attempt — not
    # just that nix eventually gives up. Lives only on the remote: proves remote answered.
    client.fail("nix-store --check-validity ${remoteOnlySeed}")
    client.succeed("timeout 30 nix-store --realise ${remoteOnlySeed}")
    client.succeed("nix-store --check-validity ${remoteOnlySeed}")
    client.succeed("grep -q from-the-remote ${remoteOnlySeed}")
  '';
}
