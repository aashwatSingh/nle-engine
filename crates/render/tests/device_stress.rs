//! Regression harness for the GPU-driver crash that `headless_context()`
//! used to cause: it built a fresh wgpu `Instance` + `Device` per call, and
//! concurrent creation/teardown faulted the process
//! (`STATUS_ACCESS_VIOLATION` / `STATUS_HEAP_CORRUPTION`). It surfaced as
//! `cargo test --workspace` crashing roughly 1 run in 6, reported as a crash
//! rather than a test failure — so a "how many tests failed" check read zero
//! and missed it entirely.
//!
//! Measured with this harness at 24 threads x 10 iterations: **4/8 runs
//! crashed before the fix, 0/8 after**, and 0/10 after at 32x20. See
//! `docs/evidence/2026-08-19-export-access-violation.md`.
//!
//! `#[ignore]`d because it is a stress test, not a unit test — it is
//! deliberately slow and hammers the GPU. Run it after touching
//! `headless_context` or anything about device lifetime:
//!   cargo test -p render --test device_stress -- --ignored --nocapture
//! Tune with STRESS_THREADS / STRESS_ITERS.

use std::sync::Arc;

#[test]
#[ignore]
fn concurrent_headless_contexts_do_not_crash() {
    let threads: usize = std::env::var("STRESS_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(18);
    let iters: usize = std::env::var("STRESS_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let barrier = Arc::new(std::sync::Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Start together so creation actually overlaps.
                barrier.wait();
                for i in 0..iters {
                    let ctx = render::headless_context();
                    assert!(ctx.is_some(), "thread {t} iter {i}: no GPU adapter");
                    // Drop here: teardown overlapping with other threads'
                    // creation is the suspected race.
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("a stress thread panicked");
    }
    println!("survived {threads} threads x {iters} device create/drop cycles");
}
