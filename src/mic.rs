//! Lossless mic capture through a tiny Swift helper (`helpers/miccap.swift`).
//!
//! ffmpeg's avfoundation input drops ~11% of mic buffers on macOS, which
//! shows up as periodic silence gaps in the stream. The helper reads the mic
//! with AVCaptureSession instead and feeds raw PCM to ffmpeg on a pipe. It is
//! compiled once with `swiftc` and cached next to the config.
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

/// Placeholder in ffmpeg argv for the helper's timeline origin (host-clock
/// seconds). The launcher replaces it with a fresh value on every (re)start.
pub const ORIGIN: &str = "@MIC_ORIGIN@";

const SOURCE: &str = include_str!("../helpers/miccap.swift");

/// A prepared helper binary plus the device it should capture.
#[derive(Debug, Clone)]
pub struct MicCapture {
    pub exe: PathBuf,
    pub device: String,
}

impl MicCapture {
    /// Build (or reuse) the cached helper for `device`. Fails with the
    /// compiler output when `swiftc` is missing or rejects the source.
    pub fn prepare(device: &str) -> Result<Self> {
        let dir = crate::streams::base_dir().join("bin");
        std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let exe = dir.join(format!("miccap-{:016x}", fnv1a(SOURCE)));
        if !exe.exists() {
            let src = exe.with_extension("swift");
            std::fs::write(&src, SOURCE).with_context(|| format!("failed to write {}", src.display()))?;
            let tmp = exe.with_extension("tmp");
            let out = Command::new("swiftc")
                .args(["-O", "-o"])
                .arg(&tmp)
                .arg(&src)
                .output()
                .context("could not run `swiftc` (install Xcode command line tools: xcode-select --install)")?;
            if !out.status.success() {
                bail!("swiftc failed to build the mic helper:\n{}", String::from_utf8_lossy(&out.stderr));
            }
            std::fs::rename(&tmp, &exe).with_context(|| format!("failed to install {}", exe.display()))?;
        }
        Ok(Self { exe, device: device.to_string() })
    }

    /// Current host-clock reading in seconds, the shared time base for the
    /// helper's audio and ffmpeg's avfoundation video timestamps.
    pub fn now(&self) -> Result<String> {
        let out = Command::new(&self.exe).arg("now").output().context("failed to run mic helper")?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || text.is_empty() {
            bail!("mic helper could not read the host clock (exit {})", out.status);
        }
        Ok(text)
    }

    /// Capture command for a timeline starting at `origin`.
    pub fn capture_command(&self, origin: &str) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg(&self.device).arg(origin);
        cmd
    }
}

/// Stable content hash so an edited helper source gets a fresh binary.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_changes_with_source() {
        assert_ne!(fnv1a("a"), fnv1a("b"));
        assert_eq!(fnv1a("same"), fnv1a("same"));
    }
}
