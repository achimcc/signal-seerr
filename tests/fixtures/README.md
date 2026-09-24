# Recorded answers

Every file here is what a running Seerr 3.2.0 / Radarr / Sonarr instance actually sent
on **2026-09-21** or **2026-09-24** (one exception, named below), not what somebody believed it sends. They exist because of
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
| `radarr-queue-downloading.json` | same, one movie downloading (recorded **2026-09-22**) | everything except `movieId`, `size`, `sizeleft`, `status`, `trackedDownloadStatus`, `trackedDownloadState`, `statusMessages[].messages` (`title`, `outputPath`, `indexer`, `downloadClient`, `downloadId` dropped; `statusMessages[].title` replaced with `"x"`) |
| `radarr-release-all-rejected-language.json` | `GET /api/v3/release?movieId={id}` — **an interactive search at every indexer; recorded once, never re-recorded casually** | `guid`, `downloadUrl`, `infoUrl`, `indexer`, `title`, `releaseGroup`, and everything else except `rejected`, `temporarilyRejected`, `approved`, `rejections`, `languages[].{id,name}`, `quality.quality.name`, `size`, `protocol`, `customFormatScore`; the film's title inside one rejection sentence was replaced |
| `seerr-user-requests-downloading.json` | `GET /api/v1/user/1/requests?take=50`, taken during the same download as `radarr-queue-downloading.json` (recorded **2026-09-22**) | same as `seerr-user-requests.json`, plus `media.downloadStatus[].title` replaced with `"x"` and `downloadId` zeroed |
| `sonarr-series-complete.json` | `GET /api/v3/series/1` (recorded **2026-09-24**, Sonarr 4.0.20.3014) | everything except `id`, `monitored`, `status`, `firstAired`, `nextAiring`, `previousAiring`, `qualityProfileId`, `seasons[].{seasonNumber,monitored,statistics.{episodeFileCount,episodeCount,totalEpisodeCount,nextAiring,previousAiring}}`, `statistics.{seasonCount,episodeFileCount,episodeCount,totalEpisodeCount}` |
| `sonarr-episode-complete.json` | `GET /api/v3/episode?seriesId=1` (2026-09-24), six episodes, all on file | everything except `id`, `seriesId`, `seasonNumber`, `episodeNumber`, `airDateUtc`, `hasFile`, `monitored`, `episodeFileId` |
| `sonarr-episode-two-missing.json` | **values edited**, structure not: the file above with episodes 4 and 5 set to `hasFile = false`, `episodeFileId = 0`, and episode 6 additionally moved to `airDateUtc = 2030-01-01T20:00:00Z` -- two missing aired episodes and one not aired yet. No running instance has been seen in this state; the shape is the recorded one | as above |
| `sonarr-history-series.json` | `GET /api/v3/history/series?seriesId=1` (2026-09-24), the newest 20 of 2693 entries | everything except `date`, `eventType`, `seriesId`, `episodeId`, `data.reason` |
| `sonarr-queue-empty.json` | `GET /api/v3/queue?pageSize=200&includeSeries=false` (2026-09-24), nothing downloading | — |
| `sonarr-release-season-all-rejected.json` | `GET /api/v3/release?seriesId=1&seasonNumber=1` (2026-09-24) — **an interactive season search at every indexer; recorded once**. 80 of 662 entries, chosen to keep the mix: all 29 whose rejection names another series, 10 `Unknown Series`, 41 carrying `German`, 10 `is not wanted in profile` | as the Radarr release file; **the series titles inside the `… matches an alias for series with TVDB ID: N` sentences were replaced by `Series X` and the id by `0`** |
| `radarr-queue-import-blocked.synthesised.json` | **not a recording.** `radarr-queue-downloading.json` with exactly one value changed: `trackedDownloadState` from `downloading` to `importBlocked`. The value comes from the source of the deployed versions (`src/NzbDrone.Core/Download/TrackedDownloads/TrackedDownload.cs`, Radarr v6.4.4.10685 and Sonarr v4.0.20.3014, identical enums: `downloading, importBlocked, importPending, importing, imported, failedPending, failed, ignored`). The first real stuck import that is seen replaces this file | — |

**Which free-text rejection sentences are backed by a recording, per service.**
`reason_from` matches three of Radarr's fixed sentences. Radarr's recording
carries all three (`larger than maximum allowed`, `smaller than minimum
allowed`, `is not wanted in profile`). Sonarr's season search carries only
`is not wanted in profile`; the size sentences have not been seen from Sonarr,
so a Sonarr search whose releases would fall on size alone reads as
`Otherwise` until a recording says otherwise. Sonarr's recording also shows
two things Radarr's never did: releases that belong to another series
(`Unknown Series`, `… matches an alias for series with TVDB ID: N`), and the
language names `Unknown` and `Original` -- both handled in `reason_from`, both
pinned by a test over this file.

**The one synthesised file** is named so (`*.synthesised.json`) and explained
in the table. Nothing else here was built by hand.
