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

    allowedAddresses = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "10.0.1.10" "10.0.2.20" ];
      description = ''
        The only IP addresses this service may exchange packets with, in
        either direction (`IPAddressAllow`, with `IPAddressDeny = any`):
        Authentik, Seerr, the *arr and treff endpoints from `settings` --
        and whatever sends the Seerr webhook, since the filter holds for
        incoming connections too. Its signal-cli socket is AF_UNIX and not
        affected. Empty (the default) means no address filter at all; the
        module cannot know the addresses of a deployment, and a wrong guess
        would cut the bot off silently.
      '';
    };

    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "256M";
      description = ''
        `MemoryMax` of the unit. The bot keeps a handful of conversations
        and one mapping table; the bound is there so that something that
        grows without end -- a flood, a leak -- ends in a restart instead of
        taking the machine with it.
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
    # The notices record is the one file this service MUST be able to write,
    # and `ProtectSystem = "strict"` below leaves it exactly one writable
    # place: the StateDirectory. Point `notices_file` anywhere else and the
    # bot comes up, answers `/status`, sends its first unasked message -- and
    # then fails to record that it did, so it sends it again on the next
    # round, and the round after that. A read-only file system is an error
    # in a log line nobody is watching; here it is a build-time refusal with
    # the path in it.
    assertions = [
      {
        assertion =
          let
            notices = cfg.settings.insight.notices_file or null;
          in
          notices == null || lib.hasPrefix "/var/lib/signal-seerr/" (toString notices);
        message = ''
          services.signal-seerr.settings.insight.notices_file is
          "${toString (cfg.settings.insight.notices_file or "")}", which is outside
          /var/lib/signal-seerr/ -- the StateDirectory, and the only path this unit
          can write to (ProtectSystem = "strict"). The bot would tell people about
          the same stalled wish on every round, because it could never record that
          it already had.
        '';
      }
    ];

    # A sandboxing option that gives a unit its own mount or network
    # namespace can break something that looks completely unrelated, and
    # break it silently -- systemd reports the unit as healthy regardless.
    # What the bot needs from outside is small and known, and each option
    # below leaves it in place: the signal-cli socket (AF_UNIX, connecting
    # needs no write access to the file system it lives on), its
    # StateDirectory (writable under `ProtectSystem = "strict"` by
    # systemd's own doing), TCP to the addresses in `settings`, and the
    # webhook port. It sets no private network namespace: the bot's traffic
    # is exactly what it is for. `nix/test.nix` boots it under all of this.
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

        # Audit 3, B129: the unit had no bounding set, no address filter, no
        # memory bound, UMask 0022 and an empty SystemCallArchitectures.
        # The set below follows the sandbox of the deployment it runs in
        # (homeserver, `lib/dienst-sandbox.nix`) and holds for any other.
        NoNewPrivileges = true;
        # EMPTY, not a list: the bot needs no capability at all (its own
        # user, a port above 1024). And several lines of this key do not
        # combine the way a single one reads.
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        # Rust, no JIT: nothing here needs memory that is both writable and
        # executable.
        MemoryDenyWriteExecute = true;
        # AF_UNIX for the signal-cli socket, AF_INET(6) for Authentik, Seerr,
        # the *arr, treff and the webhook listener. Nothing else.
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
        # EPERM rather than a kill: a forbidden call fails like any call
        # without the right, instead of taking the process down with it.
        SystemCallErrorNumber = "EPERM";
        # state.json and notices.json are nobody else's business.
        UMask = "0077";
        MemoryMax = cfg.memoryMax;
      }
      // lib.optionalAttrs (cfg.allowedAddresses != [ ]) {
        IPAddressAllow = cfg.allowedAddresses;
        IPAddressDeny = "any";
      }
      // cfg.extraServiceConfig;
    };
  };
}
