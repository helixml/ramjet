use std::{env, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::time::Instant;
use zeromq::{DealerSocket, Socket, SocketRecv, SocketSend, ZmqMessage};

const END_SEQUENCE: [u8; 8] = [u8::MAX; 8];

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint = env::args()
        .nth(1)
        .unwrap_or_else(|| "tcp://127.0.0.1:5558".to_owned());
    let from = env::args()
        .nth(2)
        .map(|value| value.parse())
        .transpose()
        .context("invalid starting sequence")?
        .unwrap_or(0_u64);
    let mut socket = DealerSocket::new();
    socket.connect(&endpoint).await.context("connect")?;
    socket
        .send(
            ZmqMessage::try_from(vec![
                Bytes::new(),
                Bytes::copy_from_slice(&from.to_be_bytes()),
            ])
            .map_err(|_| anyhow::anyhow!("request framing"))?,
        )
        .await
        .context("send")?;

    let started = Instant::now();
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    let mut first = None;
    let mut last = None;
    loop {
        let message = tokio::time::timeout(Duration::from_mins(1), socket.recv())
            .await
            .context("receive timeout")?
            .context("receive")?;
        bytes = bytes.saturating_add(message.iter().map(Bytes::len).sum::<usize>());
        anyhow::ensure!(message.len() == 4, "unexpected frame count");
        let sequence = message.get(2).context("missing sequence")?;
        if sequence.as_ref() == END_SEQUENCE {
            break;
        }
        let sequence = u64::from_be_bytes(
            sequence
                .as_ref()
                .try_into()
                .context("invalid sequence length")?,
        );
        first.get_or_insert(sequence);
        last = Some(sequence);
        count = count.saturating_add(1);
        if count.is_multiple_of(128) {
            eprintln!(
                "replay_progress count={count} last={sequence} bytes={bytes} wall_ms={}",
                started.elapsed().as_millis()
            );
        }
    }
    println!(
        "replay_ok count={count} first={} last={} bytes={bytes} wall_ms={}",
        first.map_or_else(|| "none".to_owned(), |value| value.to_string()),
        last.map_or_else(|| "none".to_owned(), |value| value.to_string()),
        started.elapsed().as_millis()
    );
    Ok(())
}
