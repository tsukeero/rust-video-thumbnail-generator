//! `POST /scenes`: scene cut detection, optionally picking frames from the scenes in the same pass.
//!
//! The `content` detector, the adaptive layer and the flash filter are ports of PySceneDetect's
//! `ContentDetector`, `AdaptiveDetector` and `FlashFilter` (PySceneDetect 0.7.3, BSD-3-Clause,
//! Copyright (C) 2014, Brandon Castellano, https://github.com/Breakthrough/PySceneDetect; see
//! PYSCENEDETECT-LICENSE.txt).
//! The `scdet` detector reimplements the score of FFmpeg's `scdet` filter.

use std::time::Instant;

use anyhow::Result;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;
use ffmpeg_next::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next::util::frame::video::Video as FfmpegFrame;

use crate::scene_frames::{self, Collector, Pick};
use crate::{bad_request, deliver_frames, encode_frame, extract, finish_frame, frames_target, probe_dimensions,
    probe_duration_opt, resolve_paths, AppState, Timeline, EncodedFrame, ImageFormat, Prepared, ProgressTracker, Unprocessable};

// ---------------------------------------------------------------------------------------------
// Request

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScenesRequest {
    pub path: String,
    #[serde(default)]
    pub hint: Option<Hint>,
    #[serde(default)]
    pub detector: Option<Detector>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub min_scene_len: Option<f64>,
    #[serde(default)]
    pub flash_filter: Option<FlashMode>,
    #[serde(default)]
    pub weights: Option<Weights>,
    /// Absent: from the hint. `null`: off. An object: on, missing fields from the defaults.
    #[serde(default, deserialize_with = "present")]
    pub adaptive: Option<Option<AdaptiveSpec>>,
    #[serde(default)]
    pub frames: Option<crate::scene_frames::SceneFramesSpec>,
    #[serde(default)]
    pub skip_blank_frames: bool,
    #[serde(default)]
    pub return_scores: bool,
    #[serde(default)]
    pub job_id: Option<String>,
}

/// Tells an explicit `null` (Some(None)) apart from an absent field (None)
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Hint { Anime, Cg, Real }

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Detector { Keyframes, Scdet, Content }

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FlashMode { Merge, Suppress, Off }

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Weights {
    #[serde(default = "one")]
    pub hue: f64,
    #[serde(default = "one")]
    pub sat: f64,
    #[serde(default = "one")]
    pub lum: f64,
}

fn one() -> f64 { 1.0 }

impl Default for Weights {
    fn default() -> Self { Weights { hue: 1.0, sat: 1.0, lum: 1.0 } }
}

#[derive(serde::Deserialize, Debug, Clone, Copy, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdaptiveSpec {
    #[serde(default)]
    pub ratio: Option<f64>,
    #[serde(default)]
    pub window: Option<usize>,
    #[serde(default)]
    pub min_score: Option<f64>,
}

#[derive(serde::Serialize, Debug, Clone, Copy, PartialEq)]
pub(crate) struct Adaptive {
    pub ratio: f64,
    pub window: usize,
    pub min_score: f64,
}

pub(crate) const DEFAULT_MIN_SCENE_LEN: f64 = 0.6;
const DEFAULT_THRESHOLD_CONTENT: f64 = 27.0;
const DEFAULT_THRESHOLD_SCDET: f64 = 8.0;
const DEFAULT_ADAPTIVE_RATIO: f64 = 3.0;
const DEFAULT_ADAPTIVE_WINDOW: usize = 2;
const DEFAULT_MIN_SCORE_CONTENT: f64 = 15.0;
const DEFAULT_MIN_SCORE_SCDET: f64 = 4.0;
/// Width of the copy of each frame that detection works on (like PySceneDetect's auto-downscale)
pub(crate) const ANALYSIS_WIDTH: u32 = 256;
/// `keyframes` refuses a stream where more than this share of video packets are keyframes
const MAX_KEYFRAME_SHARE: f64 = 0.5;
/// A decode pass that yields fewer frames than this share of the video packets read fails.
/// (Packets, not duration x frame rate: AVI's `avg_frame_rate` and `nb_frames` can be off by 5x.)
const MIN_DECODED_SHARE: f64 = 0.5;

/// The settings detection runs with: the hint's defaults merged under the explicit fields
#[derive(serde::Serialize, Debug, Clone)]
pub(crate) struct Resolved {
    pub hint: Option<Hint>,
    pub detector: Detector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    pub min_scene_len: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flash_filter: Option<FlashMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weights: Option<Weights>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<Adaptive>,
}

/// Merge the hint's defaults under the explicit fields and refuse settings the detector
/// doesn't use, so a mistaken setting never silently does nothing.
pub(crate) fn resolve(req: &ScenesRequest) -> Result<Resolved> {
    let detector = req.detector.unwrap_or(Detector::Content);
    let hint_adaptive = matches!(req.hint, Some(Hint::Cg) | Some(Hint::Real));

    let min_scene_len = req.min_scene_len.unwrap_or(DEFAULT_MIN_SCENE_LEN);
    if !(min_scene_len >= 0.0 && min_scene_len.is_finite()) {
        return bad_request("min_scene_len must be 0 or more seconds");
    }
    if let Some(t) = req.threshold {
        if !(t > 0.0 && t.is_finite()) { return bad_request("threshold must be above 0"); }
    }

    if detector == Detector::Keyframes {
        if req.threshold.is_some() { return bad_request("threshold is not used by the keyframes detector"); }
        if req.weights.is_some() { return bad_request("weights is not used by the keyframes detector"); }
        if req.flash_filter.is_some() { return bad_request("flash_filter is not used by the keyframes detector"); }
        if matches!(req.adaptive, Some(Some(_))) { return bad_request("adaptive is not used by the keyframes detector"); }
        return Ok(Resolved { hint: req.hint, detector, threshold: None, min_scene_len, flash_filter: None, weights: None, adaptive: None });
    }

    if detector == Detector::Scdet && req.weights.is_some() {
        return bad_request("weights is only used by the content detector");
    }
    if let Some(w) = req.weights {
        let all = [w.hue, w.sat, w.lum];
        if all.iter().any(|v| !v.is_finite() || *v < 0.0) || all.iter().sum::<f64>() <= 0.0 {
            return bad_request("weights must be 0 or more, and not all 0");
        }
    }

    let spec = match req.adaptive {
        Some(None) => None,
        Some(Some(spec)) => Some(spec),
        None if hint_adaptive => Some(AdaptiveSpec::default()),
        None => None,
    };
    let adaptive = match spec {
        None => None,
        Some(spec) => {
            if req.threshold.is_some() {
                return bad_request("threshold can't be combined with adaptive (use adaptive.min_score, or adaptive: null)");
            }
            let a = Adaptive {
                ratio: spec.ratio.unwrap_or(DEFAULT_ADAPTIVE_RATIO),
                window: spec.window.unwrap_or(DEFAULT_ADAPTIVE_WINDOW),
                min_score: spec.min_score.unwrap_or(match detector {
                    Detector::Scdet => DEFAULT_MIN_SCORE_SCDET,
                    _ => DEFAULT_MIN_SCORE_CONTENT,
                }),
            };
            if !(a.ratio > 0.0 && a.ratio.is_finite()) { return bad_request("adaptive.ratio must be above 0"); }
            if a.window < 1 { return bad_request("adaptive.window must be at least 1"); }
            if !(a.min_score >= 0.0 && a.min_score.is_finite()) { return bad_request("adaptive.min_score must be 0 or more"); }
            Some(a)
        }
    };
    let threshold = match (&adaptive, detector) {
        (Some(_), _) => None,
        (None, Detector::Scdet) => Some(req.threshold.unwrap_or(DEFAULT_THRESHOLD_SCDET)),
        (None, _) => Some(req.threshold.unwrap_or(DEFAULT_THRESHOLD_CONTENT)),
    };
    let weights = match detector {
        Detector::Content => Some(req.weights.unwrap_or_default()),
        _ => None,
    };
    Ok(Resolved {
        hint: req.hint,
        detector,
        threshold,
        min_scene_len,
        flash_filter: Some(req.flash_filter.unwrap_or(FlashMode::Merge)),
        weights,
        adaptive,
    })
}

// ---------------------------------------------------------------------------------------------
// Scoring

/// FFmpeg `scdet`'s score at the analysis size. On planar YUV, FFmpeg's `scdet` compares the
/// luma plane only (measured: a pure chroma change scores 0), so only Y is passed in.
pub(crate) struct Scdet {
    prev: Option<Vec<u8>>,
    prev_mafd: f64,
}

impl Scdet {
    pub fn new() -> Self { Scdet { prev: None, prev_mafd: 0.0 } }

    /// `luma` is the Y plane packed without padding. The first frame scores 0.
    pub fn score(&mut self, luma: Vec<u8>) -> f64 {
        let score = match &self.prev {
            Some(prev) if prev.len() == luma.len() && !luma.is_empty() => {
                let sad: u64 = prev.iter().zip(&luma).map(|(a, b)| a.abs_diff(*b) as u64).sum();
                let mafd = sad as f64 * 100.0 / luma.len() as f64 / 256.0;
                let diff = (mafd - self.prev_mafd).abs();
                self.prev_mafd = mafd;
                mafd.min(diff).clamp(0.0, 100.0)
            }
            _ => 0.0,
        };
        self.prev = Some(luma);
        score
    }
}

/// OpenCV's 8-bit BGR2HSV (hue 0..180), fixed-point, so scores match PySceneDetect's
pub(crate) struct HsvTables { sdiv: [i32; 256], hdiv: [i32; 256] }

const HSV_SHIFT: i32 = 12;

impl HsvTables {
    pub fn new() -> Self {
        let mut sdiv = [0i32; 256];
        let mut hdiv = [0i32; 256];
        for i in 1..256 {
            sdiv[i] = ((255 << HSV_SHIFT) as f64 / i as f64).round() as i32;
            hdiv[i] = ((180 << HSV_SHIFT) as f64 / (6.0 * i as f64)).round() as i32;
        }
        HsvTables { sdiv, hdiv }
    }

    #[inline]
    pub fn hsv(&self, r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let (r, g, b) = (r as i32, g as i32, b as i32);
        let v = r.max(g).max(b);
        let vmin = r.min(g).min(b);
        let diff = v - vmin;
        let vr = if v == r { -1 } else { 0 };
        let vg = if v == g { -1 } else { 0 };
        let s = (diff * self.sdiv[v as usize] + (1 << (HSV_SHIFT - 1))) >> HSV_SHIFT;
        let mut h = (vr & (g - b)) + (!vr & ((vg & (b - r + 2 * diff)) + (!vg & (r - g + 4 * diff))));
        h = (h * self.hdiv[diff as usize] + (1 << (HSV_SHIFT - 1))) >> HSV_SHIFT;
        if h < 0 { h += 180; }
        (h as u8, s as u8, v as u8)
    }
}

/// PySceneDetect `ContentDetector`'s `content_val` on RGB24 frames at the analysis size
pub(crate) struct Content {
    tables: HsvTables,
    weights: Weights,
    /// Previous frame's H, S, V, interleaved (reused between frames)
    prev: Vec<u8>,
    cur: Vec<u8>,
    has_prev: bool,
}

impl Content {
    pub fn new(weights: Weights) -> Self {
        Content { tables: HsvTables::new(), weights, prev: Vec::new(), cur: Vec::new(), has_prev: false }
    }

    /// `rgb` is packed RGB24 without padding. The first frame scores 0.
    pub fn score(&mut self, rgb: &[u8]) -> f64 {
        let n = rgb.len() / 3;
        self.cur.clear();
        self.cur.reserve(n * 3);
        for px in rgb.chunks_exact(3) {
            let (h, s, v) = self.tables.hsv(px[0], px[1], px[2]);
            self.cur.extend_from_slice(&[h, s, v]);
        }
        let comparable = self.has_prev && self.prev.len() == self.cur.len() && n > 0;
        let score = if comparable {
            let (mut dh, mut ds, mut dl) = (0u64, 0u64, 0u64);
            for (a, b) in self.prev.chunks_exact(3).zip(self.cur.chunks_exact(3)) {
                dh += a[0].abs_diff(b[0]) as u64;
                ds += a[1].abs_diff(b[1]) as u64;
                dl += a[2].abs_diff(b[2]) as u64;
            }
            let n = n as f64;
            let w = self.weights;
            (dh as f64 / n * w.hue + ds as f64 / n * w.sat + dl as f64 / n * w.lum) / (w.hue.abs() + w.sat.abs() + w.lum.abs())
        } else {
            0.0
        };
        std::mem::swap(&mut self.prev, &mut self.cur);
        self.has_prev = true;
        score
    }
}

// ---------------------------------------------------------------------------------------------
// Cut test, adaptive layer and flash filter

/// PySceneDetect's `FlashFilter`, with the length in seconds
pub(crate) struct FlashFilter {
    mode: FlashMode,
    len: f64,
    last_above: Option<f64>,
    merge_enabled: bool,
    merge_triggered: bool,
    merge_start: f64,
}

impl FlashFilter {
    pub fn new(mode: FlashMode, len: f64) -> Self {
        FlashFilter { mode, len, last_above: None, merge_enabled: false, merge_triggered: false, merge_start: 0.0 }
    }

    fn met(&self, from: f64, to: f64) -> bool { to - from >= self.len - 1e-9 }

    /// Feed one frame in time order: whether it passed the cut test. Returns a cut, if any.
    pub fn filter(&mut self, t: f64, above: bool) -> Option<f64> {
        if self.len <= 0.0 {
            return above.then_some(t);
        }
        let last = *self.last_above.get_or_insert(t);
        let min_length_met = self.met(last, t);
        match self.mode {
            // `off` only disables merging: the minimum length still applies as in suppress
            FlashMode::Suppress | FlashMode::Off => {
                if above && min_length_met {
                    self.last_above = Some(t);
                    Some(t)
                } else {
                    None
                }
            }
            FlashMode::Merge => {
                if above {
                    self.last_above = Some(t);
                }
                let last_above = self.last_above.unwrap_or(t);
                if self.merge_triggered {
                    if min_length_met && !above && self.met(self.merge_start, last_above) {
                        self.merge_triggered = false;
                        return Some(last_above);
                    }
                    return None;
                }
                if !above {
                    return None;
                }
                if min_length_met {
                    self.merge_enabled = true;
                    return Some(t);
                }
                if self.merge_enabled {
                    self.merge_triggered = true;
                    self.merge_start = t;
                }
                None
            }
        }
    }
}

/// Turns the score series into cuts: the cut test (threshold, or the adaptive ratio test)
/// followed by the flash filter. Frames go in in time order; errored frames are left out.
pub(crate) struct CutPipeline {
    threshold: f64,
    adaptive: Option<Adaptive>,
    /// (time, score) of the last 2 * window + 1 frames, for the adaptive ratio
    window_buf: std::collections::VecDeque<(f64, f64)>,
    frames_seen: usize,
    flash: FlashFilter,
    /// Time of the last frame given to the flash filter
    last_judged: Option<f64>,
    pub cuts: Vec<f64>,
    pub scores: Option<ScoreLog>,
}

#[derive(serde::Serialize, Debug, Default)]
pub(crate) struct ScoreLog {
    pub t: Vec<f64>,
    pub score: Vec<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratio: Option<Vec<f64>>,
}

impl CutPipeline {
    pub fn new(cfg: &Resolved, keep_scores: bool) -> Self {
        CutPipeline {
            threshold: cfg.threshold.unwrap_or(f64::INFINITY),
            adaptive: cfg.adaptive,
            window_buf: std::collections::VecDeque::new(),
            frames_seen: 0,
            flash: FlashFilter::new(cfg.flash_filter.unwrap_or(FlashMode::Merge), cfg.min_scene_len),
            last_judged: None,
            cuts: Vec::new(),
            scores: keep_scores.then(|| ScoreLog { ratio: cfg.adaptive.map(|_| Vec::new()), ..Default::default() }),
        }
    }

    /// Every cut not emitted yet will be at or after this time: frames before it are settled
    /// in their scene. Lags the newest frame by the adaptive window, and in merge mode by a
    /// pending merge whose span already reached the minimum length: its cut comes out at the
    /// last frame above the test, at most `min_scene_len` later. A merge still short of that
    /// can only end at a later frame above the test, so it holds nothing back (otherwise a long
    /// quiet scene after one would keep every candidate).
    pub fn settled_before(&self) -> f64 {
        let judged = self.last_judged.map_or(f64::NEG_INFINITY, |t| t + 1e-9);
        let f = &self.flash;
        match (f.merge_triggered, f.last_above) {
            (true, Some(last_above)) if f.met(f.merge_start, last_above) => judged.min(last_above),
            _ => judged,
        }
    }

    fn emit(&mut self, t: f64, above: bool) {
        self.last_judged = Some(t);
        if let Some(cut) = self.flash.filter(t, above) {
            self.cuts.push(cut);
        }
    }

    pub fn push(&mut self, t: f64, score: f64) {
        if let Some(log) = &mut self.scores {
            log.t.push(t);
            log.score.push(score);
            if let Some(r) = &mut log.ratio { r.push(0.0); }
        }
        let Some(a) = self.adaptive else {
            let above = score >= self.threshold;
            self.emit(t, above);
            return;
        };
        // Adaptive: the frame `window` back is judged against the frames on both sides of it.
        // The first `window` frames can't be judged and count as below the test.
        let index = self.frames_seen;
        self.frames_seen += 1;
        if index < a.window {
            self.emit(t, false);
        }
        self.window_buf.push_back((t, score));
        let need = 2 * a.window + 1;
        if self.window_buf.len() > need {
            self.window_buf.pop_front();
        }
        if self.window_buf.len() < need {
            return;
        }
        let (target_t, target_score) = self.window_buf[a.window];
        let mean = self.window_buf.iter().enumerate()
            .filter(|(i, _)| *i != a.window)
            .map(|(_, (_, s))| *s)
            .sum::<f64>() / (2.0 * a.window as f64);
        let ratio = if mean.abs() >= 1e-5 {
            (target_score / mean).min(255.0)
        } else if target_score >= a.min_score {
            255.0
        } else {
            0.0
        };
        if let Some(log) = &mut self.scores {
            if let Some(r) = &mut log.ratio {
                let i = r.len() - 1 - a.window;
                r[i] = ratio;
            }
        }
        let above = ratio >= a.ratio && target_score >= a.min_score;
        self.emit(target_t, above);
    }

    /// End of the stream: the last `window` frames can't be judged and count as below the test
    pub fn finish(&mut self) {
        if let Some(a) = self.adaptive {
            let pending: Vec<f64> = if self.window_buf.len() == 2 * a.window + 1 {
                self.window_buf.iter().skip(a.window + 1).map(|(t, _)| *t).collect()
            } else {
                // Fewer frames than one window: the ones not yet passed to the filter
                self.window_buf.iter().skip(a.window).map(|(t, _)| *t).collect()
            };
            for t in pending {
                self.emit(t, false);
            }
        }
    }
}

/// Scene list from cut times: `[0, c1), [c1, c2), ... [ck, duration)`
pub(crate) fn scenes_from_cuts(cuts: &[f64], duration: f64) -> Vec<(f64, f64)> {
    let mut starts = vec![0.0];
    for &c in cuts {
        if c > *starts.last().unwrap() && c < duration {
            starts.push(c);
        }
    }
    let mut scenes = Vec::with_capacity(starts.len());
    for (i, &s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(duration);
        scenes.push((s, e));
    }
    scenes
}

// ---------------------------------------------------------------------------------------------
// Detection passes

#[derive(serde::Serialize, Debug, Default)]
pub(crate) struct Stats {
    pub decoded_frames: usize,
    pub errored_frames: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyframes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trimmed_frames: Option<usize>,
    pub elapsed_s: f64,
}

pub(crate) struct Detection {
    pub cuts: Vec<f64>,
    pub scores: Option<ScoreLog>,
    pub stats: Stats,
    /// Where the pass saw the video end: the last frame's (or packet's) time plus its duration
    pub end: f64,
}

/// The duration to report: the probed one, else where the pass saw the video end (streams
/// written to a pipe, raw streams, truncated files). The pass end doesn't override a probed
/// duration: a timestamp jump (DVD cells, spliced TS) can put it hours past the real end.
pub(crate) fn resolve_duration(probed: Option<f64>, end: f64) -> Result<f64> {
    let d = probed.unwrap_or(end);
    if d > 0.0 { Ok(d) } else { anyhow::bail!("could not determine duration: no timestamps in the video") }
}

/// A pass's progress: time over the probed duration, else bytes read over the file size
struct PassProgress {
    duration: Option<f64>,
    size: Option<f64>,
}

impl PassProgress {
    fn new(path: &str, duration: Option<f64>) -> Self {
        let size = std::fs::metadata(path).ok().map(|m| m.len() as f64).filter(|s| *s > 0.0);
        PassProgress { duration: duration.filter(|d| *d > 0.0), size }
    }

    fn fraction(&self, t: f64, pos: isize) -> f64 {
        match (self.duration, self.size) {
            (Some(d), _) => t / d,
            (None, Some(s)) if pos >= 0 => pos as f64 / s,
            _ => 0.0,
        }
        .clamp(0.0, 1.0)
    }
}

/// Seconds per frame from the stream's frame rate (average, else real base), else 25 fps
fn frame_interval(vs: &ffmpeg::format::stream::Stream) -> f64 {
    for r in [vs.avg_frame_rate(), vs.rate()] {
        if r.0 > 0 && r.1 > 0 {
            return r.1 as f64 / r.0 as f64;
        }
    }
    1.0 / 25.0
}

/// `keyframes`: a cut at every keyframe packet, from packet flags, without decoding.
/// `min_scene_len` applies as in suppress mode.
pub(crate) fn detect_keyframes(video_path: &str, cfg: &Resolved, duration: Option<f64>, tracker: &mut ProgressTracker) -> Result<Detection> {
    let started = Instant::now();
    let mut ictx = ffmpeg::format::input(&video_path)?;
    let vs = ictx.streams().best(ffmpeg::media::Type::Video).ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = vs.index();
    let tl = Timeline::of(&ictx, &vs);
    let interval = frame_interval(&vs);
    let progress = PassProgress::new(video_path, duration);

    let mut packets = 0usize;
    let mut keys: Vec<f64> = Vec::new();
    let mut end = 0.0f64;
    let mut last_t: Option<f64> = None;
    for (s, packet) in ictx.packets() {
        if s.index() != stream_index { continue; }
        packets += 1;
        // No timestamp (raw elementary streams): one frame after the previous packet
        let t = match packet.pts().or(packet.dts()) {
            Some(ts) => tl.secs(ts),
            None => last_t.map_or(0.0, |l| l + interval),
        };
        last_t = Some(t);
        let len = if packet.duration() > 0 { tl.len(packet.duration()) } else { interval };
        end = end.max(t + len);
        if packet.is_key() {
            keys.push(t);
            tracker.update(progress.fraction(t, packet.position()), "Detecting scenes".to_string());
        }
    }
    if packets > 0 && keys.len() as f64 / packets as f64 > MAX_KEYFRAME_SHARE {
        return Err(Unprocessable(format!(
            "{} of {} video packets are keyframes (an all-intra codec, or an index that flags every packet), so they don't mark cuts; use the scdet or content detector",
            keys.len(), packets
        )).into());
    }
    keys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut flash = FlashFilter::new(FlashMode::Suppress, cfg.min_scene_len);
    let cuts: Vec<f64> = keys.iter().filter_map(|&t| flash.filter(t, true)).collect();
    Ok(Detection {
        cuts,
        scores: None,
        stats: Stats { keyframes: Some(keys.len()), elapsed_s: started.elapsed().as_secs_f64(), ..Default::default() },
        end,
    })
}

/// A decoded, error-free frame handed to the frame sink during a decode pass
pub(crate) struct PassFrame<'a> {
    pub frame: &'a FfmpegFrame,
    pub t: f64,
}

/// `scdet` and `content`: one sequential pass decoding every frame (GENPTS, as Tier 3 does,
/// which RealVideo needs), scoring each at the analysis size. Errored frames get no score.
/// `sink` sees every scored frame after its score went into the pipeline, so the cuts so far
/// are final up to the flash filter's delay.
pub(crate) fn detect_decoded(
    video_path: &str,
    cfg: &Resolved,
    duration: Option<f64>,
    keep_scores: bool,
    tracker: &mut ProgressTracker,
    mut sink: impl FnMut(PassFrame, &CutPipeline) -> Result<()>,
) -> Result<Detection> {
    let started = Instant::now();
    let mut ictx = ffmpeg::format::input(&video_path)?;
    unsafe {
        (*ictx.as_mut_ptr()).flags |= ffi::AVFMT_FLAG_GENPTS as i32;
    }
    let vs = ictx.streams().best(ffmpeg::media::Type::Video).ok_or_else(|| anyhow::anyhow!("no video stream"))?;
    let stream_index = vs.index();
    let tl = Timeline::of(&ictx, &vs);
    let interval = frame_interval(&vs);
    let progress = PassProgress::new(video_path, duration);
    // The previous frame's time, for frames without any timestamp, and the latest end seen
    let mut last_t: Option<f64> = None;
    let mut end = 0.0f64;

    let mut codec_ctx = ffmpeg::codec::context::Context::from_parameters(vs.parameters())?;
    codec_ctx.set_threading(ffmpeg::codec::threading::Config { kind: ffmpeg::codec::threading::Type::Frame, count: 0, ..Default::default() });
    let mut decoder = codec_ctx.decoder().video()?;
    unsafe {
        (*decoder.as_mut_ptr()).flags |= ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as i32;
    }
    // Suppress FFmpeg log spam for corrupted videos (as the sequential extractor does)
    ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Fatal);

    let (analysis_fmt, flags) = match cfg.detector {
        // FFmpeg's scale filter default, so `-vf scale=256:-2,scdet` compares directly
        Detector::Scdet => (ffmpeg::format::Pixel::YUV420P, Flags::BICUBIC),
        // Scaled in YUV first, then converted at the small size: swscale's direct
        // scale-to-RGB24 path costs about 4x the HSV scoring itself
        _ => (ffmpeg::format::Pixel::YUV444P, Flags::BILINEAR),
    };
    let mut to_rgb: Option<Scaler> = None;
    let mut rgb = FfmpegFrame::empty();
    let mut scdet = Scdet::new();
    let mut content = Content::new(cfg.weights.unwrap_or_default());
    let mut pipeline = CutPipeline::new(cfg, keep_scores);
    let mut scaler: Option<(Scaler, (ffmpeg::format::Pixel, u32, u32))> = None;
    let mut small = FfmpegFrame::empty();
    let mut stats = Stats::default();

    let mut handle = |decoded: &FfmpegFrame, packet_dts: Option<i64>, pos: isize, stats: &mut Stats, pipeline: &mut CutPipeline,
                      scaler: &mut Option<(Scaler, (ffmpeg::format::Pixel, u32, u32))>| -> Result<()> {
        stats.decoded_frames += 1;
        // No timestamp at all (GENPTS couldn't make one): one frame after the previous
        let t = match decoded.timestamp().or(packet_dts) {
            Some(ts) => tl.secs(ts),
            None => last_t.map_or(0.0, |l| l + interval),
        };
        last_t = Some(t);
        end = end.max(t + interval);
        let has_errors = unsafe { (*decoded.as_ptr()).decode_error_flags != 0 };
        if has_errors {
            stats.errored_frames += 1;
            return Ok(());
        }
        let key = (decoded.format(), decoded.width(), decoded.height());
        if scaler.as_ref().map(|(_, k)| *k != key).unwrap_or(true) {
            let (w, h) = analysis_size(decoded.width(), decoded.height());
            *scaler = Some((Scaler::get(key.0, key.1, key.2, analysis_fmt, w, h, flags)?, key));
        }
        scaler.as_mut().unwrap().0.run(decoded, &mut small)?;
        let score = match cfg.detector {
            Detector::Scdet => scdet.score(packed_plane(&small, 0, small.width() as usize, small.height() as usize)),
            _ => {
                if to_rgb.as_ref().map(|s| s.input().width != small.width() || s.input().height != small.height()).unwrap_or(true) {
                    to_rgb = Some(Scaler::get(small.format(), small.width(), small.height(), ffmpeg::format::Pixel::RGB24,
                        small.width(), small.height(), Flags::BILINEAR)?);
                }
                to_rgb.as_mut().unwrap().run(&small, &mut rgb)?;
                content.score(&packed_plane(&rgb, 0, rgb.width() as usize * 3, rgb.height() as usize))
            }
        };
        pipeline.push(t, score);
        tracker.update(progress.fraction(t, pos), "Detecting scenes".to_string());
        sink(PassFrame { frame: decoded, t }, pipeline)
    };

    let mut decoded = FfmpegFrame::empty();
    let mut packets = 0usize;
    let mut pos = -1isize;
    for (s, packet) in ictx.packets() {
        if s.index() != stream_index { continue; }
        packets += 1;
        pos = pos.max(packet.position());
        // A packet the decoder refuses (corrupt data) is skipped, as the extractors do
        if decoder.send_packet(&packet).is_err() { continue; }
        while decoder.receive_frame(&mut decoded).is_ok() {
            handle(&decoded, packet.dts(), pos, &mut stats, &mut pipeline, &mut scaler)?;
        }
    }
    // Drain the frames still buffered in the (threaded) decoder
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut decoded).is_ok() {
        handle(&decoded, None, pos, &mut stats, &mut pipeline, &mut scaler)?;
    }
    pipeline.finish();

    if packets == 0 {
        anyhow::bail!("no video packets could be read");
    }
    if (stats.decoded_frames as f64) < packets as f64 * MIN_DECODED_SHARE {
        anyhow::bail!(
            "decoded only {} frames from {} video packets; the video is too damaged for scene detection",
            stats.decoded_frames, packets
        );
    }
    stats.elapsed_s = started.elapsed().as_secs_f64();
    let CutPipeline { cuts, scores, .. } = pipeline;
    Ok(Detection { cuts, scores, stats, end })
}

/// The analysis copy's size: 256 px wide (or the source width if smaller), height from the
/// aspect rounded to an even number, as `scale=256:-2` does
pub(crate) fn analysis_size(w: u32, h: u32) -> (u32, u32) {
    let aw = ANALYSIS_WIDTH.min(w).max(2);
    let ah = ((aw as f64 * h as f64 / w as f64 / 2.0).round() as u32 * 2).max(2);
    (aw, ah)
}

/// Plane `i` of a frame, packed without the stride padding
fn packed_plane(f: &FfmpegFrame, i: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    let stride = f.stride(i);
    let data = f.data(i);
    let mut out = Vec::with_capacity(row_bytes * rows);
    for y in 0..rows {
        out.extend_from_slice(&data[y * stride..y * stride + row_bytes]);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The job

/// Checks that need no video, run before a worker is taken
pub(crate) fn validate_request(req: &ScenesRequest) -> Result<()> {
    resolve(req)?;
    match &req.frames {
        Some(spec) => scene_frames::validate(spec)?,
        None if req.skip_blank_frames => return bad_request("skip_blank_frames applies to frame selection only; it needs frames"),
        None => {}
    }
    Ok(())
}

/// Blocking processing for `POST /scenes`. Returns the JSON reply.
pub(crate) fn process_scenes(req: &ScenesRequest, state: &AppState, job_id: String) -> Result<serde_json::Value> {
    let cfg = resolve(req)?;
    let (video_path, meta_path) = resolve_paths(&req.path, state)?;
    // Not required: the passes read the whole file and see where it ends
    let probed = probe_duration_opt(&video_path)?;
    let mut tracker = ProgressTracker::new(job_id, req.path.clone(), state.progress_tx.clone(), state.progress_interval_ms, 0);
    tracker.force_update(0.0, "Detecting scenes".to_string());

    let spec = req.frames.as_ref();
    let quality = spec.and_then(|s| match s.format {
        ImageFormat::Jpeg => Some(s.quality.unwrap_or(state.jpeg_quality)),
        ImageFormat::Png => None,
    });
    let source = match spec {
        Some(_) => Some(probe_dimensions(&video_path)?),
        None => None,
    };
    let target = match (spec, source) {
        (Some(s), Some((w, h))) => Some(frames_target(&s.size, w, h)),
        _ => None,
    };

    let mut stats;
    let scores;
    // Picks per scene, after trimming
    let mut picks: Vec<Pick> = Vec::new();
    let scenes;
    let duration;
    match cfg.detector {
        Detector::Keyframes => {
            let det = detect_keyframes(&video_path, &cfg, probed, &mut tracker)?;
            duration = resolve_duration(probed, det.end)?;
            scenes = scenes_from_cuts(&det.cuts, duration);
            if let (Some(spec), Some(target), Some((w, h))) = (spec, target, source) {
                // Scenes are known before any decoding: place, trim, then extract by seeking
                let mut counts: Vec<usize> = scenes.iter().map(|(s, e)| scene_frames::wanted(spec, *s, *e)).collect();
                let before: usize = counts.iter().sum();
                if let Some(max) = spec.max_frames {
                    let durations: Vec<f64> = scenes.iter().map(|(s, e)| e - s).collect();
                    scene_frames::trim(&mut counts, &durations, spec.min_per_scene, max);
                }
                let wanted: Vec<(f64, f64)> = scenes.iter().zip(&counts)
                    .flat_map(|((s, e), k)| scene_frames::place(spec, *s, *e, *k).into_iter().map(move |t| (*s, t)))
                    .collect();
                let mut stats_k = det.stats;
                stats_k.trimmed_frames = Some(before - wanted.len());
                if !wanted.is_empty() {
                    let prep = Prepared {
                        video_path: video_path.clone(), meta_path: None, duration, width: w, height: h,
                        timestamps: wanted.iter().map(|(_, t)| *t).collect(),
                    };
                    tracker.force_update(0.0, "Extracting frames".to_string());
                    let ex = extract(&prep, &vec![target; wanted.len()], None, req.skip_blank_frames, state, &mut tracker)?;
                    for ((scene_start, t), frame) in wanted.iter().zip(ex.frames) {
                        if let Some((img, actual)) = frame {
                            let bytes = encode_frame(&finish_frame(&img, target), spec.format, quality)?;
                            picks.push(Pick { scene_start: *scene_start, target: *t, timestamp: actual, bytes });
                        }
                    }
                }
                stats = stats_k;
            } else {
                stats = det.stats;
            }
            scores = None;
        }
        Detector::Scdet | Detector::Content => {
            let mut collector = match (spec, target, source) {
                (Some(spec), Some(target), Some((w, h))) =>
                    Some(Collector::new(spec, target, w as f64 / h as f64, quality, req.skip_blank_frames)),
                _ => None,
            };
            let det = detect_decoded(&video_path, &cfg, probed, req.return_scores, &mut tracker, |f, pipeline| {
                match &mut collector {
                    Some(c) => c.on_frame(f, pipeline),
                    None => Ok(()),
                }
            })?;
            duration = resolve_duration(probed, det.end)?;
            scenes = scenes_from_cuts(&det.cuts, duration);
            stats = det.stats;
            if let (Some(mut c), Some(spec)) = (collector, spec) {
                c.finish(&det.cuts, duration)?;
                stats.candidates = Some(c.candidates_kept);
                // Group the picks by scene, then trim
                let mut per_scene: Vec<Vec<Pick>> = (0..scenes.len()).map(|_| Vec::new()).collect();
                for p in c.picks {
                    let i = scene_index(&scenes, p.scene_start);
                    per_scene[i].push(p);
                }
                let mut counts: Vec<usize> = per_scene.iter().map(Vec::len).collect();
                let before: usize = counts.iter().sum();
                if let Some(max) = spec.max_frames {
                    let durations: Vec<f64> = scenes.iter().map(|(s, e)| e - s).collect();
                    scene_frames::trim(&mut counts, &durations, spec.min_per_scene, max);
                }
                for (ps, k) in per_scene.into_iter().zip(&counts) {
                    picks.extend(scene_frames::thin(ps, *k));
                }
                stats.trimmed_frames = Some(before - picks.len());
            }
            scores = det.scores;
        }
    }
    // Deliver the frames, numbered in time order
    let mut scene_frames_idx: Vec<Vec<usize>> = vec![Vec::new(); scenes.len()];
    let frames_json = match spec {
        Some(spec) => {
            tracker.force_update(1.0, "Writing frames".to_string());
            let mut encoded = Vec::with_capacity(picks.len());
            for (i, p) in picks.into_iter().enumerate() {
                let scene = scene_index(&scenes, p.scene_start);
                scene_frames_idx[scene].push(i);
                let meta = serde_json::json!({ "index": i, "scene": scene, "target": p.target, "timestamp": p.timestamp });
                let com = quality.map(|_| {
                    let mut com = serde_json::json!({ "duration": duration, "index": i, "scene": scene, "target": p.target, "timestamp": p.timestamp });
                    if let Some(path) = &meta_path {
                        com["video"] = serde_json::json!(path);
                    }
                    com
                });
                encoded.push(Some(EncodedFrame { bytes: p.bytes, meta, com }));
            }
            Some(deliver_frames(encoded, &spec.output, spec.format, 4)?)
        }
        None => None,
    };

    let scenes_json: Vec<serde_json::Value> = scenes.iter().enumerate().map(|(i, (s, e))| {
        let mut v = serde_json::json!({ "index": i, "start": s, "end": e });
        if spec.is_some() {
            v["frames"] = serde_json::json!(scene_frames_idx[i]);
        }
        v
    }).collect();

    let mut config = serde_json::to_value(&cfg)?;
    if let Some(spec) = spec {
        let mut f = serde_json::to_value(spec)?;
        f["quality"] = serde_json::json!(quality);
        config["frames"] = f;
        config["skip_blank_frames"] = serde_json::json!(req.skip_blank_frames);
    }
    let mut reply = serde_json::json!({
        "duration": duration,
        "detector": cfg.detector,
        "scenes": scenes_json,
        "stats": stats,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "config": config,
    });
    if let Some(frames) = frames_json {
        reply["frames"] = serde_json::json!(frames);
        if let Some(t) = target {
            reply["size"] = serde_json::json!({ "w": t.w, "h": t.h });
        }
        reply["format"] = serde_json::json!(spec.map(|s| s.format));
    }
    if let Some(s) = scores {
        reply["scores"] = serde_json::to_value(s)?;
    }
    if let Some(p) = meta_path {
        reply["video"] = serde_json::json!(p);
    }
    tracker.force_update(1.0, format!("Detected {} scenes", scenes.len()));
    Ok(reply)
}

/// The scene a pick belongs to: the last scene starting at or before the pick's scene start
fn scene_index(scenes: &[(f64, f64)], scene_start: f64) -> usize {
    scenes.iter().rposition(|(s, _)| *s <= scene_start + 1e-9).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(threshold: Option<f64>, adaptive: Option<Adaptive>, mode: FlashMode, len: f64) -> Resolved {
        Resolved { hint: None, detector: Detector::Content, threshold, min_scene_len: len, flash_filter: Some(mode), weights: None, adaptive }
    }

    /// Feed scores at 25 fps and return the cuts
    fn run(scores: &[f64], c: &Resolved) -> Vec<f64> {
        let mut p = CutPipeline::new(c, true);
        for (i, s) in scores.iter().enumerate() {
            p.push(i as f64 / 25.0, *s);
        }
        p.finish();
        p.cuts
    }

    fn spikes(n: usize, at: &[usize]) -> Vec<f64> {
        (0..n).map(|i| if at.contains(&i) { 50.0 } else { 1.0 }).collect()
    }

    #[test]
    fn threshold_cuts_respect_min_len() {
        let c = cfg(Some(27.0), None, FlashMode::Suppress, 0.6);
        // first cut must be 0.6 s past the first frame; 20 and 25 are 5 frames apart
        assert_eq!(run(&spikes(100, &[10, 20, 25, 60]), &c), vec![20.0 / 25.0, 60.0 / 25.0]);
    }

    #[test]
    fn merge_collapses_a_burst() {
        // Reference values from eval/evalx.py `flash`
        let c = cfg(Some(27.0), None, FlashMode::Merge, 0.6);
        assert_eq!(run(&spikes(200, &[20, 25, 28, 31, 50, 80]), &c), vec![20.0 / 25.0, 50.0 / 25.0, 80.0 / 25.0]);
    }

    #[test]
    fn merge_matches_reference() {
        // Reference: eval/evalx.py `flash` (a port of PySceneDetect's FlashFilter merge)
        let c = cfg(Some(27.0), None, FlashMode::Merge, 0.6);
        let cuts = run(&spikes(200, &[20, 40, 45, 50, 55, 70, 120]), &c);
        // 20: first cut. 40: met (20 frames) -> cut. 45: not met, merge starts at 45.
        // 50, 55 extend; 70: met since 55 and burst 45..70 >= 15 frames -> above, keeps merging
        // (only a non-above frame closes); frame 86 (16 after 70) closes it, emitting 70.
        assert_eq!(cuts, vec![20.0 / 25.0, 40.0 / 25.0, 70.0 / 25.0, 120.0 / 25.0]);
    }

    #[test]
    fn adaptive_needs_a_spike_against_its_neighbours() {
        let a = Adaptive { ratio: 3.0, window: 2, min_score: 15.0 };
        let c = cfg(None, Some(a), FlashMode::Merge, 0.6);
        // constant high motion (30) with one spike of 120 at 50
        let mut s = vec![30.0; 100];
        s[50] = 120.0;
        assert_eq!(run(&s, &c), vec![50.0 / 25.0]);
        // held frames: zeros around a 20 -> ratio 255, and 20 >= min_score
        let mut s = vec![0.0; 100];
        s[40] = 20.0;
        assert_eq!(run(&s, &c), vec![40.0 / 25.0]);
    }

    #[test]
    fn hsv_matches_opencv() {
        let t = HsvTables::new();
        // values from cv2.cvtColor(np.uint8([[[b,g,r]]]), cv2.COLOR_BGR2HSV)
        assert_eq!(t.hsv(255, 0, 0), (0, 255, 255));
        assert_eq!(t.hsv(0, 255, 0), (60, 255, 255));
        assert_eq!(t.hsv(0, 0, 255), (120, 255, 255));
        assert_eq!(t.hsv(0, 0, 0), (0, 0, 0));
        assert_eq!(t.hsv(128, 128, 128), (0, 0, 128));
    }

    /// The FFmpeg CLI from `FFMPEG_DIR` or PATH, for generating test clips
    fn ffmpeg_cli() -> Option<std::path::PathBuf> {
        let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let from_dir = std::env::var_os("FFMPEG_DIR").map(|d| std::path::PathBuf::from(d).join("bin").join(exe));
        let candidates = from_dir.into_iter().chain(std::iter::once(std::path::PathBuf::from(exe)));
        candidates.into_iter().find(|p| std::process::Command::new(p).arg("-version").output().map(|o| o.status.success()).unwrap_or(false))
    }

    /// A 6 s, 25 fps clip with hard cuts at 2 s and 4 s (testsrc → smptebars → mandelbrot),
    /// keyframes only at the cuts, plus a copy where every frame is held for 3 frames
    /// `None` only when there is no FFmpeg CLI; a failed generation panics.
    fn synthetic_clip(dir: &std::path::Path) -> Option<(String, String, String)> {
        let ff = ffmpeg_cli()?;
        let cuts = dir.join("cuts.mkv");
        let held = dir.join("held.mkv");
        let src = "testsrc=size=320x240:rate=25:duration=2[a];smptebars=size=320x240:rate=25:duration=2[b];\
                   mandelbrot=size=320x240:rate=25,trim=duration=2[c];[a][b][c]concat=n=3:v=1:a=0,format=yuv420p";
        let run = |cmd: &mut std::process::Command| assert!(cmd.status().expect("run ffmpeg").success(), "ffmpeg failed: {cmd:?}");
        run(std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-filter_complex", src, "-c:v", "mpeg4", "-q:v", "3", "-g", "1000",
                   "-force_key_frames", "0,2,4", "-sc_threshold", "1000000000"])
            .arg(&cuts));
        // Limited animation: 25 fps, each picture held for 3 frames, with a slow pan
        let held_src = "testsrc=size=320x240:rate=25:duration=2,crop=200:200:'t*20':0,scale=320:240,setsar=1,fps=25/3,fps=25[a];\
                        smptebars=size=320x240:rate=25:duration=2,fps=25/3,fps=25[b];[a][b]concat=n=2:v=1:a=0,format=yuv420p";
        run(std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-filter_complex", held_src, "-c:v", "mpeg4", "-q:v", "3"])
            .arg(&held));
        // The cuts clip as MPEG-TS with timestamps starting at 10 s (as broadcast recordings do)
        let offset = dir.join("cuts.ts");
        run(std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-i"]).arg(&cuts)
            .args(["-c:v", "mpeg2video", "-q:v", "3", "-g", "1000", "-force_key_frames", "0,2,4", "-sc_threshold", "1000000000",
                   "-f", "mpegts", "-output_ts_offset", "10"])
            .arg(&offset));
        let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        Some((s(cuts), s(held), s(offset)))
    }

    fn detect(path: &str, req: serde_json::Value) -> Vec<f64> {
        let mut body = serde_json::json!({ "path": path });
        body.as_object_mut().unwrap().extend(req.as_object().unwrap().clone());
        let req: ScenesRequest = serde_json::from_value(body).unwrap();
        let cfg = resolve(&req).unwrap();
        let mut tracker = ProgressTracker::new("t".into(), path.into(), None, 1000, 0);
        // No duration given: the pass's own end has to match the headers'
        let det = match cfg.detector {
            Detector::Keyframes => detect_keyframes(path, &cfg, None, &mut tracker).unwrap(),
            _ => detect_decoded(path, &cfg, None, false, &mut tracker, |_, _| Ok(())).unwrap(),
        };
        let probed = probe_duration_opt(path).unwrap().unwrap();
        assert!((det.end - probed).abs() <= 1.0 / 25.0 + 1e-6, "end {} vs probed {probed}", det.end);
        let duration = resolve_duration(None, det.end).unwrap();
        scenes_from_cuts(&det.cuts, duration).iter().skip(1).map(|(s, _)| *s).collect()
    }

    fn near(got: &[f64], want: &[f64]) -> bool {
        got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 1.0 / 25.0 + 1e-6)
    }

    #[test]
    fn detects_synthetic_cuts() {
        ffmpeg::init().unwrap();
        let dir = std::env::temp_dir().join(format!("thumbs-collage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let Some((cuts, held, offset)) = synthetic_clip(&dir) else {
            eprintln!("skipping: no ffmpeg CLI found (set FFMPEG_DIR or put ffmpeg on PATH)");
            return;
        };
        let want = [2.0, 4.0];
        for req in [
            serde_json::json!({}),
            serde_json::json!({ "flash_filter": "suppress" }),
            serde_json::json!({ "flash_filter": "off" }),
            serde_json::json!({ "detector": "scdet" }),
            serde_json::json!({ "hint": "real" }),
            serde_json::json!({ "detector": "scdet", "hint": "cg" }),
            serde_json::json!({ "detector": "keyframes" }),
        ] {
            let got = detect(&cuts, req.clone());
            assert!(near(&got, &want), "{req}: got {got:?}, want {want:?}");
            // Times are from the stream's start, not its first timestamp
            let got = detect(&offset, req.clone());
            assert!(near(&got, &want), "{req} on a stream starting at 10 s: got {got:?}, want {want:?}");
        }
        // Held frames: plain content finds the one cut, not the pan's picture changes
        let got = detect(&held, serde_json::json!({ "hint": "anime" }));
        assert!(near(&got, &[2.0]), "held frames: got {got:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `settled_before` is safe (no later cut comes before it) and bounded (it trails the newest
    /// frame by at most `min_scene_len` plus the adaptive window), on score sequences with
    /// random bursts of frames above the test, for every flash mode, with and without adaptive
    #[test]
    fn settled_before_is_safe_and_bounded() {
        let fps = 25.0;
        let mut seed = 12345u64;
        let mut rand = move || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as f64 / (1u64 << 31) as f64 };
        for req in [
            serde_json::json!({ "threshold": 27.0, "flash_filter": "merge" }),
            serde_json::json!({ "threshold": 27.0, "flash_filter": "suppress" }),
            serde_json::json!({ "threshold": 27.0, "flash_filter": "off" }),
            serde_json::json!({ "hint": "real" }),
            serde_json::json!({ "hint": "real", "flash_filter": "suppress" }),
        ] {
            let mut body = serde_json::json!({ "path": "x" });
            body.as_object_mut().unwrap().extend(req.as_object().unwrap().clone());
            let cfg = resolve(&serde_json::from_value(body).unwrap()).unwrap();
            let lag_bound = cfg.min_scene_len + cfg.adaptive.map_or(0, |a| a.window + 1) as f64 / fps + 1e-6;
            for _ in 0..50 {
                let mut p = CutPipeline::new(&cfg, false);
                let mut settled = Vec::new();
                let mut burst = 0usize;
                for i in 0..3000 {
                    let t = i as f64 / fps;
                    if burst == 0 && rand() < 0.02 { burst = 1 + (rand() * 30.0) as usize; }
                    // inside a burst: frames above the test at random; outside: quiet, with rare single cuts
                    let above = if burst > 0 { burst -= 1; rand() < 0.5 } else { rand() < 0.005 };
                    p.push(t, if above { 80.0 } else { 2.0 + rand() });
                    let s = p.settled_before();
                    assert!(t - s <= lag_bound, "{req}: settled_before {s} trails frame {t} by more than {lag_bound}");
                    settled.push((p.cuts.len(), s));
                }
                p.finish();
                for (emitted, s) in settled {
                    for &c in &p.cuts[emitted..] {
                        assert!(c >= s, "{req}: cut {c} emitted after settled_before was {s}");
                    }
                }
            }
        }
    }

    /// A short scene right after a long one gets its frames: the long scene's candidates are
    /// thinned, but sampling of the next scene must not be (it was: the doubled step carried
    /// over, and the 1.6 s scene after a 57 s one got none)
    #[test]
    fn short_scene_after_long_scene_gets_frames() {
        ffmpeg::init().unwrap();
        let Some(ff) = ffmpeg_cli() else {
            eprintln!("skipping: no ffmpeg CLI found (set FFMPEG_DIR or put ffmpeg on PATH)");
            return;
        };
        let dir = std::env::temp_dir().join(format!("thumbs-collage-test-ls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("long_short.mkv");
        let src = "testsrc2=size=160x90:rate=24000/1001:duration=57[a];smptebars=size=160x90:rate=24000/1001:duration=1.6[b];\
                   mandelbrot=size=160x90:rate=24000/1001,trim=duration=3,setpts=PTS-STARTPTS[c];[a][b][c]concat=n=3:v=1:a=0,format=yuv420p";
        let ok = std::process::Command::new(&ff)
            .args(["-v", "error", "-y", "-filter_complex", src, "-c:v", "mpeg4", "-q:v", "3"])
            .arg(&clip).status().expect("run ffmpeg").success();
        assert!(ok, "ffmpeg failed");
        let path = clip.to_string_lossy().into_owned();
        let spec: scene_frames::SceneFramesSpec = serde_json::from_value(serde_json::json!({ "min_per_scene": 2 })).unwrap();
        let target = crate::FrameTarget { w: 32, h: 18, fit: crate::FitMode::Cover };
        for req in [serde_json::json!({}), serde_json::json!({ "flash_filter": "suppress" }), serde_json::json!({ "hint": "real" })] {
            let mut body = serde_json::json!({ "path": path });
            body.as_object_mut().unwrap().extend(req.as_object().unwrap().clone());
            let req_parsed: ScenesRequest = serde_json::from_value(body).unwrap();
            let cfg = resolve(&req_parsed).unwrap();
            let mut tracker = ProgressTracker::new("t".into(), path.clone(), None, 1000, 0);
            let mut c = Collector::new(&spec, target, 16.0 / 9.0, Some(80), false);
            let det = detect_decoded(&path, &cfg, None, false, &mut tracker, |f, p| c.on_frame(f, p)).unwrap();
            let duration = resolve_duration(None, det.end).unwrap();
            c.finish(&det.cuts, duration).unwrap();
            let scenes = scenes_from_cuts(&det.cuts, duration);
            assert_eq!(scenes.len(), 3, "{req}: scenes {scenes:?}");
            for (i, (s, e)) in scenes.iter().enumerate() {
                let picks: Vec<f64> = c.picks.iter().filter(|p| p.scene_start == *s).map(|p| p.timestamp).collect();
                assert!(picks.len() >= 2, "{req}: scene {i} ({s:.2}-{e:.2}) got {picks:?}");
                assert!(picks.iter().all(|t| t >= s && t < e), "{req}: scene {i} ({s:.2}-{e:.2}) got {picks:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run detection on a clip with explicit settings, returning (cuts as frame numbers, scores)
    fn detect_frames(path: &str, req: serde_json::Value, fps: f64) -> (Vec<i64>, ScoreLog) {
        let mut body = serde_json::json!({ "path": path });
        body.as_object_mut().unwrap().extend(req.as_object().unwrap().clone());
        let req: ScenesRequest = serde_json::from_value(body).unwrap();
        let cfg = resolve(&req).unwrap();
        let mut tracker = ProgressTracker::new("t".into(), path.into(), None, 1000, 0);
        let duration = probe_duration_opt(path).unwrap();
        let det = detect_decoded(path, &cfg, duration, true, &mut tracker, |_, _| Ok(())).unwrap();
        let cuts = scenes_from_cuts(&det.cuts, resolve_duration(duration, det.end).unwrap()).iter().skip(1).map(|(s, _)| (s * fps).round() as i64).collect();
        (cuts, det.scores.unwrap())
    }

    /// Goldens: PySceneDetect 0.7.3 on the synthetic clips of tests/golden/clips.json
    /// (generated by scripts/make_goldens.py), and FFmpeg's `scdet` run here on the same clips.
    #[test]
    fn matches_pyscenedetect_and_ffmpeg_goldens() {
        ffmpeg::init().unwrap();
        let Some(ff) = ffmpeg_cli() else {
            eprintln!("skipping: no ffmpeg CLI found (set FFMPEG_DIR or put ffmpeg on PATH)");
            return;
        };
        let clips: serde_json::Value = serde_json::from_str(include_str!("../tests/golden/clips.json")).unwrap();
        let golden: serde_json::Value = serde_json::from_str(include_str!("../tests/golden/content.json")).unwrap();
        let fps = clips["fps"].as_f64().unwrap();
        let dir = std::env::temp_dir().join(format!("thumbs-collage-golden-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let as_f64s = |v: &serde_json::Value| v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect::<Vec<_>>();
        let as_i64s = |v: &serde_json::Value| v.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect::<Vec<_>>();
        let mut failures = Vec::new();

        for clip in clips["clips"].as_array().unwrap() {
            let name = clip["name"].as_str().unwrap();
            let path = dir.join(format!("{name}.mkv"));
            let status = std::process::Command::new(&ff)
                .args(["-v", "error", "-y", "-filter_complex", clip["graph"].as_str().unwrap(), "-c:v", "ffv1", "-pix_fmt", "yuv444p"])
                .arg(&path).status().expect("run ffmpeg");
            assert!(status.success(), "generating {name} failed");
            let path = path.to_string_lossy().into_owned();
            let g = &golden["clips"][name];

            // content: per-frame content_val and cuts (defaults: threshold 27, merge, 0.6 s)
            let (cuts, scores) = detect_frames(&path, serde_json::json!({}), fps);
            let want = as_f64s(&g["content_val"]);
            let worst = scores.score.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            if scores.score.len() != want.len() || worst > CONTENT_TOLERANCE {
                failures.push(format!("{name}: content_val {} frames vs {}, worst difference {worst:.3}", scores.score.len(), want.len()));
            }
            if cuts != as_i64s(&g["content_cuts"]) {
                failures.push(format!("{name}: content cuts {cuts:?}, PySceneDetect {:?}", g["content_cuts"]));
            }

            // adaptive: PySceneDetect's AdaptiveDetector enforces the minimum length like suppress
            let (cuts, scores) = detect_frames(&path, serde_json::json!({ "adaptive": {}, "flash_filter": "suppress" }), fps);
            let want = as_f64s(&g["adaptive_ratio"]);
            let ratio = scores.ratio.unwrap();
            let worst = ratio.iter().zip(&want).map(|(a, b)| if a.max(*b) >= 255.0 { 0.0 } else { (a - b).abs() / b.abs().max(1.0) }).fold(0.0, f64::max);
            if worst > RATIO_TOLERANCE {
                failures.push(format!("{name}: adaptive ratio worst relative difference {worst:.3}"));
            }
            if cuts != as_i64s(&g["adaptive_cuts"]) {
                failures.push(format!("{name}: adaptive cuts {cuts:?}, PySceneDetect {:?}", g["adaptive_cuts"]));
            }

            // scdet: against `ffmpeg -vf scale=256:-2,scdet` on the same clip
            let meta = dir.join(format!("{name}.scd"));
            // Relative file name (run in `dir`): filter option escaping of Windows paths is fragile
            let meta_arg = format!("scale=256:-2,scdet=threshold=100:sc_pass=0,metadata=print:file={name}.scd");
            let status = std::process::Command::new(&ff).current_dir(&dir)
                .args(["-v", "error", "-y", "-i", &path, "-vf", &meta_arg, "-f", "null", "-"]).status().expect("run ffmpeg");
            assert!(status.success(), "ffmpeg scdet on {name} failed");
            let want: Vec<f64> = std::fs::read_to_string(&meta).unwrap().lines()
                .filter_map(|l| l.strip_prefix("lavfi.scd.score=")).map(|v| v.parse().unwrap()).collect();
            let (_, scores) = detect_frames(&path, serde_json::json!({ "detector": "scdet" }), fps);
            let worst = scores.score.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            if scores.score.len() != want.len() || worst > SCDET_TOLERANCE {
                failures.push(format!("{name}: scdet {} frames vs {}, worst difference {worst:.4}", scores.score.len(), want.len()));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(failures.is_empty(), "golden mismatches:\n{}", failures.join("\n"));
    }

    /// Measured 2026-11-13 (FFmpeg 8.0): content_val and the ratio match PySceneDetect to the 4
    /// decimals stored. The margin allows for lavfi sources differing slightly between FFmpeg
    /// versions (the clips are regenerated here); cuts must match exactly.
    const CONTENT_TOLERANCE: f64 = 0.05;
    const RATIO_TOLERANCE: f64 = 0.02;
    /// FFmpeg prints scores with 3 decimals
    const SCDET_TOLERANCE: f64 = 0.002;

    #[test]
    fn hints_resolve_and_refuse_unused_fields() {
        let r = |v: serde_json::Value| -> Result<Resolved> {
            let mut body = serde_json::json!({ "path": "x" });
            body.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
            resolve(&serde_json::from_value(body).unwrap())
        };
        let c = r(serde_json::json!({})).unwrap();
        assert_eq!((c.detector, c.threshold, c.adaptive), (Detector::Content, Some(27.0), None));
        let c = r(serde_json::json!({ "hint": "real" })).unwrap();
        assert_eq!((c.threshold, c.adaptive), (None, Some(Adaptive { ratio: 3.0, window: 2, min_score: 15.0 })));
        let c = r(serde_json::json!({ "hint": "real", "detector": "scdet" })).unwrap();
        assert_eq!(c.adaptive.unwrap().min_score, 4.0);
        let c = r(serde_json::json!({ "hint": "cg", "adaptive": { "ratio": 3.5 } })).unwrap();
        assert_eq!(c.adaptive, Some(Adaptive { ratio: 3.5, window: 2, min_score: 15.0 }));
        let c = r(serde_json::json!({ "hint": "cg", "adaptive": null, "threshold": 30 })).unwrap();
        assert_eq!((c.threshold, c.adaptive), (Some(30.0), None));
        let c = r(serde_json::json!({ "detector": "scdet" })).unwrap();
        assert_eq!(c.threshold, Some(8.0));
        assert!(r(serde_json::json!({ "hint": "real", "threshold": 30 })).is_err());
        assert!(r(serde_json::json!({ "detector": "scdet", "weights": { "hue": 1 } })).is_err());
        assert!(r(serde_json::json!({ "detector": "keyframes", "threshold": 5 })).is_err());
        assert!(r(serde_json::json!({ "detector": "keyframes", "flash_filter": "merge" })).is_err());
        assert!(r(serde_json::json!({ "min_scene_len": -1 })).is_err());
    }

    #[test]
    fn scenes_cover_the_duration() {
        assert_eq!(scenes_from_cuts(&[2.0, 5.0, 5.0, 12.0], 10.0), vec![(0.0, 2.0), (2.0, 5.0), (5.0, 10.0)]);
        assert_eq!(scenes_from_cuts(&[], 10.0), vec![(0.0, 10.0)]);
    }
}

