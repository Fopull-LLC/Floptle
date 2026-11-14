//! Run by hand (it opens the machine's real output device):
//! `ALSA_CONFIG_PATH=<a config routing to the null PCM> cargo test -p floptle-audio --test output_cpu_probe -- --ignored --nocapture`.
//! Prints the CPU the process spent over three seconds of an idle mixer; on
//! the null PCM, an output not held to real time spins a core (about 3 s).

#[cfg(target_os = "linux")]
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    // Fields after the command name, which is in parentheses and may hold spaces.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let f: Vec<&str> = rest.split(' ').collect();
    let ticks: f64 = f[11].parse::<f64>().unwrap() + f[12].parse::<f64>().unwrap();
    ticks / 100.0
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "opens the real output device; run by hand"]
fn an_idle_output_holds_no_core() {
    let Ok(engine) = floptle_audio::engine::AudioEngine::new() else {
        eprintln!("no output device here");
        return;
    };
    let before = cpu_seconds();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let used = cpu_seconds() - before;
    drop(engine);
    eprintln!("idle output: {used:.2} s of CPU over 3 s");
    assert!(used < 0.3, "the output held a core: {used:.2} s of CPU over 3 s");
}
