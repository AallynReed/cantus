//! yt-dlp, ffmpeg and rclone, run as separate programs the user installs.
//!
//! Arguments are always passed as a list and never through a shell, and user-supplied values
//! follow `--` so they can't be read as options.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::scan::is_audio;

const LAST_ERROR_LINES: usize = 3;

/// The tool's version line, or `None` if it isn't installed.
pub fn tool_version(program: &Path, version_flag: &str) -> Option<String> {
    let output = run(program, &[version_flag.to_owned()]).ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.lines().next().unwrap_or_default().trim().to_owned())
}

/// Downloads a URL's audio into `dest` and returns the audio files created.
pub fn download_audio(yt_dlp: &Path, url: &str, dest: &Path) -> Result<Vec<PathBuf>, String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http and https links can be imported".into());
    }
    let dest_text = dest
        .to_str()
        .ok_or("the download folder name is not valid text")?;
    let mut args: Vec<String> = [
        "--ignore-config",
        "--no-playlist",
        "--no-progress",
        "--extract-audio",
        "--audio-format",
        "best",
        "--print",
        "after_move:filepath",
        "--output",
        "%(title)s.%(ext)s",
        "--paths",
    ]
    .map(str::to_owned)
    .to_vec();
    args.push(dest_text.to_owned());
    args.push("--".to_owned());
    args.push(url.to_owned());

    let output = checked(run(yt_dlp, &args)?)?;
    let files: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .filter(|path| path.starts_with(dest) && is_audio(path) && path.is_file())
        .collect();
    if files.is_empty() {
        return Err("the download produced no audio file".into());
    }
    Ok(files)
}

/// A previewed sync. Only `preview_sync` makes one, so nothing can sync, and above all
/// nothing can delete remotely, without a dry run first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPlan {
    source: PathBuf,
    remote: String,
    mirror: bool,
    pub copies: usize,
    pub deletes: usize,
}

/// Dry-runs a sync of `source` to an rclone remote like `name:folder`. `mirror` also deletes
/// remote files that are gone locally.
pub fn preview_sync(
    rclone: &Path,
    source: &Path,
    remote: &str,
    mirror: bool,
) -> Result<SyncPlan, String> {
    if remote.starts_with('-') || !remote.contains(':') {
        return Err("the remote should look like name:folder".into());
    }
    let output = checked(run(rclone, &sync_args(source, remote, mirror, true)?)?)?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let count = |needle: &str| log.lines().filter(|l| l.contains(needle)).count();
    Ok(SyncPlan {
        source: source.to_path_buf(),
        remote: remote.to_owned(),
        mirror,
        copies: count("Skipped copy") + count("Skipped update"),
        deletes: count("Skipped delete"),
    })
}

pub fn run_sync(rclone: &Path, plan: &SyncPlan) -> Result<(), String> {
    let args = sync_args(&plan.source, &plan.remote, plan.mirror, false)?;
    checked(run(rclone, &args)?).map(drop)
}

fn sync_args(
    source: &Path,
    remote: &str,
    mirror: bool,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let source = source.to_str().ok_or("the folder name is not valid text")?;
    let mut args = vec![if mirror { "sync" } else { "copy" }.to_owned()];
    if dry_run {
        args.push("--dry-run".to_owned());
    }
    args.extend(["--".to_owned(), source.to_owned(), remote.to_owned()]);
    Ok(args)
}

fn run(program: &Path, args: &[String]) -> Result<Output, String> {
    let mut command = Command::new(program);
    command.args(args);
    hide_console(&mut command);
    command
        .output()
        .map_err(|err| format!("cannot run {}: {err}", program.display()))
}

fn checked(output: Output) -> Result<Output, String> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    let tail = lines[lines.len().saturating_sub(LAST_ERROR_LINES)..].join(" ");
    Err(if tail.trim().is_empty() {
        format!("the tool failed with {}", output.status)
    } else {
        tail
    })
}

/// A GUI app would otherwise flash a console window for every tool it runs.
#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}
