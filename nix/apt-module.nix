# SPDX-License-Identifier: EUPL-1.2
{ config, lib, ... }:

let
  cfg = config.services.nuschl-apt;
in
{
  options.services.nuschl-apt = {
    enable = lib.mkEnableOption "the static Nuschl APT archive";
    hostName = lib.mkOption {
      type = lib.types.str;
      default = "dl.nuschl.eu";
      description = "DNS name that serves the APT archive.";
    };
    archiveDirectory = lib.mkOption {
      type = lib.types.path;
      default = "/srv/dl.nuschl.eu/apt";
      description = "Directory containing the signed archive, served at /apt/.";
    };
    enableACME = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Obtain the host's TLS certificate through security.acme and redirect HTTP to HTTPS.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.nginx = {
      enable = true;
      virtualHosts.${cfg.hostName} = {
        inherit (cfg) enableACME;
        forceSSL = cfg.enableACME;
        locations."/apt/".alias = "${toString cfg.archiveDirectory}/";
        locations."/".return = "404";
      };
    };
  };
}
