use crate::{
    analysis::Moment,
    cli::{self, ImageFormat, Settings, TimeRange},
    media::{self, Video},
    process::{Cancellation, Running},
};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, path::Path, process::Stdio};

pub fn export(
    path: &Path,
    video: &Video,
    settings: &Settings,
    range: TimeRange,
    moments: &[Moment],
    directory: &Path,
    cancel: &Cancellation,
) -> Result<()> {
    let (width, height) = settings.resolution.dimensions(video.width, video.height);
    eprintln!("Image size: {width} × {height} (no upscaling)");
    ensure!(!moments.is_empty(), "No frames selected for export");
    let selection = selection_expression(moments, range.start);
    // A script avoids command-line length limits for large selections.
    let mut script = tempfile::NamedTempFile::new()?;
    write!(
        script,
        "select='{selection}',showinfo,scale={width}:{height}:flags=lanczos,setsar=1"
    )?;
    script.flush()?;
    let mut command = media::decode_command(path, range);
    command.args(["-n", "-/filter:v"]).arg(script.path()).args([
        "-fps_mode",
        "vfr",
        "-frames:v",
        &moments.len().to_string(),
    ]);
    match settings.format {
        ImageFormat::Jpg => {
            command.args(["-c:v", "mjpeg", "-q:v", "2", "-pix_fmt", "yuvj444p"]);
        }
        ImageFormat::Png => {
            command.args(["-c:v", "png", "-pix_fmt", "rgb24"]);
        }
    }
    command
        .arg(directory.join(format!("%06d.{}", settings.format.extension())))
        .stdout(Stdio::null());
    let mut running = Running::spawn(&mut command)?;
    running.wait(cancel)?;
    let times: Vec<_> =
        media::frame_times(running.log_reader()?, range.start).collect::<Result<_>>()?;
    ensure!(
        times.len() == moments.len(),
        "FFmpeg exported {} frames but {} were selected; incomplete output was removed. Try a lower --sample-rate.",
        times.len(),
        moments.len()
    );
    for (offset, time) in times.into_iter().enumerate() {
        cancel.check()?;
        let index = offset + 1;
        ensure!(
            time >= range.start - 1e-6 && time < range.end,
            "Exported frame lies outside the requested time range"
        );
        let timestamp = cli::timestamp(time);
        let filename = format!(
            "{index:04}_{}.{}",
            timestamp.replace(':', "-"),
            settings.format.extension()
        );
        let original = directory.join(format!("{index:06}.{}", settings.format.extension()));
        ensure!(
            original.is_file(),
            "FFmpeg did not write expected image {index}"
        );
        fs::rename(original, directory.join(&filename)).context("Cannot name exported frame")?;
    }
    Ok(())
}

fn selection_expression(moments: &[Moment], start: f64) -> String {
    if moments.len() == 1 {
        // Account for showinfo rounding so selection doesn't skip the sampled frame.
        let time = (moments[0].time - start - 1e-6).max(0.0);
        return format!(
            "gte(t,{time:.9})*if(isnan(prev_selected_t),1,lt(prev_selected_t,{time:.9}))"
        );
    }
    // A flat sum hits FFmpeg's expression recursion limit with large selections.
    // Parenthesised halves keep parser depth logarithmic in the number of frames.
    let (left, right) = moments.split_at(moments.len() / 2);
    format!(
        "({})+({})",
        selection_expression(left, start),
        selection_expression(right, start)
    )
}
