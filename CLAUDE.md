# CLAUDE.md

Implementation notes for working on this codebase. User-facing docs (API, CLI options, examples) live in `README.md`; keep the two consistent when changing behavior.

## Overview

`thumbs-collage` is an HTTP service that extracts frames from a video at given timestamps and assembles them into a JPEG collage (grid or custom layout, `POST /make_collage`), returns them as separate images (`POST /frames`), or detects scene cuts (`POST /scenes`). Collage and frames code is in `src/main.rs`; scene detection is in `src/scenes.rs` (detectors, hints, adaptive layer, flash filter, the job) and `src/scene_frames.rs` (frame placement, candidates, trimming).

- **Stack**: Axum 0.6 (HTTP + WebSocket), Tokio, `ffmpeg-next` 8.x, `image`/`imageproc`, `rusttype` for timestamp text, serde.
- **FFmpeg**: must be 8.x. `ffmpeg-next` 8 does not build against FFmpeg 9 (Homebrew's default `ffmpeg` and BtbN's `master` builds are 9.x; CI pins 8.1).
- **License**: GPL-3.0-or-later. Windows releases bundle GPL FFmpeg DLLs.

## Build

```bash
cargo build --release                  # Linux/macOS: needs FFmpeg 8 dev libs, clang, pkg-config
set FFMPEG_DIR=C:\path\to\ffmpeg-8.x   # Windows: build.rs copies %FFMPEG_DIR%\bin\*.dll next to the exe
```

`cargo test` runs unit tests for scene detection (flash filter and adaptive reference values from the labelled-set replay, HSV against OpenCV, hint resolution, placement and trimming) and a synthetic-clip test that generates clips with known cuts through the FFmpeg CLI (`FFMPEG_DIR\bin` or PATH; skipped when there is none). Everything else is verified by running the server and POSTing to the endpoints against real files, including corrupt ones and AVI/RealVideo.

## Request Flow

1. Both handlers go through `run_job`: it takes a semaphore permit with `try_acquire_owned` (a full semaphore returns 429 immediately, no queueing), runs the job in `spawn_blocking`, and maps errors with `job_error_response` (a `BadRequest` error is 400, anything else 500, message printed with `{:#}` so context chains show).
2. Shared steps, used by both endpoints:
   - `prepare`: `resolve_paths` → `probe_duration` / `probe_dimensions` → timestamps from the selection, in request order.
   - `extract`: the extraction tiers and failure thresholds, for one `FrameTarget { w, h, fit }` per timestamp. Returns the frames plus `fallback_level`, the tier that produced them (0 seek, 1 keyframes, 2 sequential; `extract_frames_with_genpts` reports which of its passes succeeded).
3. `/make_collage` → `process_video`: layout → targets from the rects (a timestamp without a rect gets `thumb_w`/`thumb_h` `.max(160)`/`.max(90)`) → `extract` → draw frames and timestamps on the canvas → encode JPEG → `insert_jpeg_com`. Metadata is returned twice: in a JPEG COM marker and base64 in the `X-Collage-Meta-B64` header. It includes the full request config (`selection`, `collage`, ...), so those types derive both `Serialize` and `Deserialize`.
4. `/frames` → `validate_frames_request` (in the async handler, before a permit is taken) → `process_frames`: `frames_target` computes one target for all frames from `size` → `extract` → `finish_frame` (centre crop, only `cover` decodes larger than the output) → encode JPEG (with a per-frame COM marker) or PNG → base64 inline or written to `output.dir`. `WrittenFiles` deletes the job's files on drop unless the job completes.
   - `FramesRequest`, `SizeSpec` and `OutputSpec` use `deny_unknown_fields` (422 on typos); `JobRequest` stays lenient. `OutputSpec::Inline {}` is an empty struct variant because serde ignores unknown fields on unit variants of internally tagged enums.
   - `validate_output`, `encode_frame` and `deliver_frames` (base64 or files, `WrittenFiles` cleanup, COM marker added at delivery) are shared with `/scenes`.
5. `/scenes` → `scenes::validate_request` (before the permit) → `scenes::process_scenes`:
   - `resolve` merges the `hint`'s defaults under explicit fields and refuses fields the detector doesn't use (400). `adaptive` is `Option<Option<_>>` (absent / `null` / object) via the `present` deserializer.
   - `keyframes`: `detect_keyframes` reads packet flags only; more than 50% keys → `Unprocessable` (422). With `frames`, placement and trimming run first, then the shared `extract` (seek tiers) fetches the targets.
   - `scdet` / `content`: `detect_decoded` is one sequential GENPTS pass with frame-threaded decoding (drained with `send_eof`), each frame scaled to the analysis size (256 px wide, `analysis_size`): YUV420P bicubic for `scdet` (luma plane only, as FFmpeg's `scdet` does on planar YUV, so it matches `scale=256:-2,scdet`), for `content`, bilinear to YUV444P then converted to RGB24 at the small size (swscale's direct scale-to-RGB24 was the bottleneck), then OpenCV's fixed-point BGR2HSV in `HsvTables`. Errored frames get no score. Fails if fewer frames decode than half the video packets (not duration × frame rate: AVI's `avg_frame_rate`/`nb_frames` can be 5× off).
   - `CutPipeline` = cut test (threshold, or the adaptive ratio over a ±window buffer, judged `window` frames late; edge frames count as below) → `FlashFilter` (PySceneDetect's merge/suppress with the length in seconds; `off` = suppress). It reproduces the labelled-set replay (`evalx.py`) exactly.
   - Frames: `scene_frames::Collector` keeps candidates on an `every/4` grid while a scene is open (halving density beyond 8 × `max_per_scene`); when a new cut appears in `pipeline.cuts` it picks the nearest candidate per placed target and encodes it. Then `trim` (per-scene counts), `thin` (which picks to keep), and `deliver_frames` with 4-digit names.

## Extraction Tiers

All extractors take `targets: &[FrameTarget]` (one per timestamp, passed to `decode_and_scale_frame`) and return `Vec<Option<(DynamicImage, f64)>>`; the `f64` is the **actual** frame timestamp, which goes into the metadata as `timestamp_actual` and is what gets rendered.

### Tier 1: `extract_frames_seek_each`
One input context; for each timestamp `av_seek_frame(..., AVSEEK_FLAG_BACKWARD)` (raw FFI, the safe wrapper was unreliable), `decoder.flush()`, then `extract_frame_after_seek`.

`extract_frame_after_seek` details that matter:
- **Keyframe sync**: if the first video packet after a seek isn't a keyframe (e.g. AVI whose index flags every entry as a keyframe), packets are dropped undecoded until a keyframe packet. MPEG-4 Part 2 decodes such P-frames against a gray reference *without* setting `decode_error_flags`, so this is the only signal. If no keyframe shows up within `KEYFRAME_SEARCH_WINDOW_SECS` (15s) it returns `NoKeyframeFound`, and the caller disables the requirement for the rest of the job (streams that don't flag keyframes, e.g. intra-refresh H.264).
- **Timestamps**: uses `frame.timestamp()` (best-effort, falls back to packet DTS), not `pts`, which is missing in AVI.
- **Corruption**: any frame with `decode_error_flags != 0` in the GOP marks the result as errored; P/B frames that depend on a corrupt keyframe look clean but aren't. The first errored (or failed) timestamp aborts Tier 1 and it returns `None` for **every** frame, so in practice any corruption forces the fallback below.
- **Deferred scaling**: keeps the best raw frame and only scales the final pick (`decode_and_scale_frame`).
- **Blank retry** (when `skip_blank_frames`): `is_blank_image` → seek +2.5s, up to 10 retries.

### Tier 2/3: `extract_frames_with_genpts` → `extract_frames_sequential`
Triggered in `extract` when more than a third of Tier 1 frames are `None` (and `--allow-fallback` ≥ 1).
- `extract_frames_sequential` opens with `AVFMT_FLAG_GENPTS`, sorts timestamps, and decodes in one pass. FFmpeg logging is set to `Fatal` here to suppress corruption spam.
- **Tier 2** calls it with `keyframes_only = true` (`AVDISCARD_NONKEY`), accepting only clean keyframes.
- **Tier 3** (only if Tier 2 captured zero frames and `--allow-fallback 2`) decodes every frame. Accepts frames from 10s before to 45s after the target. Needed for RealVideo, where skipping non-keyframes breaks references.
- Blank frames are checked with `is_blank_frame` (FFmpeg frame, stride-aware).

### Failure thresholds (`extract`)
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
- **Writing**: `/frames` with `output.mode: "files"` writes into any `output.dir` it is given (created with `create_dir_all`, not joined to `--root-path`). Same trusted-callers stance; `prefix` is only checked for path separators and `..`.
- **stdout**: only the `Listening on` line goes to stdout; diagnostics go to stderr. A parent that stops reading a piped stdout would otherwise block the service once the pipe fills.
- **`metadata_text`** is accepted in the request but unused.
- **FFmpeg 8 API**: keyframe detection uses `flags & AV_FRAME_FLAG_KEY`, not the removed `key_frame` field.

## Releases

`.github/workflows/release.yml` is manual (`workflow_dispatch` with a version). It bumps `Cargo.toml`, tags, creates the GitHub release, and builds:
- **Windows x86_64**: BtbN FFmpeg 8.1 GPL shared build, DLLs bundled next to the exe.
- **Linux x86_64**: same BtbN build via `FFMPEG_DIR`, `.so` files bundled, binary linked with `-rpath,$ORIGIN` and `--disable-new-dtags` (DT_RPATH, so it also covers the FFmpeg libraries' dependencies on each other). Built on `ubuntu-22.04` to keep the glibc requirement low; a smoke step fails the job if `ldd` resolves FFmpeg outside the archive.
- **macOS aarch64**: Homebrew `ffmpeg@8` (keg-only, via `PKG_CONFIG_PATH`), not bundled; the binary hard-codes `/opt/homebrew/opt/ffmpeg@8/lib`.

Windows and Linux archives carry `FFMPEG-LICENSE.txt` and `FFMPEG-NOTICE.txt` (exact FFmpeg version and source links) for GPL compliance. All archives carry `PYSCENEDETECT-LICENSE.txt`: the BSD-3 notice for the code ported into `src/scenes.rs`.
