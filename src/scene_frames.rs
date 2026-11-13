//! Frame selection for `POST /scenes`: placement within scenes, the candidates kept during the
//! single decode pass, and `max_frames` trimming.

use anyhow::Result;
use image::DynamicImage;

use crate::scenes::{CutPipeline, PassFrame};
use crate::{bad_request, decode_and_scale_frame, encode_frame, finish_frame, is_blank_image, validate_output,
    FrameTarget, ImageFormat, OutputSpec, SizeSpec};

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct SceneFramesSpec {
    #[serde(default = "default_every")]
    pub every: f64,
    #[serde(default = "default_min_per_scene")]
    pub min_per_scene: usize,
    #[serde(default = "default_max_per_scene")]
    pub max_per_scene: usize,
    #[serde(default = "default_margin")]
    pub margin: f64,
    #[serde(default)]
    pub max_frames: Option<usize>,
    #[serde(default)]
    pub size: SizeSpec,
    #[serde(default)]
    pub format: ImageFormat,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub output: OutputSpec,
}

fn default_every() -> f64 { 2.0 }
fn default_min_per_scene() -> usize { 1 }
fn default_max_per_scene() -> usize { 3 }
fn default_margin() -> f64 { 0.25 }

/// Candidates kept per open scene before they are thinned out, as a multiple of `max_per_scene`
const CANDIDATES_PER_PICK: usize = 8;

pub(crate) fn validate(spec: &SceneFramesSpec) -> Result<()> {
    if !(spec.every > 0.0 && spec.every.is_finite()) { return bad_request("frames.every must be above 0"); }
    if !(spec.margin >= 0.0 && spec.margin.is_finite()) { return bad_request("frames.margin must be 0 or more"); }
    if spec.max_per_scene < 1 { return bad_request("frames.max_per_scene must be at least 1"); }
    if spec.min_per_scene > spec.max_per_scene { return bad_request("frames.min_per_scene must not exceed frames.max_per_scene"); }
    if spec.max_frames == Some(0) { return bad_request("frames.max_frames must be at least 1"); }
    if spec.size.width == Some(0) || spec.size.height == Some(0) {
        return bad_request("frames.size: width and height must be at least 1");
    }
    validate_output(spec.format, spec.quality, &spec.output, "frames.")
}

/// How many frames a scene wants: ceil(duration / every), within min/max per scene
pub(crate) fn wanted(spec: &SceneFramesSpec, start: f64, end: f64) -> usize {
    let k = ((end - start) / spec.every).ceil().max(0.0) as usize;
    k.clamp(spec.min_per_scene, spec.max_per_scene)
}

/// `k` targets spread evenly over the scene, `margin` (at most 10% of the scene) in from both ends
pub(crate) fn place(spec: &SceneFramesSpec, start: f64, end: f64, k: usize) -> Vec<f64> {
    let dur = (end - start).max(0.0);
    let m = spec.margin.min(0.1 * dur);
    let (lo, hi) = (start + m, end - m);
    (0..k).map(|j| lo + (j as f64 + 0.5) * (hi - lo) / k as f64).collect()
}

/// `max_frames` trimming on per-scene counts:
/// 1. remove frames down to `min_per_scene`, one at a time, from the scene with the most
///    (ties: the shorter scene);
/// 2. if still over, drop whole scenes' frames, shortest scene first.
pub(crate) fn trim(counts: &mut [usize], durations: &[f64], min_per_scene: usize, max_frames: usize) {
    let mut total: usize = counts.iter().sum();
    while total > max_frames {
        let best = (0..counts.len())
            .filter(|&i| counts[i] > min_per_scene)
            .max_by(|&a, &b| counts[a].cmp(&counts[b]).then(durations[b].partial_cmp(&durations[a]).unwrap()));
        let Some(i) = best else { break };
        counts[i] -= 1;
        total -= 1;
    }
    if total > max_frames {
        let mut order: Vec<usize> = (0..counts.len()).collect();
        order.sort_by(|&a, &b| durations[a].partial_cmp(&durations[b]).unwrap());
        for i in order {
            if total <= max_frames { break; }
            total -= counts[i];
            counts[i] = 0;
        }
    }
}

/// `keep` of `picks` (in time order), spread evenly
pub(crate) fn thin<T>(picks: Vec<T>, keep: usize) -> Vec<T> {
    let k = picks.len();
    if keep >= k { return picks; }
    if keep == 0 { return Vec::new(); }
    let wanted: Vec<usize> = (0..keep).map(|j| ((j as f64 + 0.5) * k as f64 / keep as f64 - 0.5).round() as usize).collect();
    picks.into_iter().enumerate().filter(|(i, _)| wanted.contains(i)).map(|(_, p)| p).collect()
}

/// A frame chosen for a scene, encoded (JPEG without its COM marker yet, or PNG)
pub(crate) struct Pick {
    pub scene_start: f64,
    pub target: f64,
    pub timestamp: f64,
    pub bytes: Vec<u8>,
}

/// Keeps candidate frames on a time grid while scenes are open, and picks from them when a
/// scene's end cut becomes final.
pub(crate) struct Collector<'a> {
    spec: &'a SceneFramesSpec,
    target: FrameTarget,
    video_aspect: f64,
    quality: Option<u8>,
    skip_blank: bool,
    candidates: Vec<(f64, DynamicImage)>,
    step: f64,
    next_t: f64,
    cuts_seen: usize,
    scene_start: f64,
    pub picks: Vec<Pick>,
    pub candidates_kept: usize,
}

impl<'a> Collector<'a> {
    pub fn new(spec: &'a SceneFramesSpec, target: FrameTarget, video_aspect: f64, quality: Option<u8>, skip_blank: bool) -> Self {
        Collector {
            spec, target, video_aspect, quality, skip_blank,
            candidates: Vec::new(),
            step: spec.every / 4.0,
            next_t: f64::NEG_INFINITY,
            cuts_seen: 0,
            scene_start: 0.0,
            picks: Vec::new(),
            candidates_kept: 0,
        }
    }

    pub fn on_frame(&mut self, f: PassFrame, pipeline: &CutPipeline) -> Result<()> {
        // Close the scenes whose end cut became final
        while self.cuts_seen < pipeline.cuts.len() {
            let cut = pipeline.cuts[self.cuts_seen];
            self.cuts_seen += 1;
            if cut > self.scene_start {
                self.close(cut)?;
            }
        }
        if f.t >= self.next_t {
            let img = decode_and_scale_frame(f.frame, self.target, self.video_aspect)?;
            self.candidates.push((f.t, finish_frame(&img, self.target)));
            self.candidates_kept += 1;
            self.next_t = f.t + self.step;
            if self.candidates.len() > CANDIDATES_PER_PICK * self.spec.max_per_scene {
                // Bound memory per scene: keep every other candidate, halve the density
                let mut i = 0;
                self.candidates.retain(|_| { i += 1; i % 2 == 1 });
                self.step *= 2.0;
            }
        }
        Ok(())
    }

    /// End of the pass: close the scenes of cuts emitted after the last frame, then the last one
    pub fn finish(&mut self, cuts: &[f64], duration: f64) -> Result<()> {
        while self.cuts_seen < cuts.len() {
            let cut = cuts[self.cuts_seen];
            self.cuts_seen += 1;
            if cut > self.scene_start {
                self.close(cut)?;
            }
        }
        self.close(duration.max(self.scene_start))?;
        Ok(())
    }

    fn close(&mut self, end: f64) -> Result<()> {
        let start = self.scene_start;
        let split = self.candidates.iter().position(|(t, _)| *t >= end).unwrap_or(self.candidates.len());
        let mut cands: Vec<(f64, DynamicImage)> = self.candidates.drain(..split).collect();
        let k = wanted(self.spec, start, end);
        let mut used = vec![false; cands.len()];
        let mut chosen = Vec::new();
        for target in place(self.spec, start, end, k) {
            let nearest = |want_clear: bool, cands: &[(f64, DynamicImage)], used: &[bool]| {
                (0..cands.len())
                    .filter(|&i| !used[i] && (!want_clear || !is_blank_image(&cands[i].1)))
                    .min_by(|&a, &b| (cands[a].0 - target).abs().partial_cmp(&(cands[b].0 - target).abs()).unwrap())
            };
            let pick = if self.skip_blank {
                nearest(true, &cands, &used).or_else(|| nearest(false, &cands, &used))
            } else {
                nearest(false, &cands, &used)
            };
            if let Some(i) = pick {
                used[i] = true;
                chosen.push((target, i));
            }
        }
        chosen.sort_by(|a, b| cands[a.1].0.partial_cmp(&cands[b.1].0).unwrap());
        for (target, i) in chosen {
            let (t, img) = std::mem::replace(&mut cands[i], (0.0, DynamicImage::new_rgb8(1, 1)));
            let bytes = encode_frame(&img, self.spec.format, self.quality)?;
            self.picks.push(Pick { scene_start: start, target, timestamp: t, bytes });
        }
        // Restart the grid for the next scene
        self.scene_start = end;
        self.step = self.spec.every / 4.0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SceneFramesSpec {
        serde_json::from_str("{}").unwrap()
    }

    #[test]
    fn placement() {
        let s = spec();
        assert_eq!(wanted(&s, 0.0, 0.3), 1);
        assert_eq!(wanted(&s, 0.0, 5.0), 3);
        assert_eq!(wanted(&s, 0.0, 3.0), 2);
        // margin 0.25 on a 10 s scene, 2 frames: [0.25, 9.75] split in two
        assert_eq!(place(&s, 0.0, 10.0, 2), vec![0.25 + 9.5 / 4.0, 0.25 + 3.0 * 9.5 / 4.0]);
        // margin capped at 10%: 1 s scene -> 0.1
        assert_eq!(place(&s, 0.0, 1.0, 1), vec![0.5]);
    }

    #[test]
    fn trimming_order() {
        let mut c = vec![3, 3, 1, 2];
        trim(&mut c, &[10.0, 6.0, 1.0, 4.0], 1, 6);
        // down to min first, from the fullest scene (ties: shorter): 3,3,1,2 -> 3,2,1,2 -> 2,2,1,2 -> 2,2,1,1
        assert_eq!(c, vec![2, 2, 1, 1]);
        let mut c = vec![3, 3, 1, 2];
        trim(&mut c, &[10.0, 6.0, 1.0, 4.0], 1, 2);
        // all at 1 (4 frames), then the shortest scenes lose theirs
        assert_eq!(c, vec![1, 1, 0, 0]);
    }

    #[test]
    fn thinning() {
        assert_eq!(thin(vec![0, 1, 2], 1), vec![1]);
        assert_eq!(thin(vec![0, 1, 2, 3], 2), vec![1, 3]);
        assert_eq!(thin(vec![0, 1, 2], 3), vec![0, 1, 2]);
    }
}
