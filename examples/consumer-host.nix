# Example: a consumer wiring kasha selection into ONE host (not global).
#
# The key point (ADR-0005): the module is imported by a single host's config, so
# the LAN box endpoint applies only to that host. Other hosts in the same flake
# are untouched — this is host-scoped selection, not a flake-wide substituter.
#
#   # flake.nix
#   nixosConfigurations.workstation = nixpkgs.lib.nixosSystem {
#     modules = [
#       kasha.nixosModules.consumer   # <- imported here, for THIS host only
#       ./hosts/workstation.nix       # <- the config below
#     ];
#   };
#
# hosts/workstation.nix:
{
  services.kasha-consumer = {
    enable = true;

    # This host's box lives on the home LAN at this stable endpoint. On-LAN it
    # answers first; off-LAN nix falls back to remoteCache within connectTimeout,
    # no config change.
    boxEndpoint = "http://box.lan:5000";

    # Defaults to the reference remote cache; override for your own.
    remoteCache = "https://znix.zebradil.dev";

    # Low fallback tax when off-LAN.
    connectTimeout = 2;

    # Trust the existing remote-cache key so substituted paths verify. No new
    # signing key is introduced (ADR-0004).
    trustedPublicKeys = [ "znix.zebradil.dev:AAAA…" ];
  };
}
