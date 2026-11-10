//! Port of `src/QuickByte.Core/Helpers/RetryPolicy.cs`.
//!
//! Strategy-pattern retry executor with exponential backoff. Used by connections
//! to transparently recover from transient network failures.
//!
//! Several failures are deliberately *not* transient and end the loop
//! immediately: cancellation, [`CoreError::AuthenticationRequired`],
//! [`CoreError::RemoteFileChanged`] and the rest of the kinds
//! [`CoreError::is_transient`] refuses. A wrong password will be just as wrong on
//! the fourth attempt, and retrying it costs the user the real error message —
//! buried under a backoff delay — while some servers count the repeats towards a
//! lockout. A file that changed on the server is no more fixable by this
//! connection asking again: the chunks of every sibling belong to the old
//! version too, and the decision belongs to the download as a whole.
//!
//! A full disk ends it too, however it is reported. Space does not come back in
//! the seconds a backoff waits, and every retry pulls bytes off the network only
//! to fail writing them.

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};

/// Decides whether a failure is worth another attempt.
pub type RetryClassifier = fn(&CoreError) -> bool;

/// Whether asking again could plausibly succeed. Also what the connection pool
/// asks before giving a failed range a replacement connection: a fresh socket
/// does not fix a wrong password either.
pub fn is_transient(error: &CoreError) -> bool {
    error.is_transient()
}

/// The retry strategy: how many retries, the base of the exponential backoff,
/// and which failures count as transient.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_retries: i32,
    pub base_delay: Duration,
    classifier: RetryClassifier,
}

impl RetryPolicy {
    pub fn new(max_retries: i32, base_delay: Duration) -> Self {
        Self { max_retries, base_delay, classifier: is_transient }
    }

    /// The policy for a download's connections, from its settings.
    pub fn from_settings(settings: &crate::models::DownloadSettings) -> Self {
        Self::new(settings.max_retries, Duration::from_millis(settings.retry_delay_milliseconds.max(0) as u64))
    }

    /// Swaps the transient-failure classifier (the Strategy's pluggable part).
    pub fn with_classifier(mut self, classifier: RetryClassifier) -> Self {
        self.classifier = classifier;
        self
    }

    /// Runs `action` (handed the attempt number, starting at 0) until it
    /// succeeds, fails with something not transient, or the retries run out.
    /// `on_retry` is told each retry's number (from 1) and its cause.
    pub async fn execute<T, F, Fut>(
        &self,
        mut action: F,
        mut on_retry: impl FnMut(i32, &CoreError),
        cancel: &CancellationToken,
    ) -> Result<T>
    where
        F: FnMut(i32) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut attempt = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            let error = match action(attempt).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if attempt >= self.max_retries || !(self.classifier)(&error) {
                return Err(error);
            }
            attempt += 1;
            on_retry(attempt, &error);
            let delay = self.base_delay.mul_f64(2f64.powi(attempt - 1));
            tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::*;
    use crate::helpers::disk_space::SpaceShortfall;

    fn policy(max_retries: i32) -> RetryPolicy {
        RetryPolicy::new(max_retries, Duration::ZERO)
    }

    fn none() -> CancellationToken {
        CancellationToken::new()
    }

    async fn calls_until_given_up(error: CoreError) -> (i32, CoreError) {
        let calls = AtomicI32::new(0);
        let result: Result<i32> = policy(5)
            .execute(
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let e = error.clone();
                    async move { Err(e) }
                },
                |_, _| {},
                &none(),
            )
            .await;
        (calls.load(Ordering::SeqCst), result.unwrap_err())
    }

    #[tokio::test]
    async fn returns_the_first_success_without_retrying() {
        let calls = AtomicI32::new(0);
        let result = policy(3)
            .execute(
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok(7) }
                },
                |_, _| {},
                &none(),
            )
            .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_a_transient_failure_until_it_succeeds() {
        let calls = AtomicI32::new(0);
        let result = policy(5)
            .execute(
                |_| {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    async move { if n < 3 { Err(CoreError::Io("the connection was reset".into())) } else { Ok(42) } }
                },
                |_, _| {},
                &none(),
            )
            .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_configured_number_of_retries() {
        let calls = AtomicI32::new(0);
        let result: Result<i32> = policy(3)
            .execute(
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err(CoreError::Io("still broken".into())) }
                },
                |_, _| {},
                &none(),
            )
            .await;
        assert!(matches!(result, Err(CoreError::Io(_))));
        // The first attempt is not a retry: 1 + 3.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn does_not_retry_a_rejected_password() {
        let (calls, error) = calls_until_given_up(CoreError::auth("nope", true)).await;
        assert_eq!(calls, 1);
        assert!(matches!(error, CoreError::AuthenticationRequired { credentials_were_supplied: true, .. }));
    }

    #[tokio::test]
    async fn does_not_retry_a_full_disk() {
        // Space does not come back in the seconds a backoff waits.
        for os_error in [112, 39] {
            let error = CoreError::from(std::io::Error::from_raw_os_error(os_error));
            let (calls, _) = calls_until_given_up(error).await;
            assert_eq!(calls, 1);
        }
        let shortfall = SpaceShortfall { volume: "C:\\".into(), needed: 10, available: 1 };
        assert_eq!(calls_until_given_up(CoreError::InsufficientDiskSpace(shortfall)).await.0, 1);
    }

    #[tokio::test]
    async fn does_not_retry_a_changed_remote_file() {
        assert_eq!(calls_until_given_up(CoreError::RemoteFileChanged("changed".into())).await.0, 1);
    }

    #[tokio::test]
    async fn does_not_retry_a_stream_that_cannot_be_downloaded() {
        assert_eq!(calls_until_given_up(CoreError::StreamNotSupported("live".into())).await.0, 1);
    }

    #[tokio::test]
    async fn does_not_retry_a_proxy_that_refused_its_login() {
        let (calls, error) = calls_until_given_up(CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_REJECTED.into())).await;
        assert_eq!(calls, 1);
        assert_eq!(error.to_string(), CoreError::PROXY_AUTH_REJECTED);
    }

    #[tokio::test]
    async fn does_not_retry_a_cancellation() {
        assert_eq!(calls_until_given_up(CoreError::Cancelled).await.0, 1);
    }

    #[tokio::test]
    async fn stops_before_the_first_attempt_when_already_cancelled() {
        let calls = AtomicI32::new(0);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = policy(3)
            .execute(
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Ok(1) }
                },
                |_, _| {},
                &cancel,
            )
            .await;
        assert!(matches!(result, Err(CoreError::Cancelled)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn reports_each_retry_with_its_attempt_number_and_cause() {
        let mut reported = Vec::new();
        let result: Result<i32> = policy(2)
            .execute(|_| async { Err(CoreError::Io("boom".into())) }, |attempt, e| reported.push((attempt, e.to_string())), &none())
            .await;
        assert!(result.is_err());
        assert_eq!(reported, vec![(1, "boom".to_string()), (2, "boom".to_string())]);
    }

    #[tokio::test]
    async fn hands_the_action_the_attempt_number() {
        let seen = parking_lot::Mutex::new(Vec::new());
        policy(5)
            .execute(
                |attempt| {
                    seen.lock().push(attempt);
                    async move { if attempt < 2 { Err(CoreError::Io("again".into())) } else { Ok(0) } }
                },
                |_, _| {},
                &none(),
            )
            .await
            .unwrap();
        assert_eq!(*seen.lock(), vec![0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn backs_off_exponentially() {
        let started = tokio::time::Instant::now();
        let _: Result<i32> = RetryPolicy::new(3, Duration::from_millis(100))
            .execute(|_| async { Err(CoreError::Timeout) }, |_, _| {}, &none())
            .await;
        // 100 + 200 + 400.
        assert_eq!(started.elapsed(), Duration::from_millis(700));
    }

    #[tokio::test]
    async fn a_custom_classifier_decides_what_is_retried() {
        let calls = AtomicI32::new(0);
        let _: Result<i32> = policy(5)
            .with_classifier(|_| false)
            .execute(
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err(CoreError::Timeout) }
                },
                |_, _| {},
                &none(),
            )
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
