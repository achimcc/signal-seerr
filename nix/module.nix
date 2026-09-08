{ config, lib, pkgs, ... }:
let
  cfg = config.services.signal-seerr;
  configFile = (pkgs.formats.toml { }).generate "signal-seerr.toml" cfg.settings;
in
{
  options.services.signal-seerr = {
    enable = lib.mkEnableOption "the Signal bot for Seerr requests";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The signal-seerr package to run.";
    };

    settings = lib.mkOption {
      type = (pkgs.formats.toml { }).type;
      description = ''
        Contents of signal-seerr.toml. NO SECRET BELONGS HERE: this becomes a
        file in the Nix store, and the store is world readable. Point the
        *_file options at systemd credentials instead.
      '';
    };

    extraServiceConfig = lib.mkOption {
      type = lib.types.attrsOf lib.types.anything;
      default = { };
      description = ''
        Extra `serviceConfig` attributes, merged over this module's own
        `systemd.services.signal-seerr.serviceConfig`. The escape hatch for
        a deployment that needs something this module does not provide by
        itself -- for example `SupplementaryGroups`, to reach a signal-cli
        socket owned by a group this module has no opinion about.
      '';
      example = lib.literalExpression ''{ SupplementaryGroups = [ "signal" ]; }'';
    };
  };

  config = lib.mkIf cfg.enable {
    # Deliberately NOT a hardened unit with its own mount namespace beyond
    # what is listed here. The paths are few and named.
    systemd.services.signal-seerr = {
      description = "Signal bot for Seerr requests";
      after = [ "network-online.target" "signal-cli.service" ];
      wants = [ "network-online.target" ];
      requires = [ "signal-cli.service" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        # simple, not oneshot: the unit counts as started at once, so a guest
        # boot never waits on it.
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} ${configFile}";
        Restart = "on-failure";
        RestartSec = "10s";
        # A bounded restart loop. Without it a persistently failing unit keeps
        # the container in `activating` for ever instead of showing up as
        # `failed` where the guest check and the alarm mail can see it.
        StartLimitBurst = 5;
        StartLimitIntervalSec = 300;

        DynamicUser = true;
        StateDirectory = "signal-seerr";
        RuntimeDirectory = "signal-seerr";

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectKernelTunables = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        SystemCallFilter = [ "@system-service" ];
      } // cfg.extraServiceConfig;
    };
  };
}
