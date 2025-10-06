# CLAUDE.md - Technical Documentation

## Overview

This is a **video thumbnail collage service** built with Rust. It's an HTTP service that extracts frames from videos at specific timestamps and assembles them into collages (grid or custom layouts). The service includes sophisticated corruption handling with multiple fallback strategies, blank frame detection, and WebSocket-based progress monitoring.

## Architecture

### Technology Stack

- **Web Framework**: Axum (async HTTP server with WebSocket support)
- **Video Processing**: FFmpeg 8.0 (via `ffmpeg-next` bindings)
- **Image Processing**: `image` and `imageproc` crates
- **Concurrency**: Tokio async runtime with semaphore-based rate limiting
- **Serialization**: Serde for JSON parsing
- **Text Rendering**: `rusttype` and `ab_glyph` for timestamp overlays
- **Real-time Updates**: Tokio broadcast channels for WebSocket progress

### Key Design Patterns

1. **Async + Blocking Hybrid**: HTTP handlers are async, but video processing runs in `spawn_blocking` threads to avoid blocking the async runtime
2. **Semaphore Rate Limiting**: Limits concurrent video processing jobs to prevent resource exhaustion
3. **Multi-Level Fallback**: Gracefully handles corrupted videos with progressive fallback strategies
4. **Metadata Embedding**: Embeds JSON metadata into JPEG COM (comment) markers and HTTP headers
5. **Broadcast Progress**: WebSocket subscribers receive real-time progress updates via tokio broadcast channels

## Core Functionality

### 1. Frame Extraction Architecture

The service uses a multi-tiered extraction strategy with automatic fallback for corrupted videos:

#### **Tier 1: Fast Seeking** (`extract_frames_seek_each`)
**Location**: `src/main.rs:696-818`

**What it does**:
- Opens video file once with a single FFmpeg context
- Seeks to each timestamp using `av_seek_frame` (low-level API for reliability)
- Decodes frames after each seek with error detection
- **Blank Frame Detection**: Optionally skips frames with low pixel variance
- Returns `None` for frames with corruption detected
- **Actual Timestamp Tracking**: Returns tuple `(image, actual_timestamp)` for accurate metadata

**Key Features**:
- **Corruption Detection**: Checks `decode_error_flags` on every frame
- **Blank Frame Retry**: If variance < 300, seeks forward 1.5s (up to 10 retries)
- **Error Tracking**: Returns tuple `(frame, timestamp, had_errors_in_group)` to detect corrupt GOPs
- **Aspect Ratio Preservation**: Scales frames maintaining video aspect ratio
- **FFmpeg 8.0 Compatibility**: Uses `flags & AV_FRAME_FLAG_KEY` instead of deprecated `key_frame` field

**Performance**: 10-100ms per frame for healthy videos

#### **Tier 2: GENPTS Keyframe Fallback** (`extract_frames_with_genpts`)
**Location**: `src/main.rs:823-857`

Triggered when >33% of frames fail in Tier 1.

**What it does**:
- Opens video with `AVFMT_FLAG_GENPTS` to regenerate timestamps
- Sorts timestamps and decodes sequentially (one pass)
- Uses `AVDISCARD_NONKEY` to skip non-keyframes (fast)
- Only captures clean keyframes (no decode errors)
- **Blank Frame Support**: Calls `is_blank_frame()` if `skip_blank_frames` enabled

**Performance**: Fast for videos with clean keyframes (~1-3 seconds for 25 frames)

#### **Tier 3: Sequential Decode Fallback** (`extract_frames_sequential`)
**Location**: `src/main.rs:860-1012`

Triggered if Tier 2 captures 0 frames (no clean keyframes).

**What it does**:
- Decodes **all frames** sequentially without skipping
- Accepts any frame within 10 seconds before to 45 seconds after target
- Still rejects frames with `decode_error_flags != 0`
- Handles formats like RealVideo where keyframe skipping breaks B-frame references
- **Blank Frame Integration**: Skips blank frames during sequential decode

**Performance**: Slow but thorough (can take minutes for long videos)

### 2. Blank Frame Detection System

**Key Functions**:
- `is_blank_frame()`: Detects blank FFmpeg frames (for sequential extraction)
- `is_blank_image()`: Detects blank DynamicImage (for seek extraction)
- `is_blank_by_variance()`: Shared variance calculation logic

**Algorithm**:
1. Samples every 8th pixel in both dimensions (for performance)
2. Calculates luminance: `0.299 * R + 0.587 * G + 0.114 * B`
3. Computes pixel variance across samples
4. Variance < 300.0 → considered blank

**Stride Handling**:
- FFmpeg frames use stride (padding): `offset = y * stride + x * 3`
- DynamicImage uses packed RGB: `offset = y * width * 3 + x * 3`

**Integration Points**:
- **Seek Mode**: After successful extraction, checks `is_blank_image()`, retries up to 10 times
- **Sequential Mode**: During decoding loop, calls `is_blank_frame()` on FFmpeg frames
- **Metadata**: Tracks actual timestamp used after blank frame skips

### 3. Progress Tracking System

**Architecture**: `src/main.rs:59-118`

**Components**:
- `ProgressUpdate`: Serializable struct with job_id, filename, progress, fallback_level, message
- `ProgressTracker`: Per-job tracker with throttled updates (200ms default)
- `broadcast::Sender<ProgressUpdate>`: Tokio channel for fan-out to WebSocket clients

**WebSocket Handler**: `src/main.rs:282-321`
```rust
async fn handle_ws_progress(socket: WebSocket, state: AppState) {
    let mut progress_rx = state.progress_tx.subscribe();
    while let Ok(update) = progress_rx.recv().await {
        send(JSON.stringify(update));
    }
}
```

**Update Points**:
- **Seek Extraction**: Updates after each frame extraction
- **Sequential Extraction**: Updates based on video position / duration
- **Fallback Transitions**: Force-updates when changing fallback levels

**Throttling**: Updates only sent every 200ms (configurable) to avoid spam

### 4. Corruption Detection

**Multiple Layers**:
1. **FFmpeg Decode Error Flags**: Checks `frame.decode_error_flags != 0`
2. **GOP-Level Tracking**: Tracks if any frame in a Group of Pictures had errors
3. **Keyframe Preference**: In retry mode, prefers keyframes over P/B frames
4. **Error Suppression**: Sets FFmpeg log level to `Fatal` during fallback to reduce spam

**Key Insight**: P/B frames may appear "clean" but are corrupted if they depend on a corrupted keyframe. The service detects this by tracking errors across entire GOPs.

### 5. Timestamp Accuracy Tracking

**Problem**: Requested timestamps may not match actual extracted frames due to:
- Blank frame skipping
- Nearest keyframe seeking
- Corruption-based frame rejection

**Solution**: Track both timestamps in metadata
- **`timestamp_requested`**: What the user asked for
- **`timestamp_actual`**: What was actually extracted and rendered

**Implementation**:
- All extraction functions return `(DynamicImage, f64)` tuples
- Metadata structure includes both timestamps per frame
- Rendered timestamps show actual frame time

### 6. Metadata System

**Enhanced Metadata Format** (embedded in JPEG COM marker):
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
    "selection": {"mode": "uniform", "frames": 25, "start_offset": 10.0, "end_offset": 90.0},
    "collage": {"mode": "grid", "rows": 5, "cols": 5, "width": 1440, "height": 1080},
    "fit": "cover",
    "render_timestamps": true,
    "skip_blank_frames": true
  },
  "generated_at": "2025-10-06T12:34:56.789Z"
}
```

**Serialization Requirements**:
- All config types (`FrameSelection`, `CollageSpec`, `FitMode`, etc.) derive both `Serialize` and `Deserialize`
- Enables full round-trip: request → metadata → reproduce request

### 7. Collage Layout System

#### Grid Mode (`compute_grid_layout`)
**Location**: `src/main.rs:1360-1385`

- Creates uniform NxM grid with configurable spacing and margin
- **Remainder Distribution**: Distributes leftover pixels to last row/column to eliminate gaps
- Automatically calculates cell dimensions based on available space

#### Advanced Mode (`compute_advanced_layout`)
**Location**: `src/main.rs:1388-1413`

- Custom positioning using percentage-based coordinates (0.0 to 1.0)
- Each piece specifies: `x_percent`, `y_percent`, `w_percent`, `h_percent`
- Allows overlapping frames and arbitrary layouts

### 8. Frame Fitting Modes (`fit_frame_to_rect`)

**Location**: `src/main.rs:1418-1454`

Three modes:
- **Cover**: Scales to fill rectangle completely, crops overflow (like CSS `background-size: cover`)
- **Contain**: Fits entire frame within rectangle with letterboxing (like CSS `background-size: contain`)
- **Stretch**: Distorts frame to exact rectangle dimensions

Returns both source crop rectangle and destination rectangle.

### 9. Timestamp Rendering

**Location**: `src/main.rs:559-587`

- Renders timestamp overlays with **black outline for visibility** on any background
- Format: `HH:MM:SS` or `MM:SS` (adapts based on video duration)
- Uses `imageproc::drawing::draw_text_mut`
- Positioned at bottom-right with spacing/margin awareness
- Requires TTF font file via `--font-file` argument
- **Displays Actual Timestamp**: Shows the frame that was actually extracted

## API Contract

### Endpoint: `POST /make_collage`

#### Request Schema

```json
{
  "path": "/path/to/video.mp4",

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
    "spacing": 2,
    "margin": 2,
    "width": 1440,
    "height": 1080,
    "background": {"r": 0, "g": 0, "b": 0, "a": 255}
  },

  "fit": "cover",
  "render_timestamps": true,
  "skip_blank_frames": true,
  "job_id": "my-custom-job-1",
  "force_fallback": 0,
  "thumb_w": 320,
  "thumb_h": 180
}
```

#### New Request Parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `skip_blank_frames` | boolean | false | Automatically skip blank/solid frames |
| `job_id` | string | auto-generated | Custom job ID for progress tracking |
| `force_fallback` | number | null | Force fallback level (0/1/2) |

### Endpoint: `GET /ws-progress` (WebSocket)

**Protocol**: WebSocket upgrade
**Enabled by**: `--ws-progress` CLI flag

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
- `1`: Keyframe-only GENPTS fallback
- `2`: Sequential decode fallback

**Client Example** (`test_ws_progress_deno.ts`):
- Connects once to `/ws-progress`
- Receives updates for all active jobs
- Filters by `job_id` if tracking specific jobs
- Displays progress bars with fallback indicators (⚡/🔑/🐌)

## Command-Line Arguments

```bash
thumbs-collage [OPTIONS]

Options:
  --bind <ADDRESS>              Bind address [default: 127.0.0.1:0]
  --max-workers <N>             Max concurrent jobs [default: num_cpus]
  --font-file <PATH>            TTF font for timestamp rendering
  --jpeg-quality <1-100>        JPEG quality [default: 80]
  --root-path <PATH>            Root path for relative video file paths
  --no-path-meta                Exclude video path from metadata
  --allow-fallback <LEVEL>      Fallback extraction level [default: 2]
                                0 = none (seek only)
                                1 = keyframe fallback only
                                2 = sequential fallback (most permissive)
  --ws-progress                 Enable WebSocket progress endpoint at /ws-progress
  --progress-interval-ms <MS>   Progress update interval [default: 200]
  -h, --help                    Print help
```

### Fallback Level Behavior

**Level 0 (Seek Only)**:
- Uses only fast seeking (Tier 1)
- Returns 500 error if >33% frames fail
- Best for: Production with strict quality requirements

**Level 1 (Keyframe Fallback)**:
- Tries Tier 1 → Tier 2 (keyframe-only GENPTS)
- Returns 500 error if ANY frames fail after fallback
- Best for: Moderately corrupted videos, prefer speed

**Level 2 (Sequential Fallback, Default)**:
- Tries Tier 1 → Tier 2 → Tier 3 (full sequential decode)
- Returns 500 error if <80% success rate
- Best for: Maximum compatibility with corrupted videos

## FFmpeg 8.0 Migration

### Breaking Changes Handled

1. **Keyframe Detection**:
   - **Old**: `frame.key_frame == 1`
   - **New**: `(frame.flags & AV_FRAME_FLAG_KEY) != 0`
   - **Location**: `src/main.rs:967, 1174, 1187`

2. **Required Import**:
   ```rust
   use ffmpeg_next::ffi;
   ```

### Build Configuration

**Cargo.toml**:
```toml
ffmpeg-next = "8.0"
```

**Windows Build**:
```bash
set FFMPEG_DIR=C:\path\to\ffmpeg-8.0
cargo build --release
```

## Performance Characteristics

### Extraction Speed by Video Type

| Video Type | Tier Used | Typical Speed | Example |
|------------|-----------|---------------|---------|
| Clean H.264/H.265 | Tier 1 | 0.5-2s for 25 frames | Modern phone videos |
| H.264 with corruption | Tier 2 | 1-5s for 25 frames | Damaged downloads |
| RealVideo (.rm) | Tier 3 | 30-180s for 25 frames | Old formats |

### Memory Usage

- **Per Job**: ~10-50 MB (one decoded frame + canvas)
- **Concurrency**: Limited by `--max-workers` (default: CPU count)
- **FFmpeg**: Uses separate contexts per seek, minimal state
- **WebSocket**: Minimal overhead (broadcast channel + JSON serialization)

### Optimization Techniques

1. **Keyframe Skipping**: Tier 2 decodes only keyframes (10-100x speedup)
2. **Early Exit**: Stops decoding after finding target frames
3. **Pre-scaling**: Scales during extraction to reduce memory
4. **Stride-Aware Copying**: Removes FFmpeg padding for smaller images
5. **Log Suppression**: Reduces FFmpeg spam during corruption handling
6. **Throttled Progress**: Updates every 200ms to reduce WebSocket overhead
7. **Shared Variance Logic**: `is_blank_by_variance()` reused across frame types

## Code Organization

### Key Files

- **`src/main.rs`** (~1500 lines):
  - Lines 1-280: HTTP server setup, request handling
  - Lines 282-321: WebSocket progress handling
  - Lines 323-629: Video processing pipeline
  - Lines 692-818: Tier 1 extraction (seek mode)
  - Lines 823-857: Tier 2 extraction (keyframe fallback)
  - Lines 860-1012: Tier 3 extraction (sequential fallback)
  - Lines 1007-1116: Blank frame detection functions
  - Lines 1305-1454: Layout and fitting algorithms

- **`test_ws_progress_deno.ts`**: Complete Deno client example
- **`test_ws_progress.js`**: Node.js client example
- **`build.rs`**: FFmpeg DLL copying for Windows

## Error Handling

### Corruption Scenarios Handled

✅ **Missing Keyframes**: Retries with earlier seeks
✅ **Broken B-frame References**: Falls back to sequential decode
✅ **Corrupted GOPs**: Skips frames with decode errors
✅ **Invalid Indices**: Uses GENPTS to regenerate timestamps
✅ **Partial File Damage**: Extracts frames from clean sections
✅ **Blank Frames**: Automatically retries forward seeking

### Corruption Scenarios NOT Handled

❌ **Complete File Corruption**: Returns error if <80% frames extractable
❌ **Visual Artifacts**: Cannot detect macroblocks/visual glitches without decode errors
❌ **Zero Clean Keyframes**: Tier 3 may still fail if entire video is damaged

## Code Quality Notes

### Strengths
- Robust multi-level fallback system
- Comprehensive error detection and handling
- Clean separation of concerns
- Type-safe configuration via serde
- Proper async/blocking separation
- Detailed logging for debugging
- Real-time progress monitoring
- Accurate timestamp tracking
- Blank frame detection with retry logic
- FFmpeg 8.0 compatibility

### Recent Improvements
1. **Blank Frame Detection**: Automatic skipping with configurable variance threshold
2. **WebSocket Progress**: Real-time updates with fallback level tracking
3. **Timestamp Accuracy**: Metadata tracks both requested and actual timestamps
4. **Config Embedding**: Full request configuration included in output metadata
5. **Code Deduplication**: Shared variance calculation logic via `is_blank_by_variance()`
6. **FFmpeg 8.0**: Updated to use flags-based keyframe detection

### Areas for Improvement
1. **Cancellation**: No way to cancel in-progress extractions via WebSocket
2. **Caching**: Could cache recently extracted frames
3. **Parallelization**: Frame extraction could be parallelized
4. **Configuration**: Magic numbers (300 variance, 80% threshold) should be configurable
5. **Testing**: Need comprehensive unit and integration tests

## Testing Recommendations

### Unit Tests
- Layout algorithms (grid, advanced)
- Fit modes (cover, contain, stretch)
- Timestamp formatting
- COM marker embedding
- Blank frame detection variance calculation

### Integration Tests
1. **Codecs**: H.264, H.265, VP9, RealVideo, MPEG-2
2. **Containers**: MP4, MKV, RM, AVI, MOV
3. **Corruption Types**: Missing keyframes, broken B-frames, damaged GOPs
4. **Edge Cases**: Zero-length videos, single-frame videos, audio-only files
5. **Blank Frames**: Solid black, solid white, gradients
6. **Progress**: Verify WebSocket updates at all fallback levels

### Load Tests
- Verify semaphore behavior under high concurrency
- Test memory usage with many concurrent jobs
- Measure Tier 3 performance with very long videos
- WebSocket broadcast performance with many subscribers

## Example: Complete Workflow

1. **Client connects to WebSocket**: `ws://localhost:3000/ws-progress`
2. **Client submits job** with `job_id: "custom-123"`, `skip_blank_frames: true`
3. **Server starts Tier 1 extraction**, broadcasts progress updates
4. **Frame at 10.0s is blank** → server retries at 11.5s, 13.0s until non-blank
5. **33% frames fail due to corruption** → server transitions to Tier 2
6. **Progress updates** show `fallback_level: 1`
7. **Tier 2 succeeds**, captures all frames
8. **Metadata embedded** with:
   - `timestamp_requested: 10.0`
   - `timestamp_actual: 13.0` (after blank frame retries)
   - `config` includes full request parameters
9. **JPEG returned** with timestamp overlay showing "13s" (actual time)
10. **Client displays** completion with final progress update

## Future Enhancement Ideas

1. **Cancellation Support**: WebSocket command to abort jobs
2. **Parallel Extraction**: Use rayon to extract multiple frames simultaneously
3. **Smart Caching**: LRU cache for recently extracted frames
4. **Configurable Thresholds**: Expose variance threshold, retry count via API
5. **Adaptive Quality**: Auto-adjust JPEG quality based on frame complexity
6. **Batch Processing**: Multiple videos in one request
7. **Metrics**: Prometheus metrics for monitoring
8. **More Formats**: PNG, WebP, AVIF output
9. **Frame Accuracy Mode**: Optional slow-but-accurate extraction
10. **Visual Artifact Detection**: ML-based detection of visual corruption

## Debugging Tips

### Enable FFmpeg Logging
By default, FFmpeg logs are suppressed in fallback mode. To enable:
```rust
// In extract_frames_sequential, comment out:
// ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);
```

### Monitor Progress in Real-Time
```bash
# Terminal 1: Run server
./thumbs-collage --ws-progress --font-file arial.ttf

# Terminal 2: Monitor progress
deno run --allow-net test_ws_progress_deno.ts /path/to/videos /path/to/output
```

### Common Error Messages

**"Frame extraction failed: X/Y frames failed (Z%), no fallback allowed"**
- Solution: Increase `--allow-fallback` level or fix video file

**"Detected corruption at timestamp X.Xs, will use GENPTS fallback"**
- Normal behavior, indicates Tier 1 → Tier 2 transition

**"Fallback: keyframe-only mode failed, retrying with all frames (slow)"**
- Normal for RealVideo, indicates Tier 2 → Tier 3 transition

**"STATUS_DLL_NOT_FOUND" (Windows)**
- Solution: Set `FFMPEG_DIR` and rebuild, or copy DLLs manually

## Version History

- **v0.1.0**: Initial implementation with fast seeking
- **v0.2.0**: Added corruption detection and GENPTS fallback
- **v0.3.0**: Added multi-tier fallback system and `--allow-fallback` levels
- **v0.4.0**: Added fit modes (cover, contain, stretch)
- **v0.5.0**: Added timestamp rendering with visibility improvements
- **v0.6.0**: Added WebSocket progress monitoring
- **v0.7.0**: Added blank frame detection with automatic retry
- **v0.8.0**: FFmpeg 8.0 compatibility, actual timestamp tracking in metadata
