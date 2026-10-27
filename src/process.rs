use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn install(&self) -> Result<()> {
        let flag = self.0.clone();
        ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))?;
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.is_cancelled(),
            "Cancelled; temporary downloads and incomplete output were removed."
        );
        Ok(())
    }
}

pub struct Running {
    child: Child,
    stderr: tempfile::NamedTempFile,
    label: String,
}

impl Running {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let stderr = tempfile::NamedTempFile::new()?;
        let label = command.get_program().to_string_lossy().into_owned();
        let child = command
            .stdin(Stdio::null())
            .stderr(stderr.reopen()?)
            .spawn()
            .with_context(|| {
                format!("Cannot start {label}. Run vframes doctor to check dependencies.")
            })?;
        Ok(Self {
            child,
            stderr,
            label,
        })
    }
    pub fn stdout(&mut self) -> Result<std::process::ChildStdout> {
        self.child
            .stdout
            .take()
            .context("Subprocess stdout was not piped")
    }
    pub fn wait(&mut self, cancel: &Cancellation) -> Result<()> {
        loop {
            cancel.check()?;
            if let Some(status) = self.child.try_wait()? {
                if !status.success() {
                    let log = self.log_tail()?;
                    bail!("{} failed ({status}):\n{log}", self.label);
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    fn log_tail(&mut self) -> Result<String> {
        let length = self.stderr.as_file().metadata()?.len();
        self.stderr
            .seek(SeekFrom::Start(length.saturating_sub(16_384)))?;
        let mut bytes = Vec::new();
        self.stderr.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
    pub fn log_reader(&self) -> Result<BufReader<File>> {
        Ok(BufReader::new(self.stderr.reopen()?))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.child.try_wait().ok().flatten().is_none() {
            // yt-dlp may launch a JS runtime: cancel the entire owned process group.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(self.child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn capture(command: &mut Command, cancel: &Cancellation) -> Result<String> {
    cancel.check()?;
    let mut stdout = tempfile::tempfile()?;
    let mut running = Running::spawn(command.stdout(stdout.try_clone()?))?;
    running.wait(cancel)?;
    ensure!(
        stdout.metadata()?.len() <= 16 * 1024 * 1024,
        "Subprocess metadata exceeds 16 MiB"
    );
    stdout.rewind()?;
    let mut output = String::new();
    stdout.read_to_string(&mut output)?;
    Ok(output)
}

pub fn require(program: &str, cancel: &Cancellation) -> Result<String> {
    let flag = if matches!(program, "ffmpeg" | "ffprobe") {
        "-version"
    } else {
        "--version"
    };
    let output = capture(Command::new(program).arg(flag), cancel)?;
    let version = output
        .lines()
        .next()
        .unwrap_or("unknown version")
        .to_owned();
    if matches!(program, "ffmpeg" | "ffprobe") {
        let major = version
            .split_whitespace()
            .nth(2)
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u32>().ok());
        ensure!(
            major.is_none_or(|major| major >= 7),
            "{program} 7 or newer is required (found {version})"
        );
    }
    Ok(version)
}

pub fn doctor(cancel: &Cancellation) -> Result<()> {
    let mut missing = false;
    for program in ["ffmpeg", "ffprobe", "yt-dlp"] {
        match require(program, cancel) {
            Ok(version) => println!("{program}: {version}"),
            Err(error) => {
                cancel.check()?;
                missing = true;
                println!("{program}: {error:#}");
            }
        }
    }
    match js_runtime(cancel)? {
        Some(runtime) => println!("YouTube JavaScript runtime: {runtime}"),
        None => {
            missing = true;
            println!(
                "YouTube JavaScript runtime: missing (install Deno 2.3+ or Node.js 22+; X/Twitter and local videos still work)"
            );
        }
    }
    if missing {
        bail!("Install missing dependencies. On macOS: brew install ffmpeg yt-dlp deno");
    }
    Ok(())
}

pub fn js_runtime(cancel: &Cancellation) -> Result<Option<&'static str>> {
    for program in ["deno", "node"] {
        if let Ok(version) = require(program, cancel) {
            if program == "node" {
                let major = version
                    .trim_start_matches('v')
                    .split('.')
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0);
                if major < 22 {
                    continue;
                }
            } else {
                let version = version.strip_prefix("deno ").unwrap_or(&version);
                let mut parts = version.split('.').filter_map(|v| v.parse::<u32>().ok());
                let supported = (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (2, 3);
                if !supported {
                    continue;
                }
            }
            return Ok(Some(program));
        }
        cancel.check()?;
    }
    Ok(None)
}

pub fn open_folder(path: &Path, cancel: &Cancellation) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    capture(command.arg(path), cancel)?;
    Ok(())
}
