# kasha consumer — selection (client read path).
#
# The client-side selection for the MVP (ADR-0005): a static substituter list
# `[box, remote-cache]` plus a low `connect-timeout`. On-network the box answers
# first at LAN speed; off-network the box connection times out within
# `connect-timeout` and nix falls back to the remote cache — with zero
# reconfiguration between the two.
#
# Host-scoped by construction: a consumer imports this into ONE host's NixOS
# config, not a flake-wide default. It introduces no signing key (ADR-0004); it
# only *trusts* the existing remote-cache public key so substituted paths verify.
#
# Deferred (ADR-0005): the selection shim (a local proxy that probes reachability
# and eliminates the off-network timeout tax). Add when that tax is actually felt.
{ config, lib, ... }:
let
  cfg = config.services.kasha-consumer;
in
{
  options.services.kasha-consumer = {
    enable = lib.mkEnableOption "kasha selection: read from the box on-LAN, fall back to the remote cache off-LAN";

    boxEndpoint = lib.mkOption {
      type = lib.types.str;
      example = "http://box.lan:5000";
      description = ''
        The box's stable LAN HTTP endpoint. Queried first; when unreachable
        (off-LAN) the connection is bounded by `connectTimeout` before nix falls
        back to `remoteCache`.
      '';
    };

    remoteCache = lib.mkOption {
      type = lib.types.str;
      default = "https://znix.zebradil.dev";
      description = "The durable, always-reachable remote cache — the off-LAN fallback.";
    };

    connectTimeout = lib.mkOption {
      type = lib.types.ints.positive;
      default = 2;
      description = ''
        Per-substituter connect timeout (seconds). Bounds the off-LAN tax: the
        box connection fails this fast before nix tries the remote cache. Low, not
        zero — leave a real LAN a moment to answer.
      '';
    };

    trustedPublicKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "znix.zebradil.dev:AAAA…" ];
      description = ''
        Public key(s) whose signatures are accepted on substituted paths — the
        existing remote-cache key(s). Appended to nix's defaults (require-sigs
        stays on). No new signing key is introduced (ADR-0004).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # The existing remote-cache key must be trusted, else off-LAN fallback paths
    # fail signature verification (nix trusts only cache.nixos.org by default) —
    # the "existing public key already trusted" acceptance criterion. This module
    # introduces no signing key (ADR-0004); it only declares trust in an existing one.
    assertions = [
      {
        assertion = cfg.trustedPublicKeys != [ ];
        message = "services.kasha-consumer.trustedPublicKeys must list the remote-cache public key(s); otherwise off-LAN fallback paths fail signature verification.";
      }
    ];

    # Static selection: box first, remote cache second. Order is the on-LAN
    # preference; `connect-timeout` is the off-LAN fallback. mkForce so the box
    # genuinely leads — a leftover cache.nixos.org default must not race ahead of
    # the box on the LAN.
    nix.settings = {
      substituters = lib.mkForce [
        cfg.boxEndpoint
        cfg.remoteCache
      ];
      connect-timeout = cfg.connectTimeout;
      trusted-public-keys = cfg.trustedPublicKeys;
    };
  };
}
