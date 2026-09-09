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
    # what is listed here. A sandboxing option that gives a unit its own
    # mount or network namespace (several of the Protect* family do) can
    # break something that looks completely unrelated, and break it
    # silently -- systemd reports the unit as healthy regardless. Add one
    # only after checking, on the running service, that nothing it actually
    # needs lives outside what that namespace still allows.
    systemd.services.signal-seerr = {
      description = "Signal bot for Seerr requests";
      after = [ "network-online.target" "signal-cli.service" ];
      wants = [ "network-online.target" ];
      requires = [ "signal-cli.service" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        # simple, not oneshot: a unit that has not finished starting holds
        # up everything ordered after it, and a service that waits for a
        # socket to appear inside a oneshot can hold up the whole boot.
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} ${configFile}";
        Restart = "on-failure";
        RestartSec = "10s";
        # A bounded restart loop. Without it, a persistently failing
        # service restarts forever and never reaches the `failed` state --
        # so nothing that watches for failed units ever sees it.
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
