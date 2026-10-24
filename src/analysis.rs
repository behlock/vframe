use crate::{
    cli::{Settings, TimeRange},
    media::{self, Video},
    process::{Cancellation, Running},
};
use anyhow::{Context, Result, ensure};
use std::{
    io::{ErrorKind, Read},
    path::Path,
    process::Stdio,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct Moment {
    pub time: f64,
    pub change: f64,
}

struct Candidate {
    started: f64,
    stable_since: f64,
    best_time: f64,
    best_sharpness: f64,
    best: Vec<u8>,
    change: f64,
}

struct Selector<'a> {
    settings: &'a Settings,
    width: usize,
    reference: Vec<u8>,
    previous: Vec<u8>,
    previous_time: f64,
    pending: Option<Candidate>,
    moments: Vec<Moment>,
}

impl<'a> Selector<'a> {
    fn new(settings: &'a Settings, width: usize) -> Self {
        Self {
            settings,
            width,
            reference: Vec::new(),
            previous: Vec::new(),
            previous_time: 0.0,
            pending: None,
            moments: Vec::new(),
        }
    }

    fn push(&mut self, time: f64, image: Vec<u8>) {
        if self.reference.is_empty() {
            self.reference = image.clone();
            self.previous = image;
            self.previous_time = time;
            self.moments.push(Moment { time, change: 1.0 });
            return;
        }
        let gap = time - self.previous_time;
        if gap > 1.0 / self.settings.sample_rate as f64 + 1e-6 && gap >= self.settings.settle {
            // Sparse recordings hold frames without producing duplicates to settle on.
            self.flush();
        }
        let change = difference(&self.reference, &image);
        let motion = difference(&self.previous, &image);
        let threshold = self.settings.sensitivity.threshold();
        let last_time = self.moments.last().map_or(time, |m| m.time);
        if change < threshold {
            self.pending = None;
        } else if time - last_time + 1e-9 >= self.settings.min_gap {
            let sharpness = sharpness(&image, self.width);
            let pending = self.pending.get_or_insert_with(|| Candidate {
                started: time,
                stable_since: time,
                best_time: time,
                best_sharpness: sharpness,
                best: image.clone(),
                change,
            });
            if motion > threshold.min(0.025) * 0.5 {
                pending.stable_since = time;
            }
            if sharpness >= pending.best_sharpness {
                pending.best_sharpness = sharpness;
                pending.best_time = time;
                pending.best.clone_from(&image);
                pending.change = change;
            }
            let settled = time - pending.stable_since + 1e-9 >= self.settings.settle;
            // Bound the wait during continuous motion.
            let deadline = time - pending.started >= (self.settings.settle * 3.0).max(1.0);
            if settled {
                self.reference.clone_from(&image);
                self.moments.push(Moment { time, change });
                self.pending = None;
            } else if deadline {
                self.flush();
            }
        }
        self.previous = image;
        self.previous_time = time;
    }

    fn flush(&mut self) {
        if let Some(candidate) = self.pending.take() {
            self.reference = candidate.best;
            self.moments.push(Moment {
                time: candidate.best_time,
                change: candidate.change,
            });
        }
    }
}

// Combine mean RGB error with changed area to retain small edits and colour changes.
fn difference(a: &[u8], b: &[u8]) -> f64 {
    let mut total = 0u64;
    let mut changed = 0u64;
    for (a, b) in a.as_chunks::<3>().0.iter().zip(b.as_chunks::<3>().0) {
        let delta: u32 = a
            .iter()
            .zip(b)
            .map(|(&a, &b)| u32::from(a.abs_diff(b)))
            .sum();
        total += u64::from(delta);
        changed += u64::from(delta > 60);
    }
    total as f64 / (a.len() as f64 * 255.0) + 0.25 * changed as f64 / (a.len() / 3) as f64
}

fn sharpness(image: &[u8], width: usize) -> f64 {
    let stride = width * 3;
    if image.len() <= stride {
        return 0.0;
    }
    let mut edges = 0u64;
    for y in 1..image.len() / stride {
        for x in 1..width {
            let i = y * stride + x * 3;
            for channel in 0..3 {
                edges += u64::from(image[i + channel].abs_diff(image[i + channel - 3]));
                edges += u64::from(image[i + channel].abs_diff(image[i + channel - stride]));
            }
        }
    }
    edges as f64 / image.len() as f64
}

pub fn analyse(
    path: &Path,
    video: &Video,
    settings: &Settings,
    range: TimeRange,
    cancel: &Cancellation,
) -> Result<Vec<Moment>> {
    let (width, height) = video.analysis_dimensions();
    let frame_bytes = width as usize * height as usize * 3;
    // Sample existing frames; fps conversion would invent timestamps in sparse videos.
    let interval = 1.0 / settings.sample_rate as f64 - 1e-9;
    let filter = format!(
        "select='isnan(prev_selected_t)+gte(t-prev_selected_t,{interval:.12})',scale={width}:{height}:flags=area,setsar=1,showinfo"
    );
    let mut running = Running::spawn(
        media::decode_command(path, range)
            .args([
                "-vf",
                &filter,
                "-pix_fmt",
                "rgb24",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdout(Stdio::piped()),
    )?;
    // showinfo logs each timestamp before its matching RGB frame reaches stdout.
    let mut timestamps = media::frame_times(running.log_reader()?, range.start);
    let mut stdout = running.stdout()?;
    // Backpressure bounds memory; timed receives keep a stalled decoder cancellable.
    let (sender, receiver) = mpsc::sync_channel(2);
    let reader = thread::spawn(move || {
        loop {
            let mut image = vec![0; frame_bytes];
            match stdout.read(&mut image[..1]) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
            if let Err(error) = stdout.read_exact(&mut image[1..]) {
                let _ = sender.send(Err(error));
                break;
            }
            if sender.send(Ok(image)).is_err() {
                break;
            }
        }
    });
    let mut selector = Selector::new(settings, width as usize);
    let mut index = 0u64;
    let mut progress = Instant::now();
    loop {
        cancel.check()?;
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                let frame = frame.context("Truncated analysis frame")?;
                let time = timestamps
                    .next()
                    .transpose()?
                    .context("Missing source-frame timestamp in FFmpeg analysis log")?;
                if time < range.end {
                    selector.push(time, frame);
                }
                index += 1;
                if progress.elapsed() >= Duration::from_secs(2) {
                    let percent =
                        ((time - range.start) / (range.end - range.start) * 100.0).min(100.0);
                    eprintln!(
                        "Analysis {percent:.0}% · {} candidates",
                        selector.moments.len()
                    );
                    progress = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("Analysis reader failed"))?;
    running.wait(cancel)?;
    selector.flush();
    ensure!(index > 0, "FFmpeg decoded no frames in the requested range");
    Ok(selector.moments)
}

// Keep the endpoints and the strongest change in each time bucket.
pub fn limit_frames(moments: &mut Vec<Moment>, limit: usize) {
    if moments.len() <= limit {
        return;
    }
    if limit == 1 {
        moments.truncate(1);
        return;
    }
    let first = moments[0];
    let last = moments[moments.len() - 1];
    let mut selected = vec![first];
    if limit > 2 {
        let mut buckets: Vec<Option<Moment>> = vec![None; limit - 2];
        for moment in &moments[1..moments.len() - 1] {
            let index = (((moment.time - first.time) / (last.time - first.time))
                * buckets.len() as f64) as usize;
            let index = index.min(buckets.len() - 1);
            if buckets[index].is_none_or(|saved| moment.change > saved.change) {
                buckets[index] = Some(*moment);
            }
        }
        selected.extend(buckets.into_iter().flatten());
    }
    selected.push(last);
    *moments = selected;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ImageFormat, Resolution, Sensitivity};
    fn settings() -> Settings {
        Settings {
            resolution: Resolution(None),
            sensitivity: Sensitivity::Medium,
            format: ImageFormat::Jpg,
            start: 0.0,
            end: None,
            max_frames: 200,
            min_gap: 0.5,
            settle: 0.5,
            sample_rate: 4,
        }
    }
    fn solid(value: u8) -> Vec<u8> {
        vec![value; 12 * 12 * 3]
    }
    #[test]
    fn holds_transitions_removes_duplicates_and_preserves_revisits() {
        let settings = settings();
        let mut selector = Selector::new(&settings, 12);
        for (i, value) in [0, 0, 0, 0, 100, 200, 200, 200, 200, 0, 0, 0]
            .into_iter()
            .enumerate()
        {
            selector.push(i as f64 / 4.0, solid(value));
        }
        selector.flush();
        let times: Vec<_> = selector.moments.iter().map(|m| m.time).collect();
        assert_eq!(times, vec![0.0, 1.75, 2.75]);
    }
    #[test]
    fn small_changes_accumulate_and_flashes_are_ignored() {
        let settings = settings();
        let mut selector = Selector::new(&settings, 12);
        for (i, value) in [0, 0, 100, 0, 0, 5, 10, 15, 20, 25, 30, 30, 30]
            .into_iter()
            .enumerate()
        {
            selector.push(i as f64 / 4.0, solid(value));
        }
        selector.flush();
        assert_eq!(selector.moments.len(), 2);
        assert!(selector.moments[1].time >= 2.0);
    }
    #[test]
    fn frame_cap_covers_the_entire_timeline() {
        let mut moments: Vec<_> = (0..100)
            .map(|n| Moment {
                time: n as f64,
                change: n as f64,
            })
            .collect();
        limit_frames(&mut moments, 5);
        assert_eq!(moments.len(), 5);
        assert_eq!(moments.first().unwrap().time, 0.0);
        assert_eq!(moments.last().unwrap().time, 99.0);
        assert!(moments.windows(2).all(|pair| pair[0].time < pair[1].time));
        assert!(moments[2].time > 33.0 && moments[2].time < 70.0);
    }

    #[test]
    fn sparse_source_frames_do_not_require_synthetic_duplicates_to_settle() {
        let settings = settings();
        let mut selector = Selector::new(&settings, 12);
        for (time, value) in [(0.0, 0), (1.0, 100), (2.0, 200)] {
            selector.push(time, solid(value));
        }
        selector.flush();
        assert_eq!(
            selector.moments.iter().map(|m| m.time).collect::<Vec<_>>(),
            vec![0.0, 1.0, 2.0]
        );
    }
}
