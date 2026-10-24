use crate::{
    cli::{TimeRange, fit},
    process::{self, Cancellation},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{io::BufRead, path::Path, process::Command};

#[derive(Debug)]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Option<Format>,
}
#[derive(Deserialize)]
struct Stream {
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
    sample_aspect_ratio: Option<String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}
#[derive(Deserialize)]
struct SideData {
    rotation: Option<i32>,
}
#[derive(Deserialize)]
struct Format {
    duration: Option<String>,
}

pub fn probe(path: &Path, cancel: &Cancellation) -> Result<Video> {
    let output = process::capture(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "V:0",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(path),
        cancel,
    )?;
    let probe: Probe = serde_json::from_str(&output).context("Cannot read ffprobe metadata")?;
    let stream = probe
        .streams
        .first()
        .context("Input contains no video stream")?;
    let width = stream.width.context("Missing video width")?;
    let height = stream.height.context("Missing video height")?;
    ensure!(width > 0 && height > 0, "Invalid video dimensions");
    let duration = stream
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            probe
                .format
                .as_ref()?
                .duration
                .as_deref()?
                .parse::<f64>()
                .ok()
        })
        .context("Cannot determine video duration (live streams are not supported)")?;
    ensure!(
        duration.is_finite() && duration > 0.0,
        "Invalid video duration"
    );
    let sar = stream
        .sample_aspect_ratio
        .as_deref()
        .and_then(|s| s.split_once(':'))
        .and_then(|(a, b)| Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?))
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(1.0);
    let rotation = stream
        .side_data_list
        .iter()
        .find_map(|d| d.rotation)
        .unwrap_or(0);
    let display_width = (width as f64 * sar).round().max(1.0) as u32;
    let (width, height) = if rotation.rem_euclid(180) == 90 {
        (height, display_width)
    } else {
        (display_width, height)
    };
    Ok(Video {
        width,
        height,
        duration,
    })
}

impl Video {
    pub fn analysis_dimensions(&self) -> (u32, u32) {
        fit(self.width, self.height, 256, 144)
    }
}

pub fn decode_command(path: &Path, range: TimeRange) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "info",
            "-nostats",
            "-nostdin",
            "-ss",
            &format!("{:.9}", range.start),
            "-i",
        ])
        .arg(path)
        .args([
            "-t",
            &format!("{:.9}", range.end - range.start),
            "-map",
            "0:V:0",
            "-an",
            "-sn",
            "-dn",
        ]);
    command
}

pub fn frame_times(reader: impl BufRead, start: f64) -> impl Iterator<Item = Result<f64>> {
    reader.lines().filter_map(move |line| match line {
        Ok(line) => frame_time(&line, start).map(Ok),
        Err(error) => Some(Err(error.into())),
    })
}

fn frame_time(line: &str, start: f64) -> Option<f64> {
    if !line.contains("Parsed_showinfo_") {
        return None;
    }
    let (_, value) = line.split_once("pts_time:")?;
    let time = value.split_whitespace().next()?.parse::<f64>().ok()? + start;
    time.is_finite().then_some(time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_timestamps_and_skips_other_log_lines() {
        let log = "[Parsed_showinfo_1 @ 0] n: 0 pts: 12 pts_time:0.400000 fmt:rgb24\n\
                   [Parsed_showinfo_1 @ 0] color_range:unknown\n\
                   [Parsed_showinfo_1 @ 0] n: 1 pts: 91 pts_time:3.033333 fmt:rgb24";
        let times: Vec<_> = frame_times(log.as_bytes(), 10.0)
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(times.len(), 2);
        assert!((times[0] - 10.4).abs() < 1e-9);
        assert!((times[1] - 13.033333).abs() < 1e-9);
    }
}
