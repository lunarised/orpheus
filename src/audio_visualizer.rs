use std::collections::VecDeque;
use std::f32::consts::PI;
use std::net::UdpSocket;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

pub const BAR_COUNT: usize = 24;
const FFT_SIZE: usize = 512;
const SAMPLE_RATE: f32 = 8_000.0;
const MIN_FREQUENCY: f32 = 45.0;
const MAX_FREQUENCY: f32 = 3_900.0;
const UPDATE_INTERVAL: Duration = Duration::from_millis(30);

pub type SpectrumFrame = [f32; BAR_COUNT];

/// Listen for signed 16-bit, little-endian, 8 kHz mono PCM sent by Mopidy's
/// GStreamer branch or the Spotifyd audio bridge. Analysis stays entirely off
/// the UI thread, and the UI drains every queued update so it always renders
/// the newest spectrum rather than preserving stale frames.
pub fn spawn_worker(bind_address: &str) -> Receiver<SpectrumFrame> {
    let (updates_tx, updates_rx) = mpsc::channel();
    let bind_address = bind_address.to_string();
    std::thread::spawn(move || run_worker(&bind_address, updates_tx));
    updates_rx
}

fn run_worker(bind_address: &str, updates: Sender<SpectrumFrame>) {
    let socket = match UdpSocket::bind(bind_address) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("[VIS] Failed to listen on {bind_address}: {error}");
            return;
        }
    };
    if let Err(error) = socket.set_read_timeout(Some(Duration::from_millis(500))) {
        eprintln!("[VIS] Failed to configure audio socket: {error}");
    }
    println!("[VIS] Listening for playback PCM on udp://{bind_address}");

    let mut analyzer = SpectrumAnalyzer::new();
    let mut packet = [0u8; 16 * 1024];
    let mut received_audio = false;
    loop {
        match socket.recv(&mut packet) {
            Ok(length) => {
                if !received_audio {
                    println!("[VIS] Live playback audio detected");
                    received_audio = true;
                }
                if let Some(frame) = analyzer.push_pcm(&packet[..length])
                    && updates.send(frame).is_err()
                {
                    return;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                eprintln!("[VIS] Audio socket failed: {error}");
                return;
            }
        }
    }
}

struct SpectrumAnalyzer {
    samples: VecDeque<f32>,
    smoothed: SpectrumFrame,
    adaptive_peak: f32,
    last_update: Instant,
}

impl SpectrumAnalyzer {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(FFT_SIZE),
            smoothed: [0.0; BAR_COUNT],
            adaptive_peak: 0.001,
            last_update: Instant::now() - UPDATE_INTERVAL,
        }
    }

    fn push_pcm(&mut self, bytes: &[u8]) -> Option<SpectrumFrame> {
        for sample in bytes.chunks_exact(2) {
            let value = i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32_768.0;
            if self.samples.len() == FFT_SIZE {
                self.samples.pop_front();
            }
            self.samples.push_back(value);
        }

        if self.samples.len() < FFT_SIZE || self.last_update.elapsed() < UPDATE_INTERVAL {
            return None;
        }
        self.last_update = Instant::now();
        let samples: Vec<_> = self.samples.iter().copied().collect();
        let raw = frequency_bands(&samples);
        let frame_peak = raw.iter().copied().fold(0.0_f32, f32::max);
        self.adaptive_peak = if frame_peak > self.adaptive_peak {
            frame_peak
        } else {
            (self.adaptive_peak * 0.985).max(0.000_05)
        };

        for (index, level) in self.smoothed.iter_mut().enumerate() {
            let frequency_balance = 0.75 + index as f32 / (BAR_COUNT - 1) as f32 * 0.5;
            let target = if frame_peak < 0.000_01 {
                0.0
            } else {
                (raw[index] * frequency_balance / self.adaptive_peak)
                    .sqrt()
                    .clamp(0.0, 1.0)
            };
            let smoothing = if target > *level { 0.62 } else { 0.16 };
            *level += (target - *level) * smoothing;
        }
        Some(self.smoothed)
    }
}

fn frequency_bands(samples: &[f32]) -> SpectrumFrame {
    debug_assert_eq!(samples.len(), FFT_SIZE);
    let mut real = vec![0.0_f32; FFT_SIZE];
    let mut imaginary = vec![0.0_f32; FFT_SIZE];
    for (index, sample) in samples.iter().enumerate() {
        let window = 0.5 - 0.5 * (2.0 * PI * index as f32 / (FFT_SIZE - 1) as f32).cos();
        real[index] = sample * window;
    }
    fft(&mut real, &mut imaginary);

    let mut bands = [0.0; BAR_COUNT];
    let frequency_ratio = MAX_FREQUENCY / MIN_FREQUENCY;
    for (band, value) in bands.iter_mut().enumerate() {
        let low = MIN_FREQUENCY * frequency_ratio.powf(band as f32 / BAR_COUNT as f32);
        let high = MIN_FREQUENCY * frequency_ratio.powf((band + 1) as f32 / BAR_COUNT as f32);
        let first_bin = ((low * FFT_SIZE as f32 / SAMPLE_RATE).floor() as usize).max(1);
        let last_bin = ((high * FFT_SIZE as f32 / SAMPLE_RATE).ceil() as usize)
            .max(first_bin + 1)
            .min(FFT_SIZE / 2);
        let mut energy = 0.0;
        for bin in first_bin..last_bin {
            energy += real[bin] * real[bin] + imaginary[bin] * imaginary[bin];
        }
        *value = (energy / (last_bin - first_bin) as f32).sqrt() / FFT_SIZE as f32;
    }
    bands
}

fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    let size = real.len();
    debug_assert!(size.is_power_of_two());
    let mut reversed = 0;
    for index in 1..size {
        let mut bit = size >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            real.swap(index, reversed);
            imaginary.swap(index, reversed);
        }
    }

    let mut length = 2;
    while length <= size {
        let angle = -2.0 * PI / length as f32;
        let step_real = angle.cos();
        let step_imaginary = angle.sin();
        for start in (0..size).step_by(length) {
            let mut twiddle_real = 1.0;
            let mut twiddle_imaginary = 0.0;
            for offset in 0..length / 2 {
                let even = start + offset;
                let odd = even + length / 2;
                let odd_real = real[odd] * twiddle_real - imaginary[odd] * twiddle_imaginary;
                let odd_imaginary = real[odd] * twiddle_imaginary + imaginary[odd] * twiddle_real;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;

                let next_real = twiddle_real * step_real - twiddle_imaginary * step_imaginary;
                twiddle_imaginary = twiddle_real * step_imaginary + twiddle_imaginary * step_real;
                twiddle_real = next_real;
            }
        }
        length *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_produces_empty_bands() {
        assert_eq!(frequency_bands(&[0.0; FFT_SIZE]), [0.0; BAR_COUNT]);
    }

    #[test]
    fn sine_wave_energy_lands_near_its_logarithmic_band() {
        let frequency = 440.0;
        let samples: Vec<_> = (0..FFT_SIZE)
            .map(|index| (2.0 * PI * frequency * index as f32 / SAMPLE_RATE).sin())
            .collect();
        let bands = frequency_bands(&samples);
        let strongest = bands
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .unwrap()
            .0;
        let expected = ((frequency / MIN_FREQUENCY).ln() / (MAX_FREQUENCY / MIN_FREQUENCY).ln()
            * BAR_COUNT as f32)
            .floor() as usize;

        assert!(
            strongest.abs_diff(expected) <= 1,
            "{strongest} vs {expected}"
        );
        assert!(bands[strongest] > 0.1);
    }
}
