//! Shared single-thread GTK harness for unit tests.
//!
//! `gtk::init()` pins "the GTK main thread" to whichever thread runs
//! it first, and the libtest harness gives every test its own thread
//! — so two tests that each initialize GTK trip the gtk4-rs
//! cross-thread assertion ("Attempted to initialize GTK from two
//! different threads") depending on scheduling. Every test that
//! touches GTK API routes its body through [`run_gtk`], which
//! executes it on ONE dedicated worker thread where GTK was
//! initialized exactly once; jobs run sequentially, so GTK-touching
//! tests are also naturally serialized (thread-locals like
//! `factory::MENU_CACHE` live on that worker and stay consistent).

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use std::sync::Mutex;

type Job = Box<dyn FnOnce() + Send>;
type JobResult = std::thread::Result<()>;

static GTK_THREAD: OnceLock<Option<Mutex<Sender<(Job, Sender<JobResult>)>>>> = OnceLock::new();

fn gtk_thread() -> Option<&'static Mutex<Sender<(Job, Sender<JobResult>)>>> {
    GTK_THREAD
        .get_or_init(|| {
            let (tx, rx) = channel::<(Job, Sender<JobResult>)>();
            let (ready_tx, ready_rx) = channel();
            std::thread::Builder::new()
                .name("gtk-test".into())
                .spawn(move || {
                    let ok = gtk::init().is_ok();
                    let _ = ready_tx.send(ok);
                    if !ok {
                        return;
                    }
                    while let Ok((job, done)) = rx.recv() {
                        let result = catch_unwind(AssertUnwindSafe(job));
                        let _ = done.send(result);
                    }
                })
                .expect("spawn gtk-test thread");
            match ready_rx.recv() {
                Ok(true) => Some(Mutex::new(tx)),
                _ => None,
            }
        })
        .as_ref()
}

/// Run `f` on the shared GTK thread. Returns `false` (caller should
/// skip, matching the established headless-skip convention) when GTK
/// could not initialize — no display. A panic inside `f` propagates
/// to the calling test thread so the test still fails normally.
pub(crate) fn run_gtk<F: FnOnce() + Send + 'static>(f: F) -> bool {
    let Some(tx) = gtk_thread() else {
        return false;
    };
    let (done_tx, done_rx) = channel();
    tx.lock()
        .unwrap_or_else(|e| e.into_inner())
        .send((Box::new(f), done_tx))
        .expect("gtk-test thread alive");
    match done_rx.recv().expect("gtk-test thread returned a result") {
        Ok(()) => true,
        Err(panic) => resume_unwind(panic),
    }
}
