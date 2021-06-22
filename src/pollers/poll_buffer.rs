use crate::{
    pollers, protos::temporal::api::workflowservice::v1::PollActivityTaskQueueResponse,
    protos::temporal::api::workflowservice::v1::PollWorkflowTaskQueueResponse, ServerGatewayApis,
};
use futures::prelude::stream::FuturesUnordered;
use futures::StreamExt;
use std::{fmt::Debug, future::Future, sync::Arc};
use tokio::sync::{
    mpsc::{channel, Receiver},
    watch, Mutex, Semaphore,
};
use tokio::task::JoinHandle;

pub struct LongPollBuffer<T> {
    buffered_polls: Mutex<Receiver<pollers::Result<T>>>,
    shutdown: watch::Sender<bool>,
    /// This semaphore exists to ensure that we only poll server as many times as core actually
    /// *asked* it to be polled - otherwise we might spin and buffer polls constantly. This also
    /// means unit tests can continue to function in a predictable manner when calling mocks.
    polls_requested: Arc<Semaphore>,
    join_handles: FuturesUnordered<JoinHandle<()>>,
}

impl<T> LongPollBuffer<T>
where
    T: Send + Debug + 'static,
{
    pub fn new<FT>(
        poll_fn: impl Fn() -> FT + Send + Sync + 'static,
        concurrent_pollers: usize,
        buffer_size: usize,
    ) -> Self
    where
        FT: Future<Output = pollers::Result<T>> + Send,
    {
        let (tx, rx) = channel(buffer_size);
        let polls_requested = Arc::new(Semaphore::new(0));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let join_handles = FuturesUnordered::new();
        let pf = Arc::new(poll_fn);
        for _ in 0..concurrent_pollers {
            let tx = tx.clone();
            let pf = pf.clone();
            let mut shutdown = shutdown_rx.clone();
            let polls_requested = polls_requested.clone();
            let jh = tokio::spawn(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    let sp = tokio::select! {
                        sp = polls_requested.acquire() => sp.expect("Polls semaphore not dropped"),
                        _ = shutdown.changed() => continue,
                    };
                    let r = tokio::select! {
                        r = pf() => r,
                        _ = shutdown.changed() => continue,
                    };
                    sp.forget();
                    let _ = tx.send(r).await;
                }
            });
            join_handles.push(jh);
        }
        Self {
            buffered_polls: Mutex::new(rx),
            shutdown: shutdown_tx,
            polls_requested,
            join_handles,
        }
    }

    /// Poll the buffer. Adds one permit to the polling pool - the point of this being that the
    /// buffer may support many concurrent pollers, but there is no reason to have them poll unless
    /// enough polls have actually been requested. Calling this function adds a permit that any
    /// concurrent poller may fulfill.
    ///
    /// EX: If this function is only ever called serially and always `await`ed, there will be no
    /// concurrent polling. If it is called many times and the futures are awaited concurrently,
    /// then polling will happen concurrently.
    ///
    /// Returns `None` if the poll buffer has been shut down
    pub async fn poll(&self) -> Option<pollers::Result<T>> {
        self.polls_requested.add_permits(1);
        let mut locked = self.buffered_polls.lock().await;
        (*locked).recv().await
    }

    pub fn notify_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        while self.join_handles.next().await.is_some() {}
    }
}

pub type PollWorkflowTaskBuffer = LongPollBuffer<PollWorkflowTaskQueueResponse>;
pub fn new_workflow_task_buffer(
    sg: Arc<impl ServerGatewayApis + Send + Sync + 'static>,
    task_queue: String,
    concurrent_pollers: usize,
    buffer_size: usize,
) -> PollWorkflowTaskBuffer {
    LongPollBuffer::new(
        move || {
            let sg = sg.clone();
            let task_queue = task_queue.clone();
            async move { sg.poll_workflow_task(task_queue).await }
        },
        concurrent_pollers,
        buffer_size,
    )
}

pub type PollActivityTaskBuffer = LongPollBuffer<PollActivityTaskQueueResponse>;
pub fn new_activity_task_buffer(
    sg: Arc<impl ServerGatewayApis + Send + Sync + 'static>,
    task_queue: String,
    concurrent_pollers: usize,
    buffer_size: usize,
) -> PollActivityTaskBuffer {
    LongPollBuffer::new(
        move || {
            let sg = sg.clone();
            let task_queue = task_queue.clone();
            async move { sg.poll_activity_task(task_queue).await }
        },
        concurrent_pollers,
        buffer_size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pollers::manual_mock::MockManualGateway;
    use futures::FutureExt;
    use std::time::Duration;
    use tokio::{select, sync::mpsc::channel};

    #[tokio::test]
    async fn only_polls_once_with_1_poller() {
        let mut mock_gateway = MockManualGateway::new();
        mock_gateway
            .expect_poll_workflow_task()
            .times(2)
            .returning(move |_| {
                async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(Default::default())
                }
                .boxed()
            });
        let mock_gateway = Arc::new(mock_gateway);

        let pb = new_workflow_task_buffer(mock_gateway, "someq".to_string(), 1, 1);

        // Poll a bunch of times, "interrupting" it each time, we should only actually have polled
        // once since the poll takes a while
        let (interrupter_tx, mut interrupter_rx) = channel(50);
        for _ in 0..10 {
            interrupter_tx.send(()).await.unwrap();
        }

        // We should never get anything out since we interrupted 100% of polls
        let mut last_val = false;
        for _ in 0..10 {
            select! {
                _ = interrupter_rx.recv() => {
                    last_val = true;
                }
                _ = pb.poll() => {
                }
            }
        }
        assert!(last_val);
        // Now we grab the buffered poll response, the poll task will go again but we don't grab it,
        // therefore we will have only polled twice.
        pb.poll().await.unwrap().unwrap();
        pb.shutdown().await;
    }
}