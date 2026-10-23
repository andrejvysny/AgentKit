use crate::*;
use agentkit_store::EventRecord;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct RunHandle {
    pub(crate) host: AgentHost,
    pub task_id: String,
}

#[derive(Debug)]
pub enum RunUpdate {
    Event(EventRecord),
    Settled(Box<TaskRecord>),
}

impl RunHandle {
    pub async fn status(&self) -> HostResult<Option<TaskRecord>> {
        Ok(self.host.store.get_task(&self.task_id).await?)
    }

    pub async fn cancel(&self) -> HostResult<()> {
        self.host.cancel(&self.task_id).await
    }

    pub async fn resume(&self) -> HostResult<TaskRecord> {
        self.host.resume(&self.task_id).await
    }

    pub fn subscribe(&self, after_seq: Option<u64>) -> mpsc::Receiver<HostResult<RunUpdate>> {
        self.subscribe_after_number(after_seq.map(|value| value as f64))
    }

    pub fn subscribe_after_number(
        &self,
        after_seq: Option<f64>,
    ) -> mpsc::Receiver<HostResult<RunUpdate>> {
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
        sender: &mpsc::Sender<HostResult<RunUpdate>>,
    ) -> HostResult<()> {
        loop {
            if sender.is_closed() {
                return Ok(());
            }
            let task = self
                .status()
                .await?
                .ok_or_else(|| HostError::Execution("task not found".into()))?;
            let events = self
                .host
                .store
                .events_after_number(&self.task_id, cursor, 64)
                .await?;
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
