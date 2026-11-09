//! Core `Event` → `TermEvent` mapping with per-`advance` coalescing.
//!
//! [`EventSink`] is owned by the core `Term`, which exposes no accessor for it.
//! The terminal therefore shares the buffer through an `Arc<Mutex<...>>`: the
//! sink pushes while the core runs, and the terminal drains it after the parse
//! returns (never while the core is borrowed). A plain `RefCell` would make
//! `Terminal` `!Send` and break `SharedTerminal = Arc<FairMutex<Terminal>>`.

use alacritty_terminal::event::{Event, EventListener};
use std::sync::{Arc, Mutex};

/// Collects core events during a parse; drained after the parse returns.
#[derive(Clone, Default)]
pub(crate) struct EventSink {
    /// Shared with [`crate::Terminal`], which drains it between parses.
    pub(crate) pending: Arc<Mutex<Vec<Event>>>,
}

impl EventSink {
    /// A sink whose buffer is also reachable by the owning terminal.
    pub(crate) fn new(pending: Arc<Mutex<Vec<Event>>>) -> Self {
        Self { pending }
    }
}

impl EventListener for EventSink {
    fn send_event(&self, event: Event) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}
