use std::sync::mpsc;

use nanocode_executor::{Builder, JoinError, sim, spawn_blocking};

use crate::rng::Rng;

const REFUSED_MIN: u64 = 1;
const REFUSED_MAX: u64 = 4;
const QUEUED_MAX: u64 = 4;
const THREADS_MIN: u64 = 1;
const THREADS_MAX: u64 = 3;

#[derive(Clone, Copy, Debug)]
pub struct Refusals {
    pub refused: u64,
    pub queued: u64,
    pub threads: u64,
}

pub fn refusals_of(rng: &mut Rng) -> Refusals {
    Refusals {
        refused: rng.range(REFUSED_MIN, REFUSED_MAX),
        queued: rng.below(QUEUED_MAX + 1),
        threads: rng.range(THREADS_MIN, THREADS_MAX),
    }
}

impl Refusals {
    pub fn describe(self) -> String {
        format!(
            "refusals=r{}q{}t{}",
            self.refused, self.queued, self.threads
        )
    }
}

fn refused(joined: Result<u64, JoinError>) -> Result<(), String> {
    match joined {
        Err(error) if error.is_refused() => {
            let error = std::io::Error::from(error);
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(())
            } else {
                Err(format!(
                    "a refused blocking task names {:?}, not the thread spawn's WouldBlock",
                    error.kind()
                ))
            }
        }
        other => Err(format!(
            "a blocking task with no thread to run it joined {other:?}, not a refusal"
        )),
    }
}

async fn empty(plan: Refusals) -> Result<(), String> {
    sim::refuse_threads(plan.refused);
    for index in 0..plan.refused {
        let handle = spawn_blocking(move || index);
        if !handle.is_finished() {
            return Err(format!(
                "blocking task {index} is pending with no blocking thread after its thread spawn failed: it waits forever"
            ));
        }
        refused(handle.await)?;
    }
    Ok(())
}

async fn served(plan: Refusals) -> Result<(), String> {
    sim::refuse_threads(0);
    let (release, released) = mpsc::channel::<()>();
    let blocker = spawn_blocking(move || u64::from(released.recv().is_ok()));
    sim::refuse_threads(plan.queued);
    let handles: Vec<_> = (0..plan.queued)
        .map(|index| spawn_blocking(move || index))
        .collect();
    sim::refuse_threads(0);
    release
        .send(())
        .map_err(|_| "the blocker went away before its release".to_owned())?;
    match blocker.await {
        Ok(1) => {}
        other => return Err(format!("the blocker joined {other:?}")),
    }
    for (index, handle) in (0..plan.queued).zip(handles) {
        match handle.await {
            Ok(value) if value == index => {}
            other => {
                return Err(format!(
                    "blocking task {index}, queued behind a live thread while thread spawns failed, joined {other:?}"
                ));
            }
        }
    }
    Ok(())
}

pub fn refusals(plan: Refusals) -> Result<(), String> {
    let runtime = Builder::new_current_thread()
        .max_blocking_threads(usize::try_from(plan.threads).expect("a thread count fits"))
        .build()
        .expect("a current-thread runtime builds");
    let checked = runtime.block_on(async { empty(plan).await.and(served(plan).await) });
    sim::refuse_threads(0);
    drop(runtime);
    checked
}
