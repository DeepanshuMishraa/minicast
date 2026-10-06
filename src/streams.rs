//! Background stream supervision (macOS only).
//!
//! `start --detach` launches ffmpeg detached from the CLI, snapshots the
//! effective config to disk, and registers the stream in `streams.json`.
//! Later config edits only touch `config.toml`, so running streams are
//! unaffected — `stream list` always shows each stream's start-time snapshot.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRecord {
    pub id: String,
    pub name: String,
    pub pid: u32,
    pub rtmp_url: String,
    pub platform: String,
    pub started_at: u64,
    /// e.g. "1920x1080@30"
    pub output: String,
    pub snapshot_path: String,
    pub log_path: String,
    /// True when `pid` is a supervisor shell that restarts ffmpeg on failure
    /// (all new streams). False for pre-supervisor records (raw ffmpeg pid).
    #[serde(default)]
    pub supervised: bool,
}

/// Restart policy for a stream run, resolved from config + `start` flags.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: Option<u32>,
    pub base_secs: u64,
    pub cap_secs: u64,
}

impl RetryPolicy {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            enabled: cfg.output.auto_reconnect,
            max_retries: cfg.output.max_retries,
            base_secs: cfg.output.retry_delay_secs.max(1),
            cap_secs: cfg.output.max_retry_delay_secs.max(1),
        }
    }

    /// Wait before attempt N (1-based): base * 2^(N-1), capped.
    pub fn delay_for(&self, attempt: u32) -> u64 {
        backoff_secs(self.base_secs, self.cap_secs, attempt)
    }

    /// False when `attempt` already exhausted the budget.
    pub fn budget_left(&self, attempt: u32) -> bool {
        match self.max_retries {
            Some(max) => attempt <= max,
            None => true,
        }
    }
}

/// Exponential backoff: base * 2^(attempt-1), capped at `cap`, min 1s.
pub fn backoff_secs(base: u64, cap: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(10);
    base.max(1)
        .saturating_mul(1u64 << shift)
        .min(cap.max(1))
}

/// Quote one argv item for bash (single-quote style).
/// `shell_escape`, except the mic helper's origin placeholder becomes the
/// supervisor's `$MIC_ORIGIN` variable (re-read on every restart).
fn shell_escape_origin(arg: &str) -> String {
    arg.split(crate::mic::ORIGIN).map(shell_escape).collect::<Vec<_>>().join("\"$MIC_ORIGIN\"")
}

pub fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let safe = arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,".contains(c));
    if safe {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn base_dir() -> PathBuf {
    home_dir().join(".config").join("minicast")
}

pub fn state_path() -> PathBuf {
    base_dir().join("streams.json")
}

pub fn snapshots_dir() -> PathBuf {
    base_dir().join("snapshots")
}

pub fn logs_dir() -> PathBuf {
    base_dir().join("logs")
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Guess the destination platform from the RTMP URL so `stream list`
/// shows where each stream is going.
pub fn detect_platform(url: &str) -> &'static str {
    let l = url.to_lowercase();
    if l.contains("youtube.com") || l.contains("youtu.be") {
        "YouTube"
    } else if l.contains("twitch.tv") {
        "Twitch"
    } else if l.contains("facebook.com") || l.contains("fbcdn.net") {
        "Facebook"
    } else if l.contains("tiktok") {
        "TikTok"
    } else if l.contains("kick.com") {
        "Kick"
    } else if l.contains("localhost") || l.contains("127.0.0.1") {
        "Local"
    } else {
        "Custom"
    }
}

/// Mask the stream key (last URL segment) so `stream list` never leaks secrets:
/// `rtmp://host/app/KEY` -> `rtmp://host/app/****`.
pub fn mask_rtmp_url(url: &str) -> String {
    let scheme_end = url.find("://").map(|p| p + 3).unwrap_or(0);
    match url.rfind('/') {
        Some(i) if i > scheme_end => format!("{}/****", &url[..i]),
        _ => url.to_string(),
    }
}

pub fn format_uptime(since: u64, now: u64) -> String {
    let s = now.saturating_sub(since);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

pub fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn load_state() -> Result<Vec<StreamRecord>> {
    let p = state_path();
    if !p.exists() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("failed to read {}", p.display()))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", p.display()))
}

fn save_state(records: &[StreamRecord]) -> Result<()> {
    let p = state_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(records).context("failed to serialize streams")?;
    std::fs::write(&p, text).with_context(|| format!("failed to write {}", p.display()))?;
    restrict(&p)?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// Drop records whose ffmpeg exited. Returns (alive, pruned_count).
pub fn prune_dead() -> Result<(Vec<StreamRecord>, usize)> {
    let records = load_state()?;
    let before = records.len();
    let alive: Vec<StreamRecord> = records.into_iter().filter(|r| is_alive(r.pid)).collect();
    let pruned = before - alive.len();
    if pruned > 0 {
        save_state(&alive)?;
    }
    Ok((alive, pruned))
}

/// Live records without touching the state file.
pub fn alive_records() -> Result<Vec<StreamRecord>> {
    Ok(load_state()?.into_iter().filter(|r| is_alive(r.pid)).collect())
}

/// Snapshot-friendly snapshot: write the effective config TOML plus full
/// ffmpeg argv next to it, both chmod 600 (they contain the stream key).
fn write_snapshot(id: &str, config_toml: &str, argv: &[String]) -> Result<PathBuf> {
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("stream-{id}.toml"));
    let mut text = format!(
        "# snapshot for stream {id} — effective config at start time.\n\
         # Editing config.toml does NOT affect this running stream.\n"
    );
    text.push_str(config_toml);
    text.push_str("\n# ffmpeg argv at start:\n# ffmpeg ");
    text.push_str(&argv.join(" "));
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    restrict(&path)?;
    Ok(path)
}

/// Launch ffmpeg detached (survives CLI exit), snapshot config, register stream.
/// The returned record borrows nothing from config.toml afterwards.
///
/// When `retry.enabled`, ffmpeg runs under a small supervisor shell that
/// restarts it with exponential backoff after failures (network loss, server
/// hiccups) — the recorded pid is the supervisor. Otherwise ffmpeg is
/// launched directly, as before.
pub fn spawn_detached(
    name: Option<&str>,
    cfg: &crate::config::Config,
    argv: &[String],
    retry: RetryPolicy,
    mic: Option<&crate::mic::MicCapture>,
) -> Result<StreamRecord> {
    let (mut alive, _) = prune_dead()?;
    let next: u32 = alive
        .iter()
        .filter_map(|r| r.id.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let id = next.to_string();

    let config_toml = toml::to_string_pretty(cfg).context("failed to serialize snapshot")?;
    let snap_path = write_snapshot(&id, &config_toml, argv)?;

    let log_dir = logs_dir();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create {}", log_dir.display()))?;
    let log_path = log_dir.join(format!("stream-{id}.log"));
    let log_out = std::fs::File::create(&log_path)
        .with_context(|| format!("failed to create {}", log_path.display()))?;
    restrict(&log_path)?;
    let log_err = log_out
        .try_clone()
        .with_context(|| format!("failed to clone {}", log_path.display()))?;

    let supervised = retry.enabled;
    // The mic helper and ffmpeg must start and stop together, which only the
    // supervisor script does. Without retries it gives up after one run.
    let pid = if supervised || mic.is_some() {
        let policy = if supervised { retry } else { RetryPolicy { max_retries: Some(0), ..retry } };
        let script = write_supervisor(&id, argv, &policy, &log_path, mic)?;
        // nohup + stdin /dev/null: survives CLI exit and terminal hangup.
        // (macOS ships nohup but not setsid.)
        let child = Command::new("nohup")
            .arg("bash")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(log_out)
            .stderr(log_err)
            .spawn()
            .context("failed to spawn stream supervisor (is bash installed?)")?;
        let pid = child.id();
        // Detach: never wait/reap; the supervisor outlives this CLI process.
        std::mem::forget(child);
        pid
    } else {
        let child = Command::new("ffmpeg")
            .args(argv)
            .stdin(Stdio::null())
            .stdout(log_out)
            .stderr(log_err)
            .spawn()
            .context("failed to spawn ffmpeg (is it installed?)")?;
        let pid = child.id();
        // Detach: never wait/reap; the stream outlives this CLI process.
        std::mem::forget(child);
        pid
    };

    let rec = StreamRecord {
        name: name
            .filter(|n| !n.trim().is_empty())
            .map(|n| n.trim().to_string())
            .unwrap_or_else(|| format!("stream-{id}")),
        id,
        pid,
        platform: detect_platform(&cfg.output.rtmp_url).to_string(),
        rtmp_url: cfg.output.rtmp_url.clone(),
        started_at: unix_now(),
        output: format!("{}x{}@{}", cfg.video.width, cfg.video.height, cfg.video.fps),
        snapshot_path: snap_path.display().to_string(),
        log_path: log_path.display().to_string(),
        supervised,
    };
    alive.push(rec.clone());
    save_state(&alive)?;
    Ok(rec)
}

/// Stream logs are trimmed to the last `KEEP_LOG_BYTES` once they pass `MAX_LOG_BYTES`.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const KEEP_LOG_BYTES: u64 = 1024 * 1024;

/// Write the supervisor shell script for a detached stream. It runs ffmpeg in
/// a loop: a clean exit (code 0) stops the loop, any failure sleeps with
/// exponential backoff and retries (forever unless `max_retries` is set).
fn write_supervisor(
    id: &str,
    argv: &[String],
    retry: &RetryPolicy,
    log_path: &Path,
    mic: Option<&crate::mic::MicCapture>,
) -> Result<PathBuf> {
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("stream-{id}-supervisor.sh"));
    let ffmpeg_cmd = std::iter::once("ffmpeg".to_string())
        .chain(argv.iter().map(|a| shell_escape_origin(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let max = retry
        .max_retries
        .map(|n| n.to_string())
        .unwrap_or_default();
    let log = shell_escape(&log_path.display().to_string());
    // Plain: ffmpeg alone. With the mic helper: helper -> FIFO -> ffmpeg's
    // stdin, started on a fresh timeline origin each attempt. bash 3.2 (macOS)
    // has no `wait -n`, so poll; whichever side dies first takes the other
    // down and counts as a failure, so the retry loop restarts both.
    let (fifo_setup, fifo_cleanup, run) = match mic {
        None => (String::new(), String::new(), format!("{ffmpeg_cmd} >> {log} 2>&1\nCODE=$?\n")),
        Some(mic) => {
            let fifo = shell_escape(&dir.join(format!("stream-{id}-mic.fifo")).display().to_string());
            let helper = shell_escape(&mic.exe.display().to_string());
            let device = shell_escape(&mic.device);
            let run = format!(
                "MIC_ORIGIN=$({helper} now)\n\
                 {helper} {device} \"$MIC_ORIGIN\" > {fifo} 2>> {log} &\n\
                 HP=$!\n\
                 {ffmpeg_cmd} < {fifo} >> {log} 2>&1 &\n\
                 FP=$!\n\
                 while kill -0 $HP 2>/dev/null && kill -0 $FP 2>/dev/null; do sleep 1; done\n\
                 if kill -0 $FP 2>/dev/null; then\n\
                   echo \"[supervisor] mic helper exited; stopping ffmpeg to restart both.\" >> {log}\n\
                   kill $FP 2>/dev/null; wait $FP 2>/dev/null; wait $HP 2>/dev/null; CODE=1\n\
                 else\n\
                   wait $FP; CODE=$?\n\
                   kill $HP 2>/dev/null; wait $HP 2>/dev/null\n\
                 fi\n"
            );
            // Background children outlive a SIGTERM'd bash (and `stop` only
            // looks for children of a live supervisor), so turn TERM/INT into
            // a normal exit and let the EXIT trap kill helper + ffmpeg.
            (
                format!("rm -f {fifo}; mkfifo {fifo}\ntrap 'exit 143' TERM INT\n"),
                // ffmpeg can sit on SIGTERM while its output is blocked, so
                // escalate to SIGKILL after ~2s.
                format!(
                    " $HP; kill $FP 2>/dev/null; for _ in 1 2 3 4; do kill -0 $FP 2>/dev/null || break; sleep 0.5; done; \
                     kill -9 $FP 2>/dev/null; rm -f {fifo}"
                ),
                run,
            )
        }
    };
    let text = format!(
        "#!/bin/bash\n\
         # minicast supervisor for stream {id} — restarts ffmpeg after failures.\n\
         # Stop with: minicast stream stop {id}   (do NOT kill ffmpeg directly)\n\
         # Cap the log: ffmpeg appends (>>), so trimming to the tail every\n\
         # minute keeps disk use flat however long the stream runs.\n\
         ( while sleep 60; do\n\
             SIZE=$(stat -f%z {log} 2>/dev/null || echo 0)\n\
             if [ \"$SIZE\" -gt {max_log} ]; then\n\
               tail -c {keep_log} {log} > {log}.tmp && cat {log}.tmp > {log}; rm -f {log}.tmp\n\
             fi\n\
           done ) &\n\
         WATCHDOG=$!\n\
         disown $WATCHDOG\n\
         {fifo_setup}\
         trap 'kill $WATCHDOG 2>/dev/null{fifo_cleanup}' EXIT\n\
         ATTEMPT=0\n\
         DELAY={base}\n\
         MAX_RETRIES=\"{max}\"\n\
         while true; do\n\
           {run}\
           if [ \"$CODE\" -eq 0 ]; then\n\
             echo \"[supervisor] ffmpeg exited cleanly — not restarting.\" >> {log}\n\
             exit 0\n\
           fi\n\
           ATTEMPT=$((ATTEMPT+1))\n\
           if [ -n \"$MAX_RETRIES\" ] && [ \"$ATTEMPT\" -gt \"$MAX_RETRIES\" ]; then\n\
             echo \"[supervisor] gave up after $ATTEMPT attempts (exit $CODE).\" >> {log}\n\
             exit \"$CODE\"\n\
           fi\n\
           if [ \"$DELAY\" -gt {cap} ]; then DELAY={cap}; fi\n\
           echo \"[supervisor] ffmpeg exited (code $CODE); retry $ATTEMPT in ${{DELAY}}s…\" >> {log}\n\
           sleep \"$DELAY\"\n\
           DELAY=$((DELAY*2))\n\
           if [ \"$DELAY\" -gt {cap} ]; then DELAY={cap}; fi\n\
         done\n",
        id = id,
        base = retry.base_secs,
        cap = retry.cap_secs,
        max_log = MAX_LOG_BYTES,
        keep_log = KEEP_LOG_BYTES,
        max = max,
        fifo_setup = fifo_setup,
        fifo_cleanup = fifo_cleanup,
        run = run,
        log = log,
    );
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    restrict(&path)?;
    Ok(path)
}

/// SIGTERM, wait ~5s, escalate to SIGKILL. Returns true when the pid is gone.
pub fn terminate(pid: u32) -> Result<bool> {
    terminate_impl(pid, false)
}

/// Stop a supervised stream: SIGTERM the supervisor AND its ffmpeg child(ren),
/// wait ~5s, escalate to SIGKILL. Returns true when everything is gone.
pub fn terminate_tree(pid: u32) -> Result<bool> {
    terminate_impl(pid, true)
}

fn terminate_impl(pid: u32, tree: bool) -> Result<bool> {
    if !is_alive(pid) {
        return Ok(true);
    }
    let _ = Command::new("kill").arg(pid.to_string()).status();
    if tree {
        // Supervisor's children (the ffmpeg it restarted, possibly grandkids).
        let _ = Command::new("pkill").args(["-P", &pid.to_string()]).status();
    }
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !is_alive(pid) {
            // Supervisor gone — make sure no orphaned ffmpeg child survived it.
            if tree {
                let _ = Command::new("pkill")
                    .args(["-9", "-P", &pid.to_string()])
                    .status();
            }
            return Ok(true);
        }
    }
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    if tree {
        let _ = Command::new("pkill")
            .args(["-9", "-P", &pid.to_string()])
            .status();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    Ok(!is_alive(pid))
}

/// Find a record by id or name without removing it.
pub fn find_record(id_or_name: &str) -> Result<Option<StreamRecord>> {
    Ok(load_state()?
        .into_iter()
        .find(|r| r.id == id_or_name || r.name == id_or_name))
}

/// Last `lines` lines of a stream log file.
pub fn tail_log(path: &str, lines: usize) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read log {path}"))?;
    if lines == 0 {
        return Ok(String::new());
    }
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    let mut out = all[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Remove a record by id or name. Returns the removed record, if any.
pub fn remove_record(id_or_name: &str) -> Result<Option<StreamRecord>> {
    let records = load_state()?;
    let mut kept = Vec::with_capacity(records.len());
    let mut removed = None;
    for r in records {
        if removed.is_none() && (r.id == id_or_name || r.name == id_or_name) {
            removed = Some(r);
        } else {
            kept.push(r);
        }
    }
    if removed.is_some() {
        save_state(&kept)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_platforms() {
        assert_eq!(detect_platform("rtmp://a.rtmp.youtube.com/live2/key"), "YouTube");
        assert_eq!(detect_platform("rtmp://live.twitch.tv/app/key"), "Twitch");
        assert_eq!(detect_platform("rtmps://live-api-s.facebook.com:443/rtmp/key"), "Facebook");
        assert_eq!(detect_platform("rtmp://localhost/live/key"), "Local");
        assert_eq!(detect_platform("rtmp://127.0.0.1/live/key"), "Local");
        assert_eq!(detect_platform("rtmp://my.server.com/app/key"), "Custom");
    }

    #[test]
    fn masks_stream_key() {
        assert_eq!(
            mask_rtmp_url("rtmp://a.rtmp.youtube.com/live2/s3cr3t"),
            "rtmp://a.rtmp.youtube.com/live2/****"
        );
        // No path beyond host: nothing secret to hide.
        assert_eq!(mask_rtmp_url("rtmp://host"), "rtmp://host");
    }

    #[test]
    fn formats_uptime() {
        assert_eq!(format_uptime(100, 105), "5s");
        assert_eq!(format_uptime(0, 125), "2m05s");
        assert_eq!(format_uptime(0, 3700), "1h01m");
    }

    #[test]
    fn record_roundtrips_json() {
        let rec = StreamRecord {
            id: "1".into(),
            name: "yt".into(),
            pid: 123,
            rtmp_url: "rtmp://a.rtmp.youtube.com/live2/key".into(),
            platform: "YouTube".into(),
            started_at: 1_700_000_000,
            output: "1920x1080@30".into(),
            snapshot_path: "/tmp/snap.toml".into(),
            log_path: "/tmp/s.log".into(),
            supervised: true,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: StreamRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.platform, "YouTube");
        assert_eq!(back.pid, 123);
        assert!(back.supervised);
    }

    #[test]
    fn old_records_without_supervised_still_parse() {
        let json = r#"{"id":"1","name":"yt","pid":123,
            "rtmp_url":"rtmp://a.rtmp.youtube.com/live2/key","platform":"YouTube",
            "started_at":1700000000,"output":"1920x1080@30",
            "snapshot_path":"/tmp/snap.toml","log_path":"/tmp/s.log"}"#;
        let back: StreamRecord = serde_json::from_str(json).unwrap();
        assert!(!back.supervised);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(2, 30, 1), 2);
        assert_eq!(backoff_secs(2, 30, 2), 4);
        assert_eq!(backoff_secs(2, 30, 3), 8);
        assert_eq!(backoff_secs(2, 30, 4), 16);
        assert_eq!(backoff_secs(2, 30, 5), 30);
        assert_eq!(backoff_secs(2, 30, 50), 30);
        // Zero base is treated as 1s rather than busy-looping.
        assert_eq!(backoff_secs(0, 30, 3), 4);
    }

    #[test]
    fn retry_budget_counts_down() {
        let p = RetryPolicy { enabled: true, max_retries: Some(2), base_secs: 2, cap_secs: 30 };
        assert!(p.budget_left(1));
        assert!(p.budget_left(2));
        assert!(!p.budget_left(3));
        let infinite = RetryPolicy { enabled: true, max_retries: None, base_secs: 2, cap_secs: 30 };
        assert!(infinite.budget_left(10_000));
    }

    #[test]
    fn supervisor_script_is_valid_bash_with_backoff_loop() {
        let retry = RetryPolicy { enabled: true, max_retries: None, base_secs: 2, cap_secs: 30 };
        let argv = vec![
            "-hide_banner".to_string(),
            "-filter_complex".to_string(),
            "[0:v]scale=1920:1080,crop=1920:1080[video]".to_string(),
            "rtmp://a.rtmp.youtube.com/live2/key".to_string(),
        ];
        let log = std::env::temp_dir().join("mc-supervisor-test.log");
        let path = write_supervisor("selftest-999", &argv, &retry, &log, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Loop + backoff + clean-exit stop + quoted filter with brackets.
        assert!(text.contains("while true"), "{text}");
        assert!(text.contains("DELAY=2"), "{text}");
        assert!(text.contains("DELAY=$((DELAY*2))"), "{text}");
        assert!(text.contains("exit 0"), "{text}");
        // Log trimming keeps disk use flat on long streams.
        assert!(text.contains("tail -c 1048576"), "{text}");
        assert!(text.contains("-gt 10485760"), "{text}");
        assert!(text.contains("'[0:v]scale=1920:1080,crop=1920:1080[video]'"), "{text}");
        // Infinite budget: MAX_RETRIES empty, guarded by -n test.
        assert!(text.contains("MAX_RETRIES=\"\""), "{text}");
        let check = std::process::Command::new("bash").arg("-n").arg(&path).status().unwrap();
        assert!(check.success(), "supervisor script failed bash -n");
        std::fs::remove_file(&path).ok();

        // Capped budget renders the number.
        let capped = RetryPolicy { enabled: true, max_retries: Some(5), base_secs: 3, cap_secs: 20 };
        let path = write_supervisor("selftest-999", &argv, &capped, &log, None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("MAX_RETRIES=\"5\""), "{text}");
        assert!(text.contains("DELAY=3"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn supervisor_pipes_the_mic_helper_into_ffmpeg() {
        let retry = RetryPolicy { enabled: true, max_retries: None, base_secs: 2, cap_secs: 30 };
        let argv = vec!["-filter_complex".to_string(), "[0:v]setpts=PTS-@MIC_ORIGIN@/TB,scale=1:1[v]".to_string()];
        let mic = crate::mic::MicCapture { exe: "/x/mic cap".into(), device: "Mac's Mic".into() };
        let log = std::env::temp_dir().join("mc-supervisor-mic-test.log");
        let path = write_supervisor("selftest-998", &argv, &retry, &log, Some(&mic)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mkfifo"), "{text}");
        assert!(text.contains("MIC_ORIGIN=$('/x/mic cap' now)"), "{text}");
        // Fresh origin per attempt, spliced into the filter string as a variable.
        assert!(text.contains("'[0:v]setpts=PTS-'\"$MIC_ORIGIN\"'/TB,scale=1:1[v]'"), "{text}");
        assert!(text.contains("< "), "{text}");
        assert!(text.contains("mic helper exited"), "{text}");
        let check = std::process::Command::new("bash").arg("-n").arg(&path).status().unwrap();
        assert!(check.success(), "supervisor script failed bash -n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn shell_escape_quotes_filter_strings() {        assert_eq!(shell_escape("-hide_banner"), "-hide_banner");
        assert_eq!(shell_escape(""), "''");
        assert_eq!(
            shell_escape("[0:v]scale=1920:1080,crop=1920:1080[video]"),
            "'[0:v]scale=1920:1080,crop=1920:1080[video]'"
        );
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }
}
