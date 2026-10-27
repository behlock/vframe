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
    display_id: Option<String>,
    is_live: Option<bool>,
}

#[derive(Debug)]
struct RemoteVideo {
    url: String,
    id: String,
    cache_key: String,
    is_x: bool,
}

impl RemoteVideo {
    fn matches(&self, metadata: &DownloadInfo) -> bool {
        if self.is_x {
            // X media IDs differ from the post ID, which yt-dlp calls display_id.
            metadata.display_id.as_deref() == Some(self.id.as_str())
        } else {
            metadata.id == self.id
        }
    }
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
    let remote = remote_video(input).context(
        "Expected an existing local video, a YouTube video URL, or an X/Twitter post URL",
    )?;
    let id = &remote.cache_key;
    let cache_root = if cli.cache || cli.cache_dir.is_some() {
        Some(
            cli.cache_dir
                .clone()
                .or_else(|| dirs::cache_dir().map(|p| p.join("vframes")))
                .context("Cannot locate a cache directory; specify --cache-dir")?
                .join(id),
        )
    } else {
        None
    };
    if let Some(root) = &cache_root {
        if let Some(source) = cached_source(root, id, resolution, cancel)? {
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
    let runtime = if remote.is_x {
        None
    } else {
        process::js_runtime(cancel)?
    };
    if !remote.is_x && runtime.is_none() {
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
    if remote.is_x {
        // A post can yield a playlist even with --no-playlist. Export one video
        // and keep playlist metadata from replacing source.info.json.
        command.args(["--playlist-items", "1", "--no-write-playlist-metafiles"]);
    }
    if let Some(runtime) = runtime {
        command.args(["--js-runtimes", runtime]);
    }
    if let Some(height) = resolution.0 {
        command.args(["--format-sort", &format!("res:{height}")]);
    }
    command.arg("--paths").arg(staging.path()).args([
        "--output",
        "source.%(ext)s",
        "--",
        &remote.url,
    ]);
    let output = process::capture(&mut command, cancel)
        .context("Video download failed. Run vframes doctor; updating yt-dlp can fix site changes. The post must contain an accessible video; restricted videos may require authentication.")?;
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
        remote.matches(&metadata),
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
            video_id: id.clone(),
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

fn remote_video(input: &str) -> Result<RemoteVideo> {
    let url = Url::parse(input)?;
    ensure!(
        matches!(url.scheme(), "https" | "http"),
        "Use an HTTP(S) YouTube or X/Twitter URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.port().is_none(),
        "Invalid video URL"
    );
    let host = url.host_str().context("URL has no hostname")?;
    if matches!(
        host,
        "x.com"
            | "www.x.com"
            | "m.x.com"
            | "mobile.x.com"
            | "twitter.com"
            | "www.twitter.com"
            | "m.twitter.com"
            | "mobile.twitter.com"
    ) {
        let parts: Vec<_> = url
            .path()
            .trim_end_matches('/')
            .split('/')
            .skip(1)
            .collect();
        let (id, suffix) = match parts.as_slice() {
            ["i", "web", "status", id, rest @ ..] | ["statuses", id, rest @ ..] => (*id, rest),
            [user, "status", id, rest @ ..]
                if !user.is_empty()
                    && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
            {
                (*id, rest)
            }
            _ => bail!("X/Twitter URL must point to a single post"),
        };
        ensure!(
            !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()),
            "Invalid X/Twitter post ID"
        );
        let index = match suffix {
            [] => None,
            ["video", index] => Some(
                index
                    .parse::<u32>()
                    .ok()
                    .filter(|index| *index > 0)
                    .context("Invalid X/Twitter video number")?,
            ),
            _ => bail!("X/Twitter URL must point to a post or /video/NUMBER"),
        };
        let suffix = index.map_or_else(String::new, |index| format!("/video/{index}"));
        let cache_suffix = index.map_or_else(String::new, |index| format!("-video-{index}"));
        return Ok(RemoteVideo {
            url: format!("https://x.com/i/web/status/{id}{suffix}"),
            id: id.to_owned(),
            cache_key: format!("x-{id}{cache_suffix}"),
            is_x: true,
        });
    }
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
        _ => bail!("Only YouTube URLs, X/Twitter post URLs, and local files are supported"),
    }
    .context("URL must point to a single video")?;
    ensure!(
        id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid YouTube video ID"
    );
    Ok(RemoteVideo {
        url: format!("https://www.youtube.com/watch?v={id}"),
        cache_key: id.clone(),
        id,
        is_x: false,
    })
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
                remote_video(input).unwrap().url,
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
            assert!(remote_video(bad).is_err());
        }
    }
    #[test]
    fn normalizes_x_posts_and_preserves_video_selection() {
        for input in [
            "https://x.com/body___shop/status/2114798995340144722?s=20",
            "https://www.x.com/body___shop/status/2114798995340144722/",
            "http://mobile.twitter.com/body___shop/status/2114798995340144722#video",
            "https://twitter.com/i/web/status/2114798995340144722",
            "https://x.com/statuses/2114798995340144722",
        ] {
            let remote = remote_video(input).unwrap();
            assert!(remote.is_x);
            assert_eq!(remote.url, "https://x.com/i/web/status/2114798995340144722");
            assert_eq!(remote.cache_key, "x-2114798995340144722");
        }
        let remote =
            remote_video("https://x.com/test/status/2114798995340144722/video/2?s=20").unwrap();
        assert_eq!(
            remote.url,
            "https://x.com/i/web/status/2114798995340144722/video/2"
        );
        assert_eq!(remote.cache_key, "x-2114798995340144722-video-2");
        for bad in [
            "https://x.com.evil.test/test/status/123",
            "https://x.com@test.example/test/status/123",
            "https://user@x.com/test/status/123",
            "https://x.com:123/test/status/123",
            "ftp://x.com/test/status/123",
            "https://x.com/test",
            "https://x.com/test/status/",
            "https://x.com/test/status/not-a-number",
            "https://x.com/test/status/123/video/0",
            "https://x.com/test/status/123/video/2/extra",
            "https://x.com/test/status/123/photo/1",
            "https://x.com/i/spaces/123",
        ] {
            assert!(remote_video(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn checks_x_post_identity_separately_from_media_identity() {
        let remote = remote_video("https://x.com/test/status/123").unwrap();
        let mut metadata = DownloadInfo {
            id: "456".into(),
            display_id: Some("123".into()),
            title: None,
            is_live: Some(false),
        };
        assert!(remote.matches(&metadata));
        metadata.display_id = Some("789".into());
        assert!(!remote.matches(&metadata));
        metadata.display_id = None;
        assert!(!remote.matches(&metadata));
        let youtube = remote_video("https://youtu.be/BaW_jenozKc").unwrap();
        metadata.id = youtube.id.clone();
        assert!(youtube.matches(&metadata));
        metadata.id = "wrong-video".into();
        metadata.display_id = Some(youtube.id.clone());
        assert!(!youtube.matches(&metadata));
    }

    #[test]
    fn filenames_are_safe_and_byte_bounded() {
        assert_eq!(safe_name("../hello:world/video"), "_hello_world_video");
        assert_eq!(safe_name("..."), "video");
        assert!(safe_name(&"é".repeat(200)).len() <= 120);
    }
}
