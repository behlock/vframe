mod analysis;
mod cli;
mod contact;
mod export;
mod media;
mod process;
mod source;
mod storage;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Operation};
use process::Cancellation;
use serde::Serialize;
use std::{fs, io::IsTerminal, path::PathBuf};

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    tool_version: &'static str,
    source: &'a source::SourceInfo,
    settings: &'a cli::Settings,
    video: &'a media::Video,
    candidates_found: usize,
    frames: &'a [export::Frame],
    contact_sheets: &'a [String],
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let cancel = Cancellation::default();
    match cancel.install().and_then(|()| run(cli, &cancel)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            if cancel.is_cancelled() {
                std::process::ExitCode::from(130)
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

fn run(mut cli: Cli, cancel: &Cancellation) -> Result<()> {
    if matches!(cli.operation, Some(Operation::Doctor)) {
        return process::doctor(cancel);
    }
    let input = match cli.input.take() {
        Some(input) => input,
        None if std::io::stdin().is_terminal() => {
            let input = cli::prompt_input(cancel)?;
            cli.settings.resolution = cli::prompt_resolution(cli.settings.resolution, cancel)?;
            input
        }
        None => anyhow::bail!("Provide a YouTube URL or local video path. See vframes --help."),
    };
    let settings = &cli.settings;
    settings.validate()?;
    process::require("ffmpeg", cancel)?;
    process::require("ffprobe", cancel)?;
    let source = source::resolve(&input, &cli, cancel)?;
    let video = &source.video;
    let range = settings.range(video.duration)?;
    eprintln!(
        "Video: {} · {} × {} · {}",
        source.info.title,
        video.width,
        video.height,
        cli::timestamp(video.duration)
    );
    eprintln!(
        "Analysing {}–{} at {} samples/s…",
        cli::timestamp(range.start),
        cli::timestamp(range.end),
        settings.sample_rate
    );
    let mut moments = analysis::analyse(&source.path, video, settings, range, cancel)?;
    let candidates_found = moments.len();
    analysis::limit_frames(&mut moments, settings.max_frames as usize);
    anyhow::ensure!(
        !moments.is_empty(),
        "No frames found in the requested time range."
    );

    let output_base = cli.output.unwrap_or_else(default_output);
    fs::create_dir_all(&output_base)
        .with_context(|| format!("Cannot create {}", output_base.display()))?;
    let output_base = fs::canonicalize(output_base)?;
    let staging = tempfile::Builder::new()
        .prefix(".vframes-")
        .tempdir_in(&output_base)?;
    let image_dir = staging.path().join("images");
    fs::create_dir(&image_dir)?;
    eprintln!(
        "Exporting {} frames ({} candidates)…",
        moments.len(),
        candidates_found
    );
    let frames = export::export(
        &source.path,
        video,
        settings,
        range,
        &moments,
        &image_dir,
        cancel,
    )?;
    eprintln!("Building contact sheets…");
    let sheets = contact::create(staging.path(), &source.info.title, &frames, cancel)?;
    let manifest = Manifest {
        schema_version: 1,
        tool_version: env!("CARGO_PKG_VERSION"),
        source: &source.info,
        settings,
        video,
        candidates_found,
        frames: &frames,
        contact_sheets: &sheets,
    };
    fs::write(
        staging.path().join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let destination = storage::publish(staging, &source::safe_name(&source.info.title), cancel)?;
    println!(
        "Saved {} images and {} contact sheet(s) to:\n{}",
        frames.len(),
        sheets.len(),
        destination.display()
    );
    if !cli.no_open
        && let Err(error) = process::open_folder(&destination.join("images"), cancel)
    {
        eprintln!("Images are saved, but opening the folder failed: {error:#}");
    }
    Ok(())
}

fn default_output() -> PathBuf {
    dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|p| p.join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("VideoFrames")
}
