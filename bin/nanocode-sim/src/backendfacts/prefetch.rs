use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use nanocode::claude::durability::Prefetches;

use crate::rng::{FNV_OFFSET, Rng, fnv};

const SCOPE: &str = "backend-facts-prefetch";
const OPERATIONS: [&str; 3] = ["operation-0", "operation-1", "operation-2"];

fn polled(future: impl Future<Output = ()>) -> bool {
    let mut future = pin!(future);
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_ready()
}

fn index(rng: &mut Rng) -> usize {
    usize::try_from(rng.below(OPERATIONS.len() as u64)).expect("an operation index fits usize")
}

pub(super) fn run(seed: u64) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, SCOPE);
    let claimed = OPERATIONS[index(&mut rng)];
    let other = OPERATIONS
        .iter()
        .copied()
        .filter(|id| *id != claimed)
        .nth(usize::from(rng.chance(500_000)))
        .expect("another operation exists");
    let prefetches = Prefetches::default();
    if !polled(prefetches.settled(claimed)) {
        return Err(format!(
            "a load of {claimed} waited with no prefetch in flight"
        ));
    }
    let claim = prefetches.claim(claimed);
    if !polled(prefetches.settled(other)) {
        return Err(format!(
            "a load of {other} waited on the prefetch of {claimed}"
        ));
    }
    let mut same = pin!(prefetches.settled(claimed));
    let mut context = Context::from_waker(Waker::noop());
    if same.as_mut().poll(&mut context).is_ready() {
        return Err(format!(
            "a load of {claimed} did not wait for its own prefetch"
        ));
    }
    drop(claim);
    if same.as_mut().poll(&mut context) == Poll::Pending {
        return Err(format!(
            "a load of {claimed} still waited after its prefetch ended"
        ));
    }
    if !polled(prefetches.settled(claimed)) {
        return Err(format!(
            "an ended prefetch of {claimed} still holds its claim"
        ));
    }
    Ok(fnv(fnv(FNV_OFFSET, claimed.as_bytes()), other.as_bytes()))
}
