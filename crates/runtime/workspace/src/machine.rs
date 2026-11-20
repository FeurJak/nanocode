use std::{future::Future, pin::Pin, time::Duration};

use nanocode_shell::Machine;

pub struct Platform;

impl Machine for Platform {
    fn now(&self) -> Duration {
        nanocode_platform::system_time()
            .duration_since(nanocode_platform::UNIX_EPOCH)
            .unwrap_or_default()
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(nanocode_platform::sleep(duration))
    }

    fn random(&self, bytes: &mut [u8]) {
        nanocode_platform::fill_random(bytes).expect("the platform supplies random bytes");
    }
}
