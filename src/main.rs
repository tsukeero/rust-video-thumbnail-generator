// src/main.rs
use std::sync::Arc;
use std::path::PathBuf;
use base64::Engine;
use anyhow::{Result, Context};
use axum::{routing::{post, get}, Router, Json, extract::{State, ws::{WebSocket, WebSocketUpgrade}}, http::{StatusCode, HeaderMap, HeaderValue}, response::IntoResponse};
use bytes::Bytes;
use clap::Parser;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::util::frame::video::Video as FfmpegFrame;
use ffmpeg_next::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next::ffi;
use image::{DynamicImage, RgbaImage, Rgba, ImageOutputFormat, imageops};
use imageproc::drawing::draw_text_mut;
use rusttype::{Font, Scale};
use tokio::sync::{Semaphore, broadcast};
use tokio::time::{Duration, Instant};
use futures::{StreamExt, SinkExt};

#[derive(Parser, Debug)]
struct Args {
    /// Bind address, e.g. 0.0.0.0:3000 or 127.0.0.1:0 for random port
    #[arg(long)]
    bind: Option<String>,

    /// Maximum concurrent workers
    #[arg(long, default_value_t = num_cpus::get())]
    max_workers: usize,

    /// Optional TTF font path for text overlays (timestamps / metadata)
    #[arg(long)]
    font_file: Option<PathBuf>,

    /// JPEG quality 1..100
    #[arg(long, default_value_t = 80)]
    jpeg_quality: u8,

    /// Root path for video files - requests can use relative paths
    #[arg(long)]
    root_path: Option<PathBuf>,

    /// Exclude video path from metadata
    #[arg(long)]
    no_path_meta: bool,

    /// Fallback extraction level: 0=none (seek only), 1=keyframe fallback, 2=sequential fallback (default: 2)
    #[arg(long, default_value_t = 2)]
    allow_fallback: u8,

    /// Enable WebSocket progress endpoint at /ws-progress
    #[arg(long)]
    ws_progress: bool,

    /// Progress update interval in milliseconds (default: 200ms)
    #[arg(long, default_value_t = 200)]
    progress_interval_ms: u64,
}

// Progress tracking
#[derive(Clone, Debug, serde::Serialize)]
struct ProgressUpdate {
    job_id: String,
    filename: String,
    progress: f64, // 0.0 to 1.0
    fallback_level: u8,
    message: String,
}

struct ProgressTracker {
    job_id: String,
    filename: String,
    tx: Option<broadcast::Sender<ProgressUpdate>>,
    last_update: Instant,
    interval: Duration,
    fallback_level: u8,
}

impl ProgressTracker {
    fn new(job_id: String, filename: String, tx: Option<broadcast::Sender<ProgressUpdate>>, interval_ms: u64, fallback_level: u8) -> Self {
        Self {
            job_id,
            filename,
            tx,
            last_update: Instant::now(),
            interval: Duration::from_millis(interval_ms),
            fallback_level,
        }
    }

    fn update(&mut self, progress: f64, message: String) {
        let now = Instant::now();
        if now.duration_since(self.last_update) >= self.interval {
            if let Some(tx) = &self.tx {
                let _ = tx.send(ProgressUpdate {
                    job_id: self.job_id.clone(),
                    filename: self.filename.clone(),
                    progress,
                    fallback_level: self.fallback_level,
                    message,
                });
                self.last_update = now;
            }
        }
    }

    fn force_update(&mut self, progress: f64, message: String) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ProgressUpdate {
                job_id: self.job_id.clone(),
                filename: self.filename.clone(),
                progress,
                fallback_level: self.fallback_level,
                message,
            });
            self.last_update = Instant::now();
        }
    }
}

#[derive(Clone)]
struct AppState {
    sem: Arc<Semaphore>,
    font: Option<Font<'static>>,
    jpeg_quality: u8,
    root_path: Option<PathBuf>,
    include_path_in_meta: bool,
    allow_fallback: u8,
    progress_tx: Option<broadcast::Sender<ProgressUpdate>>,
    progress_interval_ms: u64,
}

/// Job request.
///
/// Example JSON input (grid mode, uniform selection):
/// {
///   "path": "/videos/movie.mp4",
///   "selection": { "mode": "uniform", "frames": 12, "start_offset": 5.0, "end_offset": 10.0 },
///   "collage": { "mode": "grid", "rows": 3, "cols": 4, "spacing": 5, "margin": 10, "width": 1280 },
///   "fit": "cover",
///   "render_timestamps": true
/// }
#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum FrameSelection {
    Timestamps { timestamps: Vec<f64> },
    Uniform { frames: usize, #[serde(default)] start_offset: f64, #[serde(default)] end_offset: f64 },
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum CollageSpec {
    Grid {
        rows: usize,
        cols: usize,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        margin: u32,
        width: Option<u32>,
        height: Option<u32>,
        #[serde(default = "default_bg")] background: RgbaSpec,
    },
    Advanced {
        pieces: Vec<PieceSpec>,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        margin: u32,
        width: Option<u32>,
        height: Option<u32>,
        #[serde(default = "default_bg")] background: RgbaSpec,
    }
}

fn default_bg() -> RgbaSpec { RgbaSpec { r: 0, g: 0, b: 0, a: 255 } }

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
struct PieceSpec {
    x_percent: f32,
    y_percent: f32,
    w_percent: f32,
    h_percent: f32,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
struct RgbaSpec { r: u8, g: u8, b: u8, a: u8 }

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum FitMode { Contain, Cover, Stretch }

#[derive(serde::Deserialize, Debug)]
struct JobRequest {
    path: String,
    selection: FrameSelection,
    collage: CollageSpec,
    #[serde(default)]
    thumb_w: Option<u32>, // hint
    #[serde(default)]
    thumb_h: Option<u32>,
    #[serde(default)]
    fit: Option<FitMode>,
    #[serde(default)]
    render_timestamps: bool,
    #[serde(default)]
    #[allow(dead_code)]
    metadata_text: Option<String>, // optional textual metadata box; supports {width},{height},{duration},{filename},{aspect_ratio}
    #[serde(default)]
    job_id: Option<String>, // optional job_id for progress tracking
    #[serde(default)]
    force_fallback: Option<u8>, // force specific fallback level (0=seek, 1=keyframe, 2=sequential)
    #[serde(default)]
    skip_blank_frames: bool, // skip mostly blank/white/black frames (default: false)
}

/// Simple rect
#[derive(Clone, Debug)]
struct Rect { x: u32, y: u32, w: u32, h: u32 }

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // init ffmpeg once
    ffmpeg::init().context("ffmpeg init failed")?;
    ffmpeg::log::set_level(ffmpeg::log::Level::Error);

    let font = match args.font_file {
        Some(p) => {
            let data = std::fs::read(p).context("failed to read font file")?;
            Some(Font::try_from_vec(data).ok_or_else(|| anyhow::anyhow!("failed to load font"))?)
        }
        None => None,
    };

    // Setup progress broadcasting if enabled
    let progress_tx = if args.ws_progress {
        let (tx, _rx) = broadcast::channel(1000);
        Some(tx)
    } else {
        None
    };

    let state = AppState {
        sem: Arc::new(Semaphore::new(args.max_workers)),
        font,
        jpeg_quality: args.jpeg_quality,
        root_path: args.root_path,
        include_path_in_meta: !args.no_path_meta,
        allow_fallback: args.allow_fallback,
        progress_tx: progress_tx.clone(),
        progress_interval_ms: args.progress_interval_ms,
    };

    let mut app = Router::new()
        .route("/make_collage", post(make_collage));

    // Add WebSocket endpoint if enabled
    if args.ws_progress {
        app = app.route("/ws-progress", get(ws_progress_handler));
    }

    let app = app.with_state(state.clone());

    // Determine bind address (default to random port on localhost)
    let bind_addr = args.bind.unwrap_or_else(|| "127.0.0.1:0".to_string());
    let addr = bind_addr.parse()?;

    // Start server and get the actual bound address
    let server = axum::Server::bind(&addr)
        .serve(app.into_make_service());

    let actual_addr = server.local_addr();
    println!("Listening on http://{}", actual_addr);

    server.await.context("server failed")?;

    Ok(())
}

/// WebSocket handler for progress updates
async fn ws_progress_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws_progress(socket, state))
}

async fn handle_ws_progress(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Subscribe to progress updates
    if let Some(progress_tx) = &state.progress_tx {
        let mut progress_rx = progress_tx.subscribe();

        // Forward progress updates to WebSocket
        let send_task = tokio::spawn(async move {
            while let Ok(update) = progress_rx.recv().await {
                let json = serde_json::to_string(&update).unwrap_or_default();
                if sender.send(axum::extract::ws::Message::Text(json)).await.is_err() {
                    break;
                }
            }
        });

        // Keep connection alive until client disconnects
        let recv_task = tokio::spawn(async move {
            while let Some(msg) = receiver.next().await {
                if msg.is_err() {
                    break;
                }
            }
        });

        // Wait for either task to complete (disconnect)
        tokio::select! {
            _ = send_task => {},
            _ = recv_task => {},
        }
    }
}

/// HTTP handler
async fn make_collage(State(state): State<AppState>, Json(req): Json<JobRequest>) -> impl IntoResponse {
    // hard-cap: try to acquire without waiting
    let permit = match state.sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return (StatusCode::TOO_MANY_REQUESTS, "too many concurrent jobs").into_response(),
    };

    // Use job_id from request or generate a unique one
    let job_id = req.job_id.clone().unwrap_or_else(||
        format!("job_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis())
    );

    // run blocking work on a thread pool
    let state2 = state.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let res = process_video(&req, &state2, job_id);
        drop(permit); // release permit here
        res
    });

    match handle.await {
        Ok(Ok((collage_jpeg, overall_meta))) => {
            // Return the assembled collage as a JPEG image
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("image/jpeg"));

            // Include overall metadata as JSON header (base64 encoded)
            if let Ok(meta_bytes) = serde_json::to_vec(&overall_meta) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&meta_bytes);
                if let Ok(header_val) = HeaderValue::from_str(&b64) {
                    headers.insert("x-collage-meta-b64", header_val);
                }
            }

            (headers, Bytes::from(collage_jpeg)).into_response()
        }
        Ok(Err(e)) => {
            let error_json = serde_json::json!({
                "error": format!("{}", e)
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_json)).into_response()
        }
        Err(join_err) => {
            let error_json = serde_json::json!({
                "error": format!("task join error: {}", join_err)
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_json)).into_response()
        }
    }
}

/// Resolve video file path - combines root_path with request path if root_path is set
/// Returns: (absolute_path_for_reading, path_for_metadata)
fn resolve_paths(req_path: &str, state: &AppState) -> Result<(String, Option<String>)> {
    let absolute_path = if let Some(root) = &state.root_path {
        // Join root_path with request path (OS-agnostic)
        let full_path = root.join(req_path);
        full_path.to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid path"))?
            .to_string()
    } else {
        req_path.to_string()
    };

    let meta_path = if state.include_path_in_meta {
        Some(if state.root_path.is_some() {
            // Use relative path in metadata when root_path is set
            req_path.to_string()
        } else {
            // Use absolute path in metadata when no root_path
            absolute_path.clone()
        })
    } else {
        None
    };

    Ok((absolute_path, meta_path))
}

/// Blocking processing function (runs inside spawn_blocking)
///
/// Returns: Assembled collage JPEG bytes, plus overall metadata JSON.
fn process_video(req: &JobRequest, state: &AppState, job_id: String) -> Result<(Vec<u8>, serde_json::Value)> {
    // Resolve paths
    let (video_path, meta_path) = resolve_paths(&req.path, state)?;

    // 1) probe duration and dimensions
    let duration = probe_duration(&video_path)?;
    if(duration <= 1.0) {
        anyhow::bail!("duration is too short");
    }
    let (video_w, video_h) = probe_dimensions(&video_path)?;
    let video_aspect = video_w as f64 / video_h as f64;

    // 2) compute timestamps
    let timestamps = match &req.selection {
        FrameSelection::Timestamps { timestamps } => timestamps.clone(),
        FrameSelection::Uniform { frames, start_offset, end_offset } => {
            let start = *start_offset;
            let end = if *end_offset == 0.0 { duration } else { duration - end_offset };
            let usable = end - start;
            if usable <= 0.0 { anyhow::bail!("invalid offsets") }
            if *frames == 0 { Vec::new() }
            else {
                (0..*frames).map(|i| start + usable * ((i as f64 + 0.5) / *frames as f64)).collect()
            }
        }
    };

    // 3) compute collage output size and rects
    let (out_w, out_h, rects) = match &req.collage {
        CollageSpec::Grid { rows, cols, spacing, margin, width, height, background: _ } => {
            let (ow, oh) = derive_output_size_grid(*width, *height, video_aspect, *rows, *cols, *spacing, *margin);
            let rects = compute_grid_layout(*rows, *cols, *spacing, *margin, ow, oh);
            (ow, oh, rects)
        }
        CollageSpec::Advanced { pieces, spacing, margin, width, height, background: _ } => {
            let (ow, oh) = derive_output_size_adv(*width, *height, video_aspect);
            let rects = compute_advanced_layout(&pieces, *margin, *spacing, ow, oh);
            (ow, oh, rects)
        }
    };

    let fit_mode = req.fit.unwrap_or(FitMode::Cover);
    let thumb_hint_w = req.thumb_w.unwrap_or(0);
    let thumb_hint_h = req.thumb_h.unwrap_or(0);

    // Create progress tracker for fallback 0 (seek mode)
    let mut tracker = ProgressTracker::new(
        job_id.clone(),
        req.path.clone(),
        state.progress_tx.clone(),
        state.progress_interval_ms,
        0
    );

    // 4) Extract frames - check if fallback is forced
    let mut imgs = if let Some(forced_level) = req.force_fallback {
        if forced_level >= 1 {
            // Force fallback mode immediately
            eprintln!("Forcing fallback level {} as requested", forced_level);
            tracker.fallback_level = forced_level;
            tracker.force_update(0.0, format!("Forcing fallback level {}", forced_level));
            extract_frames_with_genpts(&video_path, &timestamps, &rects, thumb_hint_w, thumb_hint_h,
                fit_mode, video_aspect, forced_level, duration, req.skip_blank_frames, &mut tracker)?
        } else {
            // force_fallback=0 means use seek mode
            extract_frames_seek_each(&video_path, &timestamps, &rects, thumb_hint_w, thumb_hint_h, fit_mode, video_aspect, true, &mut tracker, req.skip_blank_frames, duration)?
        }
    } else {
        // Normal mode: try seek first, fallback if needed
        extract_frames_seek_each(&video_path, &timestamps, &rects, thumb_hint_w, thumb_hint_h, fit_mode, video_aspect, true, &mut tracker, req.skip_blank_frames, duration)?
    };

    // Check if we should use fallback based on failure rate and allow_fallback level (only if not forced)
    if req.force_fallback.is_none() {
        let failed_count = imgs.iter().filter(|i| i.is_none()).count();
        let failure_rate = failed_count as f64 / timestamps.len() as f64;

        // Level 0: No fallback, error if >33% failed
        if state.allow_fallback == 0 && failure_rate > 0.33 {
            anyhow::bail!(
                "Frame extraction failed: {}/{} frames failed ({}%), no fallback allowed",
                failed_count, timestamps.len(), (failure_rate * 100.0) as u32
            );
        }

        // Level 1+: Try GENPTS fallback if >33% failed
        if failed_count > timestamps.len() / 3 && state.allow_fallback >= 1 {
            eprintln!("WARNING: {}/{} frames failed, triggering GENPTS fallback (level {})",
                failed_count, timestamps.len(), state.allow_fallback);

            // Update tracker for fallback mode
            tracker.fallback_level = state.allow_fallback;
            tracker.force_update(0.0, "Starting fallback extraction".to_string());

            imgs = extract_frames_with_genpts(&video_path, &timestamps, &rects, thumb_hint_w, thumb_hint_h,
                fit_mode, video_aspect, state.allow_fallback, duration, req.skip_blank_frames, &mut tracker)?;
        }
    }

    // Check final failure rate
    let final_failed = imgs.iter().filter(|i| i.is_none()).count();
    let final_rate = final_failed as f64 / timestamps.len() as f64;

    // Level 1: Error if any frames failed after keyframe fallback
    if state.allow_fallback == 1 && final_failed > 0 {
        anyhow::bail!(
            "Frame extraction failed: {}/{} frames failed ({}%) after keyframe fallback",
            final_failed, timestamps.len(), (final_rate * 100.0) as u32
        );
    }

    // Level 2: Error if >20% failed after sequential fallback
    if state.allow_fallback == 2 && final_rate > 0.20 {
        anyhow::bail!(
            "Frame extraction failed: {}/{} frames failed ({}%) after sequential fallback, need 80% success",
            final_failed, timestamps.len(), (final_rate * 100.0) as u32
        );
    }

    // 5) Compose collage canvas and paste frames
    let bg = match &req.collage {
        CollageSpec::Grid { background, .. } => background,
        CollageSpec::Advanced { background, .. } => background,
    };
    let mut canvas = RgbaImage::from_pixel(out_w, out_h, rgba_from_spec(bg));

    // optional font
    let font = state.font.as_ref();

    // Build metadata for each frame
    let mut frames_meta = Vec::new();

    for i in 0..rects.len() {
        // if we have a frame for this cell index (timestamps aligned by index)
        let ts_opt = timestamps.get(i).cloned();
        if let Some(requested_ts) = ts_opt {
            if let Some((img, actual_ts)) = imgs.get(i).and_then(|o| o.as_ref()) {
                // The extracted image is already scaled to rect dimensions during extraction
                // So we just need to fit it into the rect (may need cropping if aspect ratios differ)
                let fitted = fit_frame_to_rect(img.width(), img.height(), rects[i].clone(), fit_mode);

                // Crop the extracted frame if needed and paste onto canvas
                let src_crop = img.crop_imm(fitted.src_rect.x, fitted.src_rect.y, fitted.src_rect.w, fitted.src_rect.h);

                // Only resize if the cropped size doesn't match destination
                let final_img = if src_crop.width() != fitted.dst_rect.w || src_crop.height() != fitted.dst_rect.h {
                    src_crop.resize_exact(fitted.dst_rect.w, fitted.dst_rect.h, imageops::FilterType::Lanczos3)
                } else {
                    src_crop
                };

                imageops::overlay(&mut canvas, &final_img, fitted.dst_rect.x as i64, fitted.dst_rect.y as i64);

                // optional timestamp overlay on canvas (draw white text with font)
                if req.render_timestamps {
                    if let Some(f) = font {
                        // Use actual timestamp for the label
                        let label = format_timecode(*actual_ts, duration);
                        let font_size = 16.0;
                        let padding = 4; // padding from edges

                        // Estimate text width (rough approximation: ~9 pixels per char for size 16)
                        let text_width = (label.len() as u32 * 9).max(1);

                        // Position at bottom-right with padding, staying within fitted rect bounds
                        let x = (fitted.dst_rect.x + fitted.dst_rect.w).saturating_sub(text_width + padding);
                        let y = (fitted.dst_rect.y + fitted.dst_rect.h).saturating_sub(18 + padding);

                        // Draw black outline/border by drawing text in 8 directions around the main text
                        let black = image::Rgba([0, 0, 0, 255]);
                        for dx in -1..=1 {
                            for dy in -1..=1 {
                                if dx != 0 || dy != 0 {
                                    draw_text_mut(&mut canvas, black, x as i32 + dx, y as i32 + dy, Scale::uniform(font_size), f, &label);
                                }
                            }
                        }

                        // Draw white text on top
                        draw_text_mut(&mut canvas, image::Rgba([255,255,255,255]), x as i32, y as i32, Scale::uniform(font_size), f, &label);
                    }
                }

                // per-frame metadata
                let frame_meta = serde_json::json!({
                    "timestamp_requested": requested_ts,
                    "timestamp_actual": actual_ts,
                    "rect": { "x": fitted.dst_rect.x, "y": fitted.dst_rect.y, "w": fitted.dst_rect.w, "h": fitted.dst_rect.h },
                });
                frames_meta.push(frame_meta);
            } else {
                // no image decoded for this timestamp — record null entry
                frames_meta.push(serde_json::Value::Null);
            }
        } else {
            // no timestamp for this cell (e.g. fewer frames than cells)
            frames_meta.push(serde_json::Value::Null);
        }
    }

    // Encode the final assembled collage canvas as JPEG
    let mut collage_buf: Vec<u8> = Vec::new();
    DynamicImage::ImageRgba8(canvas)
        .write_to(&mut std::io::Cursor::new(&mut collage_buf), ImageOutputFormat::Jpeg(state.jpeg_quality))?;

    // Build overall metadata
    let mut meta_obj = serde_json::json!({
        "duration": duration,
        "size": { "w": out_w, "h": out_h },
        "frames": frames_meta,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "config": {
            "selection": &req.selection,
            "collage": &req.collage,
            "fit": req.fit,
            "render_timestamps": req.render_timestamps,
            "skip_blank_frames": req.skip_blank_frames,
        }
    });

    // Add video path if not excluded
    if let Some(path) = meta_path {
        meta_obj["video"] = serde_json::json!(path);
    }

    let overall_meta = meta_obj;

    // Embed metadata into the collage JPEG
    let meta_bytes = serde_json::to_vec(&overall_meta)?;
    let collage_with_meta = insert_jpeg_com(&collage_buf, &meta_bytes)?;

    Ok((collage_with_meta, overall_meta))
}

/// Insert a JPEG COM marker immediately after the SOI marker (0xFFD8).
/// The COM marker format:
/// 0xFF 0xFE (2 bytes) + length (2 bytes, big-endian, includes these two length bytes) + data.
/// This function returns a new Vec<u8> with the COM marker inserted.
fn insert_jpeg_com(jpeg: &[u8], comment: &[u8]) -> Result<Vec<u8>> {
    // basic sanity: JPEG should start with SOI bytes 0xFF 0xD8
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        anyhow::bail!("invalid jpeg");
    }
    // COM marker: 0xFF 0xFE
    // length = comment_len + 2 (length bytes themselves)
    if comment.len() + 2 > std::u16::MAX as usize {
        anyhow::bail!("comment too large");
    }
    let mut out = Vec::with_capacity(jpeg.len() + comment.len() + 8);
    // copy SOI
    out.extend_from_slice(&jpeg[0..2]);
    // insert COM marker
    out.push(0xFF);
    out.push(0xFE);
    let len_be = ((comment.len() + 2) as u16).to_be_bytes();
    out.extend_from_slice(&len_be);
    out.extend_from_slice(comment);
    // copy remainder of original jpeg
    out.extend_from_slice(&jpeg[2..]);
    Ok(out)
}

/// Probe duration (seconds). Uses stream duration if available, otherwise container duration.
fn probe_duration(path: &str) -> Result<f64> {
    let ictx = ffmpeg::format::input(&path)?;
    let stream = ictx.streams().best(ffmpeg::media::Type::Video).ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    if stream.duration() > 0 {
        let tb = stream.time_base();
        let dur = stream.duration() as f64 * (tb.0 as f64 / tb.1 as f64);
        Ok(dur)
    } else {
        let dur = ictx.duration();
        if dur > 0 {
            // container duration is in AV_TIME_BASE (microseconds)
            Ok((dur as f64) / (ffmpeg_next::ffi::AV_TIME_BASE as f64))
        } else {
            anyhow::bail!("could not determine duration");
        }
    }
}

fn probe_dimensions(path: &str) -> Result<(u32,u32)> {
    let ictx = ffmpeg::format::input(&path)?;
    let stream = ictx.streams().best(ffmpeg::media::Type::Video).ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?.decoder().video()?;
    Ok((decoder.width(), decoder.height()))
}

/// Frame extraction with single context and multiple seeks.
///
/// Opens the video file once and seeks to each timestamp within the same context.
/// This is reliable and fast enough for most use cases.
fn extract_frames_seek_each(
    path: &str,
    timestamps: &[f64],
    rects: &[Rect],
    thumb_w_hint: u32,
    thumb_h_hint: u32,
    fit_mode: FitMode,
    video_aspect: f64,
    allow_corrupt: bool,
    tracker: &mut ProgressTracker,
    skip_blank_frames: bool,
    duration: f64,
) -> Result<Vec<Option<(DynamicImage, f64)>>> {
    if timestamps.is_empty() { return Ok(vec![]); }

    // Open input context ONCE
    let mut ictx = ffmpeg::format::input(&path)?;
    let vs = ictx.streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = vs.index();
    let tb = vs.time_base();
    let seconds_per_tick = tb.0 as f64 / tb.1 as f64;
    let codec_params = vs.parameters();

    // Create decoder ONCE
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(codec_params)?
        .decoder()
        .video()?;

    // Enable output of corrupted frames if requested
    if allow_corrupt {
        unsafe {
            (*decoder.as_mut_ptr()).flags |= ffmpeg_next::ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;
        }
    }

    let mut results: Vec<Option<(DynamicImage, f64)>> = Vec::with_capacity(timestamps.len());
    let mut found_corruption = false;

    for (i, &ts) in timestamps.iter().enumerate() {
        // Target size hint
        let target_w = if i < rects.len() { rects[i].w } else { thumb_w_hint.max(160) };
        let target_h = if i < rects.len() { rects[i].h } else { thumb_h_hint.max(90) };

        // Try extracting frame, with retry if blank frame detected
        let mut current_ts = ts;
        let mut retry_count = 0;
        let max_retries = 10;
        let mut frame_result;

        loop {
            // Seek to timestamp using low-level API
            let target_pts = (current_ts / seconds_per_tick) as i64;

            // Use unsafe FFmpeg API directly since the safe wrapper doesn't seem to work
            unsafe {
                ffmpeg_next::ffi::av_seek_frame(
                    ictx.as_mut_ptr(),
                    stream_index as i32,
                    target_pts,
                    ffmpeg_next::ffi::AVSEEK_FLAG_BACKWARD,
                );
            }

            // Flush decoder to discard buffered frames from previous position
            decoder.flush();

            // Extract frame at this position
            frame_result = extract_frame_after_seek(&mut ictx, &mut decoder, stream_index, seconds_per_tick, current_ts, target_w, target_h, fit_mode, video_aspect, false, duration);

            // Check for blank frames ONLY if we got a successful result
            if frame_result.is_ok() && skip_blank_frames {
                if let Ok((ref img, _, _)) = frame_result {
                    if is_blank_image(img) {
                        // Frame is blank, retry with forward seek if possible
                        retry_count += 1;
                        if retry_count <= max_retries && current_ts + 2.0 < duration {
                            current_ts += 2.5; // Move forward 1.5 seconds
                            continue;
                        }
                    }
                }
            }

            // Frame is good or we can't retry, use it
            break;
        }

        // Check if we got a frame but it had errors in its GOP (corrupted P/B frames)
        let had_errors = frame_result.as_ref().map(|(_, _, errors)| *errors).unwrap_or(false);

        // If we detect corruption, mark it and return None for ALL frames
        // This will trigger the GENPTS fallback which does sequential decode properly
        if had_errors || frame_result.is_err() {
            found_corruption = true;
            // Corruption detected, will use GENPTS fallback
            break;
        }

        match frame_result {
            Ok((img, actual_ts, _)) => results.push(Some((img, actual_ts))),
            Err(_) => results.push(None),
        }

        // Update progress based on timestamps captured
        let progress = (i + 1) as f64 / timestamps.len() as f64;
        tracker.update(progress, format!("Extracted {}/{} frames", i + 1, timestamps.len()));
    }

    // Final progress update
    if !found_corruption {
        tracker.force_update(1.0, format!("Completed {}/{} frames", results.len(), timestamps.len()));
    }

    // If corruption detected, return None for all frames to trigger GENPTS fallback
    if found_corruption {
        return Ok(vec![None; timestamps.len()]);
    }

    Ok(results)
}

/// Frame extraction with PTS regeneration for corrupted videos.
/// Used as fallback when normal seeking fails extensively.
/// Decodes sequentially from start, capturing frames at target timestamps.
/// Uses a two-pass approach: first with keyframe skipping (fast), then without if needed (slow).
fn extract_frames_with_genpts(
    path: &str,
    timestamps: &[f64],
    rects: &[Rect],
    thumb_w_hint: u32,
    thumb_h_hint: u32,
    fit_mode: FitMode,
    video_aspect: f64,
    allow_fallback: u8,
    duration: f64,
    skip_blank_frames: bool,
    tracker: &mut ProgressTracker,
) -> Result<Vec<Option<(DynamicImage, f64)>>> {
    // First pass: try with keyframe skipping (fast) - this is fallback level 1
    tracker.fallback_level = 1;
    tracker.force_update(0.0, "Trying keyframe-only mode".to_string());
    let results = extract_frames_sequential(path, timestamps, rects, thumb_w_hint, thumb_h_hint, fit_mode, video_aspect, true, duration, skip_blank_frames, tracker)?;

    // Check if we got any frames
    let captured = results.iter().filter(|r| r.is_some()).count();
    if captured > 0 {
        tracker.force_update(1.0, format!("Keyframe mode completed: {}/{} frames", captured, timestamps.len()));
        return Ok(results);
    }

    // Level 1: Only keyframe fallback allowed, don't try sequential
    if allow_fallback == 1 {
        return Ok(results);
    }

    // Level 2: Try sequential decode as second pass
    tracker.fallback_level = 2;
    tracker.force_update(0.0, "Trying sequential decode (all frames)".to_string());
    extract_frames_sequential(path, timestamps, rects, thumb_w_hint, thumb_h_hint, fit_mode, video_aspect, false, duration, skip_blank_frames, tracker)
}

/// Internal function to decode frames sequentially with optional keyframe skipping
fn extract_frames_sequential(
    path: &str,
    timestamps: &[f64],
    rects: &[Rect],
    thumb_w_hint: u32,
    thumb_h_hint: u32,
    fit_mode: FitMode,
    video_aspect: f64,
    keyframes_only: bool,
    duration: f64,
    skip_blank_frames: bool,
    tracker: &mut ProgressTracker,
) -> Result<Vec<Option<(DynamicImage, f64)>>> {
    if timestamps.is_empty() { return Ok(vec![]); }

    // Sort timestamps with their original indices to maintain order
    let mut indexed_timestamps: Vec<(usize, f64)> = timestamps.iter().copied().enumerate().collect();
    indexed_timestamps.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // Open input context with GENPTS flag to regenerate timestamps
    let mut ictx = ffmpeg::format::input(&path)?;
    unsafe {
        (*ictx.as_mut_ptr()).flags |= ffmpeg_next::ffi::AVFMT_FLAG_GENPTS as i32;
    }

    let vs = ictx.streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = vs.index();
    let tb = vs.time_base();
    let seconds_per_tick = tb.0 as f64 / tb.1 as f64;
    let codec_params = vs.parameters();

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(codec_params)?
        .decoder()
        .video()?;

    // Always enable corrupt frame output for this fallback mode
    unsafe {
        (*decoder.as_mut_ptr()).flags |= ffmpeg_next::ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;
    }

    // Suppress FFmpeg log spam for corrupted videos
    ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);

    // Enable keyframe skipping if requested
    if keyframes_only {
        unsafe {
            (*decoder.as_mut_ptr()).skip_frame = ffmpeg_next::ffi::AVDiscard::AVDISCARD_NONKEY;
        }
    }

    // Results indexed by original timestamp order
    let mut results: Vec<Option<(DynamicImage, f64)>> = vec![None; timestamps.len()];
    let mut target_idx = 0;
    let mut frames_decoded = 0;
    let mut clean_keyframes_found = 0;

    // Decode sequentially and capture frames at target timestamps
    let mut packets_processed = 0;
    'outer: for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }

        packets_processed += 1;

        decoder.send_packet(&packet)?;
        let mut decoded = FfmpegFrame::empty();

        while decoder.receive_frame(&mut decoded).is_ok() {
            frames_decoded += 1;
            let frame_pts = decoded.pts().unwrap_or(0);
            let frame_seconds = frame_pts as f64 * seconds_per_tick;

            // Update progress based on video position
            let progress = (frame_seconds / duration).min(1.0);
            tracker.update(progress, format!("Decoding at {:.1}s/{:.1}s ({}/{})", frame_seconds, duration, target_idx, indexed_timestamps.len()));

            // Check if this is a clean keyframe
            let (has_errors, is_keyframe) = unsafe {
                let frame_ptr = decoded.as_ptr();
                //FOR FFMPEG 8.0, old ways was ((*frame_ptr).decode_error_flags != 0, (*frame_ptr).key_frame == 1)
                ((*frame_ptr).decode_error_flags != 0, ((*frame_ptr).flags & ffi::AV_FRAME_FLAG_KEY) != 0)
            };
            if is_keyframe && !has_errors {
                clean_keyframes_found += 1;
            }

            // No keyframe skipping in fallback mode - decode all frames

            // Check if this frame is close to our next target
            while target_idx < indexed_timestamps.len() {
                let (orig_idx, target_ts) = indexed_timestamps[target_idx];

                // Accept frames 5 seconds before to 30 seconds after target
                if frame_seconds >= target_ts - 10.0 && frame_seconds <= target_ts + 45.0 {
                    // Use already-computed has_errors value
                    if !has_errors {
                        // Check if frame is blank (if skip_blank_frames is enabled)
                        let is_blank = if skip_blank_frames {
                            is_blank_frame(&decoded)
                        } else {
                            false
                        };

                        if is_blank {
                            // Blank frame detected, skip it and keep looking for this target
                            break; // Exit inner while loop to continue decoding
                        }

                        // This frame is close enough to our target and has no errors
                        let target_w = if orig_idx < rects.len() { rects[orig_idx].w } else { thumb_w_hint.max(160) };
                        let target_h = if orig_idx < rects.len() { rects[orig_idx].h } else { thumb_h_hint.max(90) };

                        // Scale frame based on fit mode
                        if let Ok(img) = decode_and_scale_frame(&decoded, target_w, target_h, fit_mode, video_aspect) {
                            results[orig_idx] = Some((img, frame_seconds));
                            // Only move to next target if we successfully captured this one
                            target_idx += 1;
                        } else {
                            // Frame decode/scale failed, skip to next target
                            target_idx += 1;
                        }
                    } else {
                        // Frame has errors, keep searching for a good frame at this timestamp
                        // Don't increment target_idx yet, keep looking
                        break; // Exit inner while loop to continue decoding
                    }

                    // If we got all frames, stop
                    if target_idx >= indexed_timestamps.len() {
                        break 'outer;
                    }
                } else if frame_seconds > target_ts + 45.0 {
                    // We passed this target without finding a good frame
                    target_idx += 1;
                } else {
                    // Haven't reached target yet, continue decoding
                    break;
                }
            }
        }
    }

    // Fallback complete

    // Final progress update
    let captured = results.iter().filter(|r| r.is_some()).count();
    tracker.force_update(1.0, format!("Sequential decode completed: {}/{} frames", captured, results.len()));

    Ok(results)
}

/// Calculate pixel variance from luminance samples
/// Returns true if variance is below threshold (indicating blank/solid frame)
/// Also detects nearly-white or nearly-black frames by checking mean luminance
fn is_blank_by_variance(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return false;
    }

    let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
    let variance: f32 = samples.iter()
        .map(|&x| (x - mean).powi(2))
        .sum::<f32>() / samples.len() as f32;

    println!("Blank check: variance={:.2}, mean={:.2}, samples={}", variance, mean, samples.len());

    // Low variance frames (solid colors)
    if variance < 500.0 {
        return true;
    }

    // Nearly-white frames: high mean (close to 255) with low-to-medium variance
    // White frames often have compression artifacts, so variance can be 1000-2000
    if mean > 230.0 && variance < 3000.0 {
        return true;
    }

    // Nearly-black frames: low mean (close to 0) with low-to-medium variance
    if mean < 25.0 && variance < 3000.0 {
        return true;
    }
    println!("NOT BLANK: variance={:.2}, mean={:.2}, samples={}", variance, mean, samples.len());
    false
}

/// Check if a frame is mostly blank (black, white, or solid color) by calculating pixel variance
/// Returns true if the frame appears to be blank (variance below threshold)
fn is_blank_frame(decoded: &FfmpegFrame) -> bool {
    let width = decoded.width() as usize;
    let height = decoded.height() as usize;
    let stride = decoded.stride(0);
    let data = decoded.data(0);

    let step = 8;
    let mut samples = Vec::new();

    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            let offset = y * stride + x * 3; // RGB24 format with stride
            if offset + 2 < data.len() {
                let r = data[offset] as f32;
                let g = data[offset + 1] as f32;
                let b = data[offset + 2] as f32;
                let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
                samples.push(luminance);
            }
        }
    }

    is_blank_by_variance(&samples)
}

/// Check if a DynamicImage is blank by calculating pixel variance
fn is_blank_image(img: &DynamicImage) -> bool {
    let rgb_img = img.to_rgb8();
    let width = rgb_img.width() as usize;
    let height = rgb_img.height() as usize;
    let data = rgb_img.as_raw();

    let step = 8;
    let mut samples = Vec::new();

    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            let offset = y * width * 3 + x * 3; // Packed RGB, no stride
            if offset + 2 < data.len() {
                let r = data[offset] as f32;
                let g = data[offset + 1] as f32;
                let b = data[offset + 2] as f32;
                let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
                samples.push(luminance);
            }
        }
    }

    is_blank_by_variance(&samples)
}

/// Helper to decode and scale a single frame
fn decode_and_scale_frame(
    decoded: &FfmpegFrame,
    target_w: u32,
    target_h: u32,
    fit_mode: FitMode,
    video_aspect: f64,
) -> Result<DynamicImage> {
    // Calculate scaled dimensions based on fit mode
    let (scale_w, scale_h) = match fit_mode {
        FitMode::Stretch => (target_w, target_h),
        FitMode::Cover | FitMode::Contain => {
            let target_aspect = target_w as f64 / target_h as f64;
            if video_aspect > target_aspect {
                let h = target_h;
                let w = (h as f64 * video_aspect).round() as u32;
                (w, h)
            } else {
                let w = target_w;
                let h = (w as f64 / video_aspect).round() as u32;
                (w, h)
            }
        }
    };

    // Convert to RGB and scale
    let mut rgb = FfmpegFrame::empty();
    let mut scaler = Scaler::get(
        decoded.format(),
        decoded.width(),
        decoded.height(),
        ffmpeg::format::pixel::Pixel::RGB24,
        scale_w,
        scale_h,
        Flags::BILINEAR,
    )?;
    scaler.run(decoded, &mut rgb)?;

    // Copy line-by-line respecting stride
    let width = rgb.width() as usize;
    let height = rgb.height() as usize;
    let stride = rgb.stride(0);
    let data = rgb.data(0);

    let mut rgb_data = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let line_start = y * stride;
        let line_end = line_start + (width * 3);
        rgb_data.extend_from_slice(&data[line_start..line_end]);
    }

    let img = image::RgbImage::from_raw(width as u32, height as u32, rgb_data)
        .ok_or_else(|| anyhow::anyhow!("failed to create image from raw data"))?;

    Ok(DynamicImage::ImageRgb8(img).to_rgba8().into())
}

/// Extract a frame after seeking (assumes seek has already been performed).
/// Decodes frames and finds the one closest to the target timestamp.
/// Returns (frame, had_errors_in_group) where had_errors_in_group indicates
/// if any frame since the last keyframe had decode errors.
fn extract_frame_after_seek(
    ictx: &mut ffmpeg::format::context::Input,
    decoder: &mut ffmpeg::decoder::Video,
    stream_index: usize,
    seconds_per_tick: f64,
    timestamp: f64,
    target_w: u32,
    target_h: u32,
    fit_mode: FitMode,
    video_aspect: f64,
    relaxed_limits: bool,
    duration: f64,
) -> Result<(DynamicImage, f64, bool)> {
    let mut best_frame: Option<DynamicImage> = None;
    let mut best_frame_timestamp = 0.0;
    let mut best_frame_distance = f64::MAX;
    let mut frames_decoded = 0;
    let mut had_errors_in_group = false; // Track if any frame had errors

    // Decode frames and find the one closest to our target timestamp
    for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }

        decoder.send_packet(&packet)?;
        let mut decoded = FfmpegFrame::empty();

        while decoder.receive_frame(&mut decoded).is_ok() {
            frames_decoded += 1;

            // Get frame timestamp
            let frame_pts = decoded.pts().unwrap_or(0);
            let frame_seconds = frame_pts as f64 * seconds_per_tick;

            // Calculate distance from target
            let distance = (frame_seconds - timestamp).abs();

            // Check if this frame has decode errors (indicates corruption/garbage)
            // Also check if it's a keyframe (more reliable for corrupted videos)
            let (has_errors, is_keyframe) = unsafe {
                let frame_ptr = decoded.as_ptr();
                let errors = (*frame_ptr).decode_error_flags != 0;
                //FOR FFMPEG 8.0, old ways was ((*frame_ptr).decode_error_flags != 0, (*frame_ptr).key_frame == 1)
                let keyframe = ((*frame_ptr).flags & ffi::AV_FRAME_FLAG_KEY) != 0;
                (errors, keyframe)
            };

            // Track if we encountered any errors in this GOP (Group of Pictures)
            if has_errors {
                had_errors_in_group = true;
            }

            // Only use frames that:
            // 1. Have no decode errors
            // 2. Are keyframes (in relaxed mode, for corrupted videos)
            // 3. Are closer to target than current best
            let should_use = if !has_errors {
                if relaxed_limits {
                    // Relaxed mode (corrupted video): ONLY use clean keyframes
                    is_keyframe && distance < best_frame_distance
                } else {
                    // Normal mode: use any clean frame
                    distance < best_frame_distance
                }
            } else {
                false
            };

            if should_use {
                // Calculate scaled dimensions based on fit mode
                let (scale_w, scale_h) = match fit_mode {
                    FitMode::Stretch => {
                        // Stretch: scale to exact rect dimensions (may distort)
                        (target_w, target_h)
                    }
                    FitMode::Cover | FitMode::Contain => {
                        // Cover/Contain: maintain aspect ratio, scale to cover target
                        let target_aspect = target_w as f64 / target_h as f64;
                        if video_aspect > target_aspect {
                            // Source is wider: fit to height
                            let h = target_h;
                            let w = (h as f64 * video_aspect).round() as u32;
                            (w, h)
                        } else {
                            // Source is taller: fit to width
                            let w = target_w;
                            let h = (w as f64 / video_aspect).round() as u32;
                            (w, h)
                        }
                    }
                };

                // Convert to RGB and scale
                let mut rgb = FfmpegFrame::empty();
                let mut scaler = Scaler::get(
                    decoded.format(),
                    decoded.width(),
                    decoded.height(),
                    ffmpeg::format::pixel::Pixel::RGB24,
                    scale_w,
                    scale_h,
                    Flags::BILINEAR,
                )?;
                scaler.run(&decoded, &mut rgb)?;

                // Important: FFmpeg might add padding to each line for alignment
                // We need to copy line-by-line respecting the stride
                let width = rgb.width() as usize;
                let height = rgb.height() as usize;
                let stride = rgb.stride(0);
                let data = rgb.data(0);

                // Copy without padding
                let mut rgb_data = Vec::with_capacity(width * height * 3);
                for y in 0..height {
                    let line_start = y * stride;
                    let line_end = line_start + (width * 3);
                    rgb_data.extend_from_slice(&data[line_start..line_end]);
                }

                let img = image::RgbImage::from_raw(width as u32, height as u32, rgb_data)
                    .ok_or_else(|| anyhow::anyhow!("failed to create image from raw data"))?;

                best_frame = Some(DynamicImage::ImageRgb8(img).to_rgba8().into());
                best_frame_timestamp = frame_seconds;
                best_frame_distance = distance;
            }

            // If we've gone past the target by more than 2 seconds, stop
            if frame_seconds > timestamp + 2.0 {
                break;
            }

            // Hard limit: stop after decoding too many frames to avoid infinite loops
            if frames_decoded >= 1000 {
                break;
            }
        }

        // Early exit optimization: stop after a few frames if we found something close
        // Use relaxed limits for retry attempts on corrupted videos
        if best_frame.is_some() {
            if relaxed_limits {
                // Corrupted video retry: only exit if very close to target
                if best_frame_distance < 0.1 {
                    break;
                }
            } else {
                // Normal case: exit after 5 frames or when close enough
                if frames_decoded >= 5 || best_frame_distance < 0.5 {
                    break;
                }
            }
        }
    }

    best_frame
        .map(|f| (f, best_frame_timestamp, had_errors_in_group))
        .ok_or_else(|| anyhow::anyhow!("no frames found"))
}

/// derive grid output size when only width or only height provided
fn derive_output_size_grid(width: Option<u32>, height: Option<u32>, video_aspect: f64, rows: usize, cols: usize, spacing: u32, margin: u32) -> (u32,u32) {
    match (width, height) {
        (Some(w), Some(h)) => (w,h),
        (Some(w), None) => {
            // Calculate height based on width, accounting for spacing and margin
            // Total spacing and margin consumed
            let total_spacing_x = (cols.saturating_sub(1) as u32) * spacing;
            let total_spacing_y = (rows.saturating_sub(1) as u32) * spacing;
            let avail_w = w.saturating_sub(2 * margin + total_spacing_x);

            // Calculate cell width and height to maintain video aspect ratio
            let cell_w = if cols > 0 { avail_w / cols as u32 } else { 0 };
            let cell_h = ((cell_w as f64) / video_aspect).round() as u32;

            // Total height including all cells, spacing, and margin
            let h = (rows as u32 * cell_h) + total_spacing_y + (2 * margin);
            (w, h.max(1))
        }
        (None, Some(h)) => {
            // Calculate width based on height, accounting for spacing and margin
            let total_spacing_x = (cols.saturating_sub(1) as u32) * spacing;
            let total_spacing_y = (rows.saturating_sub(1) as u32) * spacing;
            let avail_h = h.saturating_sub(2 * margin + total_spacing_y);

            // Calculate cell height and width to maintain video aspect ratio
            let cell_h = if rows > 0 { avail_h / rows as u32 } else { 0 };
            let cell_w = ((cell_h as f64) * video_aspect).round() as u32;

            // Total width including all cells, spacing, and margin
            let w = (cols as u32 * cell_w) + total_spacing_x + (2 * margin);
            (w.max(1), h)
        }
        (None, None) => (1280, 720)
    }
}

fn derive_output_size_adv(width: Option<u32>, height: Option<u32>, video_aspect: f64) -> (u32,u32) {
    match (width, height) {
        (Some(w), Some(h)) => (w,h),
        (Some(w), None) => (w, (w as f64 / video_aspect).round() as u32),
        (None, Some(h)) => ((h as f64 * video_aspect).round() as u32, h),
        (None, None) => (1280, 720)
    }
}

fn compute_grid_layout(rows: usize, cols: usize, spacing: u32, margin: u32, out_w: u32, out_h: u32) -> Vec<Rect> {
    let total_spacing_x = (cols.saturating_sub(1) as u32) * spacing;
    let total_spacing_y = (rows.saturating_sub(1) as u32) * spacing;
    let avail_w = out_w.saturating_sub(2 * margin + total_spacing_x);
    let avail_h = out_h.saturating_sub(2 * margin + total_spacing_y);
    let cell_w = if cols > 0 { avail_w / cols as u32 } else { 0 };
    let cell_h = if rows > 0 { avail_h / rows as u32 } else { 0 };

    // Calculate remainder pixels from integer division
    let remainder_w = avail_w - (cell_w * cols as u32);
    let remainder_h = avail_h - (cell_h * rows as u32);

    let mut rects = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let x = margin + c as u32 * (cell_w + spacing);
            let y = margin + r as u32 * (cell_h + spacing);

            // Add remainder pixels to last column/row to fill the space exactly
            let w = if c == cols - 1 { cell_w + remainder_w } else { cell_w };
            let h = if r == rows - 1 { cell_h + remainder_h } else { cell_h };

            rects.push(Rect { x, y, w, h });
        }
    }
    rects
}

fn compute_advanced_layout(pieces: &[PieceSpec], margin: u32, spacing: u32, out_w: u32, out_h: u32) -> Vec<Rect> {
    let mut rects = Vec::with_capacity(pieces.len());
    for p in pieces {
        let mut x = (p.x_percent * out_w as f32).round() as i32;
        let mut y = (p.y_percent * out_h as f32).round() as i32;
        let mut w = (p.w_percent * out_w as f32).round() as i32;
        let mut h = (p.h_percent * out_h as f32).round() as i32;

        x += margin as i32;
        y += margin as i32;
        w -= 2 * margin as i32;
        h -= 2 * margin as i32;

        if spacing > 0 {
            let s = spacing as i32 / 2;
            x += s; y += s; w -= spacing as i32; h -= spacing as i32;
        }

        if w <= 0 || h <= 0 {
            rects.push(Rect { x: 0, y: 0, w: 0, h: 0 });
        } else {
            rects.push(Rect { x: x.max(0) as u32, y: y.max(0) as u32, w: w as u32, h: h as u32 });
        }
    }
    rects
}

#[derive(Debug, Clone)]
struct FittedRect { src_rect: Rect, dst_rect: Rect }

fn fit_frame_to_rect(frame_w: u32, frame_h: u32, rect: Rect, mode: FitMode) -> FittedRect {
    let src_aspect = frame_w as f64 / frame_h as f64;
    let dst_aspect = rect.w as f64 / rect.h as f64;
    match mode {
        FitMode::Contain => {
            if src_aspect > dst_aspect {
                let new_h = (rect.w as f64 / src_aspect).round() as u32;
                let y_off = rect.y + (rect.h.saturating_sub(new_h) / 2);
                FittedRect { src_rect: Rect { x: 0, y: 0, w: frame_w, h: frame_h }, dst_rect: Rect { x: rect.x, y: y_off, w: rect.w, h: new_h } }
            } else {
                let new_w = (rect.h as f64 * src_aspect).round() as u32;
                let x_off = rect.x + (rect.w.saturating_sub(new_w) / 2);
                FittedRect { src_rect: Rect { x: 0, y: 0, w: frame_w, h: frame_h }, dst_rect: Rect { x: x_off, y: rect.y, w: new_w, h: rect.h } }
            }
        }
        FitMode::Cover => {
            // Cover mode: scale to fill rect completely, crop the overflow, center the crop
            if src_aspect > dst_aspect {
                // Source is wider: fit to height (full height), crop width
                let crop_h = frame_h;
                let crop_w = (crop_h as f64 * dst_aspect).round() as u32;
                let x_crop = frame_w.saturating_sub(crop_w) / 2;
                FittedRect { src_rect: Rect { x: x_crop, y: 0, w: crop_w, h: crop_h }, dst_rect: rect }
            } else {
                // Source is taller: fit to width (full width), crop height
                let crop_w = frame_w;
                let crop_h = (crop_w as f64 / dst_aspect).round() as u32;
                let y_crop = frame_h.saturating_sub(crop_h) / 2;
                FittedRect { src_rect: Rect { x: 0, y: y_crop, w: crop_w, h: crop_h }, dst_rect: rect }
            }
        }
        FitMode::Stretch => {
            // Stretch mode: use entire frame, stretch to fill rect (may distort)
            FittedRect { src_rect: Rect { x: 0, y: 0, w: frame_w, h: frame_h }, dst_rect: rect }
        }
    }
}

fn rgba_from_spec(s: &RgbaSpec) -> Rgba<u8> { Rgba([s.r, s.g, s.b, s.a]) }

fn format_timecode(seconds: f64, video_duration: f64) -> String {
    let s = seconds.round() as u64;
    let hh = s / 3600;
    let mm = (s % 3600) / 60;
    let ss = s % 60;

    // Show hours if video is >= 1 hour
    if video_duration >= 3600.0 {
        format!("{:02}:{:02}:{:02}", hh, mm, ss)
    } else {
        format!("{:02}:{:02}", mm, ss)
    }
}
