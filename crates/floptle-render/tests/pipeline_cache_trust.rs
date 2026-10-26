//! A pipeline cache is trusted only once a run has saved it back cleanly.
//!
//! One saved while the driver was failing to map memory broke every start on
//! the machine that loaded it: every resource came out invalid from the first
//! texture on. Its own binary, because a reported GPU error is process-wide.

use floptle_render::Gpu;

#[test]
fn a_cache_left_by_a_run_that_never_finished_is_thrown_away() {
    let mut gpu = Gpu::headless(16, 16);
    if !gpu.device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        eprintln!("skipped: this device keeps no pipeline cache");
        return;
    }
    let dir = std::env::temp_dir().join(format!("floptle-pipeline-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let key = wgpu::util::pipeline_cache_key(&gpu.adapter.get_info()).expect("a cache key");
    let file = dir.join(&key);
    std::fs::write(&file, b"left by a run that never got as far as a clean frame").unwrap();
    // A run long gone (no such process) left its marker.
    let dead = dir.join(format!("{key}.open.4000000000"));
    std::fs::write(&dead, b"").unwrap();
    gpu.use_pipeline_cache(&dir);
    assert!(!file.exists(), "the cache an abandoned run left was kept");
    assert!(!dead.exists(), "the abandoned run's marker was kept");
    let mine = dir.join(format!("{key}.open.{}", std::process::id()));
    assert!(mine.exists(), "this run did not say it is using the cache");

    // A clean frame: saved back, and trusted next time.
    gpu.save_pipeline_cache();
    assert!(!mine.exists(), "saved cleanly, the cache is still marked untrusted");
    for _ in 0..100 {
        if file.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(file.exists(), "the clean save wrote no cache");

    // After a GPU error the cache is thrown away rather than saved.
    gpu.use_pipeline_cache(&dir);
    floptle_render::report_gpu_error(&wgpu::Error::Internal {
        source: Box::new(std::io::Error::other("a driver that failed")),
        description: "test".into(),
    });
    gpu.save_pipeline_cache();
    assert!(!file.exists(), "after a GPU error the cache was kept");
    let _ = std::fs::remove_dir_all(&dir);
}
