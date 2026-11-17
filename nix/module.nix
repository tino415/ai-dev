{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.ai-dev;
in
{
  options.services.ai-dev = {
    enable = lib.mkEnableOption "the ai-dev debug notebook";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = "The user whose systemd manager runs ai-dev, and whose home holds the notebooks.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:7471";
    };

    origin = lib.mkOption {
      type = lib.types.str;
      description = "The address the browser reaches ai-dev at, which the passkey is bound to.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.ai-dev = {
      wantedBy = [ "default.target" ];
      unitConfig.ConditionUser = cfg.user;
      environment = {
        AI_DEV_LISTEN = cfg.listen;
        AI_DEV_ORIGIN = cfg.origin;
      };
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/ai-dev serve";
        Restart = "on-failure";
        NoNewPrivileges = true;
      };
    };
  };
}
