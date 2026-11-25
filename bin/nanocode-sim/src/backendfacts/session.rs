use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use nanocode::claude::session::Session;

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const SCOPE: &str = "backend-facts-session";
const BUMPS_MAX: u64 = 4;
const STALL_POLLS_MAX: usize = 64;
const DRIVE_POLLS_MAX: usize = 4096;

async fn yielded() {
    let mut yielded = false;
    std::future::poll_fn(|_| {
        if std::mem::replace(&mut yielded, true) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

async fn bump(session: &Session) -> Result<(), String> {
    let seen = session.revision();
    session
        .persist(|| Box::pin(async {}))
        .await
        .map_err(|error| format!("a persistence command failed: {error}"))?;
    if session.revision() != seen + 1 {
        return Err(format!(
            "a persistence command moved the revision from {seen} to {}",
            session.revision()
        ));
    }
    Ok(())
}

async fn landed(session: &Session, seen: u64) -> Result<bool, String> {
    session
        .persist_at(seen, || Box::pin(async {}))
        .await
        .map(|landed| landed.is_some())
        .map_err(|error| format!("a persistence command at revision {seen} failed: {error}"))
}

async fn lanes(session: &Arc<Session>) -> Result<(), String> {
    let persisting = Arc::clone(session);
    let nested = session
        .admit(move || {
            Box::pin(async move { persisting.persist(|| Box::pin(async { 1_u64 })).await })
        })
        .await
        .map_err(|error| format!("an admission failed: {error}"))?
        .map_err(|error| format!("a persistence command inside an admission failed: {error}"))?;
    if nested != 1 {
        return Err(format!("a nested persistence command answered {nested}"));
    }
    Ok(())
}

async fn abandon(session: &Session, seen: u64) -> Result<(), String> {
    let started = Arc::new(AtomicBool::new(false));
    let starting = Arc::clone(&started);
    let mut stalled = pin!(session.persist_at(seen, move || {
        Box::pin(async move {
            starting.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        })
    }));
    for _ in 0..STALL_POLLS_MAX {
        let polled =
            std::future::poll_fn(|context| Poll::Ready(stalled.as_mut().poll(context))).await;
        if polled.is_ready() {
            return Err("a persistence command whose write stalls returned".into());
        }
        if started.load(Ordering::SeqCst) {
            return Ok(());
        }
        yielded().await;
    }
    Err(format!(
        "the stalled persistence command did not start in {STALL_POLLS_MAX} polls"
    ))
}

async fn check(bumps: u64) -> Result<u64, String> {
    let session = Arc::new(Session::spawn().map_err(|error| error.to_string())?);
    lanes(&session).await?;
    for _ in 0..bumps {
        bump(&session).await?;
    }
    let current = session.revision();
    if landed(&session, current - 1).await? {
        return Err(format!(
            "a persistence command read at revision {} landed at revision {current}",
            current - 1
        ));
    }
    if !landed(&session, current).await? {
        return Err(format!(
            "a persistence command at the current revision {current} was refused"
        ));
    }
    let seen = session.revision();
    abandon(&session, seen).await?;
    yielded().await;
    if landed(&session, seen).await? {
        return Err(format!(
            "a persistence command read at revision {seen} landed after an abandoned write at that revision"
        ));
    }
    let after = session.revision();
    if !landed(&session, after).await? {
        return Err(format!(
            "the session refused a write at revision {after} after an abandoned one"
        ));
    }
    Ok(fnv(FNV_OFFSET, &session.revision().to_le_bytes()))
}

fn drive(check: impl Future<Output = Result<u64, String>>) -> Result<u64, String> {
    let mut check = pin!(check);
    let mut context = Context::from_waker(Waker::noop());
    let mut spawned = Vec::new();
    for _ in 0..DRIVE_POLLS_MAX {
        if let Poll::Ready(checked) = check.as_mut().poll(&mut context) {
            return checked;
        }
        spawned.extend(crate::host::spawned().into_iter().map(|(owner, _)| owner));
        spawned.retain_mut(|owner| owner.as_mut().poll(&mut context).is_pending());
    }
    Err(format!(
        "the session owner check did not finish in {DRIVE_POLLS_MAX} polls"
    ))
}

pub(super) fn run(seed: u64) -> Result<u64, String> {
    let bumps = Rng::derive(seed, SCOPE).range(1, BUMPS_MAX);
    crate::host::entered(seed, &Clock::new(), || drive(check(bumps)))
}
