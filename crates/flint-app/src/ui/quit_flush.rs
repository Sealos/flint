//! The log flush on the way out. GPUI calls every quit hook, then waits on the UI thread for
//! their futures for a short time. Each hook that may log joins here first. The last to finish
//! flushes once, off the UI thread, so a stalled log writer cannot use up that time.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{App, BackgroundExecutor, Global};

/// How many quit hooks have not finished yet.
#[derive(Default)]
struct Unfinished(Rc<Cell<usize>>);

impl Global for Unfinished {}

/// One quit hook's place in the final flush. Join in the hook itself, before its future runs, so
/// every hook has joined before the first one finishes.
pub(super) struct QuitFlush {
    unfinished: Rc<Cell<usize>>,
    executor: BackgroundExecutor,
}

impl QuitFlush {
    pub(super) fn join(cx: &mut App) -> Self {
        let unfinished = cx.default_global::<Unfinished>().0.clone();
        unfinished.set(unfinished.get() + 1);
        Self {
            unfinished,
            executor: cx.background_executor().clone(),
        }
    }

    /// This hook's work is done. Flushes the log if no other hook is still working.
    pub(super) async fn finish(self) {
        self.finish_with(flint_core::bootstrap::flush_logs).await;
    }

    async fn finish_with(self, flush: impl FnOnce() + Send + 'static) {
        let left = self.unfinished.get() - 1;
        self.unfinished.set(left);
        if left == 0 {
            self.executor.spawn(async move { flush() }).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use gpui_kit::TestAppContext;

    use super::QuitFlush;

    #[gpui_kit::test]
    async fn the_last_hook_to_finish_flushes_once(cx: &mut TestAppContext) {
        let flushes = Arc::new(AtomicUsize::new(0));
        let counted = || {
            let flushes = flushes.clone();
            move || {
                flushes.fetch_add(1, Ordering::SeqCst);
            }
        };
        let first = cx.update(QuitFlush::join);
        let second = cx.update(QuitFlush::join);
        first.finish_with(counted()).await;
        assert_eq!(flushes.load(Ordering::SeqCst), 0);
        second.finish_with(counted()).await;
        assert_eq!(flushes.load(Ordering::SeqCst), 1);
    }
}
