# Video Thumbnail Collage Service

A Rust HTTP service that extracts frames from videos and assembles them into thumbnail collages. Built on FFmpeg for decoding and Axum for async HTTP.

## Features

- ⚡ **Fast Extraction**: Index-based seeking, typically 10-100ms per frame on healthy files
- 🛡️ **Corruption Handling**: Multi-tier fallback (seek → keyframe-only → full sequential decode) for damaged videos
- 🎬 **Frame Selection**: Uniformly spaced frames or explicit timestamps
- 🖼️ **Flexible Layouts**: Grid mode or custom percentage-based layouts
- ⏱️ **Timestamp Overlays**: Optional timestamp rendering showing the actual extracted frame time
- 📊 **Embedded Metadata**: JSON metadata in the JPEG COM marker and an HTTP header
- 🎨 **Fit Modes**: Cover, contain, or stretch
- 🔍 **Blank Frame Skipping**: Detects black, white, and solid-color frames and seeks past them
- 📡 **WebSocket Progress**: Real-time progress updates for long-running jobs

## Quick Start

### Pre-built Binaries

Windows, Linux and macOS (Apple Silicon) builds are available on the [Releases](https://github.com/tsukeero/rust-video-thumbnail-generator/releases) page. The Windows archive includes the FFmpeg DLLs; on Linux and macOS install FFmpeg 8.x shared libraries from your package manager.

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

`timestamp_actual` may differ from `timestamp_requested` when blank frames are skipped, the seek lands on a nearby keyframe, or fallback extraction is used. `video` is omitted with `--no-path-meta`.

#### Error Responses

- `429 Too Many Requests`: all `--max-workers` slots are busy
- `500 Internal Server Error`: processing error, body is `{"error": "..."}`

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
4. **No access control**: the service opens whatever path it is given (`--root-path` is a base directory, not a sandbox), has no authentication, and broadcasts job file paths to every WebSocket client. Run it on localhost or behind something that controls access, for trusted callers only.

## License

Copyright (C) 2025-2026 tsukeero

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

The Windows release archives bundle FFmpeg shared libraries built by [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds) under the GPL. FFmpeg source is available from [ffmpeg.org](https://ffmpeg.org/download.html).

## Contributing

Issues and pull requests are welcome. Please run `cargo build` and `cargo clippy` before submitting, and describe how you tested changes against real video files.
