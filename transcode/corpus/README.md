# ffmpeg command corpus (P0, 2026-09-26)

Golden inputs for the `ir` crate: every command line here must parse. Until P1 lands, renders are compared with the spike's Python translation, which serves as the parity oracle.

| File | Source | Commands | Shape |
|---|---|---|---|
| `prod-qsv.tsv` | prod `media/jellyfin-qsv` `/config/log/FFmpeg.*.log` | 67 (45 Transcode, 20 DirectStream, 2 Remux) | Jellyfin with hwaccel=qsv: its own VA-API/QSV chains. Reference for the renderers |
| `lab-sw.tsv` | `tc-lab/tc-jellyfin` (hwaccel=none), mostly `tclab.py corpus` | 67 (64 Transcode, 3 DirectStream) | what the pool shim actually receives: libx264/libx265, tonemapx, text and image subtitle burn-in, copy, fMP4, AC3 surround, alternate audio, bitrate ladder 1–20 Mbps |
| `lab-trickplay.tsv` | synthetic (P2, 2026-09-27; not scraped from `FFmpeg.*.log` -- `MediaEncoder` spawns trickplay's ffmpeg directly, bypassing Jellyfin's transcode logger) | 5 (all `synthetic`) | `mjpeg`/`%0Nd.jpg` sprite extraction: SDR and 4K HDR sources, keyframe-only (`-skip_frame nokey`) variants, both `-fps_mode passthrough` (jellyfin-ffmpeg >= 5.1, majority) and `-vsync 0` (older) forms |

Format: `<log kind>\t<full command line>`. Paths reference the lab titles and prod media.

Shapes seen (MEASURED from these files):
- `-filter_complex` overlay for image (PGS) subtitle burn-in, and `subtitles=` for text burn-in;
- `crop` and `pad`;
- `-readrate` and `-readrate_catchup` on copy/remux;
- fMP4 HLS (`-hls_segment_options`, `-hls_fmp4_init_filename`, `-tag hvc1`, `-bsf`);
- `-x264opts` / `-x265-params`, `-crf` with `-maxrate`/`-bufsize`, `-force_key_frames`, `-sc_threshold`.

Known gaps:
- **Seek variant:** `StartTimeTicks` on master.m3u8 returns HTTP 400. Failover restarts already produce `-ss` commands.
- **Shared-scratch race:** one stream-copy session of Sample B hit it intermittently. Jellyfin served a segment while the file was still growing (`Content-Length mismatch 8404992 of 8388608`); it did not reproduce in 6 more sessions. The native agent writes segments via a temp name and an atomic rename (P1).
