# kasha box — read path.
#
# A net-local Nix binary cache that serves its own nix store as a signed binary
# cache over HTTP (harmonia), at a stable LAN endpoint. The box holds NO signing
# key (ADR-0004): it serves paths exactly as signed upstream; clients verify them
# under the existing remote-cache public key. This module is the reusable box; the
# consumer (znix) wires it into a container/k3s deployment (see deploy/README.md).
{ config, lib, pkgs, ... }:
let
  cfg = config.services.kasha-box;

  # The NFS guard, packaged from the same script the fixture tests exercise.
  # writeShellApplication runs shellcheck at build time, so a broken guard fails
  # the build rather than the box at 3am.
  storeGuard = pkgs.writeShellApplication {
    name = "kasha-check-store-fs";
    runtimeInputs = [ pkgs.coreutils ];
    text = builtins.readFile ../scripts/check-store-fs.sh;
  };
in
{
  options.services.kasha-box = {
    enable = lib.mkEnableOption "kasha net-local nix binary cache box (read path)";

    storeDir = lib.mkOption {
      type = lib.types.str;
      default = "/nix";
      description = ''
        Nix store mount point. Must be block storage — never NFS (ADR-0002);
        startup is refused if this is NFS-backed.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 5000;
      description = "TCP port harmonia serves the binary cache on.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Open the cache port in the firewall for the LAN endpoint.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.harmonia.cache = {
      enable = true;
      # No signing key on the box (ADR-0004): serve upstream signatures as-is.
      signKeyPaths = [ ];
      settings.bind = "[::]:${toString cfg.port}";
    };

    # Refuse to start on an NFS-backed store (ADR-0002). harmonia is socket-
    # activated, so gate the socket (not just the service) and run at boot: a bad
    # mount fails the box loudly at boot instead of quietly on the first request.
    systemd.services.kasha-box-store-guard = {
      description = "kasha box: reject NFS-backed nix store (ADR-0002)";
      wantedBy = [ "multi-user.target" ];
      before = [ "harmonia.socket" "harmonia.service" ];
      requiredBy = [ "harmonia.socket" "harmonia.service" ];
      environment.KASHA_STORE_DIR = cfg.storeDir;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = lib.getExe storeGuard;
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
  };
}
