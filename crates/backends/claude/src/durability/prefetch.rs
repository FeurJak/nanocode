use std::sync::Arc;

use nanocode_sync::{Atom, Signal};

#[derive(Clone)]
struct Prefetching {
    id: String,
    done: Arc<Signal>,
}

#[doc(hidden)]
#[derive(Default)]
pub struct Prefetches(Atom<Option<Prefetching>>);

#[doc(hidden)]
pub struct Prefetch<'a> {
    prefetches: &'a Prefetches,
    done: Arc<Signal>,
}

impl Drop for Prefetch<'_> {
    fn drop(&mut self) {
        let done = &self.done;
        self.prefetches.0.update(|prefetching| {
            if prefetching
                .as_ref()
                .is_some_and(|prefetching| Arc::ptr_eq(&prefetching.done, done))
            {
                *prefetching = None;
            }
        });
        self.done.fire();
    }
}

impl Prefetches {
    pub fn claim(&self, id: &str) -> Prefetch<'_> {
        let done = Arc::new(Signal::new());
        self.0.set(Some(Prefetching {
            id: id.to_owned(),
            done: Arc::clone(&done),
        }));
        Prefetch {
            prefetches: self,
            done,
        }
    }

    pub async fn settled(&self, id: &str) {
        let any = crate::sim::mutated(crate::sim::Mutation::DemandWaitsAnyPrefetch);
        let pending = self.0.read(|prefetching| {
            prefetching
                .as_ref()
                .filter(|prefetching| any || prefetching.id == id)
                .map(|prefetching| Arc::clone(&prefetching.done))
        });
        if let Some(done) = pending {
            done.wait().await;
        }
    }
}
