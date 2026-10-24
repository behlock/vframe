use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn checked(command: &mut Command) -> Output {
    let output = command.output().expect("start command");
    assert!(
        output.status.success(),
        "{command:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    command
}

fn generate_video(path: &Path, graph: &str, options: &[&str]) {
    checked(
        ffmpeg()
            .args(["-f", "lavfi", "-i", graph])
            .args(options)
            .args(["-c:v", "libx264", "-preset", "ultrafast"])
            .arg(path),
    );
}

fn fixture(root: &Path, size: &str) -> PathBuf {
    let path = root.join("source 'with ; spaces.mp4");
    let colors = ["red", "green", "blue", "red"];
    let mut graph = colors
        .iter()
        .enumerate()
        .map(|(i, color)| format!("color=c={color}:s={size}:r=12:d=2[v{i}]"))
        .collect::<Vec<_>>()
        .join(";");
    graph.push_str(";[v0][v1][v2][v3]concat=n=4:v=1:a=0,format=yuv420p");
    generate_video(&path, &graph, &["-crf", "0"]);
    path
}

fn cli(input: &Path, output: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vframes"));
    command
        .arg(input)
        .arg("--output")
        .arg(output)
        .arg("--no-open");
    command
}

fn result_folder(output: &Output) -> PathBuf {
    PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap(),
    )
}

fn manifest(folder: &Path) -> Value {
    serde_json::from_slice(&fs::read(folder.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn extracts_changes_preserves_revisits_and_creates_a_contact_sheet() {
    let root = TempDir::new().unwrap();
    let source = fixture(root.path(), "640x360");
    let output = root.path().join("output");
    let result = checked(&mut cli(&source, &output));
    let folder = result_folder(&result);
    let manifest = manifest(&folder);
    let frames = manifest["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 4, "{manifest}");
    let mut previous = -1.0;
    for frame in frames {
        let time = frame["timestamp_seconds"].as_f64().unwrap();
        assert!(time > previous);
        previous = time;
        let image = image::open(folder.join(frame["file"].as_str().unwrap())).unwrap();
        assert_eq!((image.width(), image.height()), (640, 360));
    }
    let first = image::open(folder.join(frames[0]["file"].as_str().unwrap()))
        .unwrap()
        .to_rgb8();
    let last = image::open(folder.join(frames[3]["file"].as_str().unwrap()))
        .unwrap()
        .to_rgb8();
    assert!(first.get_pixel(100, 100)[0] > 240);
    assert!(
        last.get_pixel(100, 100)[0] > 240,
        "the returning red scene must remain"
    );
    let sheet = image::open(folder.join("contact-sheet-001.jpg")).unwrap();
    assert!(sheet.width() > 1000 && sheet.height() > 200);
    let second = result_folder(&checked(&mut cli(&source, &output)));
    assert_ne!(folder, second);
    assert_eq!(fs::read_dir(&output).unwrap().count(), 2);
    assert_eq!(fs::read_dir(folder.join("images")).unwrap().count(), 4);
}

#[test]
fn range_and_frame_limit_preserve_timestamps_and_late_coverage() {
    let root = TempDir::new().unwrap();
    let source = fixture(root.path(), "640x360");
    let output = root.path().join("output");
    let folder = result_folder(&checked(
        cli(&source, &output).args(["--start", "00:02.23", "--end", "00:05.9", "--format", "png"]),
    ));
    let info = manifest(&folder);
    let frames = info["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2, "{info}");
    for frame in frames {
        let time = frame["timestamp_seconds"].as_f64().unwrap();
        assert!((2.23..5.9).contains(&time), "{time}");
        assert!(frame["file"].as_str().unwrap().ends_with(".png"));
    }
    let capped = result_folder(&checked(cli(&source, &output).args(["--max-frames", "2"])));
    let info = manifest(&capped);
    let frames = info["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames[0]["timestamp_seconds"].as_f64().unwrap() < 1.0);
    assert!(frames[1]["timestamp_seconds"].as_f64().unwrap() > 6.0);
}

#[test]
fn exports_real_4k_and_downscales_without_upscaling() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("4k.mp4");
    generate_video(&source, "color=c=blue:s=3840x2160:r=1:d=1", &[]);
    let output = root.path().join("output");
    for (resolution, dimensions) in [("2160p", (3840, 2160)), ("720p", (1280, 720))] {
        let folder = result_folder(&checked(
            cli(&source, &output).args(["--resolution", resolution]),
        ));
        let info = manifest(&folder);
        let file = info["frames"][0]["file"].as_str().unwrap();
        let image = image::open(folder.join(file)).unwrap();
        assert_eq!((image.width(), image.height()), dimensions);
    }
}

#[test]
fn invalid_arguments_and_unreadable_inputs_fail_cleanly() {
    for args in [
        vec!["missing.mp4"],
        vec!["--start", "NaN", "missing.mp4"],
        vec!["--max-frames", "0", "missing.mp4"],
        vec!["--resolution", "potato", "missing.mp4"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_vframes"))
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
    }
    let root = TempDir::new().unwrap();
    let source = root.path().join("broken.mp4");
    fs::write(&source, "not a video").unwrap();
    let output = root.path().join("output");
    let result = cli(&source, &output).output().unwrap();
    assert!(!result.status.success());
    assert!(!output.exists());
}

#[test]
fn portrait_variable_rate_video_keeps_aspect_ratio_and_actual_timestamps() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("portrait-vfr.mp4");
    generate_video(
        &source,
        "testsrc2=s=360x640:r=12:d=3",
        &[
            "-vf",
            "select='if(lt(t,1),not(mod(n,3)),not(mod(n,2)))'",
            "-fps_mode",
            "vfr",
        ],
    );
    let output = root.path().join("output");
    let folder = result_folder(&checked(cli(&source, &output).args([
        "--start",
        "0.17",
        "--end",
        "2.9",
        "--resolution",
        "480p",
        "--sensitivity",
        "high",
        "--sample-rate",
        "6",
        "--min-gap",
        "0.25",
        "--settle",
        "0",
        "--max-frames",
        "5",
    ])));
    let info = manifest(&folder);
    let probe = checked(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "V:0",
                "-show_frames",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "json",
            ])
            .arg(&source),
    );
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let source_times: Vec<f64> = probe["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            f["best_effort_timestamp_time"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    let frames = info["frames"].as_array().unwrap();
    assert!(frames.len() >= 2);
    for frame in frames {
        let image = image::open(folder.join(frame["file"].as_str().unwrap())).unwrap();
        assert_eq!((image.width(), image.height()), (270, 480));
        let time = frame["timestamp_seconds"].as_f64().unwrap();
        assert!((0.17..2.9).contains(&time));
        assert!(
            source_times
                .iter()
                .any(|source| (time - source).abs() < 1e-5),
            "invented timestamp: {time}"
        );
    }
}

#[test]
fn rotated_anamorphic_video_exports_square_pixels_in_display_orientation() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("anamorphic.mp4");
    generate_video(
        &source,
        "testsrc2=s=320x240:r=4:d=2",
        &["-vf", "setsar=2/1"],
    );
    let rotated = root.path().join("rotated.mp4");
    checked(
        ffmpeg()
            .args(["-display_rotation:v:0", "90", "-i"])
            .arg(&source)
            .args(["-c", "copy"])
            .arg(&rotated),
    );
    let folder = result_folder(&checked(cli(&rotated, &root.path().join("output")).args([
        "--resolution",
        "original",
        "--max-frames",
        "1",
    ])));
    let info = manifest(&folder);
    let image = image::open(folder.join(info["frames"][0]["file"].as_str().unwrap())).unwrap();
    assert_eq!((image.width(), image.height()), (240, 640));
}

#[test]
fn contact_sheets_paginate() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("motion.mp4");
    generate_video(&source, "testsrc2=s=320x180:r=8:d=10", &[]);
    let folder = result_folder(&checked(cli(&source, &root.path().join("output")).args([
        "--sensitivity",
        "high",
        "--min-gap",
        "0.25",
        "--settle",
        "0",
        "--max-frames",
        "25",
    ])));
    let info = manifest(&folder);
    assert_eq!(info["frames"].as_array().unwrap().len(), 25);
    assert_eq!(info["contact_sheets"].as_array().unwrap().len(), 2);
    assert!(folder.join("contact-sheet-002.jpg").is_file());
}

#[test]
fn very_slow_source_exports_held_scenes_at_real_timestamps() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("one-frame-per-second.mp4");
    let graph = concat!(
        "color=red:s=320x180:r=1:d=1[a];",
        "color=green:s=320x180:r=1:d=1[b];",
        "color=blue:s=320x180:r=1:d=1[c];",
        "[a][b][c]concat=n=3:v=1:a=0",
    );
    generate_video(&source, graph, &["-r", "1"]);
    let folder = result_folder(&checked(&mut cli(&source, &root.path().join("output"))));
    let info = manifest(&folder);
    let frames = info["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3, "{info}");
    for (index, frame) in frames.iter().enumerate() {
        assert!((frame["timestamp_seconds"].as_f64().unwrap() - index as f64).abs() < 1e-6);
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::{
        os::unix::fs::PermissionsExt,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn test_path(bin: &Path) -> std::ffi::OsString {
        let mut paths = vec![bin.to_owned()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        std::env::join_paths(paths).unwrap()
    }

    #[test]
    fn cache_reuses_sources_across_links_and_redownloads_for_higher_resolution() {
        let root = TempDir::new().unwrap();
        let source = fixture(root.path(), "320x180");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let calls = root.path().join("calls");
        executable(
            &bin.join("yt-dlp"),
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'test-downloader'; exit 0; fi
while [ "$#" -gt 0 ]; do
  case "$1" in --paths) shift; dest="$1";; esac
  shift
done
echo "$dest" >> "$TEST_CALLS"
cp "$TEST_VIDEO" "$dest/source.mp4"
echo '{"id":"BaW_jenozKc","title":"Offline cache fixture","is_live":false}' > "$dest/source.info.json"
echo "$dest/source.mp4"
"#,
        );
        let cache = root.path().join("cache");
        let out = root.path().join("output");
        for (url, resolution, expected_downloads) in [
            ("https://youtu.be/BaW_jenozKc", "1080p", 1),
            (
                "https://www.youtube.com/watch?v=BaW_jenozKc&t=10",
                "720p",
                1,
            ),
            ("https://youtu.be/BaW_jenozKc", "2160p", 2),
            ("https://youtu.be/BaW_jenozKc", "original", 3),
            ("https://youtu.be/BaW_jenozKc", "1440p", 3),
        ] {
            checked(
                Command::new(env!("CARGO_BIN_EXE_vframes"))
                    .args([url, "--no-open", "--resolution", resolution])
                    .arg("--cache-dir")
                    .arg(&cache)
                    .arg("--output")
                    .arg(&out)
                    .env("PATH", test_path(&bin))
                    .env("TEST_CALLS", &calls)
                    .env("TEST_VIDEO", &source),
            );
            assert_eq!(
                fs::read_to_string(&calls).unwrap().lines().count(),
                expected_downloads
            );
        }
        assert!(
            !fs::read_dir(cache.join("BaW_jenozKc")).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.'))
        );
    }

    #[test]
    fn cancellation_kills_subprocesses_and_removes_incomplete_output() {
        let root = TempDir::new().unwrap();
        let source = fixture(root.path(), "320x180");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let ready = root.path().join("ready");
        let ffmpeg = checked(Command::new("which").arg("ffmpeg"));
        let ffmpeg = String::from_utf8(ffmpeg.stdout).unwrap().trim().to_owned();
        executable(
            &bin.join("ffmpeg"),
            r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "-/filter:v" ]; then
    echo "$$" > "$TEST_READY"
    while :; do sleep 1; done
  fi
done
exec "$REAL_FFMPEG" "$@"
"#,
        );
        let output = root.path().join("output");
        let mut child = cli(&source, &output)
            .env("PATH", test_path(&bin))
            .env("REAL_FFMPEG", ffmpeg)
            .env("TEST_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "export did not start");
            assert!(
                child.try_wait().unwrap().is_none(),
                "CLI exited before export"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let decoder_pid: i32 = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        kill(Pid::from_raw(child.id() as i32), Signal::SIGINT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "cancellation timed out");
            thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(status.code(), Some(130));
        assert_eq!(
            fs::read_dir(&output).unwrap().count(),
            0,
            "partial output remains"
        );
        assert!(
            kill(Pid::from_raw(decoder_pid), None).is_err(),
            "decoder was left running"
        );
    }

    #[test]
    fn export_failure_removes_partial_images() {
        let root = TempDir::new().unwrap();
        let source = fixture(root.path(), "320x180");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let real = checked(Command::new("which").arg("ffmpeg"));
        executable(
            &bin.join("ffmpeg"),
            r#"#!/bin/sh
exporting=no
for arg in "$@"; do
  if [ "$arg" = "-/filter:v" ]; then exporting=yes; fi
  last="$arg"
done
if [ "$exporting" = yes ]; then
  echo 'partial image' > "${last%/*}/000001.jpg"
  echo 'intentional export failure' >&2
  exit 42
fi
exec "$REAL_FFMPEG" "$@"
"#,
        );
        let output = root.path().join("output");
        let result = cli(&source, &output)
            .env("PATH", test_path(&bin))
            .env(
                "REAL_FFMPEG",
                String::from_utf8(real.stdout).unwrap().trim(),
            )
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("intentional export failure"));
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_opens_the_completed_images_subfolder() {
        let root = TempDir::new().unwrap();
        let source = fixture(root.path(), "320x180");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let opened = root.path().join("opened");
        executable(
            &bin.join("open"),
            "#!/bin/sh\nprintf '%s' \"$1\" > \"$TEST_OPENED\"\n",
        );
        let output = root.path().join("output");
        let result = checked(
            Command::new(env!("CARGO_BIN_EXE_vframes"))
                .arg(&source)
                .arg("--output")
                .arg(&output)
                .args(["--max-frames", "1"])
                .env("PATH", test_path(&bin))
                .env("TEST_OPENED", &opened),
        );
        let folder = result_folder(&result);
        let requested = PathBuf::from(fs::read_to_string(opened).unwrap());
        assert_eq!(requested, folder.join("images"));
        assert!(requested.is_dir());
        assert!(folder.join("manifest.json").is_file());
    }
}
