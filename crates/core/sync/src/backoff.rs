use nanocode_platform::sleep;
use std::{
    future::Future,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    until: Option<Instant>,
    first: Duration,
    delay: Duration,
    most: Duration,
}

impl Backoff {
    #[must_use]
    pub fn within(within: Duration, first: Duration, most: Duration) -> Self {
        Self {
            until: Some(Instant::now() + within),
            ..Self::endless(first, most)
        }
    }

    #[must_use]
    pub const fn endless(first: Duration, most: Duration) -> Self {
        Self {
            until: None,
            first,
            delay: first,
            most,
        }
    }

    #[must_use]
    pub const fn delay(&self) -> Duration {
        self.delay
    }

    pub const fn reset(&mut self) {
        self.delay = self.first;
    }
}

impl Iterator for Backoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        if let Some(until) = self.until
            && Instant::now() >= until.checked_sub(self.delay).unwrap_or(until)
        {
            return None;
        }
        let delay = self.delay;
        self.delay = (delay * 2).min(self.most);
        Some(delay)
    }
}

pub async fn retry<T, E, F>(
    mut attempt: impl FnMut() -> F,
    mut again: impl FnMut(&E) -> Option<Duration>,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    loop {
        match attempt().await {
            Err(error) => match again(&error) {
                Some(delay) => sleep(delay).await,
                None => return Err(error),
            },
            done => return done,
        }
    }
}
