# kasha box — the `kasha serve` daemon as a native systemd service.
#
# The same process the OCI image runs, configured through the same environment
# contract (`kasha serve --help`): the typed options cover what every box sets,
# `environment` passes any other KASHA_* knob through unchanged, and secrets
# (KASHA_TOKEN, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY) come from
# `environmentFile` so they never land in the Nix store. The box holds no
# signing key (ADR-0004).
{ config, lib, ... }:
let
  cfg = config.services.kasha-box;
  port = lib.toInt (lib.last (lib.splitString ":" cfg.listen));
in
{
  options.services.kasha-box = {
    enable = lib.mkEnableOption "the kasha box (cache HTTP server, mirror workers, box GC)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The kasha package; the flake's `nixosModules.box` defaults it to `packages.<system>.kasha-bin`.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/kasha";
      description = ''
        Store root (KASHA_DATA). Must support hard links. Created as a btrfs
        subvolume where systemd-tmpfiles can, else as a plain directory. The
        service waits for the mount holding it, so a missing disk stops the box
        instead of filling the parent filesystem.
      '';
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:5000";
      description = "Listen address for the cache HTTP server (KASHA_LISTEN).";
    };

    trustedKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      example = [ "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" ];
      description = "Narinfo signing keys the box accepts on ingest, mirror-down and pull-through (KASHA_TRUSTED_KEYS).";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        KASHA_REMOTE = "s3://cache?endpoint=https://example.r2.cloudflarestorage.com&region=auto";
        KASHA_PULL_THROUGH = "true";
      };
      description = "Further `kasha serve` environment; see `kasha serve --help` for every knob.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/secrets/kasha.env";
      description = "File with secret environment: KASHA_TOKEN (writes are refused without it) and the remote's AWS credentials.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the `listen` port in the firewall.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.kasha = {
      isSystemUser = true;
      group = "kasha";
    };
    users.groups.kasha = { };

    systemd.tmpfiles.settings."10-kasha-box".${cfg.dataDir}.v = {
      user = "kasha";
      group = "kasha";
      mode = "0750";
    };

    systemd.services.kasha = {
      description = "kasha box";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      unitConfig.RequiresMountsFor = [ cfg.dataDir ];
      environment = {
        KASHA_DATA = cfg.dataDir;
        KASHA_LISTEN = cfg.listen;
        KASHA_TRUSTED_KEYS = lib.concatStringsSep " " cfg.trustedKeys;
      } // cfg.environment;
      serviceConfig = {
        ExecStart = "${lib.getExe' cfg.package "kasha"} serve";
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        User = "kasha";
        Group = "kasha";
        Restart = "on-failure";
        # Every in-flight response holds an open NAR on top of the store scan.
        LimitNOFILE = 65536;
        ReadWritePaths = [ cfg.dataDir ];
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ port ];
  };
}
