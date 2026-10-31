use std::{future::Future, sync::Mutex};

static STARTED: Mutex<Vec<tokio::task::JoinHandle<()>>> = Mutex::new(Vec::new());

/// A refresh runs to completion even if the caller stops waiting: an abandoned one could lose a rotated token.
pub(crate) async fn run<T: Send + 'static, E: Send + 'static>(
    refresh: impl Future<Output = Result<T, E>> + Send + 'static,
) -> Result<T, E> {
    let (send, receive) = tokio::sync::oneshot::channel();
    {
        let mut started = STARTED.lock().expect(crate::NOT_POISONED);
        started.retain(|task| !task.is_finished());
        started.push(tokio::spawn(async move {
            if let Err(Err(_)) = send.send(refresh.await) {
                crate::terminal_text::stderr_line(format_args!(
                    "bart: a credential refresh failed; sign in again if the next request fails"
                ));
            }
        }));
    }
    receive
        .await
        .expect("refresh task completes without panicking")
}

pub(crate) async fn drain() {
    loop {
        let tasks = std::mem::take(&mut *STARTED.lock().expect(crate::NOT_POISONED));
        if tasks.is_empty() {
            return;
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
pub(crate) mod mock;
