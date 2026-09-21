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

quality_profiles = [
  "Dual Language, then German (1080p)",
  "Rarity, original language (SD too)",
]
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
- `quality_profiles` are matched against the *arr behind Seerr **by name**,
  and offered in the order written here. Radarr and Sonarr keep separate id
  spaces — on the author's instance both happen to run 7..11, which is
  exactly what makes a number-based mapping look correct until a profile is
  added on one side — so nothing in this bot ever maps a profile by number.
  A name no *arr knows is dropped with a line in the journal rather than
  renumbering the list under the people who learned it. Leave the field out
  and the bot offers Seerr's own list in Seerr's order; leave Seerr with no
  profiles and the question is skipped entirely, the request going out with
  no `profileId` at all. A wish never fails because a question could not be
  asked.
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

## Status and stalled requests

A wish that gets stuck used to be silent for ever: somebody asked, was told
"it's on the list", and never heard about it again. `/status` said *being
fetched* about everything — the same sentence for a film that is downloading
at 80 %, one that hasn't come out yet, and one that nobody anywhere has in a
version this household accepts.

The optional `[insight]` section gives the bot read-only access to Radarr and
Sonarr, and with it two things:

**`/status` says what is actually the case.** Ten states, each backed by
something measured rather than assumed — *here*, *part of it is here, the
rest is still coming*, *downloading, 43 %*, *downloaded, but stuck on the
last step*, *not out for home viewing yet (expected from …)*, *one attempt
failed, I'm still looking*, *still looking, nothing suitable so far*, *so far
only available in Portuguese*, *I couldn't put it on the list*, and plain
*waiting* where nothing was measured at all. That last one matters: without
`[insight]`, and for anything the bot has no evidence about, it says
*waiting* instead of claiming a search nobody made.

**The bot speaks up unasked when a wish is stuck.** Once per request and per
kind of problem — never twice for the same thing, and never more than one
unasked message about the same request per day — after `stall_after_hours`
have passed with no progress. Three kinds of problem count: nothing suitable
found, a failed download attempt, and a download that finished but is stuck
on the import. What has already been said is kept in `notices_file`, and a
reason an indexer search turned up is written there *before* anybody is told
about it, so a message that never got sent never costs a second search.

```toml
[insight]
radarr_url      = "http://192.0.2.30:7878"
radarr_key_file = "/run/credentials/signal-seerr.service/radarr-key"

# Optional on top of Radarr's, and a pair: one of the two without the other
# is a load error, not a half-configured Sonarr that fails later.
sonarr_url      = "http://192.0.2.40:8989"
sonarr_key_file = "/run/credentials/signal-seerr.service/sonarr-key"

poll_seconds      = 600   # how often the open wishes are looked at
stall_after_hours = 24    # no progress for this long = stuck

reason_search               = false
max_reason_searches_per_day = 5

notices_file = "/var/lib/signal-seerr/notices.json"

# Profile NAME -> the languages that profile REQUIRES. Only listed profiles
# ever get the "so far only available in <language>" reason.
[insight.profile_languages]
"Dual Language, then German (1080p)" = ["German", "English"]
```

Leave the whole section out and the bot behaves exactly as it did before any
of this existed: no arr client is built, no arr key is read, and nothing ever
connects to Radarr or Sonarr.

`notices_file` belongs next to `state_file`, in a directory the service may
write to (`StateDirectory=signal-seerr` covers both on the NixOS module). A
file that exists but cannot be read or parsed is an **error naming the
path**, never an empty record — an emptied record would tell everybody about
every wish all over again. In that case the bot logs it, keeps answering
`/status`, and simply does not run the unasked loop until somebody has looked
at the file.

### What `reason_search` costs

Off by default, and that is not timidity. Switched on, it lets the bot run
**one interactive indexer search per stalled request** — Radarr's
`/api/v3/release`, which asks every indexer the operator has configured, the
same thing a human clicking "Interactive Search" triggers. It is budgeted
twice over: at most one per request, ever, and at most
`max_reason_searches_per_day` across the whole household per UTC day. It is
**never** reachable from a chat command; `/status` cannot trigger one, no
matter who types it. A search that reached the indexers and then failed is
counted all the same — a budget that only counts successes is no budget.

What it buys is the difference between *still looking, nothing suitable so
far* and *so far only available in Portuguese* (or *too big*, *too small*,
*picture quality*). The reason comes from the releases' structured fields
alone; an indexer's name, a release's name and the operator's own
custom-format scores are never deserialised, never logged and never sent to
anybody.

**A series gets no reason search.** Sonarr has no equivalent of Radarr's
per-movie interactive search here, so a series is judged from its queue and
its history only: it can be *here*, *part of it is here*, *downloading*,
*stuck on the last step*, *one attempt failed*, or *waiting* — but never
"only available in …". That is a limit of what was measured, not a gap
waiting to be filled with a guess.

### Put a filtering proxy in front of Radarr and Sonarr

**Recommended, and the reason is worth reading before deciding against it: an
arr API key is full access.** There is no read-only key. The same key that
answers "what is the state of movie 412" will delete a film and its files,
rewrite the download clients, and — via Radarr's and Sonarr's *custom
scripts* — run an arbitrary command on the machine they sit on. Handing that
to a chat bot is handing it to whatever the chat bot's worst day looks like.

So give the bot a reverse proxy instead of the arr itself, and let through
`GET`, and only `GET`, on exactly these paths:

| Service | Path |
|---|---|
| Radarr | `/api/v3/movie/{id}` |
| Radarr | `/api/v3/queue` |
| Radarr | `/api/v3/history/movie` |
| Radarr | `/api/v3/release` |
| Sonarr | `/api/v3/queue` |
| Sonarr | `/api/v3/history/series` |

That is the complete list — the bot calls nothing else, so anything else
arriving at the proxy is worth a look rather than a rule. `radarr_url` and
`sonarr_url` may carry a path part (`http://192.0.2.50:7870/radarr`), so one
proxy can front both.

Leave `/api/v3/release` out of the list if `reason_search` stays off; the bot
then never asks for it. And note what that endpoint answers with: a release
list carries indexer URLs, and those URLs carry the operator's passkeys. The
bot deserialises three fields out of each release (`rejected`, `rejections`,
`languages`) and nothing else, so a passkey never reaches a log line or a
message — but it does cross the wire, which is one more reason for the proxy
to sit between the two rather than the key travelling further than it has to.

## License

AGPL-3.0-only — see `LICENSE`. Running a modified version of this bot for
other people to use obliges you to offer them its source, under the same
license; see the license text for what exactly that requires.
