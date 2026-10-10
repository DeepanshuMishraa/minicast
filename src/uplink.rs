//! Upload-speed probe and the auto-bitrate rule built on it.
use std::io::Write;
use std::process::{Command, Stdio};

/// Endpoint that swallows POST bodies (Cloudflare's public speed test).
const PROBE_URL: &str = "https://speed.cloudflare.com/__up";
const PROBE_BYTES: usize = 6_000_000;
const PROBE_ATTEMPTS: usize = 2;
const PROBE_TIMEOUT_SECS: &str = "15";

/// Share of measured uplink the stream may use. Leaves room for audio,
/// protocol overhead and Wi-Fi dips; going near 100% is what makes viewers
/// buffer.
const UPLINK_SHARE: f64 = 0.6;
const AUDIO_KBPS: f64 = 160.0;
const MIN_AUTO_KBPS: u32 = 1500;
/// Bits per pixel per frame that look good in H.264 live encoding.
const BITS_PER_PIXEL: f64 = 0.07;

/// Best-of-N upload speed in kbit/s, or None if every probe failed
/// (offline, curl missing, endpoint blocked).
pub fn measure_kbps() -> Option<f64> {
    // Non-repeating bytes so nothing along the path can shrink the payload.
    let body: Vec<u8> = (0..PROBE_BYTES as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    (0..PROBE_ATTEMPTS)
        .filter_map(|_| probe_once(&body))
        .fold(None, |best: Option<f64>, v| Some(best.map_or(v, |b| b.max(v))))
}

fn probe_once(body: &[u8]) -> Option<f64> {
    let mut child = Command::new("curl")
        .args(["-s", "-m", PROBE_TIMEOUT_SECS, "-o", "/dev/null", "-w", "%{speed_upload}", "-X", "POST", "--data-binary", "@-", PROBE_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Feed from a thread-free write; curl reads until the body is sent.
    child.stdin.take()?.write_all(body).ok()?;
    let out = child.wait_with_output().ok()?;
    let bytes_per_sec: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (bytes_per_sec > 0.0).then_some(bytes_per_sec * 8.0 / 1000.0)
}

/// Highest sensible video bitrate for this canvas, in kbit/s.
pub fn ceiling_kbps(width: u32, height: u32, fps: u32) -> u32 {
    ((width as f64 * height as f64 * fps as f64 * BITS_PER_PIXEL) / 1000.0).round() as u32
}

/// Video bitrate to use: the lower of the quality ceiling and what the
/// measured uplink can carry, never below MIN_AUTO_KBPS.
pub fn pick_kbps(measured_kbps: f64, ceiling_kbps: u32) -> u32 {
    let sustainable = (measured_kbps * UPLINK_SHARE - AUDIO_KBPS).max(0.0) as u32;
    sustainable.min(ceiling_kbps).max(MIN_AUTO_KBPS.min(ceiling_kbps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_scales_with_canvas_and_fps() {
        assert_eq!(ceiling_kbps(1920, 1080, 60), 8709);
        assert_eq!(ceiling_kbps(1920, 1080, 30), 4355);
    }

    #[test]
    fn picks_a_share_of_the_uplink_capped_by_the_ceiling() {
        // 10 Mbps line -> 60% minus audio, under the 1080p60 ceiling.
        assert_eq!(pick_kbps(10_000.0, 8709), 5840);
        // Fast line -> quality ceiling wins.
        assert_eq!(pick_kbps(50_000.0, 8709), 8709);
        // Slow or under-read line -> floor, not zero.
        assert_eq!(pick_kbps(1000.0, 8709), 1500);
        // Floor never exceeds the ceiling.
        assert_eq!(pick_kbps(1000.0, 1000), 1000);
    }
}
