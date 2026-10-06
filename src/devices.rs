use anyhow::Result;
use std::process::Command;

/// Parsed AVFoundation device list from ffmpeg output.
#[derive(Debug, Default)]
pub struct DeviceList {
    pub video: Vec<(String, String)>,
    pub audio: Vec<(String, String)>,
}

pub fn probe_avfoundation() -> Result<DeviceList> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .output()?;
    // ffmpeg writes the list to stderr and exits non-zero; that's expected.
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Ok(parse_avfoundation(&stderr))
}

impl DeviceList {
    /// Accept an index ("4") or a name/substring ("Capture screen 0").
    /// Returns the canonical index if matched, else None.
    pub fn resolve_video(&self, query: &str) -> Option<String> {
        let q = query.trim();
        if self.video.iter().any(|(i, _)| i == q) {
            return Some(q.to_string());
        }
        let ql = q.to_lowercase();
        self.video
            .iter()
            .find(|(_, name)| name.to_lowercase().contains(&ql))
            .map(|(i, _)| i.clone())
    }

    pub fn resolve_audio(&self, query: &str) -> Option<String> {
        let q = query.trim();
        if self.audio.iter().any(|(i, _)| i == q) {
            return Some(q.to_string());
        }
        let ql = q.to_lowercase();
        self.audio
            .iter()
            .find(|(_, name)| name.to_lowercase().contains(&ql))
            .map(|(i, _)| i.clone())
    }

    /// Human-readable name for a stored device index, e.g. audio "1" ->
    /// Some("MacBook Air Microphone"). Used to display `1 (MacBook Air Microphone)`.
    pub fn audio_name(&self, idx: &str) -> Option<&str> {
        self.audio
            .iter()
            .find(|(i, _)| i == idx)
            .map(|(_, n)| n.as_str())
    }

    pub fn video_name(&self, idx: &str) -> Option<&str> {
        self.video
            .iter()
            .find(|(i, _)| i == idx)
            .map(|(_, n)| n.as_str())
    }

    /// Screens are video devices whose name mentions "capture screen" or "display".
    pub fn screens(&self) -> Vec<(String, String)> {
        self.video
            .iter()
            .filter(|(_, n)| {
                let l = n.to_lowercase();
                l.contains("capture screen") || l.contains("display")
            })
            .cloned()
            .collect()
    }

    /// Cameras = video devices that are not screens.
    pub fn cameras(&self) -> Vec<(String, String)> {
        self.video
            .iter()
            .filter(|(_, n)| {
                let l = n.to_lowercase();
                !(l.contains("capture screen") || l.contains("display"))
            })
            .cloned()
            .collect()
    }
}

fn parse_avfoundation(stderr: &str) -> DeviceList {
    let mut list = DeviceList::default();
    let mut section = "";
    for line in stderr.lines() {
        if line.contains("AVFoundation video devices:") {
            section = "video";
            continue;
        }
        if line.contains("AVFoundation audio devices:") {
            section = "audio";
            continue;
        }
        // Lines look like: "[AVFoundation indev @ ...] [0] MacBook Air Camera"
        if let Some(idx) = extract_bracket_index(line) {
            let name = line
                .split(']')
                .last()
                .unwrap_or("")
                .trim()
                .to_string();
            if name.is_empty() {
                continue;
            }
            match section {
                "video" => list.video.push((idx, name)),
                "audio" => list.audio.push((idx, name)),
                _ => {}
            }
        }
    }
    list
}

fn extract_bracket_index(line: &str) -> Option<String> {
    // Find the LAST "[N]" where N is numeric (first bracket is the log prefix
    // like "[AVFoundation indev @ 0x...]", which is never all-digits).
    let mut result: Option<String> = None;
    let mut search = line;
    while let Some(open) = search.find('[') {
        let rest = &search[open..];
        if let Some(end) = rest.find(']') {
            let inner = &rest[1..end];
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                result = Some(inner.to_string());
            }
            search = &rest[end + 1..];
        } else {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_output() {
        let sample = "[AVFoundation indev @ 0x123] AVFoundation video devices:\n\
         [AVFoundation indev @ 0x123] [0] FaceTime Camera\n\
         [AVFoundation indev @ 0x123] [1] Capture screen 0\n\
         [AVFoundation indev @ 0x123] AVFoundation audio devices:\n\
         [AVFoundation indev @ 0x123] [0] Built-in Microphone\n";
        let list = parse_avfoundation(sample);
        assert_eq!(list.video.len(), 2);
        assert_eq!(list.audio.len(), 1);
        assert_eq!(list.video[0].0, "0");
    }

    #[test]
    fn resolves_by_index_and_name() {
        let sample = "[AVFoundation indev @ 0x123] AVFoundation video devices:\n\
         [AVFoundation indev @ 0x123] [0] FaceTime Camera\n\
         [AVFoundation indev @ 0x123] [4] Capture screen 0\n\
         [AVFoundation indev @ 0x123] AVFoundation audio devices:\n\
         [AVFoundation indev @ 0x123] [3] Built-in Microphone\n";
        let list = parse_avfoundation(sample);
        assert_eq!(list.resolve_video("4"), Some("4".into()));
        assert_eq!(list.resolve_video("capture screen"), Some("4".into()));
        assert_eq!(list.resolve_video("facetime"), Some("0".into()));
        assert_eq!(list.resolve_audio("built-in"), Some("3".into()));
        assert_eq!(list.resolve_video("nope"), None);
        assert_eq!(list.screens().len(), 1);
        assert_eq!(list.cameras().len(), 1);
    }
}

/// Modes a camera advertises, as (w, h, fps list). Asks AVFoundation for an
/// impossible size; it answers with its "Supported modes" list on stderr.
pub fn camera_modes(device: &str) -> Vec<(u32, u32, Vec<f64>)> {
    let Ok(out) = Command::new("ffmpeg")
        .args(["-hide_banner", "-f", "avfoundation", "-video_size", "1x1", "-framerate", "30", "-i"])
        .arg(format!("{device}:"))
        .args(["-t", "1", "-f", "null", "-"])
        .output()
    else {
        return Vec::new();
    };
    parse_modes(&String::from_utf8_lossy(&out.stderr))
}

fn parse_modes(stderr: &str) -> Vec<(u32, u32, Vec<f64>)> {
    stderr
        .lines()
        .filter_map(|l| {
            let (head, rest) = l.split_once("@[")?;
            let size = head.rsplit(char::is_whitespace).next()?;
            let (w, h) = crate::config::parse_size(size)?;
            let fps = rest.trim_end_matches("fps").trim_end_matches(']').split_whitespace()
                .filter_map(|f| f.parse().ok()).collect();
            Some((w, h, fps))
        })
        .collect()
}

/// Refresh rates (Hz) of the connected displays, in "Capture screen N"
/// order, read from system_profiler ("… @ 100.00Hz").
pub fn display_refresh_rates() -> Vec<u32> {
    let Ok(out) = Command::new("system_profiler").args(["SPDisplaysDataType", "-json"]).output() else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    let mut rates = Vec::new();
    for gpu in json["SPDisplaysDataType"].as_array().into_iter().flatten() {
        for d in gpu["spdisplays_ndrvs"].as_array().into_iter().flatten() {
            let res = d["_spdisplays_resolution"].as_str().unwrap_or("");
            rates.push(res.split('@').nth(1).and_then(parse_hz).unwrap_or(0));
        }
    }
    rates
}

fn parse_hz(s: &str) -> Option<u32> {
    s.trim().trim_end_matches("Hz").trim().parse::<f64>().ok().map(|f| f.round() as u32)
}

/// Pick the camera's capture mode for a target stream rate. The camera fps
/// is the highest it offers that is <= `fps` (never above what the output
/// needs, never a rate it can't do); size follows the rules below.
/// Returns (w, h, fps), or None to let the device choose.
///
/// Size: smallest landscape mode at that fps, same aspect as the PiP, that
/// covers `min` doubled (sharp downscale); relaxing aspect, then coverage.
pub fn pick_camera_mode(modes: &[(u32, u32, Vec<f64>)], fps: u32, min: (u32, u32)) -> Option<(u32, u32, u32)> {
    let cam_fps = modes
        .iter()
        .filter(|m| m.0 >= m.1)
        .flat_map(|m| m.2.iter())
        .map(|f| f.round() as u32)
        .filter(|&f| f <= fps)
        .max()?;
    let ok = |m: &&(u32, u32, Vec<f64>)| m.0 >= m.1 && m.2.iter().any(|f| (f - cam_fps as f64).abs() < 0.5);
    let area = |m: &&(u32, u32, Vec<f64>)| m.0 as u64 * m.1 as u64;
    let want = (min.0 * 2, min.1 * 2);
    // Same aspect as the PiP first (no cropped field of view), then any.
    let pip_aspect = min.0 as f64 / min.1 as f64;
    let same_aspect = |m: &&(u32, u32, Vec<f64>)| (m.0 as f64 / m.1 as f64 / pip_aspect - 1.0).abs() < 0.02;
    let covers = |m: &&(u32, u32, Vec<f64>)| m.0 >= want.0 && m.1 >= want.1;
    modes.iter().filter(ok).filter(covers).filter(same_aspect).min_by_key(area)
        .or_else(|| modes.iter().filter(ok).filter(covers).min_by_key(area))
        .or_else(|| modes.iter().filter(ok).max_by_key(area))
        .map(|m| (m.0, m.1, cam_fps))
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn picks_smallest_landscape_mode_covering_the_pip() {
        let out = "[in#0 @ 0x1] Supported modes:\n[in#0 @ 0x1]   640x480@[15.000000 30.000000]fps\n\
                   [in#0 @ 0x1]   1280x720@[15.000000 30.000000]fps\n[in#0 @ 0x1]   1080x1920@[15.000000 30.000000]fps\n";
        let modes = parse_modes(out);
        assert_eq!(modes.len(), 3);
        assert_eq!(pick_camera_mode(&modes, 30, (320, 180)), Some((1280, 720, 30)));
        assert_eq!(pick_camera_mode(&modes, 30, (320, 240)), Some((640, 480, 30)));
        assert_eq!(pick_camera_mode(&modes, 30, (480, 270)), Some((1280, 720, 30)));
        // Camera can't do 60: use its best rate (30), the filter duplicates up.
        assert_eq!(pick_camera_mode(&modes, 60, (320, 180)), Some((1280, 720, 30)));
        // Output slower than every camera rate: nothing to pick.
        assert_eq!(pick_camera_mode(&modes, 10, (320, 180)), None);
    }

    #[test]
    fn camera_prefers_its_fastest_rate_up_to_the_stream_rate() {
        let modes = vec![(1280, 720, vec![30.0]), (1920, 1080, vec![30.0, 60.0])];
        assert_eq!(pick_camera_mode(&modes, 60, (320, 180)), Some((1920, 1080, 60)));
        assert_eq!(pick_camera_mode(&modes, 30, (320, 180)), Some((1280, 720, 30)));
    }

    #[test]
    fn parses_refresh_rate() {
        assert_eq!(parse_hz(" 100.00Hz"), Some(100));
        assert_eq!(parse_hz("59.94Hz"), Some(60));
    }
}
