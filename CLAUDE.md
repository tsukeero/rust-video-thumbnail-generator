# CLAUDE.md

Implementation notes for working on this codebase. User-facing docs (API, CLI options, examples) live in `README.md`; keep the two consistent when changing behavior.

## Overview

`thumbs-collage` is an HTTP service that extracts frames from a video at given timestamps and assembles them into a JPEG collage (grid or custom layout). Everything is in `src/main.rs`.

- **Stack**: Axum 0.6 (HTTP + WebSocket), Tokio, `ffmpeg-next` 8.x, `image`/`imageproc`, `rusttype` for timestamp text, serde.
- **FFmpeg**: must be 8.x. `ffmpeg-next` 8 does not build against FFmpeg 9 (Homebrew's default `ffmpeg` and BtbN's `master` builds are 9.x; CI pins 8.1).
- **License**: GPL-3.0-or-later. Windows releases bundle GPL FFmpeg DLLs.

## Build

```bash
cargo build --release                  # Linux/macOS: needs FFmpeg 8 dev libs, clang, pkg-config
set FFMPEG_DIR=C:\path\to\ffmpeg-8.x   # Windows: build.rs copies %FFMPEG_DIR%\bin\*.dll next to the exe
```

There are no automated tests yet. Verify changes by running the server and POSTing to `/make_collage` against real files, including corrupt ones and AVI/RealVideo.

## Request Flow

1. `make_collage` (async handler) takes a semaphore permit with `try_acquire_owned`; a full semaphore returns 429 immediately, no queueing.
2. `process_video` runs in `spawn_blocking`: `resolve_paths` → `probe_duration` / `probe_dimensions` → compute timestamps → layout → extraction tiers → draw frames and timestamps on the canvas → encode JPEG → `insert_jpeg_com`.
3. Metadata is returned twice: in a JPEG COM marker and base64 in the `X-Collage-Meta-B64` header. It includes the full request config (`selection`, `collage`, ...), so those types derive both `Serialize` and `Deserialize`.

## Extraction Tiers

All extractors return `Vec<Option<(DynamicImage, f64)>>`; the `f64` is the **actual** frame timestamp, which goes into the metadata as `timestamp_actual` and is what gets rendered.

### Tier 1: `extract_frames_seek_each`
One input context; for each timestamp `av_seek_frame(..., AVSEEK_FLAG_BACKWARD)` (raw FFI, the safe wrapper was unreliable), `decoder.flush()`, then `extract_frame_after_seek`.

`extract_frame_after_seek` details that matter:
- **Keyframe sync**: if the first video packet after a seek isn't a keyframe (e.g. AVI whose index flags every entry as a keyframe), packets are dropped undecoded until a keyframe packet. MPEG-4 Part 2 decodes such P-frames against a gray reference *without* setting `decode_error_flags`, so this is the only signal. If no keyframe shows up within `KEYFRAME_SEARCH_WINDOW_SECS` (15s) it returns `NoKeyframeFound`, and the caller disables the requirement for the rest of the job (streams that don't flag keyframes, e.g. intra-refresh H.264).
- **Timestamps**: uses `frame.timestamp()` (best-effort, falls back to packet DTS), not `pts`, which is missing in AVI.
- **Corruption**: any frame with `decode_error_flags != 0` in the GOP marks the result as errored; P/B frames that depend on a corrupt keyframe look clean but aren't. The first errored (or failed) timestamp aborts Tier 1 and it returns `None` for **every** frame, so in practice any corruption forces the fallback below.
- **Deferred scaling**: keeps the best raw frame and only scales the final pick (`decode_and_scale_frame`).
- **Blank retry** (when `skip_blank_frames`): `is_blank_image` → seek +2.5s, up to 10 retries.

### Tier 2/3: `extract_frames_with_genpts` → `extract_frames_sequential`
Triggered in `process_video` when more than a third of Tier 1 frames are `None` (and `--allow-fallback` ≥ 1).
- `extract_frames_sequential` opens with `AVFMT_FLAG_GENPTS`, sorts timestamps, and decodes in one pass. FFmpeg logging is set to `Fatal` here to suppress corruption spam.
- **Tier 2** calls it with `keyframes_only = true` (`AVDISCARD_NONKEY`), accepting only clean keyframes.
- **Tier 3** (only if Tier 2 captured zero frames and `--allow-fallback 2`) decodes every frame. Accepts frames from 10s before to 45s after the target. Needed for RealVideo, where skipping non-keyframes breaks references.
- Blank frames are checked with `is_blank_frame` (FFmpeg frame, stride-aware).

### Failure thresholds (`process_video`)
| `--allow-fallback` | Fails when |
|---|---|
| 0 | >33% of frames fail in seek mode |
| 1 | any frame still missing after keyframe mode |
| 2 | >20% still missing after sequential mode |

`force_fallback` in the request skips the automatic logic and runs the chosen tier directly.

## Blank Frame Detection

`is_blank_image` (packed RGB) and `is_blank_frame` (FFmpeg frame, `offset = y * stride + x * 3`) sample every 8th pixel's luminance and call `is_blank_by_variance`. Blank if variance < 500, or mean > 230 with variance < 3000 (white), or mean < 25 with variance < 3000 (black). Thresholds are hard-coded.

## Layout and Fitting

- `derive_output_size_grid` / `derive_output_size_adv`: fill in a missing width/height from the video aspect ratio; default 1280x720.
- `compute_grid_layout`: leftover pixels go to the last row/column so there are no gaps.
- `compute_advanced_layout`: percentage-based pieces, may overlap.
- `fit_frame_to_rect`: returns source crop + destination rect for `Cover` / `Contain` / `Stretch`.
- Timestamp labels: drawn bottom-right with a 1px black outline, `format_timecode` picks `MM:SS` or `HH:MM:SS` by video duration. Only drawn if `--font-file` was given.

## Progress (WebSocket)

`ProgressTracker` holds a `broadcast::Sender<ProgressUpdate>` (capacity 1000) and throttles `update` to `--progress-interval-ms`; `force_update` bypasses the throttle (used on tier transitions). `/ws-progress` is only routed with `--ws-progress`; each socket subscribes and receives updates for all jobs.

## Other Behavior

- **Default bind** is `127.0.0.1:0` (random port); the server prints `Listening on http://<addr>` so a parent process can parse the port.
- **`--exit-on-stdin-close`**: `spawn_stdin_close_watcher` exits the process when stdin hits EOF/errors, so it dies with a parent that spawned it with a piped stdin.
- **Paths**: `resolve_paths` simply joins `--root-path` with the request path. This is not a sandbox (absolute paths and `..` escape it, and FFmpeg accepts URLs). The service assumes trusted callers.
- **`metadata_text`** is accepted in the request but unused.
- **FFmpeg 8 API**: keyframe detection uses `flags & AV_FRAME_FLAG_KEY`, not the removed `key_frame` field.

## Releases

`.github/workflows/release.yml` is manual (`workflow_dispatch` with a version). It bumps `Cargo.toml`, tags, creates the GitHub release, and builds Windows x86_64 (BtbN FFmpeg 8.1 GPL shared, DLLs bundled), Linux x86_64 (distro FFmpeg dev packages), and macOS aarch64 (Homebrew `ffmpeg@8`, keg-only, via `PKG_CONFIG_PATH`).
