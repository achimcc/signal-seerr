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

      # StartLimitBurst/StartLimitIntervalSec are [Unit]-section keys, not
      # [Service] ones, and belong here rather than in serviceConfig below
      # -- confirmed with `systemd-analyze verify` against the built unit
      # file after an earlier version of this module put
      # StartLimitIntervalSec under serviceConfig: systemd logged "Unknown
      # key 'StartLimitIntervalSec' in section [Service], ignoring" and
      # silently fell back to its own built-in default interval, which is
      # far shorter than a single restart cycle of this service -- so the
      # limit could never accumulate more than one attempt inside its own
      # window and never tripped, no matter how many times the service
      # actually restarted. StartLimitBurst alone happened to be accepted
      # in [Service] too (an accepted legacy alias) and so caused no
      # warning, which is what made this easy to miss.
      unitConfig = {
        # A bounded restart loop. Without it, a persistently failing
        # service restarts forever and never reaches the `failed` state --
        # so nothing that watches for failed units ever sees it.
        StartLimitBurst = 5;
        StartLimitIntervalSec = 300;
      };

      serviceConfig = {
        # simple, not oneshot: a unit that has not finished starting holds
        # up everything ordered after it, and a service that waits for a
        # socket to appear inside a oneshot can hold up the whole boot.
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} ${configFile}";
        Restart = "on-failure";
        RestartSec = "10s";

        DynamicUser = true;
        StateDirectory = "signal-seerr";
        # No RuntimeDirectory here, deliberately: this service does not use
        # one, and declaring one anyway is not harmless. systemd creates a
        # RuntimeDirectory fresh -- clearing anything already in it -- on
        # every start, and removes it on every stop. A directory of the
        # same name that something else (signal-cli, in particular) also
        # writes into gets wiped out from under it on every restart of
        # *this* unit. Confirmed with a `systemd-run --property=RuntimeDirectory=`
        # experiment: a file placed in the directory before a restart is
        # gone immediately after.

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
