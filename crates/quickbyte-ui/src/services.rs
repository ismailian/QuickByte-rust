//! The service handles every window talks to — the constructor parameters of the
//! C# `MainForm`, gathered in one cloneable struct. The composition root
//! (`main.rs`) is the only place that knows which implementations are behind
//! these traits: the real Core services, or the in-memory demo ones.

use std::future::Future;
use std::sync::Arc;

use quickbyte_core::traits::{
    BrowserIntegrationService, DownloadManager, QueueManager, RemoteFileInfoProvider,
    SettingsService, StreamResolver, UpdateService,
};

#[derive(Clone)]
pub struct AppServices {
    pub downloads: Arc<dyn DownloadManager>,
    pub queues: Arc<dyn QueueManager>,
    pub settings: Arc<dyn SettingsService>,
    pub file_info: Arc<dyn RemoteFileInfoProvider>,
    pub streams: Arc<dyn StreamResolver>,
    pub updates: Arc<dyn UpdateService>,
    pub browser: Arc<dyn BrowserIntegrationService>,
    /// The tokio runtime. The UI thread never blocks on it: it spawns, and the
    /// results come back as events or through a channel a window polls per frame.
    pub runtime: tokio::runtime::Handle,
}

impl AppServices {
    /// Runs `future` on the runtime and forgets it — for commands whose outcome
    /// arrives as a download event anyway (resume, retry, add).
    pub fn spawn<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        drop(self.runtime.spawn(future));
    }

    /// Runs `future` on the runtime; its result can be polled each frame with
    /// [`Pending::poll`], so a window can wait for it without blocking.
    pub fn run<T, F>(&self, future: F) -> Pending<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(self.runtime.spawn(async move {
            let _ = tx.send(future.await);
        }));
        Pending { rx, value: None }
    }
}

impl AppServices {
    /// [`run`](Self::run), and the window behind `wake` draws again once the
    /// answer is in — so a dialog waiting on a probe needs no timer.
    pub fn run_waking<T, F>(&self, wake: crate::shell::Waker, future: F) -> Pending<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(self.runtime.spawn(async move {
            let _ = tx.send(future.await);
            wake.wake();
        }));
        Pending { rx, value: None }
    }
}

/// The answer to some async work, collected on the UI thread.
pub struct Pending<T> {
    rx: crossbeam_channel::Receiver<T>,
    value: Option<T>,
}

impl<T> Pending<T> {
    /// The result once it has arrived (taken out once).
    pub fn poll(&mut self) -> Option<T> {
        if self.value.is_none() {
            self.value = self.rx.try_recv().ok();
        }
        self.value.take()
    }

    pub fn is_done(&self) -> bool {
        self.value.is_some() || !self.rx.is_empty()
    }
}
