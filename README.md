# signal-seerr

A Signal bot that lets people in a household or a small community search for
and request movies and series through [Seerr](https://github.com/sct/overseerr)
(Overseerr or Jellyseerr) — from a normal Signal chat, no app or account of
their own to set up.

Send it a title, it searches; reply with a number to request it; it tells you
once it's there. No conversation with the bot ever happens outside Signal —
it never asks anyone for a Seerr password, because it never needs one.

```
you:  Blade Runner 2049
bot:  1. Blade Runner 2049 (2017) — Movie
      2. Blade Runner (1982) — Movie
      Reply with the number. "m" shows more.
you:  1
bot:  Blade Runner 2049 (2017) is on the list. I'll let you know once it's here.
      Wrong one? /withdraw 1849 takes it off again.
…later…
bot:  "Blade Runner 2049 (2017)" is here.
      https://jellyfin.example.invalid
```

## How it fits together

```
Signal ⇄ signal-cli (JSON-RPC, unix socket) ⇄ signal-seerr ⇄ Seerr (REST + webhook)
                                                    ↕
                                                Authentik (who is allowed to ask)
```

- **[signal-cli](https://github.com/AsamK/signal-cli)** holds the actual
  Signal registration and talks JSON-RPC over a unix socket. signal-seerr
  does not implement the Signal protocol itself — it is a client of
  signal-cli, nothing more.
- **Seerr** (Overseerr or Jellyseerr) is where requests actually land. The
  bot searches its API, places requests through it, and is in turn notified
  by its webhook when something becomes available or fails.
- **Authentik** is the source of truth for *who is allowed to ask*. The bot
  polls it for a user attribute holding somebody's Signal username, and for
  group membership; it never invents an account of its own. Any directory
  could stand in here in principle, but the code speaks Authentik's API
  today — see `src/directory/mod.rs`.

## What you need before you start

1. **A Seerr instance** you already use, with an **administrator** API key
   (Settings → General → API Key — it has to be an admin's: the bot places
   every request as the person who asked, via Seerr's `X-API-User` header,
   which only an admin key may set).
2. **Seerr accounts created through a Jellyfin login**, not Seerr's own
   local accounts. The bot matches a Seerr user to an Authentik user by
   `jellyfinUsername`, so it only ever finds someone who has signed into
   Seerr via Jellyfin at least once — and that Jellyfin username has to be
   identical to their Authentik username. Where Jellyfin itself
   authenticates against Authentik (for example through its LDAP outpost),
   that identity is already the same string; if yours doesn't, this is the
   piece to line up first.
3. **signal-cli**, running as a daemon with a JSON-RPC unix socket
   (`signal-cli --config <dir> daemon --socket <path>`), under **its own
   registered Signal phone number** — see the two manual steps below.
4. **An Authentik instance** with:
   - a group whose members are allowed to make requests,
   - a custom user attribute holding each allowed person's Signal
     *username* (not phone number — see below), for example
     `signal_username`,
   - an API token with read access to users and groups.

## The two manual steps

signal-cli's registration is not something this bot does for you, and
neither is claiming a username — both are one-time, interactive, and belong
to the *bot's own* Signal identity, done once with signal-cli itself before
signal-seerr ever runs:

```
signal-cli -a +1555… register --voice
signal-cli -a +1555… verify <code from the call>
signal-cli -a +1555… updateAccount --username signal-seerr.99
```

The voice call, not SMS, because a freshly bought number often has no SMS
route yet. The username step matters beyond cosmetics: without it, everyone
in the chat sees the bot's phone number instead of a name.

The same mechanism is why **each person who wants to use the bot needs a
Signal username too**, not just a phone number: signal-seerr resolves the
`signal_username` attribute in Authentik to a Signal account id by asking
signal-cli "who is behind this username" (`getUserStatus`). A phone number
alone cannot be looked up this way without that person's number being in
signal-cli's own address book — a username can always be resolved. Signal
users claim one for themselves in Signal's own app, under Settings →
Profile → Username.

## Configuring it

Copy `config.example.toml` and fill it in:

```toml
signal_socket       = "/run/signal-cli/socket"
signal_account_file = "/run/credentials/signal-seerr.service/signal-account"

authentik_url        = "http://authentik.example.invalid:9000"
authentik_token_file = "/run/credentials/signal-seerr.service/authentik-token"

seerr_url      = "http://seerr.example.invalid:5055"
seerr_key_file = "/run/credentials/signal-seerr.service/seerr-key"

webhook_listen     = "0.0.0.0:8080"
webhook_token_file = "/run/credentials/signal-seerr.service/webhook-token"

state_file = "/var/lib/signal-seerr/state.json"

poll_seconds  = 30
media_group   = "Medien"
jellyfin_url  = "https://jellyfin.example.invalid"
settings_url  = "https://example.invalid/account"
operator_name = "the operator"
```

**No secret goes in this file.** Every credential is a `*_file` option
pointing at a file that holds it — a systemd credential, a Docker secret, or
just a file with tight permissions; signal-seerr only ever reads it, trimmed
of surrounding whitespace. Run it under whatever process supervisor you use
and give it those four files plus the config.

A field-by-field note on what is not obvious from the name:

- `signal_account_file` holds the bot's own registered phone number, not a
  username — the same value you passed to `signal-cli -a` in the manual
  steps above. It is a `*_file` option, not a plain string, because the
  whole point of the username step is to keep that number away from the
  people the bot talks to; putting it in `settings` would leave it sitting
  in a world-readable Nix store path (and in the config's own git history)
  instead.
- `authentik_url` / `seerr_url` need an explicit scheme (`http://` or
  `https://`) — a bare host:port is rejected at startup, not at the first
  request.
- `media_group` is the Authentik group that gates *making* requests. Being
  known to the bot and being in this group are different things: an
  Authentik user without it gets told to ask `operator_name`, not a generic
  error.
- `settings_url` is shown to a Signal sender the bot does not recognise at
  all — wherever your Authentik-backed self-service page lives, where they
  can enter their Signal username.
- `jellyfin_url` is only ever put into the "it's here" message; the bot
  never talks to Jellyfin directly.
- `poll_seconds` is how often the Authentik directory is reconciled — a new
  member's greeting, and a removed member's goodbye, land within this
  window, not instantly.

Messages the bot sends live in `i18n/en.toml` and `i18n/de.toml`, chosen per
person by their Authentik locale — there is no in-chat language switch. Add
a third file for a third language and it also needs wiring into
`src/i18n.rs`.

### Running it

```
cargo build --release
./target/release/signal-seerr config.toml
```

signal-seerr waits (with a bound, not forever) for signal-cli's socket to
appear, then runs until stopped. A NixOS module is included
(`nix/module.nix`, exported as `nixosModules.default`) for anyone on NixOS;
everyone else runs the binary under systemd, runit, or whatever else
supervises long-running processes on their system — there's nothing
NixOS-specific about the binary itself.

## Setting up Seerr's webhook

In Seerr's notification settings, add a **Webhook** agent pointed at:

```
http://<host where signal-seerr runs>:8080/seerr
```

(the port comes from `webhook_listen`). Keep the default JSON payload
template — signal-seerr reads only `notification_type`, `subject`, and
`request.request_id` from it and ignores the rest, so there is nothing to
customise there. Set whatever custom-header field your Seerr version offers
to send:

```
X-Webhook-Token: <the same value that is in webhook_token_file>
```

A request with a missing or wrong token gets a plain 401 and is never
parsed. Enable at least the "Media Available" and "Media Failed"
notification types; anything else Seerr sends is accepted and dropped
without an error, so enabling more does no harm.

## License

AGPL-3.0-only — see `LICENSE`. Running a modified version of this bot for
other people to use obliges you to offer them its source, under the same
license; see the license text for what exactly that requires.
