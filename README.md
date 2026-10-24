# vframes

Turn a YouTube link or local video into timestamped images.

## Install

```sh
brew install ffmpeg yt-dlp deno
cargo install --path . --locked --root ~/.local
vframes doctor
```

## Use

```sh
# Paste a link and choose a resolution
vframes

# Export at up to 4K and reuse the download on later runs
vframes "YOUTUBE_URL" --resolution 2160p --cache

# Capture a section with a frame limit
vframes "YOUTUBE_URL" --start 02:00 --end 08:30 --max-frames 100

# Use a local video, PNG images, and a custom destination
vframes "/path/to/video.mp4" --format png --output ~/Pictures/Frames
```

Each run creates a unique folder under `~/Pictures/VideoFrames` containing an `images/` subfolder, then opens the images in Finder.

### CLI Flags

- `--resolution`, `-r`: `480p`, `720p`, `1080p` (default), `1440p`, `2160p`/`4k`, or `original`.
- `--format`, `-f`: `jpg` (default) or `png`.
- `--sensitivity`, `-s`: `low`, `medium` (default), or `high`. Higher captures smaller changes.
- `--start`: Start time; defaults to `0`.
- `--end`: Exclusive end time; defaults to the end of the video.
- `--max-frames`: Maximum images across the selected timeline; `1–10000`, default `200`.
- `--min-gap`: Minimum seconds between images; default `1`.
- `--settle`: Seconds to let a changed image stabilise; `0–10`, default `0.5`.
- `--sample-rate`: Analysis samples per second; `1–30`, default `4`.
- `--output`, `-o`: Parent directory for the new result folder.
- `--cache`: Keep and reuse downloaded sources; off by default.
- `--cache-dir`: Custom cache directory; also enables caching.
- `--no-open`: Save without opening Finder.
- `--help`, `-h`: Show usage and options.
- `--version`, `-V`: Show the installed version.
