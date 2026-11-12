use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::io::Read;

/// RTP files carry samples, not an executable firmware image or a header.
/// Preserve all bytes, including silence and braking samples. Limit UI
/// effects to one second and require an explicit sample rate.
pub fn load(rtp: &skyward_config::Rtp) -> Result<(Vec<i8>, u32)> {
    ensure!(matches!(rtp.rate, 8000 | 24000), "unsupported RTP sample rate {}", rtp.rate);
    let file = File::open(&rtp.path).with_context(|| format!("open RTP waveform {}", rtp.path))?;
    ensure!(file.metadata()?.is_file(), "RTP waveform is not a regular file: {}", rtp.path);
    let mut bytes = Vec::new();
    file.take(u64::from(rtp.rate) + 1).read_to_end(&mut bytes)?;
    ensure!(!bytes.is_empty() && bytes.len() <= rtp.rate as usize, "RTP waveform must contain up to one second of samples: {}", rtp.path);
    ensure!(bytes.iter().any(|&b| b != 0), "RTP waveform contains only silence: {}", rtp.path);
    Ok((bytes.into_iter().map(|b| b as i8).collect(), rtp.rate))
}

/// Scale RTP samples directly so intensity remains adjustable on drivers
/// that do not apply periodic.magnitude to custom FIFO streams.
pub fn scaled(samples: &[i8], share: f32) -> Vec<i8> {
    let share = if share.is_finite() { share.clamp(0.0, 1.0) } else { 0.0 };
    samples.iter().map(|&s| (f32::from(s) * share).round() as i8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strength_scales_both_polarities_and_keeps_silence() {
        let samples = [0,127,-128,64,-64,0];
        assert_eq!(scaled(&samples,0.25),[0,32,-32,16,-16,0]);
        assert_eq!(scaled(&samples,0.5),[0,64,-64,32,-32,0]);
        assert_eq!(scaled(&samples,1.0),samples);
        assert_eq!(scaled(&samples,f32::NAN),[0;6]);
    }
    #[test]
    fn rtp_preserves_signed_samples_and_silence_without_synthesis() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), [0u8, 127, 255, 128, 1, 0]).unwrap();
        let rtp = skyward_config::Rtp { path: file.path().display().to_string(), rate: 24000 };
        let (data, rate) = load(&rtp).unwrap();
        assert_eq!(data, [0, 127, -1, -128, 1, 0]);
        assert_eq!(rate, 24000);
        std::fs::write(file.path(), [0u8; 8]).unwrap();
        assert!(load(&rtp).is_err());
        std::fs::write(file.path(), vec![1u8; 24001]).unwrap();
        assert!(load(&rtp).is_err());
    }
}
