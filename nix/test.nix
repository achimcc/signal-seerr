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
        #
        # EXEC:cat, not the bare "-" (this process's own stdio): under
        # systemd a service's stdin is /dev/null by default, so "-" hits
        # EOF the instant a client connects, socat tears the connection
        # down again immediately, and signal-seerr treats that exactly as
        # it should treat a real dropped connection -- as fatal, exiting
        # so its restart limit can do its job (see nix/module.nix). That is
        # correct behaviour for a connection that really closes, but it
        # means this fixture could never hold still long enough to test the
        # steady running state at all -- the assertions below need the bot
        # to actually stay up. EXEC:cat gives each accepted connection a
        # fresh subprocess with its own pipe, which just blocks reading
        # (nothing is ever sent to it) rather than hitting EOF, so the
        # connection stays open the way a real signal-cli's would.
        exec ${pkgs.socat}/bin/socat UNIX-LISTEN:/run/signal-cli/fake.sock,fork,mode=0777 EXEC:${pkgs.coreutils}/bin/cat
      '';
    };
  };

  testScript = ''
    machine.wait_for_unit("signal-seerr.service")
    # Measured at the result, not at the exit code: the listener must answer.
    machine.wait_for_open_port(8080)
    # Both assertions compare the status code itself instead of piping into
    # "grep -q". The test driver runs each command under "set -o pipefail",
    # and "grep -q" exits at its first match, closing the pipe; the writer
    # ahead of it can then take a SIGPIPE and turn a matched status code into
    # a failed test. Whether that fires depends on whether the response fits
    # a pipe buffer, not on whether the code is right -- a failure that comes
    # and goes with the size of the payload.
    machine.succeed(
        "test $(curl -s -o /dev/null -w '%{http_code}' -X POST "
        "-H 'X-Webhook-Token: h' -H 'content-type: application/json' "
        "-d '{\"notification_type\":\"TEST_NOTIFICATION\"}' "
        "http://127.0.0.1:8080/seerr) = 200"
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
