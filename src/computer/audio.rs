//! Cross-platform audio capture for desktop agents.
//!
//! Provides microphone input and (where supported) system audio loopback
//! capture.  Includes VAD (Voice Activity Detection) and simple audio
//! event classification for error chimes and notifications.
//!
//! # Usage
//!
//! ```rust,no_run
//! use syscity::computer::audio::{AudioCapture, AudioSource};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut capture = AudioCapture::new()?;
//! let mut rx = capture.start(AudioSource::Microphone).await?;
//!
//! while let Some(segment) = rx.recv().await {
//!     println!("Captured {}ms of audio", segment.duration_ms);
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{error, info};

/// A captured audio segment (16 kHz mono f32).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSegment {
    /// Capture timestamp.
    pub timestamp: Instant,
    /// PCM samples at 16 kHz, mono, f32 [-1.0, 1.0].
    pub samples: Vec<f32>,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Source of the audio.
    pub source: AudioSource,
}

impl AudioSegment {
    /// Compute RMS (root-mean-square) energy of the segment.
    pub fn rms_energy(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = self.samples.iter().map(|s| s * s).sum();
        (sum_sq / self.samples.len() as f32).sqrt()
    }

    /// Detect whether this segment contains voice activity (simple
    /// energy-based VAD).
    pub fn has_voice_activity(&self, threshold_db: f32) -> bool {
        let rms = self.rms_energy();
        let db = 20.0 * rms.max(1e-10).log10();
        db > threshold_db
    }
}

/// Audio input source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioSource {
    /// System audio output (loopback / monitor).  Not available on all
    /// platforms without a virtual audio driver.
    SystemOutput,
    /// Physical microphone input.
    Microphone,
}

impl std::fmt::Display for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioSource::SystemOutput => write!(f, "system_output"),
            AudioSource::Microphone => write!(f, "microphone"),
        }
    }
}

/// Detected audio event classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedAudioEvent {
    /// System error chime / beep.
    ErrorChime,
    /// System notification sound.
    Notification,
    /// Detected human speech (after VAD).
    Speech,
    /// Silence lasting longer than threshold.
    Silence { duration_ms: u64 },
}

/// Cross-platform audio capture using cpal.
pub struct AudioCapture {
    sample_rate: u32,
    running: Arc<AtomicBool>,
}

impl AudioCapture {
    /// Create a new capture instance with default 16 kHz sample rate.
    pub fn new() -> crate::computer::Result<Self> {
        Ok(Self {
            sample_rate: 16_000,
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start capturing audio from the given source.
    ///
    /// Returns a channel receiver that yields [`AudioSegment`]s as they
    /// are captured.
    pub async fn start(
        &mut self,
        source: AudioSource,
    ) -> crate::computer::Result<mpsc::Receiver<AudioSegment>> {
        let (tx, rx) = mpsc::channel(64);
        self.running.store(true, Ordering::SeqCst);

        let running = self.running.clone();
        let sample_rate = self.sample_rate;

        // Spawn the cpal capture loop on a blocking thread because cpal
        // uses OS callbacks.
        tokio::task::spawn_blocking(move || {
            if let Err(e) = run_cpal_capture(source, sample_rate, tx, running) {
                error!("Audio capture failed: {}", e);
            }
        });

        Ok(rx)
    }

    /// Stop the capture stream.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Analyse a single segment and classify detected events.
    ///
    /// This is a lightweight heuristic classifier (not ML-based).  For
    /// production-grade detection, integrate a small ONNX audio classifier.
    pub fn analyze_segment(&self, segment: &AudioSegment) -> Vec<DetectedAudioEvent> {
        let mut events = Vec::new();
        let rms = segment.rms_energy();

        // Silence detection.
        if rms < 0.001 {
            events.push(DetectedAudioEvent::Silence {
                duration_ms: segment.duration_ms,
            });
            return events;
        }

        // Voice activity detection.
        if segment.has_voice_activity(-40.0) {
            // Distinguish speech from short chirps by duration.
            if segment.duration_ms >= 300 {
                events.push(DetectedAudioEvent::Speech);
                return events;
            }
        }

        // Short tonal sounds: classify by duration and spectral centroid
        // heuristic (simple zero-crossing rate approximation).
        if segment.duration_ms < 500 {
            let zcr = zero_crossing_rate(&segment.samples);
            if zcr < 0.05 {
                // Very low ZCR → likely a tonal beep/chime.
                if rms > 0.1 {
                    events.push(DetectedAudioEvent::ErrorChime);
                } else {
                    events.push(DetectedAudioEvent::Notification);
                }
            }
        }

        events
    }
}

impl Default for AudioCapture {
    fn default() -> Self {
        #[allow(clippy::expect_used)] // Default trait cannot return Result
        Self::new().expect("default AudioCapture should not fail")
    }
}

// ---------------------------------------------------------------------------
// cpal backend
// ---------------------------------------------------------------------------

fn run_cpal_capture(
    source: AudioSource,
    target_rate: u32,
    tx: mpsc::Sender<AudioSegment>,
    running: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();

    let device = match source {
        AudioSource::Microphone => host
            .default_input_device()
            .ok_or("no input device available")?,
        AudioSource::SystemOutput => {
            #[cfg(target_os = "linux")]
            {
                find_pulse_monitor_device(&host)?
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err("SystemOutput loopback not supported on this platform without \
                            virtual audio driver"
                    .into());
            }
        }
    };

    let config = device.default_input_config()?;
    let sample_rate = config.sample_rate();
    let channels = config.channels() as usize;

    info!("Audio capture started: {} @ {} Hz, {} channels", source, sample_rate, channels);

    // Channel from cpal callback → accumulator thread.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let running_for_callback = running.clone();

    let err_fn = move |err: cpal::Error| {
        error!("Audio stream error: {}", err);
    };

    // Build the stream.  The callback only sends raw mono samples
    // through a std::sync::mpsc channel — no unsafe required.
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config.into(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !running_for_callback.load(Ordering::SeqCst) {
                    return;
                }
                let mono: Vec<f32> = data
                    .chunks(channels)
                    .map(|ch| ch.iter().sum::<f32>() / channels as f32)
                    .collect();
                let _ = raw_tx.send(mono);
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => {
            let max = i16::MAX as f32;
            device.build_input_stream(
                config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    if !running_for_callback.load(Ordering::SeqCst) {
                        return;
                    }
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|ch| ch.iter().map(|s| *s as f32 / max).sum::<f32>() / channels as f32)
                        .collect();
                    let _ = raw_tx.send(mono);
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let offset = u16::MAX as f32 / 2.0;
            let scale = offset;
            device.build_input_stream(
                config.into(),
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    if !running_for_callback.load(Ordering::SeqCst) {
                        return;
                    }
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|ch| {
                            ch.iter().map(|s| (*s as f32 - offset) / scale).sum::<f32>()
                                / channels as f32
                        })
                        .collect();
                    let _ = raw_tx.send(mono);
                },
                err_fn,
                None,
            )?
        }
        _ => {
            return Err(format!("unsupported sample format: {:?}", config.sample_format()).into());
        }
    };

    stream.play()?;

    // Accumulate raw samples into 100 ms segments and forward to tokio.
    let chunk_samples = (target_rate as usize * 100) / 1000;
    let mut buffer = Vec::with_capacity(chunk_samples);
    let start_time = Instant::now();

    while running.load(Ordering::SeqCst) {
        match raw_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => {
                buffer.extend(samples);
                while buffer.len() >= chunk_samples {
                    let segment = AudioSegment {
                        timestamp: start_time
                            + Duration::from_millis(
                                (buffer.len() as f64 / target_rate as f64 * 1000.0) as u64,
                            ),
                        samples: buffer[..chunk_samples].to_vec(),
                        duration_ms: (chunk_samples as u64 * 1000) / target_rate as u64,
                        source,
                    };
                    if tx.blocking_send(segment).is_err() {
                        break;
                    }
                    buffer.drain(..chunk_samples);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    drop(stream);
    info!("Audio capture stopped");
    Ok(())
}

#[cfg(target_os = "linux")]
fn find_pulse_monitor_device(
    host: &cpal::Host,
) -> Result<cpal::Device, Box<dyn std::error::Error + Send + Sync>> {
    use cpal::traits::HostTrait;

    // cpal on Linux with PulseAudio typically names monitor sources
    // similarly to output devices.  Try to find one with "monitor" in
    // the name.
    for device in host.input_devices()? {
        // `DeviceTrait::name` was replaced by `Display` in cpal 0.18.
        let name = device.to_string();
        if name.to_lowercase().contains("monitor") {
            return Ok(device);
        }
    }
    // Fallback to default input device (which might be a monitor).
    host.default_input_device()
        .ok_or_else(|| "no monitor device found".into())
}

// ---------------------------------------------------------------------------
// Audio analysis helpers
// ---------------------------------------------------------------------------

/// Zero-crossing rate: proportion of samples that cross the zero axis.
fn zero_crossing_rate(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let crossings = samples
        .windows(2)
        .filter(|w| w[0].signum() != w[1].signum())
        .count();
    crossings as f32 / (samples.len() - 1) as f32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_segment_rms_energy() {
        // Sine wave at full scale.
        let samples: Vec<f32> = (0..160)
            .map(|i| (i as f32 / 160.0 * std::f32::consts::TAU).sin())
            .collect();
        let seg = AudioSegment {
            timestamp: Instant::now(),
            samples: samples.clone(),
            duration_ms: 10,
            source: AudioSource::Microphone,
        };
        let rms = seg.rms_energy();
        assert!(rms > 0.5 && rms <= 1.0, "RMS of sine wave should be ~0.707");
    }

    #[test]
    fn test_audio_segment_silence() {
        let seg = AudioSegment {
            timestamp: Instant::now(),
            samples: vec![0.0; 160],
            duration_ms: 10,
            source: AudioSource::Microphone,
        };
        assert!(!seg.has_voice_activity(-40.0));
    }

    #[test]
    fn test_zero_crossing_rate() {
        // High-frequency sine wave → high ZCR.
        let high_freq: Vec<f32> = (0..1000)
            .map(|i| (i as f32 / 10.0 * std::f32::consts::TAU).sin())
            .collect();
        let zcr_high = zero_crossing_rate(&high_freq);
        assert!(zcr_high > 0.1, "high freq sine should have high ZCR");

        // Near-DC signal → very low ZCR.
        let low_freq: Vec<f32> = (0..1000)
            .map(|i| (i as f32 / 10000.0 * std::f32::consts::TAU).sin())
            .collect();
        let zcr_low = zero_crossing_rate(&low_freq);
        assert!(zcr_low < 0.01, "near-DC should have very low ZCR");
    }

    #[test]
    fn test_analyze_silence() {
        let capture = AudioCapture::new().unwrap();
        let seg = AudioSegment {
            timestamp: Instant::now(),
            samples: vec![0.0; 1600], // 100ms @ 16kHz
            duration_ms: 100,
            source: AudioSource::Microphone,
        };
        let events = capture.analyze_segment(&seg);
        assert!(events.contains(&DetectedAudioEvent::Silence { duration_ms: 100 }));
    }

    #[test]
    fn test_analyze_speech() {
        let capture = AudioCapture::new().unwrap();
        // Simulated speech: moderate energy, longer duration.
        let mut samples = vec![0.0; 4800]; // 300ms
        for (i, s) in samples.iter_mut().enumerate() {
            *s = 0.3 * (i as f32 / 100.0 * std::f32::consts::TAU).sin();
        }
        let seg = AudioSegment {
            timestamp: Instant::now(),
            samples,
            duration_ms: 300,
            source: AudioSource::Microphone,
        };
        let events = capture.analyze_segment(&seg);
        assert!(events.contains(&DetectedAudioEvent::Speech));
    }
}
