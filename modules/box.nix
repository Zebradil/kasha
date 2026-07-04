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

    trustedPublicKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "znix.zebradil.dev:AAAA…" ];
      description = ''
        Public keys whose signatures are accepted on pushed paths — the existing
        remote-cache key(s). Appended to nix's defaults; require-sigs stays on, so
        an ssh-ng push is admitted only if every new path is signed by one of these
        (or a default trusted key). The box holds no private key (ADR-0004).
      '';
    };

    push = {
      enable = lib.mkEnableOption ''
        the LAN-speed ssh-ng push target (reverse flow): the box runs sshd and
        accepts `nix copy --to ssh-ng://box` from authorized client keys
      '';

      user = lib.mkOption {
        type = lib.types.str;
        default = "kasha-push";
        description = ''
          Unix user authorized clients push as. Deliberately a normal (non-root)
          user so it is NOT a nix trusted-user: an untrusted push must present
          paths signed by a trusted-public-key, which is the require-sigs gate.
        '';
      };

      authorizedKeys = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Client SSH public keys allowed to push into the box store.";
      };
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      services.harmonia.cache = {
        enable = true;
        # No signing key on the box (ADR-0004): serve upstream signatures as-is.
        signKeyPaths = [ ];
        settings.bind = "[::]:${toString cfg.port}";
      };

      # Refuse to start on an NFS-backed store (ADR-0002). harmonia is socket-
      # activated, so gate the socket (not just the service) and run at boot: a bad
      # mount fails the box loudly at boot instead of quietly on the first request.
      #
      # harmonia.socket lives in early-boot sockets.target, which is ordered *before*
      # basic.target. A unit with default dependencies is ordered *after* basic.target,
      # so gating the socket from there forms an ordering cycle (systemd then drops the
      # socket). Opt out of default deps and order the guard right after the store is
      # mounted; the socket's Requires= (requiredBy below) pulls it into the boot
      # transaction, so failure still stops the box loudly at boot.
      systemd.services.kasha-box-store-guard = {
        description = "kasha box: reject NFS-backed nix store (ADR-0002)";
        unitConfig.DefaultDependencies = false;
        after = [ "local-fs.target" ];
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

      # Trust the existing remote-cache key(s) for signature checks (appended to
      # nix's defaults). require-sigs stays on so unsigned paths are rejected; the
      # box still signs nothing itself (ADR-0004).
      nix.settings.require-sigs = true;
      nix.settings.trusted-public-keys = cfg.trustedPublicKeys;
    }

    # Push path (reverse flow): accept ssh-ng pushes at LAN speed, serve them
    # immediately over the same harmonia HTTP endpoint (no up-mirror dependency).
    (lib.mkIf cfg.push.enable {
      services.openssh = {
        enable = true;
        settings = {
          PasswordAuthentication = false;
          KbdInteractiveAuthentication = false;
        };
      };

      # A normal (non-root) user: NOT a nix trusted-user, so its pushes go through
      # signature verification (require-sigs) rather than bypassing it. The client
      # pushes as `ssh-ng://${user}@box`.
      users.users.${cfg.push.user} = {
        isNormalUser = true;
        openssh.authorizedKeys.keys = cfg.push.authorizedKeys;
      };
    })
  ]);
}
