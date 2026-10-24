use crate::{
    cli::{Cli, Resolution},
    media,
    process::{self, Cancellation},
    storage,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;
use url::Url;

pub struct Source {
    pub path: PathBuf,
    pub video: media::Video,
    pub info: SourceInfo,
    // Own temporary downloads until export finishes.
    _temporary: Option<TempDir>,
}

#[derive(Debug)]
pub struct SourceInfo {
    pub title: String,
}

#[derive(Serialize, Deserialize)]
struct CacheRecord {
    schema_version: u32,
    requested_resolution: Option<u32>,
    filename: String,
    title: String,
    video_id: String,
}

#[derive(Deserialize)]
struct DownloadInfo {
    title: Option<String>,
    id: String,
    is_live: Option<bool>,
}

pub fn resolve(input: &str, cli: &Cli, cancel: &Cancellation) -> Result<Source> {
    let resolution = cli.settings.resolution;
    if Path::new(input).is_file() {
        let path = fs::canonicalize(input)?;
        let title = path
            .file_stem()
            .context("Invalid video filename")?
            .to_string_lossy()
            .into_owned();
        return Ok(Source {
            video: media::probe(&path, cancel)?,
            info: SourceInfo { title },
            path,
            _temporary: None,
        });
    }
    let (url, id) =
        youtube_url(input).context("Expected an existing local video or a YouTube video URL")?;
    let cache_root = if cli.cache || cli.cache_dir.is_some() {
        Some(
            cli.cache_dir
                .clone()
                .or_else(|| dirs::cache_dir().map(|p| p.join("vframes")))
                .context("Cannot locate a cache directory; specify --cache-dir")?
                .join(&id),
        )
    } else {
        None
    };
    if let Some(root) = &cache_root {
        if let Some(source) = cached_source(root, &id, resolution, cancel)? {
            eprintln!("Reusing cached source: {}", source.path.display());
            return Ok(source);
        }
        if root.exists() {
            eprintln!(
                "No suitable cached source for {}; downloading a new source.",
                resolution.label()
            );
        }
    }

    process::require("yt-dlp", cancel)?;
    let runtime = process::js_runtime(cancel)?;
    if runtime.is_none() {
        eprintln!(
            "No supported JavaScript runtime found. YouTube downloads may fail; install Deno or Node.js 22+."
        );
    }
    let staging = if let Some(root) = &cache_root {
        fs::create_dir_all(root)?;
        tempfile::Builder::new()
            .prefix(".partial-")
            .tempdir_in(root)?
    } else {
        tempfile::Builder::new()
            .prefix("vframes-source-")
            .tempdir()?
    };
    eprintln!(
        "Downloading video ({} source preference)…",
        resolution.label()
    );
    let mut command = Command::new("yt-dlp");
    command.args([
        "--ignore-config",
        "--no-playlist",
        "--no-progress",
        "--no-warnings",
        "--socket-timeout",
        "15",
        "--retries",
        "2",
        "--fragment-retries",
        "2",
        "--match-filters",
        "!is_live",
        "--format",
        "bv/b",
        "--write-info-json",
        "--print",
        "after_move:filepath",
        "--no-simulate",
        "--no-overwrites",
    ]);
    if let Some(runtime) = runtime {
        command.args(["--js-runtimes", runtime]);
    }
    if let Some(height) = resolution.0 {
        command.args(["--format-sort", &format!("res:{height}")]);
    }
    command
        .arg("--paths")
        .arg(staging.path())
        .args(["--output", "source.%(ext)s", "--", &url]);
    let output = process::capture(&mut command, cancel)
        .context("YouTube download failed. Run vframes doctor; updating yt-dlp can fix YouTube changes. Restricted videos may require authentication.")?;
    let reported = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .context("yt-dlp did not report a downloaded file")?;
    let path = fs::canonicalize(reported.trim()).context("Downloaded file is missing")?;
    let parent = fs::canonicalize(staging.path())?;
    ensure!(
        path.parent() == Some(parent.as_path()),
        "Downloader reported a path outside its working folder"
    );
    let metadata: DownloadInfo =
        serde_json::from_slice(&fs::read(staging.path().join("source.info.json"))?)
            .context("Cannot read downloaded video metadata")?;
    ensure!(
        !metadata.is_live.unwrap_or(false),
        "Live streams are not supported"
    );
    ensure!(
        metadata.id == id,
        "Downloaded video ID does not match the requested video"
    );
    let video = media::probe(&path, cancel).context("Downloaded source is not a readable video")?;
    let title = metadata
        .title
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    let filename = path
        .file_name()
        .context("Missing downloaded filename")?
        .to_string_lossy()
        .into_owned();
    let info = SourceInfo {
        title: title.clone(),
    };
    if cache_root.is_some() {
        let record = CacheRecord {
            schema_version: 1,
            requested_resolution: resolution.0,
            filename: filename.clone(),
            title,
            video_id: id,
        };
        fs::write(
            staging.path().join("cache.json"),
            serde_json::to_vec_pretty(&record)?,
        )?;
        let destination = storage::publish(staging, &resolution.label(), cancel)?;
        Ok(Source {
            path: destination.join(filename),
            video,
            info,
            _temporary: None,
        })
    } else {
        Ok(Source {
            path,
            video,
            info,
            _temporary: Some(staging),
        })
    }
}

fn cached_source(
    root: &Path,
    id: &str,
    resolution: Resolution,
    cancel: &Cancellation,
) -> Result<Option<Source>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(root)? {
        cancel.check()?;
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path().join("cache.json")) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<CacheRecord>(&bytes) else {
            continue;
        };
        if record.schema_version != 1
            || record.video_id != id
            || !Resolution(record.requested_resolution).satisfies(resolution)
        {
            continue;
        }
        if Path::new(&record.filename).components().count() != 1
            || !record.filename.starts_with("source.")
        {
            continue;
        }
        entries.push((entry.path(), record));
    }
    entries.sort_by_key(|(_, record)| record.requested_resolution.unwrap_or(u32::MAX));
    for (entry, record) in entries {
        let path = entry.join(record.filename);
        match media::probe(&path, cancel) {
            Ok(video) => {
                return Ok(Some(Source {
                    path,
                    video,
                    info: SourceInfo {
                        title: record.title,
                    },
                    _temporary: None,
                }));
            }
            Err(_) => {
                cancel.check()?;
                eprintln!("Ignoring unreadable cached source: {}", path.display());
            }
        }
    }
    Ok(None)
}

fn youtube_url(input: &str) -> Result<(String, String)> {
    let url = Url::parse(input)?;
    ensure!(
        matches!(url.scheme(), "https" | "http"),
        "Use an HTTP(S) YouTube URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.port().is_none(),
        "Invalid YouTube URL"
    );
    let host = url.host_str().context("URL has no hostname")?;
    let id = match host {
        "youtu.be" => url
            .path_segments()
            .and_then(|mut s| s.next())
            .map(str::to_owned),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            if url.path() == "/watch" {
                url.query_pairs()
                    .find(|(key, _)| key == "v")
                    .map(|(_, value)| value.into_owned())
            } else {
                let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
                if parts.len() >= 2 && matches!(parts[0], "shorts" | "embed" | "live") {
                    Some(parts[1].to_owned())
                } else {
                    None
                }
            }
        }
        _ => bail!("Only YouTube URLs and local files are supported"),
    }
    .context("URL must point to a single video")?;
    ensure!(
        id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid YouTube video ID"
    );
    Ok((format!("https://www.youtube.com/watch?v={id}"), id))
}

pub fn safe_name(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches([' ', '.']);
    // Keep UTF-8 intact and leave room for the suffix on filesystems with 255-byte names.
    let mut name = String::new();
    for c in cleaned.chars() {
        if name.len() + c.len_utf8() > 120 {
            break;
        }
        name.push(c);
    }
    if name.is_empty() {
        "video".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_video_links_and_rejects_other_hosts() {
        for input in [
            "https://youtu.be/BaW_jenozKc?t=30",
            "https://www.youtube.com/watch?v=BaW_jenozKc&list=example",
            "https://m.youtube.com/shorts/BaW_jenozKc",
        ] {
            assert_eq!(
                youtube_url(input).unwrap().0,
                "https://www.youtube.com/watch?v=BaW_jenozKc"
            );
        }
        for bad in [
            "https://youtube.com.evil.test/watch?v=BaW_jenozKc",
            "https://youtube.com/playlist?list=x",
            "file:///etc/passwd",
            "https://youtube.com/watch?v=../../etc",
            "https://youtu.be/short",
        ] {
            assert!(youtube_url(bad).is_err());
        }
    }
    #[test]
    fn filenames_are_safe_and_byte_bounded() {
        assert_eq!(safe_name("../hello:world/video"), "_hello_world_video");
        assert_eq!(safe_name("..."), "video");
        assert!(safe_name(&"é".repeat(200)).len() <= 120);
    }
}
