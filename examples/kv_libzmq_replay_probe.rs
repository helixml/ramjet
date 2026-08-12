use std::{env, time::Instant};

use anyhow::{Context, Result};

const END_SEQUENCE: [u8; 8] = [u8::MAX; 8];

fn main() -> Result<()> {
    let endpoint = env::args()
        .nth(1)
        .unwrap_or_else(|| "tcp://127.0.0.1:5558".to_owned());
    let from = env::args()
        .nth(2)
        .map(|value| value.parse())
        .transpose()
        .context("invalid starting sequence")?
        .unwrap_or(0_u64);

    let context = zmq::Context::new();
    let socket = context.socket(zmq::DEALER).context("create socket")?;
    socket.set_linger(0).context("set linger")?;
    socket.set_rcvhwm(100_000).context("set receive HWM")?;
    socket.set_rcvtimeo(60_000).context("set receive timeout")?;
    socket.connect(&endpoint).context("connect")?;
    socket
        .send_multipart([&[][..], &from.to_be_bytes()], 0)
        .context("send replay request")?;

    let started = Instant::now();
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    let mut first = None;
    let mut last = None;
    loop {
        let message = socket.recv_multipart(0).context("receive")?;
        bytes = bytes.saturating_add(message.iter().map(Vec::len).sum::<usize>());
        anyhow::ensure!(message.len() == 4, "unexpected frame count");
        if message[2].as_slice() == END_SEQUENCE {
            break;
        }
        let sequence = u64::from_be_bytes(
            message[2]
                .as_slice()
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
