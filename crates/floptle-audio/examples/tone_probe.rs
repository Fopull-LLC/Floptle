//! Device-path probe: opens the real output device, plays a synthesized tone
//! through a mixer track with an effect, and verifies status/meters flow back.
//! The unit tests cover the DSP headless; this exercises the one part they
//! can't — the cpal stream. Run: `cargo run -p floptle-audio --example tone_probe`

use std::sync::Arc;

use floptle_audio::{
    AudioEngine, Clip, EffectDesc, EffectSlot, MixerDesc, PlayParams, SpatialMode, TrackDesc,
};

fn main() {
    env_logger::init();
    let mut engine = match AudioEngine::new() {
        Ok(e) => e,
        Err(e) => {
            println!("no audio device ({e}) — probe skipped");
            return;
        }
    };
    println!("device open at {} Hz", engine.sample_rate);

    // A mixer with one effected track.
    let mut mixer = MixerDesc::default();
    let mut track = TrackDesc::new("Probe");
    track.effects.push(EffectSlot {
        effect: EffectDesc::Delay {
            time_ms: 140.0,
            feedback: 0.4,
            mix: 0.4,
            ping_pong: true,
            damping: 0.3,
        },
        bypass: false,
    });
    mixer.tracks.push(track);
    engine.set_mixer(&mixer);

    // A 440 Hz tone clip, 0.4 s.
    let sr = engine.sample_rate;
    let clip = Arc::new(Clip {
        sample_rate: sr,
        channels: 1,
        samples: (0..(sr as usize * 2 / 5))
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / sr as f32).sin() * 0.25)
            .collect(),
    });
    let params = PlayParams { mode: SpatialMode::Flat, track: "Probe".into(), ..Default::default() };
    let id = engine.play(clip, None, params);

    std::thread::sleep(std::time::Duration::from_millis(200));
    let mid = engine.status(id);
    let meters = engine.meters();
    std::thread::sleep(std::time::Duration::from_millis(1300)); // tone + delay tail
    let done = engine.drain_finished();

    println!("mid-play status: {mid:?}");
    println!("meters: {meters:?}");
    println!("finished: {done:?}");
    let ok = mid.is_some_and(|s| s.playing)
        && meters.iter().any(|(n, l)| n == "Probe" && *l > 0.01)
        && done.contains(&id);
    println!("{}", if ok { "PROBE OK" } else { "PROBE FAILED" });
}
