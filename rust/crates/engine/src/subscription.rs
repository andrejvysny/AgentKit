use crate::{AgentHost, EngineError, EngineResult, EventRecord, TaskRecord, TaskRepository};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct RunHandle<R: TaskRepository> {
    pub(crate) host: AgentHost<R>,
    pub task_id: String,
}

#[derive(Debug)]
pub enum RunUpdate {
    Event(EventRecord),
    Settled(Box<TaskRecord>),
}

impl<R: TaskRepository> RunHandle<R> {
    pub async fn status(&self) -> EngineResult<Option<TaskRecord>, R::Error> {
        self.host
            .store
            .get_task(&self.task_id)
            .await
            .map_err(EngineError::Store)
    }

    pub async fn cancel(&self) -> EngineResult<(), R::Error> {
        self.host.cancel(&self.task_id).await
    }

    pub async fn resume(&self) -> EngineResult<TaskRecord, R::Error> {
        self.host.resume(&self.task_id).await
    }

    pub fn subscribe(
        &self,
        after_seq: Option<u64>,
    ) -> mpsc::Receiver<EngineResult<RunUpdate, R::Error>> {
        self.subscribe_after_number(after_seq.map(|value| value as f64))
    }

    pub fn subscribe_after_number(
        &self,
        after_seq: Option<f64>,
    ) -> mpsc::Receiver<EngineResult<RunUpdate, R::Error>> {
        let (sender, receiver) = mpsc::channel(64);
        let handle = self.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.follow(after_seq, &sender).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        receiver
    }

    async fn follow(
        &self,
        mut cursor: Option<f64>,
        sender: &mpsc::Sender<EngineResult<RunUpdate, R::Error>>,
    ) -> EngineResult<(), R::Error> {
        loop {
            if sender.is_closed() {
                return Ok(());
            }
            let task = self
                .status()
                .await?
                .ok_or_else(|| EngineError::Execution("task not found".into()))?;
            let events = self
                .host
                .store
                .events_after_number(&self.task_id, cursor, 64)
                .await
                .map_err(EngineError::Store)?;
            let empty = events.is_empty();
            for event in events {
                cursor = Some(event.seq);
                if sender.send(Ok(RunUpdate::Event(event))).await.is_err() {
                    return Ok(());
                }
            }
            // Read terminal status before the final event page: terminal state
            // cannot race a late verification event into a completed stream.
            if task.status.is_terminal() && empty {
                let _ = sender.send(Ok(RunUpdate::Settled(Box::new(task)))).await;
                return Ok(());
            }
            if empty {
                tokio::select! {
                    _ = sender.closed() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        }
    }
}
