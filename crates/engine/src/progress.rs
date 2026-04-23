//! Progress reporting trait for the fix engine.
//!
//! The engine crate outputs status messages during long-running operations
//! (particularly goose subprocess management). This trait abstracts the
//! output mechanism so the CLI can route messages through `indicatif`
//! progress bars while tests can capture or ignore them.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;

/// Thread-safe handle for printing progress lines without clobbering
/// any active `indicatif` progress bars.
///
/// Clone-able and `Send + Sync` — safe to pass into parallel threads.
#[derive(Clone)]
pub struct ProgressPrinter {
    multi: Arc<MultiProgress>,
}

impl ProgressPrinter {
    /// Create a printer backed by the given `MultiProgress`.
    pub fn new(multi: Arc<MultiProgress>) -> Self {
        Self { multi }
    }

    /// Create a printer that writes directly to stderr (no indicatif routing).
    pub fn stderr() -> Self {
        Self {
            multi: Arc::new(MultiProgress::new()),
        }
    }

    /// Print a line without clobbering active progress bars.
    pub fn println(&self, msg: &str) {
        let _ = self.multi.println(msg);
    }

    /// Start a counted progress bar.
    pub fn start_counted(&self, message: &str, total: u64) -> CountedBar {
        let style = ProgressStyle::with_template(
            "{msg}  {bar:30.cyan/dim} {pos}/{len}  [elapsed: {elapsed}, eta: {eta}]",
        )
        .unwrap()
        .progress_chars("██░");

        let pb = self.multi.add(ProgressBar::new(total));
        pb.set_style(style);
        pb.set_message(message.to_string());

        CountedBar { pb }
    }
}

/// A progress bar for work with a known item count.
///
/// Clone-able and thread-safe. All clones share the same underlying
/// `ProgressBar`. Call [`finish`](CountedBar::finish) on the primary
/// owner when all work is done — clones used in worker threads should
/// simply be dropped without finishing.
#[derive(Clone)]
pub struct CountedBar {
    pb: ProgressBar,
}

impl CountedBar {
    /// Increment by one.
    pub fn inc(&self) {
        self.pb.inc(1);
    }

    /// Update the display message.
    pub fn set_message(&self, msg: &str) {
        self.pb.set_message(msg.to_string());
    }

    /// Finish the bar — replaces the live animation with a static
    /// completion line. Call this exactly once on the primary owner
    /// after all work threads are done.
    pub fn finish(self) {
        let done_style =
            ProgressStyle::with_template("✓ {msg}  {len} items  [{elapsed}]").unwrap();
        self.pb.set_style(done_style);
        self.pb.finish();
    }
}
