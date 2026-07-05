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

  mirrorDown = pkgs.writeShellApplication {
    name = "kasha-mirror-down";
    runtimeInputs = [ pkgs.awscli2 pkgs.coreutils pkgs.gnused pkgs.jq pkgs.nix pkgs.util-linux ];
    text = builtins.readFile ../scripts/mirror-down.sh;
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
        Existing remote-cache public key(s). Appended to nix's defaults for
        push verification and down-mirror substitution; the box holds no
        private key (ADR-0004).
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

    mirrorDown = {
      enable = lib.mkEnableOption ''
        the eager down replica: periodically list remote root manifests and pull
        new generation closures into the box store
      '';

      remoteCache = lib.mkOption {
        type = lib.types.str;
        example = "s3://znix-cache?endpoint=example.r2.cloudflarestorage.com&region=auto";
        description = "Remote cache URL used for root-manifest discovery and `nix copy --from`.";
      };

      flakes = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "znix" ];
        description = "Flake ids under roots/<flake>/ to mirror down.";
      };

      interval = lib.mkOption {
        type = lib.types.str;
        default = "5min";
        description = "systemd OnUnitActiveSec interval for the down-mirror timer.";
      };

      stateDir = lib.mkOption {
        type = lib.types.str;
        default = "/var/lib/kasha/mirror-down";
        description = "Directory holding last-seen generation sets and overlap locks.";
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
    }

    # Push path (reverse flow): accept ssh-ng pushes at LAN speed, serve them
    # immediately over the same harmonia HTTP endpoint (no up-mirror dependency).
    (lib.mkIf cfg.push.enable {
      # The push gate: trust the existing remote-cache key(s) for signature checks
      # (a listOf definition, so appended to nix's cache.nixos.org default — not a
      # replacement). require-sigs stays on so unsigned pushes are rejected; the box
      # still signs nothing itself (ADR-0004). Only meaningful for the push path, so
      # scoped here rather than applied to every box.
      nix.settings.require-sigs = true;
      nix.settings.trusted-public-keys = cfg.trustedPublicKeys;

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

    (lib.mkIf cfg.mirrorDown.enable {
      assertions = [
        {
          assertion = cfg.mirrorDown.flakes != [ ];
          message = "services.kasha-box.mirrorDown.flakes must list at least one roots/<flake>/ prefix.";
        }
        {
          assertion = lib.hasPrefix "s3://" cfg.mirrorDown.remoteCache;
          message = "services.kasha-box.mirrorDown.remoteCache must be an s3:// URL so roots/<flake>/ can be listed.";
        }
      ];

      nix.settings.require-sigs = true;
      nix.settings.experimental-features = [ "nix-command" ];
      nix.settings.trusted-public-keys = cfg.trustedPublicKeys;

      systemd.services = lib.listToAttrs (map (flake: lib.nameValuePair "kasha-mirror-down-${flake}" {
        description = "kasha box: mirror ${flake} roots from remote cache";
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "oneshot";
          StateDirectory = "kasha";
        };
        environment = {
          KASHA_REMOTE = cfg.mirrorDown.remoteCache;
          KASHA_FLAKE = flake;
          KASHA_STATE_DIR = cfg.mirrorDown.stateDir;
        };
        path = [ pkgs.nix ];
        script = lib.getExe mirrorDown;
      }) cfg.mirrorDown.flakes);

      systemd.timers = lib.listToAttrs (map (flake: lib.nameValuePair "kasha-mirror-down-${flake}" {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "1min";
          OnUnitActiveSec = cfg.mirrorDown.interval;
          Unit = "kasha-mirror-down-${flake}.service";
        };
      }) cfg.mirrorDown.flakes);
    })
  ]);
}
