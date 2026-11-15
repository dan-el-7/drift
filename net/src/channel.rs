//! Framed endpoint channel that can move to a new transport mid-connection.
//!
//! One UKEY2 context (and one sequence counter per direction) spans every
//! channel a connection uses, so all writes go through a single writer task and
//! all reads through a single reader task. Bandwidth upgrades swap the
//! underlying stream inside those tasks, following Nearby's protocol:
//! writes pause after our LAST_WRITE and resume on the peer's SAFE_TO_CLOSE;
//! reads switch once the peer's LAST_WRITE and SAFE_TO_CLOSE have both arrived
//! on the old channel.

use std::time::Duration;

use drift_core::framing;
use drift_core::payload::{self, BwuEvent};
use drift_core::proto::connections as nc;
use drift_core::secure::{Opener, Sealer};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    r.read_exact(&mut h).await?;
    let len = framing::decode_len(h).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Big socket buffers for transfer links: the kernel keeps data moving while either side is busy
/// encrypting or decrypting a chunk (the raw TCP reference used 8 MiB; defaults left ~25% on the table).
pub fn tune(s: &tokio::net::TcpStream) {
    let r = socket2::SockRef::from(s);
    let _ = r.set_send_buffer_size(8 << 20);
    let _ = r.set_recv_buffer_size(8 << 20);
}

pub async fn write_frame<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, b: &[u8]) -> std::io::Result<()> {
    w.write_all(&framing::encode(b)).await
}

enum WriteCmd {
    Frame(nc::OfflineFrame),
    /// Bulk data; the permit is released once the frame is on the wire (flow control).
    Data(nc::OfflineFrame, tokio::sync::OwnedSemaphorePermit),
    Old(nc::OfflineFrame),
    Pause(BoxWrite, bool),
    Resume,
}

/// Cheap handle for queueing frames to the peer. Control frames have their own
/// lane that the writer always serves first: on a slow link (Bluetooth) a FIFO
/// shared with bulk data would hold e.g. an upgrade offer behind seconds of
/// file chunks, and the phone gives up on the upgrade before it arrives.
#[derive(Clone)]
pub struct Writer(mpsc::UnboundedSender<WriteCmd>, mpsc::UnboundedSender<WriteCmd>);

impl Writer {
    /// Normal frame; queued while an upgrade has paused writes.
    pub fn send(&self, f: nc::OfflineFrame) {
        let _ = self.0.send(WriteCmd::Frame(f));
    }
    /// Bulk data with back-pressure: waits for one of `inflight`'s permits, which
    /// the writer releases after writing. Bounds memory when streaming big files.
    pub async fn send_data(&self, f: nc::OfflineFrame, inflight: &std::sync::Arc<tokio::sync::Semaphore>) -> bool {
        let t = std::time::Instant::now();
        match inflight.clone().acquire_owned().await {
            Ok(permit) => {
                crate::stats::PERMIT_WAIT.add(t);
                self.1.send(WriteCmd::Data(f, permit)).is_ok()
            }
            Err(_) => false,
        }
    }
    /// Must go on the prior channel even while paused (LAST_WRITE / SAFE_TO_CLOSE).
    pub fn send_on_prior(&self, f: nc::OfflineFrame) {
        let _ = self.0.send(WriteCmd::Old(f));
    }
    /// Start queueing normal frames; `next` (encrypted or plaintext) takes over on resume.
    pub fn pause_for(&self, next: BoxWrite, encrypted: bool) {
        let _ = self.0.send(WriteCmd::Pause(next, encrypted));
    }
    pub fn resume(&self) {
        let _ = self.0.send(WriteCmd::Resume);
    }
}

/// Socket writes for one connection, in their own task: the writer seals the next frame while this
/// one is on the wire (sealing then writing one by one capped the phone at ~70% of the link: 3.7 ms to
/// seal 512 KiB at 143 MB/s, 10 ms to send it at 51 MB/s). A frame's permit is released once written.
struct Sink {
    tx: mpsc::Sender<(Vec<u8>, Option<tokio::sync::OwnedSemaphorePermit>)>,
    /// Gives the stream back when the queue closes (to say goodbye / shut it down).
    task: JoinHandle<BoxWrite>,
}

fn spawn_sink(mut w: BoxWrite) -> Sink {
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, Option<tokio::sync::OwnedSemaphorePermit>)>(4);
    let task = tokio::spawn(async move {
        while let Some((bytes, _permit)) = rx.recv().await {
            let t = std::time::Instant::now();
            let r = write_frame(&mut w, &bytes).await;
            crate::stats::SOCKET_WRITE.add(t);
            if let Err(e) = r {
                tracing::debug!("writer stopped: {e}");
                break;
            }
        }
        w
    });
    Sink { tx, task }
}

/// The writer task ends (after flushing) when every `Writer` clone is dropped.
pub fn spawn_writer(w: BoxWrite, mut sealer: Sealer) -> (Writer, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<WriteCmd>();
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<WriteCmd>();
    let handle = tokio::spawn(async move {
        let mut sink = spawn_sink(w);
        let mut encrypted = true;
        let mut queue: Vec<(nc::OfflineFrame, Option<tokio::sync::OwnedSemaphorePermit>)> = vec![];
        let mut next: Option<(BoxWrite, bool)> = None;
        loop {
            let cmd = tokio::select! {
                biased;
                c = rx.recv() => match c {
                    Some(c) => c,
                    None => match data_rx.recv().await {
                        Some(d) => d,
                        None => break,
                    },
                },
                Some(d) = data_rx.recv() => d,
            };
            let batch = match cmd {
                WriteCmd::Frame(f) if next.is_some() => {
                    queue.push((f, None));
                    continue;
                }
                WriteCmd::Data(f, p) if next.is_some() => {
                    queue.push((f, Some(p)));
                    continue;
                }
                WriteCmd::Frame(f) | WriteCmd::Old(f) => vec![(f, None)],
                WriteCmd::Data(f, p) => vec![(f, Some(p))],
                WriteCmd::Pause(w, enc) => {
                    next = Some((w, enc));
                    continue;
                }
                WriteCmd::Resume => {
                    if let Some((w, enc)) = next.take() {
                        let Sink { tx: prior_tx, task: prior_task } = std::mem::replace(&mut sink, spawn_sink(w));
                        encrypted = enc;
                        // Upstream (BwuManager::ProcessSafeToClosePriorChannelEvent):
                        // disable encryption on the prior channel, write a plaintext
                        // DISCONNECTION, then close it. The phone only switches its
                        // read loop (and starts streaming) once the old channel ends.
                        // Diagnostic: delay the prior-channel close (moves the BT teardown later).
                        if let Some(ms) = std::env::var("DRIFT_DISCONNECT_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
                            tracing::info!(ms, "diagnostic: delaying prior-channel DISCONNECTION");
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                        let bye = payload::disconnection().encode_to_vec();
                        let _ = prior_tx.send((bye, None)).await; // after everything queued for it
                        drop(prior_tx);
                        tokio::spawn(async move {
                            if let Ok(mut prior) = prior_task.await {
                                let _ = prior.flush().await;
                                // Give the peer a moment to read it before we hang up.
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                let _ = prior.shutdown().await;
                            }
                        });
                    }
                    std::mem::take(&mut queue)
                }
            };
            for (f, permit) in batch {
                if permit.is_none() {
                    tracing::debug!(kind = ?f.v1.as_ref().and_then(|v| v.r#type), "writing control frame");
                }
                let t = std::time::Instant::now();
                // Sealing a 512 KiB chunk is ~4 ms of pure CPU. The socket task we just woke sits in this
                // worker's LIFO slot (other workers can't steal it) until we yield, so without this it
                // only ran between seals: measured 54% sealing / 38% socket idle on the phone. Marking
                // the seal as blocking hands this worker's queue to another thread meanwhile.
                let bytes = if encrypted && tokio::runtime::Handle::try_current().is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread) {
                    tokio::task::block_in_place(|| sealer.seal(&f.encode_to_vec()))
                } else if encrypted {
                    sealer.seal(&f.encode_to_vec())
                } else {
                    f.encode_to_vec()
                };
                crate::stats::SEAL.add(t);
                let t = std::time::Instant::now();
                let sent = sink.tx.send((bytes, permit)).await;
                crate::stats::QUEUE_WAIT.add(t);
                if sent.is_err() {
                    return; // the connection's write failed
                }
            }
        }
        drop(sink.tx);
        if let Ok(mut w) = sink.task.await {
            let _ = w.shutdown().await;
        }
    });
    (Writer(tx, data_tx), handle)
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("connection closed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] drift_core::Error),
}

/// Hands the reader its next stream once the upgrade handshake succeeds.
#[derive(Clone)]
pub struct ReaderSwitch(mpsc::UnboundedSender<(BoxRead, bool)>);

impl ReaderSwitch {
    pub fn switch_to(&self, next: BoxRead, encrypted: bool) {
        let _ = self.0.send((next, encrypted));
    }
}

fn bwu_event(f: &nc::OfflineFrame) -> Option<BwuEvent> {
    let b = f.v1.as_ref()?.bandwidth_upgrade_negotiation.as_ref()?;
    BwuEvent::try_from(b.event_type?).ok()
}

/// Spawn the reader. Frames arrive on the returned receiver (bounded, so a slow
/// consumer back-pressures the socket); a final `Err` reports why it stopped.
/// `hold_after_switch`: pause before reading a new channel. Workaround for a
/// Realtek combo-chip coex bug: heavy Wi-Fi Direct traffic while the Bluetooth
/// bootstrap link is being torn down (~3.3 s after handover) wedges the GO
/// (NOTES.md, failure F12). Holding reads keeps Wi-Fi idle until BT is gone.
pub fn spawn_reader(
    rd: BoxRead,
    mut opener: Opener,
    hold_after_switch: Option<Duration>,
) -> (mpsc::Receiver<Result<nc::OfflineFrame, ReadError>>, ReaderSwitch) {
    let (tx, rx) = mpsc::channel(64);
    let (sw_tx, mut sw_rx) = mpsc::unbounded_channel::<(BoxRead, bool)>();
    tokio::spawn(async move {
        let mut rd = rd;
        let mut encrypted = true;
        let (mut last_write, mut safe_to_close) = (false, false);
        loop {
            let result: Result<nc::OfflineFrame, ReadError> = async {
                let t = std::time::Instant::now();
                let raw = read_frame(&mut rd).await?;
                crate::stats::SOCKET_READ.add(t);
                let t = std::time::Instant::now();
                let bytes = if encrypted { opener.open(&raw)? } else { raw };
                let f = nc::OfflineFrame::decode(bytes.as_slice()).map_err(drift_core::Error::from)?;
                crate::stats::OPEN.add(t);
                Ok(f)
            }
            .await;
            let frame = match result {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            match bwu_event(&frame) {
                Some(BwuEvent::LastWriteToPriorChannel) => last_write = true,
                Some(BwuEvent::SafeToClosePriorChannel) => safe_to_close = true,
                _ => {}
            }
            let t = std::time::Instant::now();
            if tx.send(Ok(frame)).await.is_err() {
                return;
            }
            crate::stats::DISPATCH_WAIT.add(t);
            if last_write && safe_to_close {
                last_write = false;
                safe_to_close = false;
                // The peer won't write to the old channel again; move to the new one.
                match tokio::time::timeout(Duration::from_secs(10), sw_rx.recv()).await {
                    Ok(Some((next, enc))) => {
                        rd = next;
                        encrypted = enc;
                        tracing::debug!("reader switched channel (encrypted={enc})");
                        if let Some(hold) = hold_after_switch {
                            tracing::info!(?hold, "holding reads while the bootstrap link tears down");
                            tokio::time::sleep(hold).await;
                        }
                    }
                    _ => tracing::warn!("peer finished the upgrade but no new channel was registered"),
                }
            }
        }
    });
    (rx, ReaderSwitch(sw_tx))
}

/// Reply our side of the prior-channel shutdown handshake.
pub fn on_peer_bwu_shutdown(writer: &Writer, event: BwuEvent) {
    match event {
        BwuEvent::LastWriteToPriorChannel => writer.send_on_prior(payload::bwu_event(BwuEvent::SafeToClosePriorChannel)),
        BwuEvent::SafeToClosePriorChannel => writer.resume(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drift_core::secure;
    use drift_core::ukey2::{ClientHandshake, Role, ServerHandshake};

    fn keys() -> ((Sealer, Opener), (Sealer, Opener)) {
        let (c, ci) = ClientHandshake::new();
        let (s, si) = ServerHandshake::handle_client_init(&ci).unwrap();
        let (cr, cf) = c.handle_server_init(&si).unwrap();
        let sr = s.handle_client_finish(&cf).unwrap();
        (secure::split(&sr.session_keys(Role::Server)), secure::split(&cr.session_keys(Role::Client)))
    }

    /// Full upgrade dance between our writer/reader and a scripted peer over
    /// in-memory pipes, including a switch to a plaintext channel.
    #[tokio::test]
    async fn upgrade_switches_both_directions() {
        let ((our_sealer, our_opener), (mut peer_sealer, mut peer_opener)) = keys();
        let (ours_old, mut peer_old) = tokio::io::duplex(1 << 16);
        let (ours_new, mut peer_new) = tokio::io::duplex(1 << 16);
        let (old_r, old_w) = tokio::io::split(ours_old);
        let (new_r, new_w) = tokio::io::split(ours_new);

        let (writer, _wh) = spawn_writer(Box::new(old_w), our_sealer);
        let (mut frames, switch) = spawn_reader(Box::new(old_r), our_opener, None);

        // We begin the upgrade: pause onto the plaintext new channel, LAST_WRITE on old.
        writer.pause_for(Box::new(new_w), false);
        writer.send_on_prior(payload::bwu_event(BwuEvent::LastWriteToPriorChannel));
        switch.switch_to(Box::new(new_r), false);
        writer.send(payload::keep_alive(false, 1)); // queued until SAFE_TO_CLOSE

        // Peer reads our LAST_WRITE on old.
        let f = nc::OfflineFrame::decode(peer_opener.open(&read_frame(&mut peer_old).await.unwrap()).unwrap().as_slice()).unwrap();
        assert_eq!(bwu_event(&f), Some(BwuEvent::LastWriteToPriorChannel));

        // Peer sends its LAST_WRITE then SAFE_TO_CLOSE on old (encrypted).
        for ev in [BwuEvent::LastWriteToPriorChannel, BwuEvent::SafeToClosePriorChannel] {
            let sealed = peer_sealer.seal(&payload::bwu_event(ev).encode_to_vec());
            write_frame(&mut peer_old, &sealed).await.unwrap();
        }
        for want in [BwuEvent::LastWriteToPriorChannel, BwuEvent::SafeToClosePriorChannel] {
            let f = frames.recv().await.unwrap().unwrap();
            assert_eq!(bwu_event(&f), Some(want));
            on_peer_bwu_shutdown(&writer, want);
        }
        // Our SAFE_TO_CLOSE went out on old.
        let f = nc::OfflineFrame::decode(peer_opener.open(&read_frame(&mut peer_old).await.unwrap()).unwrap().as_slice()).unwrap();
        assert_eq!(bwu_event(&f), Some(BwuEvent::SafeToClosePriorChannel));
        // Then the prior channel is closed with a *plaintext* DISCONNECTION (no sequence number).
        let f = nc::OfflineFrame::decode(read_frame(&mut peer_old).await.unwrap().as_slice()).unwrap();
        assert!(f.v1.unwrap().disconnection.is_some());
        // The queued keep-alive arrives on the new channel, in plaintext.
        let f = nc::OfflineFrame::decode(read_frame(&mut peer_new).await.unwrap().as_slice()).unwrap();
        assert_eq!(f.v1.unwrap().keep_alive.unwrap().seq_num, Some(1));
        // And the reader now reads plaintext from the new channel.
        write_frame(&mut peer_new, &payload::keep_alive(true, 9).encode_to_vec()).await.unwrap();
        let f = frames.recv().await.unwrap().unwrap();
        assert_eq!(f.v1.unwrap().keep_alive.unwrap().seq_num, Some(9));
    }
}
