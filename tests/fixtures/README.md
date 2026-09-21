# Recorded answers

Every file here is what a running Seerr 3.2.0 / Radarr instance actually sent
on **2026-09-21**, not what somebody believed it sends. They exist because of
one expensive lesson: Seerr sends `request_id` as a *string*, every test in
this repository built it as a *number*, and all of them agreed with each other
and none with reality. It happened a second time with `media.title`, which
`/user/{id}/requests` never carries at all.

**Rules**

- A test for a wire format reads one of these files. It does not build the
  body itself.
- Never hand-edit the *structure*. If the shape is wrong or a case is missing,
  record it again.
- Every file was **trimmed on the server**, before it ever reached a
  workstation: the raw answers carry API keys, passkeys inside download URLs,
  e-mail addresses and avatars. What was dropped is listed below.
- *Values* that identify a person or what they wished for were replaced
  (`jellyfinUsername` → `person-<id>`, `tmdbId` → `100000 + index`, titles →
  `Film A`). This is a public repository; what somebody asked for is nobody
  else's business. The replaced values keep their type and position.
- Before committing a new recording: look at its *shape* only
  (`gestalt < file`), and scan it against the real secret values
  (`leakwatch scan --source files --files 'tests/fixtures/*.json'`).

| File | Endpoint | Dropped |
|---|---|---|
| `seerr-user-requests.json` | `GET /api/v1/user/1/requests?take=50` | `modifiedBy`; `requestedBy` except `id`, `jellyfinUsername`; `media.*Url*`, `media.jellyfinMediaId*` |
| `seerr-request-all.json` | `GET /api/v1/request?take=100&skip=0&filter=all&sort=added` | same |
| `radarr-movie-announced.json` | `GET /api/v3/movie/{id}`, a film with `status = announced` | everything except id, title, status, availability, dates, profile id |
| `radarr-movie-incinemas.json` | same, `status = inCinemas` | same |
| `radarr-movie-released-missing.json` | same, released, searched, no file | same |
| `radarr-history-movie-download-failed.json` | `GET /api/v3/history/movie?movieId={id}` for a film with a `downloadFailed` event | `sourceTitle`, `quality`, `languages`, all of `data` except `reason` (indexer, download client, release name, message) |
| `radarr-history-movie-empty.json` | same, for a film nothing was ever grabbed for | — |
| `radarr-queue-empty.json` | `GET /api/v3/queue?pageSize=200&includeMovie=false`, nothing downloading | — |
| `radarr-release-all-rejected-language.json` | `GET /api/v3/release?movieId={id}` — **an interactive search at every indexer; recorded once, never re-recorded casually** | `guid`, `downloadUrl`, `infoUrl`, `indexer`, `title`, `releaseGroup`, and everything else except `rejected`, `temporarilyRejected`, `approved`, `rejections`, `languages[].{id,name}`, `quality.quality.name`, `size`, `protocol`, `customFormatScore`; the film's title inside one rejection sentence was replaced |

**Not recorded yet, and therefore not built:** a queue with a running
download, a queue entry stuck in import, and Seerr's `media.downloadStatus`
while something downloads. The queue was empty every time anybody looked.
