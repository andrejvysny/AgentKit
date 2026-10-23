use crate::json_boundary;
use futures::StreamExt;
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{Transport, async_rw::AsyncRwTransport},
};
use std::{io, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncWriteExt, DuplexStream},
    process::{Child, ChildStdin, Command},
};
use tokio_util::codec::{FramedRead, LinesCodec};

pub(crate) struct JsonChild {
    child: Child,
    reader: tokio::task::JoinHandle<()>,
    transport: AsyncRwTransport<RoleClient, DuplexStream, ChildStdin>,
}
impl JsonChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Missing child stdout"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Missing child stdin"))?;
        let (mut producer, consumer) = tokio::io::duplex(65_536);
        let reader = tokio::spawn(async move {
            let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(4_194_304));
            while let Some(Ok(line)) = lines.next().await {
                let Ok(protected) = json_boundary::sdk_text(&line) else {
                    break;
                };
                if producer.write_all(protected.as_bytes()).await.is_err()
                    || producer.write_all(b"\n").await.is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            reader,
            transport: AsyncRwTransport::new_client(consumer, stdin),
        })
    }
}
impl Transport<RoleClient> for JsonChild {
    type Error = io::Error;
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        self.transport.send(item)
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.transport.receive().await
    }
    async fn close(&mut self) -> io::Result<()> {
        self.transport.close().await?;
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            self.child.kill().await?;
        }
        self.reader.abort();
        Ok(())
    }
}
impl Drop for JsonChild {
    fn drop(&mut self) {
        self.reader.abort();
        let _ = self.child.start_kill();
    }
}
