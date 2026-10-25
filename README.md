# Video Thumbnail Collage Service

A high-performance Rust HTTP service that extracts frames from videos and generates thumbnail collages. Built with FFmpeg for video processing and Axum for async HTTP handling.

## Features

- ⚡ **Ultra-Fast Extraction**: Index-based seeking extracts frames 10-100x faster than naive decoding
- 🛡️ **Robust Corruption Handling**: Multi-tier fallback system handles damaged videos gracefully
- 🎬 **Frame Extraction**: Extract frames at specific timestamps or uniformly across a video
- 🖼️ **Flexible Layouts**: Grid mode or custom percentage-based layouts
- ⏱️ **Timestamp Overlays**: Optional timestamp rendering on thumbnails (shows actual extracted frame time)
- 📊 **Rich Metadata**: JSON metadata embedded in each JPEG file with request config and actual timestamps
- 🚀 **High Performance**: Async HTTP with semaphore-based concurrency control
- 🎨 **Image Fitting**: Cover, contain, or stretch modes for aspect ratio handling
- 🔍 **Blank Frame Detection**: Automatic detection and skipping of blank/solid color frames
- 📡 **WebSocket Progress**: Real-time progress updates via WebSocket for long-running jobs

## Quick Start

### Prerequisites

- **Rust**: 1.70 or later
- **FFmpeg**: Shared libraries (`.dll` on Windows, `.so` on Linux, `.dylib` on macOS)
  - Recommended: FFmpeg 8.0 or later

### Installation

1. **Clone the repository**:
   ```bash
   git clone <repo-url>
   cd rust-thumbnail-service
   ```

2. **Set FFmpeg path (Windows)**:
   ```bash
   set FFMPEG_DIR=C:\path\to\ffmpeg
   ```

3. **Build the project**:
   ```bash
   cargo build --release
   ```

4. **Run the service**:
   ```bash
   ./target/release/thumbs-collage --bind 0.0.0.0:3000 --font-file /path/to/font.ttf --ws-progress
   ```

### With All Features Enabled

```bash
./target/release/thumbs-collage \
  --bind 0.0.0.0:3000 \
  --font-file /path/to/arial.ttf \
  --ws-progress \
  --progress-interval-ms 200 \
  --max-workers 4 \
  --jpeg-quality 85 \
  --allow-fallback 2
```

## Usage

### Basic Example: 5x5 Grid with Blank Frame Skipping

Extract 25 evenly-spaced frames and arrange them in a 5x5 grid, automatically skipping blank frames:

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

Create a custom layout with specific frame positions:

```bash
curl -X POST http://localhost:3000/make_collage \
  -H "Content-Type": application/json" \
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
  }'
```

### Using WebSocket Progress Monitoring

See `test_ws_progress_deno.ts` for a complete example of monitoring progress via WebSocket:

```typescript
// Connect to progress endpoint
const ws = new WebSocket('ws://127.0.0.1:3000/ws-progress');

ws.onmessage = (event) => {
  const progress = JSON.parse(event.data);
  console.log(`Job ${progress.job_id}: ${(progress.progress * 100).toFixed(1)}%`);
  console.log(`  Fallback level: ${progress.fallback_level}`);
  console.log(`  Message: ${progress.message}`);
};

// Make request with custom job_id
fetch('http://127.0.0.1:3000/make_collage', {
  method: 'POST',
  headers: {'Content-Type': 'application/json'},
  body: JSON.stringify({
    job_id: 'my-custom-job-1',  // Optional: track this specific job
    path: '/videos/movie.mp4',
    // ... rest of config
  })
});
```

## API Reference

### Endpoint: `POST /make_collage`

#### Request Body

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Path to video file on server |
| `selection` | object | Yes | Frame selection strategy (see below) |
| `collage` | object | Yes | Layout specification (see below) |
| `fit` | string | No | `"cover"`, `"contain"`, or `"stretch"` (default: `"cover"`) |
| `render_timestamps` | boolean | No | Render timestamps on frames (default: `false`) |
| `skip_blank_frames` | boolean | No | Skip blank/solid frames (default: `false`) |
| `job_id` | string | No | Custom job ID for progress tracking |
| `force_fallback` | number | No | Force specific fallback level (0=seek, 1=keyframe, 2=sequential) |
| `thumb_w` | number | No | Width hint for frame extraction |
| `thumb_h` | number | No | Height hint for frame extraction |

#### Selection Modes

**Uniform Mode** - Evenly spaced frames:
```json
{
  "mode": "uniform",
  "frames": 25,
  "start_offset": 10.0,  // seconds from start (optional)
  "end_offset": 90.0     // seconds from end (optional)
}
```

**Timestamps Mode** - Specific timestamps:
```json
{
  "mode": "timestamps",
  "timestamps": [1.5, 5.0, 10.2, 15.7]
}
```

#### Collage Modes

**Grid Mode** - Uniform grid:
```json
{
  "mode": "grid",
  "rows": 5,
  "cols": 5,
  "spacing": 2,           // pixels between cells (optional)
  "margin": 2,            // pixels around grid (optional)
  "width": 1440,          // output width (optional)
  "height": 1080,         // output height (optional)
  "background": {         // background color (optional)
    "r": 0, "g": 0, "b": 0, "a": 255
  }
}
```

**Advanced Mode** - Custom positions:
```json
{
  "mode": "advanced",
  "pieces": [
    {
      "x_percent": 0.0,   // x position (0.0 to 1.0)
      "y_percent": 0.0,   // y position (0.0 to 1.0)
      "w_percent": 0.5,   // width (0.0 to 1.0)
      "h_percent": 0.5    // height (0.0 to 1.0)
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
- **Header**: `X-Collage-Meta-B64` - Base64-encoded JSON metadata
- **Body**: Assembled collage image as JPEG with embedded metadata in COM marker

#### Metadata Format

The returned JPEG contains embedded metadata with both requested and actual timestamps:

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

**Note**: `timestamp_actual` may differ from `timestamp_requested` when blank frames are skipped or fallback extraction is used.

#### Error Responses

- `429 Too Many Requests`: Max concurrent jobs reached
- `500 Internal Server Error`: Processing error with JSON error details

### Endpoint: `GET /ws-progress` (WebSocket)

Real-time progress updates for active jobs (enabled with `--ws-progress` flag).

**Progress Message Format**:
```json
{
  "job_id": "job-1728212345678-0",
  "filename": "/videos/movie.mp4",
  "progress": 0.45,
  "fallback_level": 0,
  "message": "Extracted 12/25 frames"
}
```

**Fallback Levels**:
- `0`: Seek mode (fast)
- `1`: Keyframe-only GENPTS fallback (medium speed)
- `2`: Sequential decode fallback (slow, most compatible)

## Configuration

### Command-Line Options

```
Options:
  --bind <ADDRESS>              Bind address [default: 127.0.0.1:0]
  --max-workers <N>             Max concurrent jobs [default: CPU count]
  --font-file <PATH>            TTF font for timestamp overlays
  --jpeg-quality <1-100>        JPEG quality [default: 80]
  --root-path <PATH>            Root path for video files
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

- `FFMPEG_DIR`: Path to FFmpeg installation (Windows only, for DLL copying)

## Features in Detail

### Blank Frame Detection

When `skip_blank_frames: true` is set, the service:
1. Calculates pixel variance using luminance sampling
2. Frames with variance < 300 are considered "blank"
3. Automatically retries 1.5 seconds forward (up to 3 retries)
4. Updates metadata with actual timestamp used

**Use case**: Skip intros/credits with solid black/white frames

### Multi-Tier Fallback System

Handles corrupted videos gracefully with automatic fallback:

1. **Tier 1: Fast Seeking** - 10-100ms per frame for healthy videos
2. **Tier 2: Keyframe Fallback** - Uses GENPTS timestamp regeneration, decodes only keyframes
3. **Tier 3: Sequential Decode** - Decodes all frames, handles severely corrupted videos

Fallback triggers when >33% of frames fail. Controlled by `--allow-fallback` level.

### WebSocket Progress Monitoring

Real-time progress updates via WebSocket:
- Tracks current fallback level (seek/keyframe/sequential)
- Shows extraction progress (0.0 to 1.0)
- Provides status messages
- Supports custom job IDs for tracking

See `test_ws_progress_deno.ts` for a complete working example with:
- Concurrent job limiting (5 max)
- Visual progress bars with ANSI escape codes
- Fallback level indicators (⚡/🔑/🐌)
- Folder scanning and batch processing

## Performance

- **Concurrency**: Controlled by semaphore (default: CPU count)
- **Memory**: One decoded frame per active job
- **Speed**: 10-100x faster than naive sequential decoding
- **Typical Frame Extraction**: 10-100ms per frame (seek mode)
- **Example**: 25-frame collage from 2-hour video in ~0.5-2 seconds

### Performance by Fallback Level

| Video Type | Fallback Level | Typical Speed | Example |
|------------|----------------|---------------|---------|
| Clean H.264/H.265 | 0 (seek) | 0.5-2s for 25 frames | Modern videos |
| H.264 with corruption | 1 (keyframe) | 1-5s for 25 frames | Damaged downloads |
| RealVideo (.rm) | 2 (sequential) | 30-180s for 25 frames | Old formats |

## Development

### Building from Source

```bash
# Debug build
cargo build

# Release build with optimizations
cargo build --release

# Run with all features
./target/release/thumbs-collage --ws-progress --font-file /path/to/arial.ttf
```

### Project Structure

```
rust-thumbnail-service/
├── src/
│   └── main.rs                    # Main application code
├── test_ws_progress_deno.ts       # Deno example client with progress
├── test_ws_progress.js            # Node.js example client
├── build.rs                       # Build script (FFmpeg DLL copying)
├── Cargo.toml                     # Rust dependencies
├── CLAUDE.md                      # Technical documentation
└── README.md                      # This file
```

### Example Client

Run the Deno example to process a folder of videos with progress monitoring:

```bash
deno run --allow-net --allow-read --allow-write \
  test_ws_progress_deno.ts \
  /path/to/videos \
  /path/to/output
```

Features:
- Recursive video file scanning
- Concurrent processing (5 max)
- Real-time progress bars
- Fallback level indicators
- Completion summaries

## Troubleshooting

### Windows: "STATUS_DLL_NOT_FOUND" Error

FFmpeg DLLs are missing. Solutions:

1. Set `FFMPEG_DIR` environment variable and rebuild:
   ```bash
   set FFMPEG_DIR=C:\path\to\ffmpeg
   cargo build
   ```

2. Or manually copy DLLs to executable directory:
   - `avcodec-*.dll`
   - `avformat-*.dll`
   - `avutil-*.dll`
   - `swscale-*.dll`
   - `swresample-*.dll`

### Linux: FFmpeg Library Not Found

Install FFmpeg development libraries:

```bash
# Ubuntu/Debian
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev

# Fedora/RHEL
sudo dnf install ffmpeg-devel

# Arch
sudo pacman -S ffmpeg
```

### Timestamps Don't Match

This is expected when `skip_blank_frames: true` or fallback extraction is used. Check metadata:
- `timestamp_requested`: What you asked for
- `timestamp_actual`: What was actually extracted

The rendered timestamp shows the actual frame time.

## Known Limitations

1. **Seek Accuracy**: Not frame-accurate for all codecs (grabs nearest keyframe)
2. **Blank Detection**: Based on pixel variance, may not catch all visual blanks
3. **Single Video**: One video per request
4. **No Authentication**: Service has no built-in access control

See `CLAUDE.md` for detailed technical documentation.

## License

[Add your license here]

## Contributing

[Add contribution guidelines here]
