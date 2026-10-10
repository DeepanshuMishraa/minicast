//! Stream state shown by the GUI, read from the registry the CLI writes
//! (`streams.json` + the per-stream log), so the CLI and GUI always agree.
use crate::streams;
use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

/// A stream is "stalled" when its log has not grown for this long. ffmpeg
/// prints a stats line every 5 s, so three missed lines means no progress.
const STALL_AFTER: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub enum Phase {
    Offline,
    Starting,
    Live(Live),
    /// A setting changed while live: the stream is restarting to pick it up.
    Applying,
    Stopping,
}

#[derive(Clone, Debug)]
pub struct Live {
    pub source: Source,
    pub started_at: u64,
    pub platform: String,
    pub output: String,
    pub health: Health,
}

/// Where a running stream came from, which decides how it is stopped.
#[derive(Clone, Debug)]
pub enum Source {
    /// Started with `minicast start --detach` (or this GUI); in the registry.
    Detached { id: String },
    /// An ffmpeg started from a terminal. Only its pid is known.
    Terminal { pid: u32 },
}

#[derive(Clone, Copy, Debug)]
pub enum Health {
    /// Process is up, no stats line in the log yet.
    Warming,
    Sending { fps: f32, speed: f32 },
    Stalled,
    /// A terminal stream writes no log we can read.
    Unmonitored,
}

/// Read the registry once. Returns `Offline` when nothing is running.
pub fn poll() -> Phase {
    if let Some(rec) = streams::alive_records().ok().and_then(|r| r.into_iter().next()) {
        return Phase::Live(Live {
            health: health(&rec.log_path),
            source: Source::Detached { id: rec.id },
            started_at: rec.started_at,
            platform: rec.platform,
            output: rec.output,
        });
    }
    terminal_stream().map_or(Phase::Offline, Phase::Live)
}

/// A stream ffmpeg that minicast started in a terminal: it publishes through
/// the fifo muxer to an rtmp(s) URL, which no other ffmpeg job here does.
fn terminal_stream() -> Option<Live> {
    let out = std::process::Command::new("ps").args(["-axo", "pid=,etime=,command="]).output().ok()?;
    parse_terminal_stream(&String::from_utf8_lossy(&out.stdout), streams::unix_now())
}

fn parse_terminal_stream(ps: &str, now: u64) -> Option<Live> {
    ps.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let pid: u32 = parts.next()?.parse().ok()?;
        let age = parse_etime(parts.next()?)?;
        let command: Vec<&str> = parts.collect();
        let is_stream = command.first().is_some_and(|c| c.ends_with("ffmpeg"))
            && command.windows(2).any(|w| w == ["-f", "fifo"])
            && command.iter().any(|a| a.starts_with("rtmp"));
        if !is_stream {
            return None;
        }
        let url = command.iter().find(|a| a.starts_with("rtmp"))?;
        Some(Live {
            source: Source::Terminal { pid },
            started_at: now.saturating_sub(age),
            platform: streams::detect_platform(url).to_string(),
            output: "started in a terminal".to_string(),
            health: Health::Unmonitored,
        })
    })
}

/// `ps` elapsed time: [[dd-]hh:]mm:ss, in seconds.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = s.split_once('-').map_or((0, s), |(d, r)| (d.parse().unwrap_or(0), r));
    let mut secs = 0;
    for part in rest.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}
fn health(log: &str) -> Health {
    let fresh = std::fs::metadata(log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < STALL_AFTER);
    if !fresh {
        return Health::Stalled;
    }
    match last_stats(log) {
        Some((fps, speed)) => Health::Sending { fps, speed },
        None => Health::Warming,
    }
}

/// (fps, speed) from the newest ffmpeg stats line in the log tail.
fn last_stats(log: &str) -> Option<(f32, f32)> {
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(8192))).ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    parse_stats(&tail)
}

fn parse_stats(tail: &str) -> Option<(f32, f32)> {
    let number_after = |key: &str| -> Option<f32> {
        let rest = tail[tail.rfind(key)? + key.len()..].trim_start();
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        rest[..end].parse().ok()
    };
    Some((number_after("fps=")?, number_after("speed=")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_newest_stats_line() {
        let log = "frame=  100 fps= 30 q=-0.0 speed=0.5x\rframe= 6366 fps= 61 q=-0.0 size=N/A dup=26 drop=0 speed=0.995x elapsed=0:01:45";
        assert_eq!(parse_stats(log), Some((61.0, 0.995)));
    }

    #[test]
    fn etime_parses_every_ps_shape() {
        assert_eq!(parse_etime("05:07"), Some(307));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-00:00:10"), Some(172_810));
        assert_eq!(parse_etime("soon"), None);
    }

    #[test]
    fn finds_a_terminal_stream_and_ignores_other_ffmpegs() {
        let ps = "  101 00:05 ffmpeg -hide_banner -f avfoundation -i 5: -frames:v 1 -f null -\n\
                  \x20 202 12:25 ffmpeg -hide_banner -i x -f fifo -fifo_format flv rtmp://a.rtmp.youtube.com/live2/KEY\n\
                  \x20 303 00:01 vim notes.txt\n";
        let live = parse_terminal_stream(ps, 10_000).unwrap();
        assert!(matches!(live.source, Source::Terminal { pid: 202 }));
        assert_eq!(live.started_at, 10_000 - 745);
        assert_eq!(live.platform, "YouTube");
        assert!(parse_terminal_stream("  101 00:05 ffmpeg -i a -f null -\n", 0).is_none());
    }

    #[test]
    fn no_stats_line_is_none() {
        assert_eq!(parse_stats("Press [q] to stop"), None);
    }
}
