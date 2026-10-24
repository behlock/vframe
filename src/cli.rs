use crate::process::Cancellation;
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    io::{self, Write},
    path::PathBuf,
    str::FromStr,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Turn a YouTube video or local file into meaningful, timestamped images."
)]
pub struct Cli {
    /// YouTube URL or local video path. Omit to paste a link interactively.
    pub input: Option<String>,
    #[command(subcommand)]
    pub operation: Option<Operation>,
    #[command(flatten)]
    pub settings: Settings,
    /// Parent directory for a new, unique result folder.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Retain and reuse downloaded sources for later runs.
    #[arg(long)]
    pub cache: bool,
    /// Custom source cache location; also enables caching.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,
    /// Save results without opening Finder/the system file manager.
    #[arg(long)]
    pub no_open: bool,
}

#[derive(Debug, Args)]
pub struct Settings {
    /// Maximum image dimensions: 480p, 720p, 1080p, 1440p, 2160p/4k, or original.
    #[arg(short, long, default_value = "1080p")]
    pub resolution: Resolution,
    /// Higher sensitivity captures smaller visual changes.
    #[arg(short, long, value_enum, default_value_t = Sensitivity::Medium)]
    pub sensitivity: Sensitivity,
    #[arg(short, long, value_enum, default_value_t = ImageFormat::Jpg)]
    pub format: ImageFormat,
    /// Start time in seconds, MM:SS, or HH:MM:SS (fractions accepted).
    #[arg(long, value_parser = parse_time, default_value = "0")]
    pub start: f64,
    /// Exclusive end time, measured from the beginning of the video.
    #[arg(long, value_parser = parse_time)]
    pub end: Option<f64>,
    /// Maximum output images, spread over the full selected timeline.
    #[arg(long, default_value = "200", value_parser = clap::value_parser!(u32).range(1..=10000))]
    pub max_frames: u32,
    /// Minimum seconds between selected images.
    #[arg(long, default_value = "1", value_parser = nonnegative)]
    pub min_gap: f64,
    /// Seconds to wait for a changed image to settle.
    #[arg(long, default_value = "0.5", value_parser = nonnegative)]
    pub settle: f64,
    /// Analysis samples per second (1–30). Shorter events may be missed.
    #[arg(long, default_value = "4", value_parser = clap::value_parser!(u32).range(1..=30))]
    pub sample_rate: u32,
}

#[derive(Debug, Subcommand)]
pub enum Operation {
    /// Check FFmpeg, ffprobe, yt-dlp, and a YouTube JavaScript runtime.
    Doctor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution(pub Option<u32>);

impl FromStr for Resolution {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let height = match value.to_ascii_lowercase().as_str() {
            "original" => None,
            "480p" | "480" => Some(480),
            "720p" | "720" => Some(720),
            "1080p" | "1080" => Some(1080),
            "1440p" | "1440" => Some(1440),
            "2160p" | "2160" | "4k" => Some(2160),
            _ => return Err("Choose 480p, 720p, 1080p, 1440p, 2160p/4k, or original".into()),
        };
        Ok(Self(height))
    }
}

impl Resolution {
    pub fn dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let Some(bound_height) = self.0 else {
            return (width, height);
        };
        fit(width, height, bound_height * 16 / 9, bound_height)
    }
    pub fn label(self) -> String {
        self.0
            .map_or_else(|| "original".into(), |h| format!("{h}p"))
    }
    pub fn satisfies(self, requested: Self) -> bool {
        match (self.0, requested.0) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(cached), Some(wanted)) => cached >= wanted,
        }
    }
}

pub fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Sensitivity {
    Low,
    Medium,
    High,
}

impl Sensitivity {
    pub fn threshold(self) -> f64 {
        match self {
            Self::Low => 0.12,
            Self::Medium => 0.055,
            Self::High => 0.018,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ImageFormat {
    #[value(alias = "jpeg")]
    Jpg,
    Png,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Png => "png",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimeRange {
    pub start: f64,
    pub end: f64,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.end.is_none_or(|end| end > self.start),
            "--end must be later than --start"
        );
        ensure!(self.settle <= 10.0, "--settle must be at most 10 seconds");
        Ok(())
    }
    pub fn range(&self, duration: f64) -> Result<TimeRange> {
        let end = self.end.unwrap_or(duration).min(duration);
        ensure!(
            self.start < end,
            "--start is outside the video (duration: {})",
            timestamp(duration)
        );
        Ok(TimeRange {
            start: self.start,
            end,
        })
    }
}

fn nonnegative(value: &str) -> Result<f64, String> {
    let number: f64 = value.parse().map_err(|_| "Expected a number".to_owned())?;
    if !number.is_finite() || number < 0.0 {
        return Err("Expected a finite, nonnegative number".into());
    }
    Ok(number)
}

pub fn parse_time(value: &str) -> Result<f64, String> {
    let parts: Vec<_> = value.split(':').collect();
    if !(1..=3).contains(&parts.len()) {
        return Err("Use seconds, MM:SS, or HH:MM:SS".into());
    }
    let mut result = 0.0;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err("Time components cannot be empty".into());
        }
        let number = nonnegative(part)?;
        if index < parts.len() - 1 && number.fract() != 0.0 {
            return Err("Only seconds may contain a fraction".into());
        }
        if index > 0 && number >= 60.0 {
            return Err("Minutes and seconds after ':' must be below 60".into());
        }
        result = result * 60.0 + number;
    }
    if !result.is_finite() {
        return Err("Time is too large".into());
    }
    Ok(result)
}

pub fn timestamp(seconds: f64) -> String {
    let millis = (seconds * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000
    )
}

pub fn prompt_input(cancel: &Cancellation) -> Result<String> {
    let input = read_prompt("Paste a YouTube link or local video path: ", cancel)?;
    let input = input.trim().trim_matches(['\'', '"']).to_owned();
    if input.is_empty() {
        bail!("No input provided");
    }
    Ok(input)
}

pub fn prompt_resolution(default: Resolution, cancel: &Cancellation) -> Result<Resolution> {
    loop {
        let input = read_prompt(
            &format!(
                "Resolution (480p / 720p / 1080p / 1440p / 2160p / original) [{}]: ",
                default.label()
            ),
            cancel,
        )?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match input.trim().parse() {
            Ok(resolution) => return Ok(resolution),
            Err(error) => eprintln!("{error}"),
        }
    }
}

fn read_prompt(prompt: &str, cancel: &Cancellation) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    // Blocking stdin stays off the main thread so Ctrl-C can cancel an unanswered prompt.
    thread::spawn(move || {
        let mut line = String::new();
        let result = io::stdin().read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    loop {
        cancel.check()?;
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result.context("Cannot read input"),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => bail!("Input reader stopped unexpectedly"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_times_and_rejects_invalid_values() {
        assert_eq!(parse_time("01:02:03.25").unwrap(), 3723.25);
        assert_eq!(parse_time("90").unwrap(), 90.0);
        for bad in ["NaN", "inf", "-1", "1:60", "1.5:00", "1::2", "1:2:3:4"] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert_eq!(timestamp(59.9996), "00:01:00.000");
    }
    #[test]
    fn resolution_preserves_aspect_ratio_without_upscaling() {
        let full_hd = Resolution(Some(1080));
        assert_eq!(full_hd.dimensions(3840, 2160), (1920, 1080));
        assert_eq!(full_hd.dimensions(1080, 1920), (608, 1080));
        assert_eq!(full_hd.dimensions(640, 360), (640, 360));
        assert_eq!(Resolution(None).dimensions(7680, 4320), (7680, 4320));
        assert!(!full_hd.satisfies(Resolution(Some(2160))));
        assert!(Resolution(None).satisfies(full_hd));
    }
}
