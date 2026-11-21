//! The log flush on the way out. GPUI calls every quit hook, then waits on the UI thread for
//! their futures for [`gpui_kit::SHUTDOWN_TIMEOUT`]. Each hook that may log joins here first. The
//! last to finish flushes, off the UI thread, so a stalled log writer cannot use up that time. A
//! hook still working at [`EARLY_DRAIN`] gets a drain first, since on macOS the process then ends.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::{App, BackgroundExecutor, Global};

/// When to drain the log if a quit hook is still working: inside GPUI's wait, with time left.
const EARLY_DRAIN: Duration = Duration::from_millis(120);

type Flush = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Hooks {
    /// How many quit hooks have not finished yet.
    unfinished: Rc<Cell<usize>>,
    /// The last hook finished, so the early drain is not needed.
    finished: Arc<AtomicBool>,
}

impl Global for Hooks {}

/// One quit hook's place in the final flush. Join in the hook itself, before its future runs, so
/// every hook has joined before the first one finishes.
pub(super) struct QuitFlush {
    unfinished: Rc<Cell<usize>>,
    finished: Arc<AtomicBool>,
    executor: BackgroundExecutor,
    flush: Flush,
}

impl QuitFlush {
    pub(super) fn join(cx: &mut App) -> Self {
        Self::join_with(cx, Arc::new(flint_core::bootstrap::flush_logs))
    }

    fn join_with(cx: &mut App, flush: Flush) -> Self {
        let hooks = cx.default_global::<Hooks>();
        let (unfinished, finished) = (hooks.unfinished.clone(), hooks.finished.clone());
        let executor = cx.background_executor().clone();
        if unfinished.get() == 0 {
            let timer = executor.timer(EARLY_DRAIN);
            let (finished, flush) = (finished.clone(), flush.clone());
            executor
                .spawn(async move {
                    timer.await;
                    if !finished.load(Ordering::Acquire) {
                        flush();
                    }
                })
                .detach();
        }
        unfinished.set(unfinished.get() + 1);
        Self {
            unfinished,
            finished,
            executor,
            flush,
        }
    }

    /// This hook's work is done. Flushes the log if no other hook is still working.
    pub(super) async fn finish(self) {
        let left = self.unfinished.get() - 1;
        self.unfinished.set(left);
        if left == 0 {
            self.finished.store(true, Ordering::Release);
            let flush = self.flush;
            self.executor.spawn(async move { flush() }).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui_kit::TestAppContext;

    use super::{EARLY_DRAIN, Flush, QuitFlush};

    fn counted() -> (Arc<AtomicUsize>, Flush) {
        let flushes = Arc::new(AtomicUsize::new(0));
        let counter = flushes.clone();
        (
            flushes,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[gpui_kit::test]
    async fn the_last_hook_to_finish_flushes_once(cx: &mut TestAppContext) {
        let (flushes, flush) = counted();
        let first = cx.update(|cx| QuitFlush::join_with(cx, flush.clone()));
        let second = cx.update(|cx| QuitFlush::join_with(cx, flush.clone()));
        first.finish().await;
        assert_eq!(flushes.load(Ordering::SeqCst), 0);
        second.finish().await;
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        cx.executor().advance_clock(EARLY_DRAIN);
        cx.run_until_parked();
        assert_eq!(flushes.load(Ordering::SeqCst), 1, "no early drain is left");
    }

    #[gpui_kit::test]
    async fn a_hook_still_working_does_not_hold_back_the_drain(cx: &mut TestAppContext) {
        let (flushes, flush) = counted();
        let saving = cx.update(|cx| QuitFlush::join_with(cx, flush.clone()));
        cx.run_until_parked();
        assert_eq!(flushes.load(Ordering::SeqCst), 0);
        cx.executor().advance_clock(EARLY_DRAIN);
        cx.run_until_parked();
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
        // What the save logged still gets its flush.
        saving.finish().await;
        assert_eq!(flushes.load(Ordering::SeqCst), 2);
    }
}
