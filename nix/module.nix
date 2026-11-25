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

    rev = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "The git commit ai-dev is built from, shown in the page footer.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { inherit (cfg) rev; };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { inherit (config.services.ai-dev) rev; }";
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = "The user whose systemd manager runs ai-dev, and whose home holds the notebooks.";
    };

    agentUser = lib.mkOption {
      type = lib.types.str;
      default = "llm";
      description = "The user whose agents call the MCP endpoint, and who gets a copy of the agent token.";
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
    systemd.services.ai-dev-agent-token = {
      wantedBy = [ "multi-user.target" ];
      before = [ "multi-user.target" ];
      path = [
        pkgs.coreutils
        pkgs.openssl
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        dir=/var/lib/ai-dev
        install -d -m 0755 "$dir"
        if [ ! -s "$dir/agent_token" ]; then
          (umask 077; openssl rand -hex 32 > "$dir/agent_token")
        fi
        for user in ${lib.escapeShellArg cfg.user} ${lib.escapeShellArg cfg.agentUser}; do
          install -d -m 0755 "$dir/$user"
          install -m 0400 -o "$user" -g "$(id -gn "$user")" "$dir/agent_token" "$dir/$user/agent_token"
        done
        chmod 0600 "$dir/agent_token"
      '';
    };

    systemd.user.services.ai-dev = {
      wantedBy = [ "default.target" ];
      unitConfig.ConditionUser = cfg.user;
      environment = {
        AI_DEV_LISTEN = cfg.listen;
        AI_DEV_ORIGIN = cfg.origin;
        AI_DEV_AGENT_TOKEN_FILE = "/var/lib/ai-dev/${cfg.user}/agent_token";
      };
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/ai-dev serve";
        Restart = "on-failure";
        KillMode = "process";
      };
    };
  };
}
