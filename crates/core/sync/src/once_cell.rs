use crate::{
    Signal,
    sim::{Mutation, mutated},
};
use nanocode_sys::SwapOption;
use std::{
    fmt,
    future::Future,
    sync::{Arc, OnceLock},
};

pub struct OnceCell<T> {
    value: OnceLock<T>,
    filling: SwapOption<Signal>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetError<T> {
    AlreadyInitializedError(T),
    InitializingError(T),
}

impl<T> fmt::Display for SetError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitializedError(_) => formatter.write_str("AlreadyInitializedError"),
            Self::InitializingError(_) => formatter.write_str("InitializingError"),
        }
    }
}

impl<T: fmt::Debug> std::error::Error for SetError<T> {}

impl<T> SetError<T> {
    pub const fn is_already_init_err(&self) -> bool {
        matches!(self, Self::AlreadyInitializedError(_))
    }

    pub const fn is_initializing_err(&self) -> bool {
        matches!(self, Self::InitializingError(_))
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceCell<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OnceCell")
            .field("value", &self.get())
            .finish()
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        Self::new_with(self.get().cloned())
    }
}

impl<T: PartialEq> PartialEq for OnceCell<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq> Eq for OnceCell<T> {}

impl<T> From<T> for OnceCell<T> {
    fn from(value: T) -> Self {
        Self::new_with(Some(value))
    }
}

struct Filling<'cell, T> {
    cell: &'cell OnceCell<T>,
    done: Arc<Signal>,
}

impl<T> Drop for Filling<'_, T> {
    fn drop(&mut self) {
        self.cell.filling.rcu(|filling| match filling {
            Some(current) if Arc::ptr_eq(current, &self.done) => None,
            other => other.clone(),
        });
        if !mutated(Mutation::FillKeepsAbandoned) {
            self.done.fire();
        }
    }
}

enum Claim<'cell, T> {
    Leads(Filling<'cell, T>),
    Waits(Arc<Signal>),
}

impl<T> OnceCell<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: OnceLock::new(),
            filling: SwapOption::empty(),
        }
    }

    #[must_use]
    pub fn new_with(value: Option<T>) -> Self {
        let cell = Self::new();
        if let Some(value) = value {
            assert!(cell.value.set(value).is_ok(), "a new cell is empty");
        }
        cell
    }

    pub fn initialized(&self) -> bool {
        self.value.get().is_some()
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.value.get_mut()
    }

    fn claim(&self) -> Claim<'_, T> {
        let done = Arc::new(Signal::new());
        let running = self
            .filling
            .rcu(|filling| filling.clone().or_else(|| Some(Arc::clone(&done))));
        match running {
            Some(running) => Claim::Waits(running),
            None => Claim::Leads(Filling { cell: self, done }),
        }
    }

    fn fill(&self, value: T, filling: Filling<'_, T>) -> &T {
        assert!(
            self.value.set(value).is_ok(),
            "only the caller leading the fill fills the cell"
        );
        drop(filling);
        self.value.get().expect("the cell was just filled")
    }

    pub fn set(&self, value: T) -> Result<(), SetError<T>> {
        if self.initialized() {
            return Err(SetError::AlreadyInitializedError(value));
        }
        let Claim::Leads(filling) = self.claim() else {
            return Err(SetError::InitializingError(value));
        };
        if self.initialized() {
            return Err(SetError::AlreadyInitializedError(value));
        }
        self.fill(value, filling);
        Ok(())
    }

    pub async fn get_or_init<F, Fut>(&self, init: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let filled = self
            .get_or_try_init(move || async move { Ok::<T, std::convert::Infallible>(init().await) })
            .await;
        match filled {
            Ok(value) => value,
        }
    }

    pub async fn get_or_try_init<E, F, Fut>(&self, init: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        loop {
            if let Some(value) = self.get() {
                return Ok(value);
            }
            match self.claim() {
                Claim::Waits(running) => running.wait().await,
                Claim::Leads(filling) => {
                    if let Some(value) = self.get() {
                        return Ok(value);
                    }
                    let value = init().await?;
                    return Ok(self.fill(value, filling));
                }
            }
        }
    }

    pub fn into_inner(self) -> Option<T> {
        self.value.into_inner()
    }

    pub fn take(&mut self) -> Option<T> {
        std::mem::take(self).into_inner()
    }
}
