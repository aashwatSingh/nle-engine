//! The one rule every background job in the editor has to follow: report
//! back, even when the work panics.
//!
//! Each job system here follows the same shape — mark the work as running,
//! hand it to a worker thread, and clear that mark when the result arrives.
//! A panic breaks it in the same way every time: the send (or the store)
//! is the last statement in the thread body, so a panic skips it and the
//! job stays marked as running forever. Nothing retries it, because every
//! one of these refuses to start work that is already in flight, and the
//! "still working" flags feed `request_repaint`, so a stranded job also
//! leaves the editor redrawing at full rate for the rest of the session.
//!
//! `AnalysisJobs::spawn` already guards its worker for exactly this reason
//! (the August audit's "worker panic strands jobs" finding); the other four
//! spawn sites did not, so the guard lives here now rather than as a fifth
//! and sixth copy of the same `catch_unwind`.

/// Runs `work`, returning `None` if it panicked.
///
/// The panic is swallowed deliberately: the caller's job is to turn `None`
/// into whatever "this failed" means for its own job type, so the failure
/// surfaces through the same path as any other failure instead of killing
/// the thread silently. Rust still prints the panic and its backtrace to
/// stderr before unwinding, so nothing is lost for debugging.
pub(crate) fn catch_panic<T>(work: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_none_instead_of_killing_the_thread() {
        let done = std::thread::spawn(|| catch_panic(|| -> u32 { panic!("decoder blew up") }))
            .join()
            .expect("the guard must keep the panic from reaching the thread boundary");
        assert_eq!(done, None);
    }

    #[test]
    fn a_result_passes_straight_through() {
        assert_eq!(catch_panic(|| 7), Some(7));
    }
}
