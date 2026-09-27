# Jellyfin 12.1 bug hunt

Date: 2026-09-27. Scope: a targeted hunt against Jellyfin 12.1 (tag `v12.1`), starting from six
known issues plus a broad hunt (HLS/segment handling, kill timers, device profiles, playback
decisions, library scan, trickplay, session/progress reporting, auth). Fixes ship as patches in
`jellyfin-perf/bughunt/`, applied by `jellyfin-perf/build.sh` after `jellyfin-12.1-perf.patch`, in
numeric order.

**Build:**

```
BUGHUNT=1 ./jellyfin-perf/build.sh   # perf patch + all bughunt/NN-*.patch (default)
BUGHUNT=0 ./jellyfin-perf/build.sh   # perf patch only
```

`build.sh` clones/checks out `v12.1`, applies `jellyfin-12.1-perf.patch`, then every
`bughunt/[0-9][0-9]-*.patch` in numeric order, builds `Jellyfin.Server` (Release), and copies the
touched assemblies into the overlay directory (`$JF_OVERLAY`, default
`~/.cache/jellymesh-vendor/jellyfin-perf`) for the lab (`JG_OVERLAY=… lab/jf-galera.sh up`).

**Overlay assemblies** (as of this series): `Emby.Server.Implementations`,
`Jellyfin.Server.Implementations`, `MediaBrowser.Controller`, `MediaBrowser.MediaEncoding`,
`MediaBrowser.Model`, `Jellyfin.Api`, `jellyfin`. The last three (`MediaBrowser.Model.dll`,
`Jellyfin.Api.dll`, `jellyfin.dll`) are new for this series — the query patch alone only touched
the first four. All public API changes are additive (no existing public signature changed or
removed), so plugins built against stock 12.1 keep binding.

**Deployment: nothing here has been built into an image or deployed.** Every number in this
document comes from the build/test/measurement commands shown; the owning session builds and
ships the image separately.

**Integrated verification** (MEASURED on the integrated tree, perf patch + all 14 patches
`00`-`13`, Release build): 0 warnings / 0 errors. Tests, all passing: `Jellyfin.Api.Tests` 160,
`Jellyfin.Controller.Tests` 218 (5 consecutive clean runs), `Jellyfin.MediaEncoding.Tests` 113
(+1 skipped), `Jellyfin.MediaEncoding.Hls.Tests` 18, `Jellyfin.Model.Tests` 754,
`Jellyfin.Server.Implementations.Tests` 980 (+12 skipped; 4 consecutive clean runs),
`Jellyfin.Server.Tests` 20, `Jellyfin.Providers.Tests` 481. `build.sh` re-run end to end on a
throwaway clone of the tree: all patches apply, build clean, 7 overlay assemblies produced.

## Evidence labels

- **MEASURED** — a command was run and its output is quoted or summarized.
- **INHERITED** — established by reading the code, not by running anything against a live system.
- **MIXED** — some parts of the claim are MEASURED, others (usually the production trigger
  mechanism) are INHERITED or unconfirmed.

## Summary

| # | Patch | Maps to | Severity | Evidence | Behaviour change | Test |
|---|---|---|---|---|---|---|
| 00 | `00-tests-fixup.patch` | build fix | — | MEASURED | none (test project only) | `Jellyfin.Server.Implementations.Tests` compiles again; 980/12 skipped, 5/5 clean runs |
| 01 | `01-killtimer-wipe-race.patch` | Known issue #2 | high | MIXED | Deferred kill-delete skips when a replacement transcode job is already registered on the same output path | 3 new `TranscodeManagerTests`; assembly 103 passed / 1 skipped |
| 02 | `02-killtimer-paused-grace.patch` | Known issue #2 | high | MIXED | Paused HLS/DASH job gets a 180s kill grace instead of 60s; unpaused HLS/DASH and Progressive unchanged | 5 new `TranscodeManagerPingTimerTests` (2 discriminate from baseline); assembly 110 passed / 1 skipped |
| 03 | `03-k2-incident-and-shared-dir.patch` | Known issue #2 | medium | MIXED | A node with no locally-tracked job waits on a still-fresh shared-dir segment file before starting its own ffmpeg | 9 new unit tests on the extracted helpers; `Jellyfin.Api.Tests` 148 passed |
| 04 | `04-segment-wait-request-aborted.patch` | Known issue #2 | medium | MIXED | Segment-wait loops exit on client/proxy abort instead of polling for the rest of the transcode's runtime | `DynamicHlsControllerTests` 12/12 (11 prior + 1 new) |
| 05 | `05-remux-desync.patch` | Known issue #1 | medium | MIXED | HLS copy-video remux: the transcoded audio track no longer accurate-seeks past a target the copied video track can't reach | ffmpeg PTS measurement (below); no dotnet regression test (ffmpeg-argument-level change) |
| 06 | `06-pgs-default-same-language-text.patch` | Known issue #4 | medium | MEASURED | Default/Smart subtitle selection prefers a same-language text track over a tied image (PGS/VobSub) track; fixes a dead Always-mode scoring fallback | 9 new `MediaStreamSelectorTests` cases |
| 07 | `07-progress-write-stall.patch` | Known issue #6 | medium | MIXED | `PlaybackProgress` writes get a bounded async path and a narrower conflict predicate; Start/Finished/manual edits unchanged | New `UserDataManagerTests` cases (sync + async retry, coalescing) |
| 08 | `08-mediasources-batch.patch` | Known issue #6 | high | MIXED | A page of items with `Fields=MediaSources`/`MediaStreams` batches into ~3 statements instead of 3 per item | `DtoServiceMediaSourceBatchTests`; statement count 3N → 3 |
| 09 | `09-stopped-no-position.patch` | other | medium | MEASURED | A positionless Stop no longer assumes `Played=true` when this node has no session record for the item | New `SessionManager` regression tests |
| 10 | `10-displayprefs-retry.patch` | other | medium | MIXED | A concurrent first-write for the same display-preferences key retries instead of surfacing a 500 | New `DisplayPreferencesManager` retry test |
| 11 | `11-trickplay-ctx-dispose.patch` | other | medium | MEASURED | `DeleteTrickplayDataAsync` disposes its `DbContext` promptly instead of via GC finalization | New disposal-tracking regression test |
| 12 | `12-stale-web-client.patch` | Known issue #3 | low | MIXED | `serviceworker.js` gets `Cache-Control: no-cache`; a throttled warning log fires on a stale Jellyfin Web version | New version-comparison regression test |
| 13 | `13-dv7-to-81-decision.patch` | Known issue #5 | medium | MIXED | Opt-in (`JELLYMESH_DOVI_P7_TO_81=1`): DV profile 7 to a DOVI-capable HLS client is copied with a pool conversion marker and advertised as DV 8.1 instead of stripped to HDR10; flag off = unchanged | `EncodingHelperDoviTests`, `StreamBuilderDoviP7ToP81Tests` |

All 12 implemented fixes (00 and 13 are the build-compile fix and the placeholder, not counted)
were independently re-reviewed by a second pass that re-ran the build and tests rather than
trusting the first pass's numbers; all 12 came back `approve`. Four required a fixup round before
approval (`k2-incident-and-shared-dir`, `segment-wait-request-aborted`, `progress-write-stall`,
`stopped-no-position` — see each section below for what changed).

## Known issue #1 — remux restart A/V desync

**Reported:** an HLS remux restart (video `-codec:v copy`, audio transcoded, `-copyts
-start_at_zero`, fMP4, Safari) drifts audio out of sync; video can only restart on a source
keyframe, audio restarts exactly at the requested time, and there's no `#EXT-X-DISCONTINUITY`.

**Root cause:** `EncodingHelper.GetFastSeekCommandLineParameter` (in the HLS-remux branch) adds a
defensive ~0.5s nudge to `-ss` so ffmpeg's keyframe-snapped seek lands on the intended keyframe for
the copied video stream. Left at ffmpeg's default, *accurate seek* then honors that same nudged
`-ss` literally for the audio stream (which is actually decoded/re-encoded), so audio starts up to
~0.5s after video's keyframe on an already-aligned request, and up to a full keyframe interval
late on an unaligned one — video can only ever start at the keyframe at-or-before the request;
accurate seek still puts audio at the literal, later, unreachable-for-copy target.

**Fix:** add `-noaccurate_seek` in the same `isHlsRemuxing` branch, keeping the existing nudge (still
needed for the copy-video seek to land on the right keyframe). This makes the audio track seek at
the same demuxer/packet level as the copied video track.

**Evidence (MEASURED):** real jellyfin-ffmpeg encoder, synthetic long-GOP h264+DTS 5.1 source,
10s keyframe interval, two seek cases:

| Case | Before (video / audio) | After (video / audio) |
|---|---|---|
| Aligned (`-ss 10.5`, keyframe at 10.0) | 10.000 / ~10.46–10.5 (audio ~0.46–0.5s late) | 10.000 / 9.877 (audio leads by ~0.12s) |
| Unaligned (`-ss 17.833`) | 10.000 / ~17.79 (audio ~7.79s late) | 10.000 / 9.877 (same ~0.12s lead) |

The residual ~0.12s is bounded by audio-frame granularity, not by how far into the GOP the
original request fell. Two alternatives were measured and rejected: un-nudging only the audio
target (fixes the aligned case, barely touches the unaligned one — ~7.29s residual — and needs a
second decode of the input); shrinking the video nudge to a small epsilon (unsafe: on this
build/source, epsilons up to 100ms still round to the *previous* keyframe).

Splicing the restarted segment after the prior uninterrupted one repeats ~43ms (2 AAC frames) of
audio already delivered by the previous segment's tail — a small, bounded overlap, not a gap.
`EXTINF` values are unaffected. A video-copy + audio-copy restart (e.g. AAC source) is unaffected —
neither stream is decoded, so accurate/non-accurate seek is a no-op for that combination (measured
byte-identical PTS with and without the flag). Fully transcoded video and non-HLS remuxing are
untouched (the same flag gate that already applied the nudge also gates the new flag).

**Not done:** `#EXT-X-DISCONTINUITY` was evaluated and dropped — the residual gap measured too
small to need it. **Residual risk (INHERITED):** client-side behaviour (Safari/hls.js/ExoPlayer/Kodi
handling of the ~0.12s splice) was not tested in this environment. One other code path that shares
this seek-parameter logic (an external-subtitle input alongside stream-copied video) also picks up
the new flag; reasoned safe by tracing, and the reviewer additionally judged that specific
combination effectively unreachable in this deployment, but neither is independently measured.

## Known issue #2 — kill timer never replaced

**Reported:** a transcode job killed by the ping-timeout kill timer is never replaced; the client
stalls forever (a named incident, 2026-09-27 ~12:35Z).

**What the incident actually was (MEASURED, VictoriaLogs, prod, single backend throughout that
day):** client-side, not a server bug. 147 of 148 Traefik requests for that session's segments
returned 200 (the 148th was a 304, an ordinary cache revalidation, not an error), at a steady ~3s
cadence, with no retries and no 5xx anywhere in the window. The client then simply stopped
requesting anything — roughly two minutes passed before the kill timer fired. The kill timer
correctly reaped an already-abandoned session; there was nothing to "replace" because the client
never came back.

That said, four real defects and two real HA gaps were found and traced along the way, and four of
them are fixed here:

**01 — `killtimer-wipe-race` (fixed, high, MIXED — code-derived, not reproduced in the named
incident).** The kill timer's deferred (1.5s) partial-stream-file delete ran unconditionally, with
no check for whether a replacement job had since been registered for the same output path.
`DynamicHlsController`'s restart-on-retry path can register a replacement for that exact path
inside the delay window (e.g. a client pause/resume that lands past the ping timeout); the delete
would then wipe the replacement's freshly-written segment/init file, and the replacement's
wait-for-file loop would poll a path that never reappears while holding the per-path playlist lock
— wedging every later request for that session. Fix: `DeletePartialStreamFiles` now takes the same
per-path lock the controller holds while starting a replacement (`DynamicHlsController.cs:1460` →
`StartFfMpeg` around line 1517 → `OnTranscodeBeginning`, `TranscodeManager.cs:490`, which sets
`job.Path` at `TranscodeManager.cs:633-660` — the same string as the lock key) and skips the delete
if a job with the same path/type has been re-registered by the time it acquires that lock. Verified
deadlock-safe by reading every caller of `KillTranscodingJobs`: none holds this lock when calling
in, and the controller's own restart path always passes `delete=false`, so it never reaches this
code at all.

**02 — `killtimer-paused-grace` (fixed, high, MIXED).** `TranscodingJob.IsUserPaused` is written on
every `Sessions/Playing/Progress` ping but was never read; `PingTimer` picked the kill timeout
purely from job type (10s Progressive / 60s otherwise), so a paused-but-connected HLS/DASH client
that sends fewer pings got reaped on the same cadence as an abandoned session, forcing a cold
restart on resume. Fix: HLS/DASH now get a 180s (3-minute) grace when the last ping reported
`IsPaused=true`; unpaused HLS/DASH stays at 60s, Progressive always stays at 10s. MEASURED (prod
VictoriaLogs, 8-day window): 20 "Killing transcoding" events, 3 of them against the same play
session ~17–43 minutes apart — the repeat-kill/cold-restart signature this bug produces. DEBUG-level
pause-ping logs aren't retained in prod, so it's unproven those specific 3 kills followed a pause;
the fixer and reviewer both keep this MIXED rather than claiming proof.

**03 — `k2-incident-and-shared-dir` (fixed, medium, MIXED).** `DynamicHlsController`'s
`GetCurrentTranscodingIndex` (`DynamicHlsController.cs:2084-2105`) gates entirely on this node's own
in-memory job list — per-process — before ever checking disk. In a deployment where nodes share one
transcode directory, a node with no local job can't tell "nothing is running anywhere" from
"another node already owns this," and starts its own ffmpeg into the same shared path
unconditionally. MEASURED live in the lab (not just code-inspection-reachable): two replicas
started ffmpeg — both `-start_number 0` — into the identical shared output path ~10.6 seconds
apart. Fix: before starting, check the shared directory for a segment written within the last 2x
segment length; if fresh, wait (bounded, up to 3x segment length, floor 15s) instead of racing it.
Disclosed gap: the fix's freshness check only sees a file on disk, and the motivating session's own
ffmpeg command line (`-analyzeduration 200M -probesize 1G` plus a tonemap filter chain on a 4K HDR
source) makes it plausible the first replica hadn't written its first segment yet 10.6s in — so
this exact collision may not be caught by the fix as shipped (inferred from the measured command
line and gap, not from an independently timed encode). Prod's two backend types currently write to
non-overlapping directory roots, so this fix is dormant there today; it's live for the lab pool and
for any future shared-directory topology.

**04 — `segment-wait-request-aborted` (fixed, medium, MIXED).** `GetDynamicSegment`'s lock wait and
`GetSegmentResult`'s wait loop ran on a free-standing token never linked to `HttpContext.RequestAborted`,
so an abandoned request kept polling every 100ms holding the per-playlist lock for the rest of the
transcode's runtime, stalling the same client's own retry behind it. Fix: both waits now run on a
token linked to `RequestAborted`; the abort exit path also explicitly decrements the active-request
count that a thrown exception would otherwise skip. A fixup round added a real regression test for
the decrement branch (reverted the fix in a scratch copy and confirmed the new test fails) after the
first review found only code-reading coverage for it.

**Found, not fixed here (see also "Found, not fixed" below):**

- **Cross-node ping affinity.** Progress pings are per-node (`TranscodeManager.PingTranscodingJob`).
  In the lab's dual-encoder incident, a healthy job's *owner* replica can have its kill timer fire
  because pings were landing on the *other* replica while the client kept watching through it — a
  real "killed while actively watched, in an HA pair" mechanism, but a ping-routing/affinity
  problem, not a `DynamicHlsController` bug. Not fixed; flagged for whoever picks up shared-dir
  ping routing next.
- **Unexplained in-process restart.** During the same lab incident, one replica logged a second
  full application-boot sequence back-to-back with its first, and its container's memory
  working-set collapsed by roughly 250MB in the same window — an in-process, Kubernetes-invisible
  restart (the container's own restart counter never incremented, no shutdown/fatal log line
  precedes it). What triggered it is not established from available read-only telemetry. Traced
  through the controller's own branching, the burst of "cannot serve" warnings this produced
  never started a competing ffmpeg — the segments already existed on shared disk and were served
  200 by the pre-existing fast path; only the log line was wrong (a separate, already-known,
  already-fixed "logs 'doesn't exist' without rechecking" issue). Not a stall, and not the same
  event as the dual-encoder collision above — flagged as a follow-up, not fixed.

## Known issue #3 — stale web client undetected

**Reported:** a stale cached web bundle (older Jellyfin Web against a newer server) goes
undetected.

**Service-worker theory disproved (MEASURED):** the shipped `serviceworker.js` has only
`notificationclick`/`activate` handlers — no `fetch` handler, no Cache Storage use — so it cannot
serve a stale bundle by intercepting requests. That specific theory doesn't hold.

**What's actually missing, and what's still unknown:** the server already parses `Client`/`Version`
from `X-Emby-Authorization` on every request (`AuthorizationContext`) but never compares it to its
own version, and there was no `Client`/`Version` field anywhere in the log pipeline to catch this
operationally. The mechanism by which a stale bundle actually reaches a browser against a newer
server was not established here — this stays INHERITED/undetermined; the only thing ruled out is
the service-worker theory above.

**Fix (`12-stale-web-client.patch`, low severity, MIXED):**
- `AuthorizationContext` logs a throttled Warning when a client reports `Client="Jellyfin Web"` with
  a version older than the server's, piggybacking on the existing ~3-minute activity throttle (no
  new state on this hot, per-request path).
- **The 4-part vs 3-part version trap:** the server's own `ApplicationVersion` is a 4-part assembly
  version (e.g. `12.1.0.0`); Jellyfin Web reports 3-part versions (e.g. `12.1.0`). Comparing them
  directly makes every up-to-date client look "older" (revision `-1 < 0`). Both sides are normalized
  to 3 parts before comparing; a regression test pins this (fails against the naive comparison,
  passes with normalization).
- `serviceworker.js` gets `Cache-Control: no-cache`, matching `index.html`'s existing header (added
  in its own branch so `index.html`'s header path is untouched) — it's fetched directly by the
  browser's own service-worker update check, bypassing the build-hash query string every other
  asset gets.

**Not implemented:** a websocket "reload" prompt — the per-request auth path has no session handle
and no state to make it fire once, so it would refire every ~3 minutes for as long as a client
stays stale; not clearly safe without more plumbing than this warning needed.

**Residual risk:** the log line's message argument is an internal database key, not the
client-reported identifier string, so searching logs by the real client identifier won't match it
(cosmetic). Two concurrent requests from the same device inside the same throttle window can both
log once (a pre-existing race shared with an adjacent code path, harmless).

## Known issue #4 — PGS/image subtitles force burn-in

**Reported:** PGS/image subtitles force a full video transcode (burn-in) even when the client could
render them or a text track exists.

**MEASURED, and this changes where the fix belongs:** Jellyfin Web 12.1 only declares an
External/Hls delivery profile for `pgssub` when the user turns on the "Render PGS/VobSub bitmap
subtitles" setting — off by default. The server already returns
`SubtitleDeliveryMethod.External` whenever a client declares that profile (existing behaviour,
unchanged). So the common PGS-forces-burn-in case for this library is the *client's own declared
capability* (opted out by default), not the server ignoring a capability it has.

**What was fixed instead (`06-pgs-default-same-language-text.patch`, medium — downgraded from an
initial "high" on review, since this is a quality/burn-in issue, not correctness/crash/security;
an existing "remember subtitle selection" short-circuit also narrows real-world exposure):**
`MediaStreamSelector.GetDefaultSubtitleStreamIndex` picked the default subtitle purely from
`IsExternal`/`IsDefault`/`IsForced`, with no term for format. On a BD remux where mkvmerge preserved
the disc's "default" flag on a full PGS track, and a later same-language text track (e.g. added via
Bazarr) isn't retroactively flagged default, the PGS track kept winning under
`SubtitlePlaybackMode.Default`/`Smart` — which then forces `StreamBuilder.GetSubtitleProfile` into
`SubtitleDeliveryMethod.Encode` for HLS clients that haven't declared a matching profile. Fix: a
post-selection tie-break — among candidates that already satisfy the mode's own eligibility rule
and share the winner's language/forced flag, swap in a same-language text alternative if the
current pick is image-based. This deliberately does not widen Default mode's own eligibility rule;
Smart mode's rule (language-only) is where the swap actually fires for the reported case.
Different-language, differently-forced, and single-candidate cases are unaffected byte-for-byte.

Also fixed in the same patch: a dead fallback in `MediaStreamSelector`'s Always-mode branch
(`SetSubtitleStreamScores`) — `.ToList() ?? BehaviorOnlyForced(...)` can never take the right side
since `ToList()` never returns null, so a forced-only preferred-language stream was left with an
unset score when no non-forced candidate existed.

**Tests:** 9 new cases covering the positive same-language swap (both modes), the mandatory
negative (a different-language text track must never outrank the PGS default), a
wildcard-preferred-language negative, a differently-forced negative, single-candidate stability,
and the Always-mode score fallback.

**Residual/disclosed, not defects:** a non-default same-language subtitle doesn't swap in under
Default mode's stricter eligibility rule; Smart mode's audio-language-preferred branch gets no
tie-break; an external subtitle file already outranks an embedded image default today via an
unrelated, pre-existing sort term.

## Known issue #5 — DV profile 7 not direct-playable

**Reported:** 124 of ~331 movies are DV profile 7 BD remuxes (dual layer, BL + EL + RPU). The
Android TV client on the Bravia can't direct-play them; today Jellyfin either strips DV to the
HDR10 base layer or transcodes.

**What 12.1 does (MEASURED against source + a prod ffmpeg command line):** `MediaStream`
classifies profile 7 as `VideoRangeType.DOVIWithEL`. `EncodingHelper.ShouldRemoveDynamicHdrMetadata`
then has two outcomes only: copy the untouched dual-layer stream (client declares `DOVIWithEL`) or
strip to HDR10 (`hevc_metadata=remove_dovi=1` / `dovi_rpu=strip=1`). There is no path for a client
that supports single-layer DV (`DOVI` / `DOVIWithHDR10`) but not the EL. The shipped
jellyfin-ffmpeg cannot convert either: its `dovi_rpu` bitstream filter exposes only `strip` and
`compression` (MEASURED, `ffmpeg -h bsf=dovi_rpu` in the jm4 image).

**Split of the fix (decided by the project owner):** Jellyfin decides, the JellyMesh transcode
pool converts (Rust, `dolby_vision` crate, between demux and mux: drop the EL, rewrite the RPU to
profile 8.1). This patch is the decision half only; the pool half is separate work.

**Fix (`13-dv7-to-81-decision.patch`, opt-in `JELLYMESH_DOVI_P7_TO_81=1`, default off =
byte-identical to the baseline):**

- `EncodingHelper`: a third plan, `ConvertDoviP7ToP81`, taken only when the flag is set AND the job is
  HLS AND the source is `DOVIWithEL` AND the client's requested range types include `DOVI` or
  `DOVIWithHDR10` but not `DOVIWithEL`. Video is stream-copied with no strip filter, tag `dvh1`, and the
  pool marker (below). `DOVIWithELHDR10Plus` sources are never converted.
- `DynamicHlsHelper`: the master playlist advertises the output as DV 8.1
  (`SUPPLEMENTAL-CODECS="dvh1.08.<level>/db1p"`), not as a stripped stream.
- `StreamBuilder`: for HLS transcoding-profile candidates only, a profile-7 source is checked
  against the client's range condition as `DOVIWithHDR10`, per condition, and only where that
  condition does not already declare `DOVIWithEL` (mirrors the `EncodingHelper` gate, so the decision
  and the execution never disagree). HTTP/progressive candidates and direct play see the real
  `DOVIWithEL` value, as before.
- Also folds in `dovi-el-strip-misses-doviwithhdr10-fallback` for the flag-on case (see "Found, not fixed").

**Pool marker (contract for the pool session):**

1. Literal argv token pair `-metadata:s:v:0` `JELLYMESH_DOVI_P7_TO_81=1`, emitted by
   `DynamicHlsController.GetVideoArguments` right after the `-tag:v:0 <tag>` group (and `-strict -2`
   when the client declares literal `DOVI`), before `-bsf:v hevc_mp4toannexb`. Match the pair, not the
   argv position. Meaning: convert DV profile 7 to 8.1 in-stream on output video stream 0. The
   `hevc_mp4toannexb` filter is still present and must still be applied.
2. Exactly one `-map 0:<N>` for video (`N` = the source video stream index); no extra map for the EL.
   If the RPU rewrite needs the EL, read it from the original `-i` input. The layout of a real P7
   remux (which stream index carries what) is INHERITED — check it with `ffprobe` on a real file
   before building EL extraction around an assumed index.
3. Harmless to stock ffmpeg (MEASURED, jellyfin-ffmpeg 8.1.2 via podman): accepted with exit 0 for
   fMP4, MPEG-TS and MP4 muxers; the media payload is byte-identical (`cmp`) with and without the
   marker; only the stream metadata tag differs. An invalid-bsf-option alternative was rejected
   (exit 8).
4. **Failure mode if the flag is set without the pool in the path:** stock ffmpeg copies the raw
   profile-7 bitstream while the playlist advertises profile 8.1 — worse than today's strip. Only set
   the flag where the pool is confirmed in the transcode path.

**Before / after (flag on, HLS, Jellyfin Web-shaped client range list; MEASURED from the unit-test
command lines):**

```
before: -codec:v:0 copy -tag:v:0 hvc1 -bsf:v hevc_mp4toannexb,hevc_metadata=remove_dovi=1 ...
after:  -codec:v:0 copy -tag:v:0 dvh1 -strict -2 -metadata:s:v:0 JELLYMESH_DOVI_P7_TO_81=1 -bsf:v hevc_mp4toannexb ...
```

**Tests:** `EncodingHelperDoviTests` (Controller.Tests: marker present and strip absent only for
the gated combination; unchanged with the flag off, for HDR10-only clients, for EL-declaring
clients, for `DOVIWithELHDR10Plus`, and for progressive jobs) and `StreamBuilderDoviP7ToP81Tests`
(Model.Tests: DOVI-capable vs HDR10-only profile, flag on/off, an HTTP candidate never wins on the
strength of the substitution, and an EL-declaring condition still passes as in the baseline). Three
review rounds: round 1 found HTTP candidates could win the ranking via the substitution; round 2
found an EL-declaring condition regressed with the flag on; each was fixed with a test that
fails before the fix (MEASURED) and passes after. Round 3 approved.

**Not verified here (INHERITED):** that the Bravia/Android TV client plays the converted 8.1
stream, and that FEL titles convert acceptably (the conversion drops the EL; MEL titles lose
nothing, FEL titles lose the EL's extra detail). Both need the pool half and a real device. Known,
non-regressing gaps from review: a device condition that lists only literal `DOVI` (not
`DOVIWithHDR10`) does not win the ranking via the conversion (falls through exactly as with the flag
off); the flag is an environment variable, so a future second profile-7 test fixture in the same
test assembly would need a non-parallel collection.

## Known issue #6 — N+1 / slow queries beyond `jellyfin-perf`

**08 — `mediasources-batch` (fixed, high, MIXED).** `DtoService.GetBaseItemDtos` already batches
people/user-data/lyrics for a page, but every item with `Fields=MediaSources` or `MediaStreams`
still called `BaseItem.GetVersionInfo` per version, three unbatched statements each (media streams,
media attachments, has-segments). A 200-item grid did roughly 600 extra statements for a field two
real clients request routinely (a library-sync client, and the web client's home rows/item
detail). MEASURED (new SQLite + statement-counting tests): a page of N Video items with
`Fields=MediaSources` drops from 3N statements to 3 total; JSON output identical to the unbatched
path for the same data, including an alternate version not on the page (falls back to its own 3
statements, batch untouched for the rest). Caveat from review: this figure assumes alternate-version
enumeration is empty in the test; with real alternate-version data the true cost is 3 (batched)
plus per-item enumeration, which stays out of scope here. A live-lab statement-count/parity run
(mirroring `jellyfin-perf`'s own methodology) is a follow-up, not yet done.

Collateral: the test project didn't compile on this branch before this patch (the perf patch added
a constructor parameter to `DtoService` that two existing test files never picked up); fixed both
call sites so tests can run at all. A separate flake this introduced — the new test class leaked
`BaseItem` process-wide statics without restoring them, intermittently breaking two unrelated,
pre-existing test classes depending on run order — was root-caused (MEASURED, deterministic 2-class
repro) and fixed by saving/restoring the statics in `Dispose`, matching the pattern every other
statics-mutating test class in the project already uses. 4/4 and 5/5 clean full-suite runs after.

**07 — `progress-write-stall` (fixed, medium, MIXED).** `POST /Sessions/Playing/Progress` could
block for as long as the database stalled, with no bound: the retry-on-conflict predicate matched
any `DbException`, and the write itself was a synchronous exists-check + add + `SaveChanges` +
commit. The investigate phase cited a specific slow-write incident for this; that incident predates
this fork's Galera cutover and ran on a different server image, so attribution to this exact code
path is not confirmed — the code-level defect (unbounded retry, over-eager conflict matching) is
real independent of that one incident. Fix: narrow the conflict predicate to real conflicts only
(duplicate key, deadlock/certification failure, SQLite constraint violation) — walking the full
exception chain, since a save failure can be wrapped two layers deep and a fixed-depth unwrap can
miss it — before adding a bounded, fail-fast async path for `PlaybackProgress` writes specifically.
A progress write superseded by a newer one for the same user+item drops out of its own retry loop
early. Start/Finished/manual edits, and the one progress tick that flips `Played`, keep the
original unbounded, always-retried path unchanged — never dropped. This round's fixup fixed a bug
in the conflict-classification logic itself (a specific exception-wrapping shape was wrongly
treated as non-retryable for every save reason, not just Progress) — a latent defect, since no
MySQL/Galera provider exists in this tree to have hit it yet.

**Evidence limit (both items):** there is no `mysqld`/`wsrep` metrics exporter in this deployment,
so a real Galera write stall can't be independently measured end-to-end here — the fixes are
correctness/design fixes verified at the unit level, not a live before/after against a stalled
cluster.

## Other fixes

**09 — `stopped-no-position`, medium, MEASURED.** `SessionManager`'s private Stop handler treated
any Stop report with no `PositionTicks` as "watched to completion" unconditionally — setting
`Played=true`, position 0, and double-incrementing play count (Start already increments it once).
In an HA deployment this fires even when this node never saw the matching Start/Progress (e.g. a
failover), silently marking a barely-watched item played and wiping a real resume position. Fix: a
three-way rule — use the client's position if given; otherwise fall back to this node's own
last-checked-in position only if its session actually has a record for this exact item; otherwise
leave `UserData` untouched rather than guess. The historical assume-played behaviour is kept only
for the one case where this node recorded the Start for this item but never got any
position at all (the DLNA "transport state only" case), without the double-increment. A fixup round
gated the fallback on an explicit item match (the first pass read it unconditionally, which could
apply a stale position left over from a different item on the same session) and added a regression
test for that exact stale-fallback scenario.

**10 — `displayprefs-retry`, medium, MIXED.** `DisplayPreferencesManager.GetDisplayPreferences` does
a check-then-insert against a unique index on (user, item, client). On the shared database, two
concurrent first requests for the same key (e.g. two browser tabs on a first-ever login) can both
pass the check and race on the insert, throwing an unhandled exception → HTTP 500. Fix: the same
bounded (6-attempt), jittered-backoff retry pattern `UserDataManager.SaveUserData` already uses,
scoped to `GetDisplayPreferences` only — the sibling `GetItemDisplayPreferences` has no unique index
on that key (a non-unique index only), so a retry there would be a no-op against a different,
out-of-scope bug (silent duplicate rows), per review. MEASURED: reverting the fix and re-running the
new test reproduces the real unique-constraint exception, confirming the test would have caught the
original bug. A read-only prod log sample in this window showed no observed 500s yet — a proactive
fix for a real, race-condition bug, not one caught live.

**11 — `trickplay-ctx-dispose`, medium, MEASURED.** `TrickplayManager.DeleteTrickplayDataAsync` was
the only method in the class that created a `DbContext` without disposing it — every other
DB-using method in the file wraps its context in `await using`. Fix: same pattern, so the context
and its pooled connection are released promptly instead of only via GC finalization. New test uses a
`DbContext` subclass that records disposal, and fails against the pre-fix code.

**12 — `stale-web-client`** — see Known issue #3 above.

## Found, not fixed

Sixteen investigate-phase findings and three later completeness-critic findings did not result in a
patch. Reasons vary: refuted by measurement, real but not worth the risk/complexity, matches an
intentional upstream decision, or needs an infrastructure fact this session couldn't establish
read-only. The three critic findings (`c0`–`c2`) are a single-pass read, not run through the
two-lens adversarial verify the other 28 findings went through — treat them as identified, not yet
independently verified.

| Finding | Verdict / why not fixed |
|---|---|
| `serviceworker-missing-no-cache` | The specific header gap this named is real but was folded into `12-stale-web-client.patch` (serviceworker.js now gets `Cache-Control: no-cache`) rather than shipped as its own item. |
| `hls-segment-gap-index-not-time` | Real: `DynamicHlsController.cs:1471-1494`'s 24-segment restart threshold is an index count against variable-length keyframe-bounded segments (`DynamicHlsPlaylistGenerator.ComputeSegments`, ~152-181), so it silently drifts. Simulated drift for this library's typical (~1s GOP) profile is only ~0.1-2.8%; only long-GOP (10s+) sources drift badly (73-233%), which isn't this library's profile. The proposed fix also cited the wrong field (`DownloadPositionTicks`, a client-consumption proxy, instead of `TranscodingPositionTicks`, real ffmpeg progress) — using the wrong one risks *more* spurious restarts. Deferred pending a corrected fix and a log-level bump (the relevant lines are Debug-only in prod). |
| `audio-default-track-narrowing` | Real (`StreamBuilder.cs:672-707` only considers `IsDefault` audio candidates), but matches an intentional upstream jellyfin/jellyfin change (PR #13832, merged 2025-04-09) that deliberately reversed this exact behaviour after real user reports of the server silently switching away from a file's intended track. `MediaStream` also has no commentary/descriptive flag today, so a naive fallback risks picking a commentary track over the main mix. Not fixed; would need an upstream proposal, not a silent fork patch. |
| `enable-subtitles-in-manifest-default-flip` | Real: `EnableSubtitlesInManifest=false` never survives the PlaybackInfo→HLS manifest round trip (`StreamInfo.ToUrl()` omits `false`; `DynamicHlsController.cs:273` resolves the omitted key as `?? true`), silently re-enabling it. Only affects text-subtitle items (this library is PGS-heavy, so low impact here). The finder's proposed fix would break an existing 100,000-iteration fuzz parity test; the correct minimal fix (flip the default at line 273 to `?? false`) was identified but not shipped this round. |
| `trickplay-ha-split-brain` | Real structural gap: trickplay DB rows are shared (Galera) but the tile files are not — `SaveTrickplayWithMedia=false` (the default) puts tiles on a per-node hostPath volume, not the shared media mount. MEASURED against a month of logs: zero regen/orphan-prune hits, and the nightly task consistently finishes in 1-23s (too fast to be re-encoding) — the thrash this predicts isn't currently happening. The dominant fix is a deployment config change (`SaveTrickplayWithMedia=true`, onto the already-shared media mount), not a code patch, and was out of scope for this patch series. |
| `quickconnect-inmemory-ha` | Real as a code fact (pairing state is per-process, `QuickConnectManager.cs:33-34`) but the deployment framing was wrong — this is an active/standby failover pair, not a round-robin load balancer, so ordinary pairing traffic stays on one node. No measured instance of the claimed cross-node failure exists in logs; the only real QuickConnect errors found predate this deployment's HA cutover entirely. The proposed fix (a new schema/migration against the shared database) is disproportionate risk for an unconfirmed failure mode. |
| `websocket-403-vs-401` | Real: a websocket auth failure throws uncaught and maps to 403 instead of 401 (unlike the identical REST-path failure, which is caught and correctly mapped). Single call site, safe fix — but browsers' native WebSocket API doesn't expose the handshake status to JS, so the benefit for the web client is log hygiene only, not client-visible re-login behaviour. Deferred as low-impact. |
| `subtitle-score-always-mode-dead-fallback` | This is the same dead fallback described under Known issue #4 (`MediaStreamSelector.cs:135-138`) — it **was** fixed, folded into `06-pgs-default-same-language-text.patch`, not left open. |
| `nextup-shared-db-single-reads` | Real: `TVSeriesManager.cs:220,232` issue one `UserData` query per episode/version under shared-DB mode instead of batching like the sibling call at line 266 already does. Narrow scope (only series with specials-in-season display or alternate-version episodes), trivial and safe fix — just not selected for this round. |
| `hls-stop-encoding-fire-and-forget` | Refuted on closer trace: `StopEncodingProcess` doesn't await `KillTranscodingJobs`, but `job.Stop()` (stdin write, wait, kill-if-needed) runs fully synchronously before any `await` returns — the process is already dead before the 204 response is built. Only the deferred file-cleanup step is unawaited, and it's already inside existing try/catch. The proposed fix (await the whole call) would add a mandatory 1.5s+ latency to every stop/quality-switch request for no safety gain. |
| `trickplay-tonemap-ignores-admin-config` | Real: the software (no-hw-accel) trickplay path always uses hardcoded tonemap defaults (`MediaEncoder.cs:850`, `new EncodingOptions()`), ignoring the admin's configured values, because `IsSwTonemapAvailable` (`EncodingHelper.cs:346`) has no `EnableTonemapping` gate. Low practical value here: prod trickplay runs hardware-accelerated (Intel/NVIDIA), and that hardware path's tonemap filter ignores these fields entirely regardless — the bug's only visible effect is a baked-in tonemap curve on HDR/DV trickplay thumbnails. The finder's proposed fix also had a compile error (treats a mutable class as a record). Not prioritized. |
| `dovi-el-strip-misses-doviwithhdr10-fallback` | Real, narrow gap: the profile-7 enhancement-layer strip (`EncodingHelper.cs:1445`) only matches clients declaring literal `HDR10`, not `DOVIWithHDR10`. MEASURED against prod logs: every real occurrence of `DOVIWithHDR10` co-occurs with `HDR10` in the same request, and no client in this fleet (Jellyfin Web, Android TV client, Kodi) currently sends the unhandled combination — a correct, safe one-line fix exists but would be dead code for this fleet today. Folded into `13-dv7-to-81-decision.patch` only for when that feature is enabled; no unconditional widening of the strip trigger. |
| `hls-segment-no-local-job-no-restart` | Refuted, and superseded. The theoretical null-job race requires an external kill landing in a multi-microsecond window with no `await` between two reads — it cannot mechanically produce the cited 13-line "cannot serve" burst. Re-running that exact log query shows one pod, one line per monotonically increasing segment index ~3s apart — ordinary sequential playback, not a stall. The real underlying HA transcode-ownership case this was reaching for is exactly what `03-k2-incident-and-shared-dir.patch` (Known issue #2) already fixes. |
| `nfo-saver-readonly-mount-churn` | Refuted as framed: the finding claimed ~55 ERR/week "on every scan, forever"; the actual log query shows 12 hits, and two existing gates (only saves on real metadata changes; byte-compares before writing) mean it isn't "every item every scan." The real remedy is an operator config toggle (disable the NFO saver for that library, or fix the mount), not a code change — the code already skips the write entirely when that setting says not to save NFO. |
| `library-monitor-self-write-suppression-not-cluster-aware` | Not fixed — the cross-node trigger this assumes (one node's write appearing as an external filesystem event to another node's `inotify` watcher) is unlikely over typical shared-storage mounts, which often don't propagate another client's writes as local watch events at all. |
| `cross-node-stale-now-playing` | Refuted: tracing the full websocket-close chain (not just the listener in isolation) shows the entire session, including "now playing," is torn down at the same ~60s timeout the finding worried about lingering past — it doesn't linger. The proposed fix would also be unsafe for multi-connection clients (multiple tabs/reconnects), since it has no way to check whether another live socket for the same session still needs that state. |

**Critic findings (single-pass, not adversarially verified):**

- **`c0` — scheduled tasks have no cluster leader (in stock Jellyfin).** `TaskManager`/`ScheduledTaskWorker`
  locking and queueing is purely local (`TaskManager.cs:21-22, 157-173`). True for stock Jellyfin, but
  already covered in JellyMesh by the Leader plugin, not by a patch: `leader/LeaseLeaderService.cs:233-267`
  (this repo) subscribes to `ITaskManager` task execution, and on a follower cancels the task and
  forwards it to the Lease holder, which runs it (MEASURED by reading the plugin source; the
  plugin is loaded in the jm4 image per the owning session's notes, INHERITED). No action here.
- **`c1` — password-reset PIN file not shared.** The forgot-password PIN is written to a local JSON
  file under the server's data path, not the shared database. On an HA pair, the "redeem PIN"
  request has no session affinity and can land on the node whose local file never had it. Code read
  is direct (no DB access anywhere in that class); the cross-node topology claim rests on the
  project's own deployment notes rather than independent verification against the live cluster.
- **`c2` — `MaxActiveSessions` enforced per node, not per cluster.** The session cap is checked
  against a per-process in-memory dictionary, so a user could exceed the configured cap by splitting
  sessions across both HA replicas. Pure source read (no external dependency needed to confirm).

## Operational notes (not code)

- **Traefik access logs record the `api_key` query parameter in cleartext** in the request-path
  field, for two lab services that authenticate via query string rather than a header. Worth moving
  those to header-based auth or scrubbing that field from the log pipeline; not something a Jellyfin
  patch can fix.
- **No `mysqld`/`wsrep` metrics exporter exists** in this deployment, so Galera-side write stalls
  (contention, certification aborts, flow control) can't be measured end-to-end from this side —
  see Known issue #6's evidence-limit note.
- **Ping-affinity follow-up:** progress pings are per-node; in a shared-transcode-dir HA topology
  this can let a job's owner-node kill timer fire while the client is actually still being served by
  the other replica. Not fixed here (see Known issue #2) — needs either session-aware ping routing
  or moving kill-timer ownership off "which node last saw a ping."
- **Test flake, unconfirmed:** `Jellyfin.Controller.Tests`'s
  `PlaylistTests.IsVisible_PlaylistWithOneAllowedItem_StaysVisible` failed in full-suite runs on the
  patch-13 branch alone (which lacks `00-tests-fixup`), passes standalone, and did not reproduce in 5
  full runs on the integrated tree. Not touched by any patch here; watch for it.

## Method

Thirteen independent finder passes covered the six known issues plus a broad hunt across HLS/segment
handling, kill timers, device profiles, playback decisions, library scan, trickplay, session/progress
reporting, and auth, producing 28 distinct findings after dedup. Each finding went through two
adversarial verdicts — a "code" lens (is the claim technically real, independently traced/measured)
and a "fix" lens (is fixing it worth the risk, and is the proposed fix itself correct) — before a
completeness-critic pass added three more (`c0`-`c2`, not yet run through the same two-lens process).
Twelve findings survived both lenses and were implemented; each fix was built on its own branch, with
build and full test suite independently re-run by a second reviewer who did not simply trust the
first pass's numbers — four items needed a fixup round before that reviewer approved, and patch 13
needed two. Patches were then merged in order onto one branch, rebuilt and re-tested together;
integration found and fixed one cross-patch gap (03's shared-dir wait now uses 04's
request-abort-linked token) and one test-isolation leak (in 08's new test class). Every claim in
this document is labelled MEASURED, INHERITED, or MIXED per the definitions above.
