# Video Thumbnail Collage Service

A Rust HTTP service that extracts frames from videos and assembles them into thumbnail collages, returns them as separate images, or detects scene cuts. Built on FFmpeg for decoding and Axum for async HTTP.

## Features

- ⚡ **Fast Extraction**: Index-based seeking, typically 10-100ms per frame on healthy files
- 🛡️ **Corruption Handling**: Multi-tier fallback (seek → keyframe-only → full sequential decode) for damaged videos
- 🎬 **Frame Selection**: Uniformly spaced frames or explicit timestamps
- 🖼️ **Flexible Layouts**: Grid mode or custom percentage-based layouts
- 🎞️ **Separate Frames**: `POST /frames` returns each frame as its own JPEG or PNG, inline or written to a folder
- ✂️ **Scene Detection**: `POST /scenes` finds scene cuts (PySceneDetect-style content and adaptive detectors, FFmpeg's `scdet`, or keyframes) and can pick frames per scene in the same pass
- ⏱️ **Timestamp Overlays**: Optional timestamp rendering showing the actual extracted frame time
- 📊 **Embedded Metadata**: JSON metadata in the JPEG COM marker and an HTTP header
- 🎨 **Fit Modes**: Cover, contain, or stretch
- 🔍 **Blank Frame Skipping**: Detects black, white, and solid-color frames and seeks past them
- 📡 **WebSocket Progress**: Real-time progress updates for long-running jobs

## Quick Start

### Pre-built Binaries

Builds are available on the [Releases](https://github.com/tsukeero/rust-video-thumbnail-generator/releases) page:

- **Windows (x86_64)** and **Linux (x86_64)**: self-contained, the FFmpeg 8.1 libraries are included next to the binary. Keep them in the same folder. The Linux build needs glibc 2.35 or newer (Ubuntu 22.04, Debian 12 and later).
- **macOS (Apple Silicon)**: needs Homebrew's FFmpeg 8 at its default location. Plain `ffmpeg` is now 9.x, which this build can't load:
  ```bash
  brew install ffmpeg@8
  ```

### Building from Source

Prerequisites:

- **Rust**: recent stable toolchain
- **FFmpeg 8.x**: development libraries / shared build (`ffmpeg-next` 8.x does not build against FFmpeg 9)
- **clang** and **pkg-config** (Linux/macOS, used by the FFmpeg bindings)

1. **Clone the repository**:
   ```bash
   git clone https://github.com/tsukeero/rust-video-thumbnail-generator.git
   cd rust-video-thumbnail-generator
   ```

2. **Point the build at FFmpeg (Windows only)**:
   ```bat
   set FFMPEG_DIR=C:\path\to\ffmpeg
   ```
   `build.rs` copies the DLLs from `%FFMPEG_DIR%\bin` next to the built executable.

3. **Build**:
   ```bash
   cargo build --release
   ```

4. **Run**:
   ```bash
   ./target/release/thumbs-collage --bind 127.0.0.1:3000 --font-file /path/to/font.ttf --ws-progress
   ```

   Without `--bind` the server picks a random free port on `127.0.0.1`. The bound address is printed on startup as `Listening on http://...`.

### With All Options

```bash
./target/release/thumbs-collage \
  --bind 127.0.0.1:3000 \
  --font-file /path/to/arial.ttf \
  --ws-progress \
  --progress-interval-ms 200 \
  --max-workers 4 \
  --jpeg-quality 85 \
  --allow-fallback 2 \
  --root-path /videos
```

## Usage

### Basic Example: 5x5 Grid with Blank Frame Skipping

Extract 25 evenly spaced frames into a 5x5 grid, skipping blank frames:

```bash
curl -X POST http://localhost:3000/make_collage \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/videos/movie.mp4",
    "selection": {
      "mode": "uniform",
      "frames": 25,
      "start_offset": 10.0,
      "end_offset": 90.0
    },
    "collage": {
      "mode": "grid",
      "rows": 5,
      "cols": 5,
      "width": 1440,
      "height": 1080
    },
    "fit": "cover",
    "render_timestamps": true,
    "skip_blank_frames": true
  }' --output collage.jpg
```

### Advanced Example: Custom Layout

```bash
curl -X POST http://localhost:3000/make_collage \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/videos/movie.mp4",
    "selection": {
      "mode": "timestamps",
      "timestamps": [1.5, 10.0, 30.5, 60.0]
    },
    "collage": {
      "mode": "advanced",
      "pieces": [
        {"x_percent": 0.0, "y_percent": 0.0, "w_percent": 0.5, "h_percent": 0.5},
        {"x_percent": 0.5, "y_percent": 0.0, "w_percent": 0.5, "h_percent": 0.5},
        {"x_percent": 0.0, "y_percent": 0.5, "w_percent": 0.5, "h_percent": 0.5},
        {"x_percent": 0.5, "y_percent": 0.5, "w_percent": 0.5, "h_percent": 0.5}
      ],
      "width": 1920,
      "height": 1080,
      "background": {"r": 0, "g": 0, "b": 0, "a": 255}
    },
    "fit": "contain"
  }' --output collage.jpg
```

### WebSocket Progress Monitoring

```typescript
const ws = new WebSocket('ws://127.0.0.1:3000/ws-progress');

ws.onmessage = (event) => {
  const progress = JSON.parse(event.data);
  console.log(`Job ${progress.job_id}: ${(progress.progress * 100).toFixed(1)}%`);
  console.log(`  Fallback level: ${progress.fallback_level}`);
  console.log(`  Message: ${progress.message}`);
};

fetch('http://127.0.0.1:3000/make_collage', {
  method: 'POST',
  headers: {'Content-Type': 'application/json'},
  body: JSON.stringify({
    job_id: 'my-custom-job-1',  // optional: lets you match progress messages to this request
    path: '/videos/movie.mp4',
    // ... rest of config
  })
});
```

See `test_ws_progress_deno.ts` for a complete client.

## API Reference

### Endpoint: `POST /make_collage`

#### Request Body

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Path to the video file on the server (relative to `--root-path` if set) |
| `selection` | object | Yes | Frame selection strategy (see below) |
| `collage` | object | Yes | Layout specification (see below) |
| `fit` | string | No | `"cover"`, `"contain"`, or `"stretch"` (default: `"cover"`) |
| `render_timestamps` | boolean | No | Render timestamps on frames; requires `--font-file` (default: `false`) |
| `skip_blank_frames` | boolean | No | Skip blank/solid frames (default: `false`) |
| `job_id` | string | No | Custom job ID for progress tracking |
| `force_fallback` | number | No | Force a specific extraction tier (0=seek, 1=keyframe, 2=sequential) |
| `thumb_w` | number | No | Width hint for frame extraction |
| `thumb_h` | number | No | Height hint for frame extraction |

#### Selection Modes

**Uniform** - evenly spaced frames:
```jsonc
{
  "mode": "uniform",
  "frames": 25,
  "start_offset": 10.0,  // seconds skipped at the start (optional)
  "end_offset": 90.0     // seconds skipped at the end (optional)
}
```

**Timestamps** - explicit timestamps in seconds:
```json
{
  "mode": "timestamps",
  "timestamps": [1.5, 5.0, 10.2, 15.7]
}
```

#### Collage Modes

**Grid**:
```jsonc
{
  "mode": "grid",
  "rows": 5,
  "cols": 5,
  "spacing": 2,           // pixels between cells (optional)
  "margin": 2,            // pixels around the grid (optional)
  "width": 1440,          // output width (optional)
  "height": 1080,         // output height (optional)
  "background": {"r": 0, "g": 0, "b": 0, "a": 255}  // optional
}
```

If only `width` or only `height` is given, the other is derived from the video aspect ratio. If neither is given, the output is 1280x720.

**Advanced** - custom positions as fractions of the output size:
```jsonc
{
  "mode": "advanced",
  "pieces": [
    {
      "x_percent": 0.0,   // 0.0 to 1.0
      "y_percent": 0.0,
      "w_percent": 0.5,
      "h_percent": 0.5
    }
  ],
  "spacing": 5,
  "margin": 10,
  "width": 1920,
  "height": 1080,
  "background": {"r": 0, "g": 0, "b": 0, "a": 255}
}
```

#### Response

- **Status**: `200 OK`
- **Content-Type**: `image/jpeg`
- **Header**: `X-Collage-Meta-B64` - base64-encoded JSON metadata
- **Body**: Collage JPEG with the same metadata embedded in a COM marker

#### Metadata Format

```json
{
  "video": "/path/to/video.mp4",
  "duration": 120.5,
  "size": {"w": 1440, "h": 1080},
  "frames": [
    {
      "timestamp_requested": 10.5,
      "timestamp_actual": 11.2,
      "rect": {"x": 10, "y": 10, "w": 280, "h": 210}
    }
  ],
  "fallback_level": 0,
  "config": {
    "selection": {"mode": "uniform", "frames": 25},
    "collage": {"mode": "grid", "rows": 5, "cols": 5},
    "fit": "cover",
    "render_timestamps": true,
    "skip_blank_frames": true
  },
  "generated_at": "2025-10-06T12:34:56.789Z"
}
```

`timestamp_actual` may differ from `timestamp_requested` when blank frames are skipped, the seek lands on a nearby keyframe, or fallback extraction is used. `video` is omitted with `--no-path-meta`. `fallback_level` is the extraction tier that produced the frames (see [Multi-Tier Fallback](#multi-tier-fallback)).

#### Error Responses

- `429 Too Many Requests`: all `--max-workers` slots are busy
- `500 Internal Server Error`: processing error, body is `{"error": "..."}`

### Endpoint: `POST /frames`

The same frame extraction as `/make_collage` (selection, blank-frame skipping, fallback tiers and failure thresholds), but each frame comes back as its own image instead of a collage.

```json
{
  "path": "/videos/movie.mp4",
  "selection": { "mode": "timestamps", "timestamps": [12.5, 80.0, 141.2] },
  "size": { "width": 480, "height": 270, "fit": "cover" },
  "format": "jpeg",
  "quality": 85,
  "output": { "mode": "files", "dir": "/thumbs/movie.frames", "prefix": "frame_" },
  "skip_blank_frames": true
}
```

#### Request Body

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | As in `/make_collage` |
| `selection` | object | Yes | As in `/make_collage`; must yield at least one timestamp |
| `size` | object | No | `{ width?, height?, fit? }`, see below (default: the source size) |
| `format` | string | No | `"jpeg"` or `"png"` (default: `"jpeg"`) |
| `quality` | number | No | JPEG quality 1-100 (default: `--jpeg-quality`); refused with `png` |
| `output` | object | No | `{"mode": "inline"}` (default) or `{"mode": "files", "dir": "...", "prefix": "frame_"}` |
| `skip_blank_frames` | boolean | No | As in `/make_collage` (default: `false`) |
| `force_fallback` | number | No | As in `/make_collage` |
| `job_id` | string | No | Custom job ID for progress tracking |

Unknown fields are refused (422), so a typo doesn't pass silently. Timestamps keep their request order; duplicates are allowed.

**Size**: every frame of a job gets the same size.

| Given | Result |
|-------|--------|
| neither side | the source's decoded size |
| `width` or `height` only | the other side from the video's aspect ratio |
| both, `fit: "contain"` (default) | the largest size with the video's aspect inside the box |
| both, `fit: "cover"` | exactly `width` x `height`, scaled and centre-cropped |
| both, `fit: "stretch"` | exactly `width` x `height`, distorted |

**Output**:
- `inline`: each image is base64 in the reply (`data`). The whole reply is held in memory, so keep counts small.
- `files`: images are written to `dir` (created if missing; not joined to `--root-path`) as `{prefix}{index}.jpg|png`, the index zero-padded to at least 3 digits (`frame_007.jpg`). Existing files with those names are overwritten; other files are left alone. If the job fails, the files it wrote are deleted. `prefix` may not contain a path separator or `..`.

Each JPEG carries a COM marker with `{video, duration, index, timestamp_requested, timestamp_actual}`. PNG frames carry no metadata.

#### Response

`200 OK`, `application/json`:

```json
{
  "video": "/videos/movie.mp4",
  "duration": 1621.12,
  "source": { "w": 1024, "h": 576 },
  "size": { "w": 480, "h": 270 },
  "format": "jpeg",
  "fallback_level": 0,
  "frames": [
    { "index": 0, "timestamp_requested": 12.5, "timestamp_actual": 12.47, "file": "/thumbs/movie.frames/frame_000.jpg" },
    null,
    { "index": 2, "timestamp_requested": 141.2, "timestamp_actual": 141.17, "file": "/thumbs/movie.frames/frame_002.jpg" }
  ],
  "generated_at": "2026-11-13T10:00:00Z",
  "config": { "selection": {}, "size": {}, "format": "jpeg", "quality": 85, "output": {}, "skip_blank_frames": true, "force_fallback": null }
}
```

- `frames[i]` is `null` when frame `i` couldn't be decoded (within the `--allow-fallback` thresholds; beyond them the job fails).
- In inline mode each frame has `data` (base64) instead of `file`.
- `video` is omitted with `--no-path-meta`.

#### Error Responses

- `400 Bad Request`: invalid request (empty selection, `quality` with `png`, `width`/`height` of 0, missing `output.dir`, bad `prefix`, `force_fallback` above 2), body is `{"error": "..."}`
- `422 Unprocessable Entity`: malformed JSON or an unknown field
- `429 Too Many Requests`: all `--max-workers` slots are busy
- `500 Internal Server Error`: probe, decode or write failure, or too many frames failed, body is `{"error": "..."}`

### Endpoint: `POST /scenes`

Detects scene cuts, and optionally picks frames from the scenes in the same decoding pass. The detectors, names and defaults follow [PySceneDetect](https://www.scenedetect.com/) (see [Credits](#credits)).

```json
{
  "path": "/videos/episode01.mkv",
  "hint": "real",
  "frames": {
    "every": 2.0, "max_per_scene": 3, "max_frames": 300,
    "size": { "width": 448, "height": 448, "fit": "cover" },
    "output": { "mode": "files", "dir": "/thumbs/episode01.scenes" }
  },
  "skip_blank_frames": true
}
```

#### Request Body

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `path` | string | required | As in `/make_collage` |
| `hint` | string | none | `"anime"`, `"cg"` or `"real"`: what kind of video this is. Only picks the defaults below; explicit fields win |
| `detector` | string | `"content"` | `"content"`, `"scdet"` or `"keyframes"` |
| `threshold` | number | `content` 27, `scdet` 8 | Score at which a cut fires. Not with `keyframes` or `adaptive` |
| `min_scene_len` | number | `0.6` | Minimum scene length in **seconds** |
| `flash_filter` | string | `"merge"` | `"merge"`, `"suppress"` or `"off"`: how `min_scene_len` is enforced. Not with `keyframes` |
| `weights` | object | `{"hue": 1, "sat": 1, "lum": 1}` | `content` only |
| `adaptive` | object or `null` | from `hint` | `{ratio, window, min_score}`, see below. `null` turns off an adaptive layer the hint would add |
| `frames` | object | none | Frame selection, see below. Without it only the scene list is returned |
| `skip_blank_frames` | boolean | `false` | Frame selection only: a blank pick is replaced by the nearest non-blank candidate in the scene |
| `return_scores` | boolean | `false` | Adds the per-frame scores, for tuning |
| `job_id` | string | generated | Custom job ID for progress tracking |

Unknown fields are refused (422), and so are fields the chosen detector doesn't use (400).

**Hints**: measured on 21 labelled three-minute segments (8 anime, 5 CG films, 8 live action, 898 labelled cuts).

| `hint` | Detector | F1 |
|--------|----------|----|
| none | `content`, threshold 27, merge | 0.90 overall |
| `anime` | `content`, threshold 27, merge | 0.92 |
| `cg` | `content` + adaptive (ratio 3, `min_score` 15), merge | 0.95 |
| `real` | `content` + adaptive (ratio 3, `min_score` 15), merge | 0.93 |

The adaptive layer removes false cuts from continuous camera motion, but limited animation's held frames break its ratio test, hence the split.

#### Detectors

- **`content`** (default): a port of PySceneDetect's `ContentDetector`. Each frame is converted to HSV at 256 px wide, and the score is the weighted mean absolute difference of hue, saturation and value from the previous frame.
- **`scdet`**: FFmpeg's `scdet` score (luma difference from the previous frame, as a percentage, compared with the previous difference), matching `ffmpeg -vf scale=256:-2,scdet`. Weak on very dark footage, where luma differences are tiny.
- **`keyframes`**: a cut at every keyframe, from packet flags, with no decoding. Fast (well under a second for a full episode) but only as good as the encoder's keyframe placement: an F1 of 0.19-0.95 per video on the labelled set. Refused (422) when more than half of the packets are keyframes (all-intra codecs such as MJPEG, or an index that flags every packet).

`content` and `scdet` decode every frame in one sequential pass, so they cost about one full decode of the video. A frame with decode errors gets no score; a pass that decodes fewer frames than half the video packets fails.

**Adaptive layer** (`content` and `scdet`), a port of PySceneDetect's `AdaptiveDetector`:

| Field | Default | Description |
|-------|---------|-------------|
| `ratio` | 3.0 | A cut needs `score / mean(score of the window frames on both sides) >= ratio` |
| `window` | 2 | Frames on each side |
| `min_score` | `content` 15, `scdet` 4 | The score must also be at least this |

**Flash filter**, a port of PySceneDetect's `FlashFilter`, with `min_scene_len` as its length:
- `merge`: a burst of cuts closer together than `min_scene_len` becomes one cut, at its last frame.
- `suppress`: cuts within `min_scene_len` of the last one are dropped.
- `off`: no merging; `min_scene_len` still applies as in `suppress`. Set `min_scene_len: 0` for no minimum at all.

#### Frames

| Field | Default | Description |
|-------|---------|-------------|
| `every` | 2.0 | Seconds per frame: a scene gets `ceil(duration / every)` frames |
| `min_per_scene` | 1 | At least this many per scene |
| `max_per_scene` | 3 | At most this many per scene |
| `margin` | 0.25 | Seconds skipped at both ends of a scene, at most 10% of the scene |
| `max_frames` | none | Cap on the total |
| `size`, `format`, `quality`, `output` | as `/frames` | Same fields and behaviour; file names are padded to at least 4 digits (`frame_0007.jpg`) |

- A scene with `k` frames gets them evenly spread between its margins.
- `max_frames` first removes frames down to `min_per_scene`, one at a time from the scene with the most, then drops whole scenes' frames, shortest scene first.
- With `content` and `scdet`, frames are taken from candidates kept during the detection pass, so no second decode is needed. `timestamp` is the chosen frame's time, `target` the position placement asked for.
- With `keyframes`, frames are extracted afterwards by seeking, as in `/frames`.
- Each JPEG carries a COM marker with `{video, duration, index, scene, target, timestamp}`.

#### Response

`200 OK`, `application/json`:

```json
{
  "video": "/videos/episode01.mkv",
  "duration": 1421.4,
  "detector": "content",
  "scenes": [
    { "index": 0, "start": 0.0, "end": 8.508, "frames": [0, 1, 2] },
    { "index": 1, "start": 8.508, "end": 14.514, "frames": [3, 4, 5] }
  ],
  "frames": [
    { "index": 0, "scene": 0, "target": 1.53, "timestamp": 1.543, "file": "/thumbs/episode01.scenes/frame_0000.jpg" }
  ],
  "size": { "w": 448, "h": 448 },
  "format": "jpeg",
  "stats": { "decoded_frames": 34080, "errored_frames": 0, "candidates": 2210, "trimmed_frames": 334, "elapsed_s": 21.4 },
  "generated_at": "2026-11-13T10:00:00Z",
  "config": { "hint": "real", "detector": "content", "min_scene_len": 0.6, "flash_filter": "merge", "weights": {}, "adaptive": { "ratio": 3.0, "window": 2, "min_score": 15.0 }, "frames": {} }
}
```

- `scenes` is always present. Times are in seconds from the start of the video (its first timestamp, so MPEG-TS and VOB files start at 0 too, as in every endpoint), `end` is exclusive, and the last `end` is `duration`.
- `duration` comes from the headers, or, when they have none, from where the detection pass saw the video end (streams written to a pipe, raw elementary streams, truncated files). Unlike `/make_collage` and `/frames`, `/scenes` needs neither a header duration nor a video longer than 1 s.
- `frames`, `size` and `format` are present only when frames were requested; inline mode puts `data` (base64) in place of `file`.
- `scores` (with `return_scores`): `{ "t": [...], "score": [...], "ratio": [...] }`, one entry per scored frame; `ratio` only with `adaptive`.
- `config` holds the resolved settings, the hint's defaults filled in.

**Tuning**: request `return_scores: true` and look at `score` around cuts that were missed or wrongly found. Lower `threshold` (or `adaptive.min_score`) for missed cuts, raise it for false ones; raise `adaptive.ratio` if camera motion still produces false cuts.

#### Error Responses

- `400 Bad Request`: invalid request (a field the detector doesn't use, `threshold` with `adaptive`, negative values, `min_per_scene` above `max_per_scene`, the `output` rules of `/frames`)
- `422 Unprocessable Entity`: malformed JSON or an unknown field; `keyframes` on a video where most packets are keyframes
- `429 Too Many Requests`: all `--max-workers` slots are busy
- `500 Internal Server Error`: probe, decode or write failure, or fewer frames decoded than half the video packets

### Endpoint: `GET /ws-progress` (WebSocket)

Enabled with `--ws-progress`. Every connected client receives updates for all jobs; filter by `job_id`.

```json
{
  "job_id": "job_1728212345678",
  "filename": "/videos/movie.mp4",
  "progress": 0.45,
  "fallback_level": 0,
  "message": "Extracted 12/25 frames"
}
```

**Fallback levels**: `0` seek (fast), `1` keyframe-only, `2` sequential decode (slow, most compatible).

## Configuration

```
Options:
  --bind <ADDRESS>              Bind address [default: 127.0.0.1:0]
  --max-workers <N>             Max concurrent jobs [default: CPU count]
  --font-file <PATH>            TTF font for timestamp overlays
  --jpeg-quality <1-100>        JPEG quality [default: 80]
  --root-path <PATH>            Base directory that request paths are joined to
  --no-path-meta                Exclude video path from metadata
  --allow-fallback <LEVEL>      Fallback extraction level [default: 2]
                                0 = none (seek only)
                                1 = keyframe fallback
                                2 = sequential fallback
  --ws-progress                 Enable WebSocket progress endpoint
  --progress-interval-ms <MS>   Progress update interval [default: 200]
  --exit-on-stdin-close         Exit when stdin reaches EOF/breaks (parent died).
                                Parent must spawn with a piped stdin and keep it open
  -h, --help                    Print help
```

### Environment Variables

- `FFMPEG_DIR`: FFmpeg installation directory (Windows builds; DLLs are copied from its `bin` folder)

## Features in Detail

### Multi-Tier Fallback

1. **Seek** - seeks to each timestamp and decodes the next clean frame. If a seek lands on a non-keyframe (broken index), packets are skipped until the next keyframe. Any decode error aborts seek mode for the whole request.
2. **Keyframe-only** - used when seek mode fails. Reopens the file with regenerated timestamps and decodes only keyframes in a single pass.
3. **Sequential** - used when keyframe-only mode captures nothing. Decodes every frame; needed for formats like RealVideo.

Failure handling per `--allow-fallback` level:

| Level | Behavior | Fails when |
|-------|----------|------------|
| 0 | Seek only | more than 33% of frames fail |
| 1 | Seek → keyframe-only | any frame is still missing |
| 2 | Seek → keyframe-only → sequential | more than 20% of frames are missing |

### Blank Frame Detection

With `skip_blank_frames: true`, each extracted frame is sampled (every 8th pixel) and its luminance mean and variance computed. A frame is considered blank if:

- variance < 500 (solid color), or
- mean > 230 and variance < 3000 (near-white), or
- mean < 25 and variance < 3000 (near-black)

Blank frames trigger a seek 2.5 seconds forward, up to 10 times. The metadata records the timestamp that was actually used.

## Performance

| Video Type | Tier Used | Typical Time | Example |
|------------|-----------|--------------|---------|
| Clean H.264/H.265 | Seek | 0.5-2s for 25 frames | Modern videos |
| H.264 with corruption | Keyframe-only | 1-5s for 25 frames | Damaged downloads |
| RealVideo (.rm) | Sequential | 30-180s for 25 frames | Old formats |

Concurrency is limited by `--max-workers`; memory use is roughly one decoded frame plus the output canvas per active job.

## Project Structure

```
├── src/main.rs                 # Service implementation
├── src/scenes.rs               # /scenes: detectors, adaptive layer, flash filter
├── src/scene_frames.rs         # /scenes: frame placement, candidates, trimming
├── build.rs                    # Copies FFmpeg DLLs on Windows
├── test_ws_progress_deno.ts    # Example Deno client with progress bars
├── .github/workflows/          # Release builds (Windows, Linux, macOS)
├── CLAUDE.md                   # Implementation notes
└── LICENSE
```

### Example Client

Processes a folder of videos with live progress (expects the server on `127.0.0.1:3000` with `--ws-progress`):

```bash
deno run --allow-net --allow-read --allow-write \
  test_ws_progress_deno.ts \
  /path/to/videos \
  /path/to/output
```

## Troubleshooting

### Timestamp jumps inside a file (known limitation)

Times are counted from the video's first timestamp, which handles files that don't start at 0 (MPEG-TS, VOB). Files whose timestamps *jump* partway through are not handled: DVD rips that join several cells, or spliced TV recordings. On such a file the header duration covers only part of the video, and frames or scenes past the jump come back at the wrong time or are missing. Remuxing the file (`ffmpeg -fflags +genpts -i in.vob -c copy out.mkv`) usually gives it continuous timestamps.

### Windows: `STATUS_DLL_NOT_FOUND`

The FFmpeg DLLs are not next to the executable. Either set `FFMPEG_DIR` and rebuild, or copy `avcodec-*.dll`, `avformat-*.dll`, `avutil-*.dll`, `swscale-*.dll` and `swresample-*.dll` next to `thumbs-collage.exe`.

### Linux: FFmpeg Library Not Found

```bash
# Ubuntu/Debian
sudo apt install clang pkg-config libavcodec-dev libavformat-dev libavutil-dev \
  libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev

# Fedora/RHEL
sudo dnf install ffmpeg-devel

# Arch
sudo pacman -S ffmpeg
```

### Timestamps Don't Match

Expected when blank frames are skipped or fallback extraction is used. Compare `timestamp_requested` and `timestamp_actual` in the metadata; the rendered overlay shows the actual time.

## Known Limitations

1. **Seek accuracy**: not frame-accurate; seek mode returns the first clean frame after the nearest preceding keyframe.
2. **Blank detection**: variance-based, so it can miss low-detail frames that aren't solid or flag very dark scenes.
3. **One video per request**.
4. **No access control**: the service opens whatever path it is given (`--root-path` is a base directory, not a sandbox), has no authentication, and broadcasts job file paths to every WebSocket client. `/frames` with `output.mode: "files"` also **writes** to any folder it is given. Run it on localhost or behind something that controls access, for trusted callers only; don't bind it to a public interface.

## Credits

The `content` detector, the adaptive layer, the flash filter and their setting names and defaults are ported from [PySceneDetect](https://github.com/Breakthrough/PySceneDetect) (BSD-3-Clause, Copyright (C) 2014, Brandon Castellano; see [PYSCENEDETECT-LICENSE.txt](PYSCENEDETECT-LICENSE.txt), also shipped in the release archives). The `scdet` detector reimplements the score of FFmpeg's `scdet` filter.

## License

Copyright (C) 2025-2026 tsukeero

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

The Windows release archives bundle FFmpeg shared libraries built by [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) under the GPL. FFmpeg source is available from [ffmpeg.org](https://ffmpeg.org/download.html).

## Contributing

Issues and pull requests are welcome. Please run `cargo build` and `cargo clippy` before submitting, and describe how you tested changes against real video files.
