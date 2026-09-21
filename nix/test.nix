{ pkgs, module, package }:
let
  # Everything both machines share. The second one adds an [insight]
  # section on top of this and changes nothing else, so any difference in
  # behaviour between the two is that section's doing and nothing else's.
  baseSettings = {
    # Deliberately NOT under /run/signal-seerr: that directory belongs
    # to nobody in particular (the module declares no RuntimeDirectory
    # for it, on purpose -- see nix/module.nix), and this path matches
    # where the real signal-cli's own socket lives
    # (config.example.toml's /run/signal-cli/socket), owned by
    # signal-cli's own unit, not this one.
    signal_socket = "/run/signal-cli/fake.sock";
    signal_account_file = "/run/secrets/account";
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

  # Two machines rather than two phases on one: this module renders exactly
  # one unit from exactly one `settings` attrset, so a second configuration
  # is a second machine. Both run the same two fixtures.
  node = extraSettings: { ... }: {
    imports = [ module ];
    services.signal-seerr = {
      enable = true;
      inherit package;
      settings = baseSettings // extraSettings;
    };

    # A stand-in for signal-cli: the point of this test is the unit, the
    # config file and the socket wait, not the Signal protocol.
    systemd.services.signal-cli = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig.Type = "simple";
      script = ''
        mkdir -p /run/signal-cli /run/secrets
        printf t > /run/secrets/tok; printf k > /run/secrets/key; printf h > /run/secrets/hook
        printf '+490000' > /run/secrets/account
        # A throwaway key for the [insight] machine. Written on both, so
        # the two differ in their `settings` alone.
        printf r > /run/secrets/radarr-key
        # An UNREADABLE notices record -- truncated JSON. The [insight]
        # machine points `notices_file` at it; the other never looks. Under
        # /run and not under the StateDirectory because nothing is meant to
        # write it: the whole assertion is that the bot leaves it alone
        # instead of replacing it with an empty record.
        printf '{ truncated' > /run/notices.json
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

    # A stand-in for Radarr that serves nothing and counts everything: one
    # line in /run/arr-hits per accepted connection. It answers no API at
    # all, on purpose -- the question it exists to answer is whether
    # anybody knocks, and a bot that got an answer here would be a bot that
    # was configured to ask.
    systemd.services.arr-dummy = {
      description = "a Radarr stand-in that only counts connections";
      wantedBy = [ "multi-user.target" ];
      serviceConfig.Type = "simple";
      script = ''
        rm -f /run/arr-hits
        exec ${pkgs.socat}/bin/socat TCP-LISTEN:7878,fork,reuseaddr SYSTEM:'echo hit >> /run/arr-hits'
      '';
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "signal-seerr";

  nodes.machine = node { };

  # The same bot with [insight] switched on and a notices file it cannot
  # read. Everything about this machine other than that section is the
  # first one's configuration.
  nodes.insightnode = node {
    insight = {
      radarr_url = "http://127.0.0.1:7878";
      radarr_key_file = "/run/secrets/radarr-key";
      notices_file = "/run/notices.json";
      # Short on purpose: were the record readable, the watcher would reach
      # the stand-in within seconds, so a wiring that ignored the load
      # error could not hide behind a ten-minute interval.
      poll_seconds = 5;
    };
  };

  testScript = ''
    start_all()

    # ---------------------------------------------------------------
    # 1. Without [insight], nothing ever connects to an arr.
    # ---------------------------------------------------------------
    machine.wait_for_unit("arr-dummy.service")
    machine.wait_for_open_port(7878)

    # THE POSITIVE CONTROL, and it comes first on purpose: "no connection
    # was made" also passes when the counter never worked, when socat died
    # or when the port was never open -- a check that cannot turn red proves
    # nothing. One connection from here has to move the counter.
    machine.succeed("curl -s -m 5 http://127.0.0.1:7878/ || true")
    machine.wait_until_succeeds("test -s /run/arr-hits")
    # And then start from zero. `wait_for_open_port` above opens a
    # connection of its own, so an empty counter was never the state to
    # expect here -- the first run of this test failed on exactly that, and
    # a counter that starts non-empty for a reason nobody wrote down is the
    # kind of thing that gets "fixed" by deleting the assertion. Everything
    # in the file from here on is somebody else's doing.
    machine.succeed("truncate -s 0 /run/arr-hits")

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

    # Long enough to cover a full reconciler cycle (poll_seconds = 30) on
    # top of everything the bot does at startup.
    machine.sleep(35)
    machine.succeed("test ! -s /run/arr-hits")

    # ---------------------------------------------------------------
    # 2. With [insight] and a notices file that cannot be read, the bot
    #    carries on: the dialog, the webhook and the reconciler have
    #    nothing to do with that file.
    # ---------------------------------------------------------------
    insightnode.wait_for_unit("arr-dummy.service")
    insightnode.wait_for_open_port(7878)
    # Same positive control, same reset, on this machine's own counter.
    insightnode.succeed("curl -s -m 5 http://127.0.0.1:7878/ || true")
    insightnode.wait_until_succeeds("test -s /run/arr-hits")
    insightnode.succeed("truncate -s 0 /run/arr-hits")

    insightnode.wait_for_unit("signal-seerr.service")
    insightnode.wait_for_open_port(8080)

    # The failure is in the journal, and it names the file. Written to a
    # file first: grep in a pipeline is what the comment above warns about.
    insightnode.wait_until_succeeds(
        "journalctl -u signal-seerr.service --no-pager -o cat > /tmp/journal.txt; "
        "grep -F -c /run/notices.json /tmp/journal.txt"
    )

    # The decisive one. An unreadable record replaced by an empty one would
    # repeat every notice to everybody, so the file must still be exactly
    # what it was.
    insightnode.succeed("grep -F -x -c '{ truncated' /run/notices.json")

    # And the webhook goes on working, right token and wrong.
    insightnode.succeed(
        "test $(curl -s -o /dev/null -w '%{http_code}' -X POST "
        "-H 'X-Webhook-Token: h' -H 'content-type: application/json' "
        "-d '{\"notification_type\":\"TEST_NOTIFICATION\"}' "
        "http://127.0.0.1:8080/seerr) = 200"
    )
    insightnode.succeed(
        "test $(curl -s -o /dev/null -w '%{http_code}' -X POST "
        "-H 'X-Webhook-Token: nope' -H 'content-type: application/json' "
        "-d '{\"notification_type\":\"TEST_NOTIFICATION\"}' "
        "http://127.0.0.1:8080/seerr) = 401"
    )

    # Still up after all of that -- not restarting in a loop, which a unit
    # with Restart=on-failure would do for about a minute before reaching
    # its StartLimitBurst and finally showing as failed.
    insightnode.sleep(20)
    insightnode.succeed("systemctl is-active signal-seerr.service")

    # And the watcher really did not start, measured rather than assumed:
    # its poll interval here is 5 s, so a loop that had come up would have
    # knocked at the stand-in a dozen times since the counter was reset.
    insightnode.succeed("test ! -s /run/arr-hits")
  '';
}
