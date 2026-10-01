//! Spotifyd subprocess sink that tees decoded PCM to ALSA and Orpheus.
//!
//! Spotifyd supplies signed 16-bit little-endian, 44.1 kHz stereo PCM on
//! stdin. The original bytes are paced through `aplay` unchanged, while a
//! lightweight 8 kHz mono copy is sent to the visualizer's local UDP socket.

use std::io::{self, Read, Write};
use std::net::UdpSocket;
use std::process::{Command, Stdio};

const INPUT_RATE: u32 = 44_100;
const VISUALIZER_RATE: u32 = 8_000;
const INPUT_FRAME_BYTES: usize = 4;
const DEFAULT_ALSA_DEVICE: &str = "orpheus";
const DEFAULT_VISUALIZER_TARGET: &str = "127.0.0.1:5568";

fn main() {
    if let Err(error) = run() {
        eprintln!("[SPOTIFY-BRIDGE] {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let alsa_device =
        std::env::var("ORPHEUS_ALSA_DEVICE").unwrap_or_else(|_| DEFAULT_ALSA_DEVICE.to_string());
    let visualizer_target = std::env::var("ORPHEUS_VISUALIZER_TARGET")
        .unwrap_or_else(|_| DEFAULT_VISUALIZER_TARGET.to_string());

    let mut aplay = Command::new("/usr/bin/aplay")
        .args([
            "-q",
            "-D",
            &alsa_device,
            "-t",
            "raw",
            "-f",
            "S16_LE",
            "-c",
            "2",
            "-r",
            "44100",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start aplay for '{alsa_device}': {error}"))?;
    let mut playback = aplay
        .stdin
        .take()
        .ok_or_else(|| "aplay did not provide an input pipe".to_string())?;
    let visualizer = UdpSocket::bind("127.0.0.1:0")
        .and_then(|socket| {
            socket.connect(&visualizer_target)?;
            Ok(socket)
        })
        .map_err(|error| {
            format!("could not connect visualizer UDP to '{visualizer_target}': {error}")
        })?;

    eprintln!(
        "[SPOTIFY-BRIDGE] Forwarding S16_LE/44100/stereo to '{alsa_device}' and {visualizer_target}"
    );
    let result = pump_audio(io::stdin().lock(), &mut playback, |packet| {
        // Visualization is best-effort and must never stop playback.
        let _ = visualizer.send(packet);
    });
    drop(playback);

    if result.is_err() {
        let _ = aplay.kill();
    }
    let status = aplay
        .wait()
        .map_err(|error| format!("could not reap aplay: {error}"))?;
    result.map_err(|error| format!("audio forwarding failed: {error}"))?;
    if !status.success() {
        return Err(format!("aplay exited with {status}"));
    }
    Ok(())
}

fn pump_audio<R, W, F>(mut input: R, mut playback: W, mut visualize: F) -> io::Result<()>
where
    R: Read,
    W: Write,
    F: FnMut(&[u8]),
{
    let mut downsampler = StereoDownsampler::new();
    let mut input_buffer = [0_u8; 16 * 1024];
    loop {
        let length = input.read(&mut input_buffer)?;
        if length == 0 {
            playback.flush()?;
            return Ok(());
        }

        // aplay provides the pacing/backpressure. Sending the spectrum copy
        // after this write keeps it close to the audio entering ALSA.
        playback.write_all(&input_buffer[..length])?;
        let visualization = downsampler.push(&input_buffer[..length]);
        if !visualization.is_empty() {
            visualize(&visualization);
        }
    }
}

struct StereoDownsampler {
    phase: u32,
    carry: Vec<u8>,
}

impl StereoDownsampler {
    fn new() -> Self {
        Self {
            phase: 0,
            carry: Vec::with_capacity(INPUT_FRAME_BYTES - 1),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(bytes);
        let complete_length = self.carry.len() / INPUT_FRAME_BYTES * INPUT_FRAME_BYTES;
        let mut output = Vec::with_capacity(
            complete_length / INPUT_FRAME_BYTES * VISUALIZER_RATE as usize / INPUT_RATE as usize
                * 2
                + 2,
        );

        for frame in self.carry[..complete_length].chunks_exact(INPUT_FRAME_BYTES) {
            self.phase += VISUALIZER_RATE;
            if self.phase >= INPUT_RATE {
                self.phase -= INPUT_RATE;
                let left = i16::from_le_bytes([frame[0], frame[1]]) as i32;
                let right = i16::from_le_bytes([frame[2], frame[3]]) as i32;
                let mono = ((left + right) / 2) as i16;
                output.extend_from_slice(&mono.to_le_bytes());
            }
        }

        if complete_length > 0 {
            self.carry.drain(..complete_length);
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_second_of_stereo_becomes_one_second_of_mono() {
        let mut input = Vec::with_capacity(INPUT_RATE as usize * INPUT_FRAME_BYTES);
        for _ in 0..INPUT_RATE {
            input.extend_from_slice(&12_000_i16.to_le_bytes());
            input.extend_from_slice(&8_000_i16.to_le_bytes());
        }

        let output = StereoDownsampler::new().push(&input);

        assert_eq!(output.len(), VISUALIZER_RATE as usize * 2);
        assert!(
            output
                .chunks_exact(2)
                .all(|sample| { i16::from_le_bytes([sample[0], sample[1]]) == 10_000 })
        );
    }

    #[test]
    fn unaligned_input_chunks_preserve_complete_stereo_frames() {
        let frames = [
            1_000_i16.to_le_bytes(),
            3_000_i16.to_le_bytes(),
            5_000_i16.to_le_bytes(),
            7_000_i16.to_le_bytes(),
        ]
        .concat();
        let mut downsampler = StereoDownsampler::new();

        let first = downsampler.push(&frames[..3]);
        let second = downsampler.push(&frames[3..]);

        assert!(first.is_empty());
        assert!(second.len() <= 2);
        assert!(downsampler.carry.is_empty());
    }

    #[test]
    fn pump_forwards_every_original_byte_unchanged() {
        let input = vec![0x5a; 32 * 1024 + 7];
        let mut playback = Vec::new();
        let mut visualization_packets = 0;

        pump_audio(input.as_slice(), &mut playback, |_| {
            visualization_packets += 1
        })
        .unwrap();

        assert_eq!(playback, input);
        assert!(visualization_packets >= 2);
    }
}
