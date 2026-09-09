{ pkgs, module, package }:
pkgs.testers.runNixOSTest {
  name = "signal-seerr";

  nodes.machine = { ... }: {
    imports = [ module ];
    services.signal-seerr = {
      enable = true;
      inherit package;
      settings = {
        # Deliberately NOT under /run/signal-seerr: that directory belongs
        # to nobody in particular (the module declares no RuntimeDirectory
        # for it, on purpose -- see nix/module.nix), and this path matches
        # where the real signal-cli's own socket lives
        # (config.example.toml's /run/signal-cli/socket), owned by
        # signal-cli's own unit, not this one.
        signal_socket = "/run/signal-cli/fake.sock";
        signal_account = "+490000";
        authentik_url = "http://127.0.0.1:9";
        authentik_token_file = "/run/secrets/tok";
        seerr_url = "http://127.0.0.1:9";
        seerr_key_file = "/run/secrets/key";
        webhook_listen = "127.0.0.1:8080";
        webhook_token_file = "/run/secrets/hook";
        state_file = "/var/lib/signal-seerr/state.json";
        poll_seconds = 30;
        media_group = "Medien";
        jellyfin_url = "https://example.invalid";
        settings_url = "https://example.invalid/account";
        operator_name = "the operator";
      };
    };
    # A stand-in for signal-cli: the point of this test is the unit, the
    # config file and the socket wait, not the Signal protocol.
    systemd.services.signal-cli = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.Type = "simple";
      script = ''
        mkdir -p /run/signal-cli /run/secrets
        printf t > /run/secrets/tok; printf k > /run/secrets/key; printf h > /run/secrets/hook
        # mode=0777: this script runs as root (no User= set) and socat's
        # default is 0755, owner root. signal-seerr runs under DynamicUser,
        # an ephemeral uid/gid unrelated to root's -- connecting to a UNIX
        # stream socket needs write permission on it, which "other" would
        # not have at 0755. The real deployment solves the equivalent
        # problem deliberately, via extraServiceConfig's
        # SupplementaryGroups; this stand-in has no such group to grant, so
        # it opens the socket to everyone instead. A production socket
        # should NOT be world-writable -- this one exists only for the
        # test.
        exec ${pkgs.socat}/bin/socat UNIX-LISTEN:/run/signal-cli/fake.sock,fork,mode=0777 -
      '';
    };
  };

  testScript = ''
    machine.wait_for_unit("signal-seerr.service")
    # Measured at the result, not at the exit code: the listener must answer.
    machine.wait_for_open_port(8080)
    machine.succeed(
        "curl -sf -o /dev/null -w '%{http_code}' -X POST "
        "-H 'X-Webhook-Token: h' -H 'content-type: application/json' "
        "-d '{\"notification_type\":\"TEST_NOTIFICATION\"}' "
        "http://127.0.0.1:8080/seerr | grep -q 200"
    )
    # A wrong token must be refused, and that is the assertion that can go red.
    machine.succeed(
        "test $(curl -s -o /dev/null -w '%{http_code}' -X POST "
        "-H 'X-Webhook-Token: nope' -H 'content-type: application/json' "
        "-d '{\"notification_type\":\"TEST_NOTIFICATION\"}' "
        "http://127.0.0.1:8080/seerr) = 401"
    )
  '';
}
