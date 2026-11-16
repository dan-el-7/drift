//! Quick Share receive session as a sans-IO state machine.
//!
//! Feed it decrypted `OfflineFrame`s from the phone; it returns [`Action`]s for
//! the caller to perform (send frames, write file data, surface events). No I/O,
//! no clocks, no OS — so the whole flow is unit-testable.
//!
//! Bandwidth-upgrade and keep-alive frames are *not* handled here; the
//! connection layer intercepts them before they reach the session.

use std::collections::HashMap;

use prost::Message;
use rand::RngCore;

use crate::payload::{self, BytesAssembler};
use crate::proto::connections as nc;
use crate::proto::sharing as sh;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingFile {
    pub payload_id: i64,
    pub name: String,
    pub size: i64,
    pub mime_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Url,
    Address,
    PhoneNumber,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingText {
    pub payload_id: i64,
    pub title: String,
    pub kind: TextKind,
    pub size: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The phone described what it wants to send. Caller decides, then calls
    /// [`ReceiveSession::accept`] or [`ReceiveSession::reject`].
    Introduction { files: Vec<IncomingFile>, texts: Vec<IncomingText> },
    /// Bytes received for a file so far.
    FileProgress { payload_id: i64, received: i64, total: i64 },
    /// A text/URL item arrived in full.
    Text { item: IncomingText, content: String },
    /// Every announced item has arrived.
    Completed,
    /// The phone cancelled the share.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send this frame to the phone (over the current channel).
    Send(nc::OfflineFrame),
    /// Start a file: open a destination for `file`.
    FileOpen(IncomingFile),
    /// Write `data` at `offset` into the file for `payload_id`.
    FileWrite { payload_id: i64, offset: i64, data: Vec<u8> },
    /// The file for `payload_id` is complete; flush/close it.
    FileClose { payload_id: i64 },
    /// Discard a partially written file (cancelled or failed).
    FileAbort { payload_id: i64 },
    Event(Event),
    /// The transfer is finished: tell the phone we're done and close the connection.
    /// (The phone only shows the share as "sent" once the receiver disconnects.)
    Disconnect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for the phone's PairedKeyEncryption / PairedKeyResult / Introduction.
    Handshaking,
    /// Introduction received, waiting for the user.
    AwaitingDecision,
    /// Accepted; payloads are flowing.
    Receiving,
    Done,
    Rejected,
    Cancelled,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("FILE payload {0} was not announced in the introduction")]
    UnknownPayload(i64),
    #[error("file payload {0} arrived before the share was accepted")]
    NotAccepted(i64),
    #[error("chunk for payload {id} at offset {offset}, expected {expected}")]
    OutOfOrder { id: i64, offset: i64, expected: i64 },
    #[error("file {id} exceeded its announced size {size}")]
    Oversize { id: i64, size: i64 },
    #[error("file {id} ended at {received} of its announced {size} bytes")]
    Truncated { id: i64, received: i64, size: i64 },
    #[error("bad sharing frame: {0}")]
    Decode(String),
}

struct FileState {
    file: IncomingFile,
    received: i64,
    opened: bool,
}

/// This session's trust context: our key, the connection's auth string, whether the peer is Drift.
struct TrustCtx {
    key: std::sync::Arc<crate::trust::DeviceKey>,
    auth: [u8; 32],
    peer_drift: bool,
}

pub struct ReceiveSession {
    trust: Option<TrustCtx>,
    peer_key: Option<crate::trust::PeerKey>,
    state: State,
    bytes: BytesAssembler,
    files: HashMap<i64, FileState>,
    texts: HashMap<i64, IncomingText>,
    remaining: usize,
}

impl Default for ReceiveSession {
    fn default() -> Self {
        Self::new()
    }
}

fn random_payload_id() -> i64 {
    // Positive, like Android's.
    (rand::thread_rng().next_u64() >> 1) as i64
}

/// What we send stock Quick Share in place of an account certificate.
pub(crate) fn random_paired_key_frame() -> sh::PairedKeyEncryptionFrame {
    let mut signed = vec![0u8; 72];
    let mut hash = vec![0u8; 6];
    rand::thread_rng().fill_bytes(&mut signed);
    rand::thread_rng().fill_bytes(&mut hash);
    sh::PairedKeyEncryptionFrame { signed_data: Some(signed), secret_id_hash: Some(hash), ..Default::default() }
}

/// Wrap a sharing V1 frame as the two-chunk BYTES payload Android expects.
fn send_sharing(v: sh::V1Frame) -> Vec<Action> {
    payload::bytes_payload(random_payload_id(), &payload::sharing_frame(v)).into_iter().map(Action::Send).collect()
}

fn text_kind(t: Option<i32>) -> TextKind {
    use sh::text_metadata::Type as T;
    match t.and_then(|t| T::try_from(t).ok()) {
        Some(T::Text) => TextKind::Text,
        Some(T::Url) => TextKind::Url,
        Some(T::Address) => TextKind::Address,
        Some(T::PhoneNumber) => TextKind::PhoneNumber,
        _ => TextKind::Unknown,
    }
}

impl ReceiveSession {
    pub fn new() -> Self {
        Self { trust: None, peer_key: None, state: State::Handshaking, bytes: BytesAssembler::default(), files: HashMap::new(), texts: HashMap::new(), remaining: 0 }
    }

    /// Drift trust for this connection (see `trust`). Without it the session behaves as before.
    pub fn set_trust(&mut self, key: std::sync::Arc<crate::trust::DeviceKey>, auth: [u8; 32], peer_drift: bool) {
        self.trust = Some(TrustCtx { key, auth, peer_drift });
    }

    /// The sender's device key, if it proved one in this session (Drift peers only).
    pub fn peer_key(&self) -> Option<&crate::trust::PeerKey> {
        self.peer_key.as_ref()
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// User accepted the introduction.
    pub fn accept(&mut self) -> Vec<Action> {
        if self.state != State::AwaitingDecision {
            return vec![];
        }
        self.state = State::Receiving;
        let mut out = send_sharing(sh::V1Frame {
            r#type: Some(sh::v1_frame::FrameType::Response as i32),
            connection_response: Some(sh::ConnectionResponseFrame {
                status: Some(sh::connection_response_frame::Status::Accept as i32),
                ..Default::default()
            }),
            ..Default::default()
        });
        if self.remaining == 0 {
            out.extend(self.finish());
        }
        out
    }

    /// User declined. The phone shows "declined"; we then disconnect.
    pub fn reject(&mut self) -> Vec<Action> {
        if self.state != State::AwaitingDecision {
            return vec![];
        }
        self.state = State::Rejected;
        let mut out = send_sharing(sh::V1Frame {
            r#type: Some(sh::v1_frame::FrameType::Response as i32),
            connection_response: Some(sh::ConnectionResponseFrame {
                status: Some(sh::connection_response_frame::Status::Reject as i32),
                ..Default::default()
            }),
            ..Default::default()
        });
        out.push(Action::Disconnect);
        out
    }

    /// Feed one decrypted frame from the phone.
    pub fn on_frame(&mut self, frame: &nc::OfflineFrame) -> Result<Vec<Action>, SessionError> {
        let Some(v1) = frame.v1.as_ref() else { return Ok(vec![]) };
        use nc::v1_frame::FrameType as T;
        match payload::frame_type(frame) {
            T::PayloadTransfer => self.on_payload(v1.payload_transfer.as_ref().cloned().unwrap_or_default()),
            T::Disconnection => {
                if matches!(self.state, State::Done | State::Rejected) {
                    Ok(vec![])
                } else {
                    Ok(self.cancel())
                }
            }
            _ => Ok(vec![]),
        }
    }

    /// The user cancelled on the PC: tell the phone (sharing CANCEL), drop partial files, hang up.
    /// Before accepting this is a decline. Nothing once the share has ended.
    pub fn cancel_local(&mut self) -> Vec<Action> {
        match self.state {
            State::Done | State::Rejected | State::Cancelled => vec![],
            State::AwaitingDecision => self.reject(),
            _ => {
                // Unfinished incoming files first (Nearby payload cancel), then the sharing CANCEL.
                let mut out: Vec<Action> = self
                    .files
                    .values()
                    .filter(|f| f.received < f.file.size)
                    .map(|f| Action::Send(payload::payload_canceled(f.file.payload_id, f.file.size, f.received)))
                    .collect();
                out.extend(send_sharing(sh::V1Frame { r#type: Some(sh::v1_frame::FrameType::Cancel as i32), ..Default::default() }));
                out.push(Action::Send(payload::disconnection()));
                out.extend(self.cancel());
                out
            }
        }
    }

    fn cancel(&mut self) -> Vec<Action> {
        self.state = State::Cancelled;
        let mut out: Vec<Action> = self
            .files
            .values()
            .filter(|f| f.opened && f.received < f.file.size)
            .map(|f| Action::FileAbort { payload_id: f.file.payload_id })
            .collect();
        out.push(Action::Event(Event::Cancelled));
        out
    }

    fn finish(&mut self) -> Vec<Action> {
        self.state = State::Done;
        vec![Action::Event(Event::Completed), Action::Disconnect]
    }

    fn item_done(&mut self) -> Vec<Action> {
        self.remaining = self.remaining.saturating_sub(1);
        if self.remaining == 0 && self.state == State::Receiving { self.finish() } else { vec![] }
    }

    fn on_payload(&mut self, p: nc::PayloadTransferFrame) -> Result<Vec<Action>, SessionError> {
        use nc::payload_transfer_frame as ptf;
        let header = p.payload_header.unwrap_or_default();
        let id = header.id.unwrap_or(0);

        if p.packet_type == Some(ptf::PacketType::Control as i32) {
            // PAYLOAD_CANCELED / PAYLOAD_ERROR for one payload.
            if let Some(f) = self.files.get(&id) {
                if f.opened {
                    return Ok(vec![Action::FileAbort { payload_id: id }]);
                }
            }
            return Ok(vec![]);
        }
        let Some(chunk) = p.payload_chunk else { return Ok(vec![]) };
        let last = chunk.flags.unwrap_or(0) & ptf::payload_chunk::Flags::LastChunk as i32 != 0;

        match ptf::payload_header::PayloadType::try_from(header.r#type.unwrap_or(0)) {
            Ok(ptf::payload_header::PayloadType::Bytes) => {
                let Some(data) = self.bytes.push(id, &chunk) else { return Ok(vec![]) };
                if let Some(item) = self.texts.remove(&id) {
                    let content = String::from_utf8_lossy(&data).into_owned();
                    let mut out = vec![Action::Event(Event::Text { item, content })];
                    out.extend(self.item_done());
                    return Ok(out);
                }
                self.on_sharing_frame(&data)
            }
            Ok(ptf::payload_header::PayloadType::File) => {
                if self.state != State::Receiving {
                    return Err(SessionError::NotAccepted(id));
                }
                let f = self.files.get_mut(&id).ok_or(SessionError::UnknownPayload(id))?;
                let mut out = vec![];
                if !f.opened {
                    f.opened = true;
                    out.push(Action::FileOpen(f.file.clone()));
                }
                let offset = chunk.offset.unwrap_or(f.received);
                if offset != f.received {
                    return Err(SessionError::OutOfOrder { id, offset, expected: f.received });
                }
                let body = chunk.body.unwrap_or_default();
                if !body.is_empty() {
                    f.received += body.len() as i64;
                    if f.received > f.file.size {
                        return Err(SessionError::Oversize { id, size: f.file.size });
                    }
                    out.push(Action::FileWrite { payload_id: id, offset, data: body });
                    out.push(Action::Event(Event::FileProgress { payload_id: id, received: f.received, total: f.file.size }));
                }
                if last {
                    // the last chunk must bring the file to its announced size: a short file is a failure
                    if f.received != f.file.size {
                        return Err(SessionError::Truncated { id, received: f.received, size: f.file.size });
                    }
                    out.push(Action::FileClose { payload_id: id });
                    out.extend(self.item_done());
                }
                Ok(out)
            }
            _ => Ok(vec![]),
        }
    }

    fn on_sharing_frame(&mut self, data: &[u8]) -> Result<Vec<Action>, SessionError> {
        let frame = sh::Frame::decode(data).map_err(|e| SessionError::Decode(e.to_string()))?;
        let v = frame.v1.unwrap_or_default();
        use sh::v1_frame::FrameType as T;
        match T::try_from(v.r#type.unwrap_or(0)).unwrap_or(T::UnknownFrameType) {
            T::PairedKeyEncryption => {
                // Drift peer: check its device key, answer with ours. Stock Quick Share (account
                // certificates we don't have): random data, and both sides report UNABLE.
                if let (Some(t), Some(f)) = (&self.trust, v.paired_key_encryption.as_ref()) {
                    self.peer_key = crate::trust::verify(f, &t.auth, crate::trust::ROLE_SENDER);
                }
                let frame = match &self.trust {
                    Some(t) if t.peer_drift => t.key.frame(&t.auth, crate::trust::ROLE_RECEIVER),
                    _ => random_paired_key_frame(),
                };
                Ok(send_sharing(sh::V1Frame {
                    r#type: Some(T::PairedKeyEncryption as i32),
                    paired_key_encryption: Some(frame),
                    ..Default::default()
                }))
            }
            T::PairedKeyResult => Ok(send_sharing(sh::V1Frame {
                r#type: Some(T::PairedKeyResult as i32),
                paired_key_result: Some(sh::PairedKeyResultFrame {
                    status: Some(if self.peer_key.is_some() { sh::paired_key_result_frame::Status::Success } else { sh::paired_key_result_frame::Status::Unable } as i32),
                    os_type: None,
                }),
                ..Default::default()
            })),
            T::Introduction => {
                if self.state != State::Handshaking {
                    return Ok(vec![]);
                }
                let intro = v.introduction.unwrap_or_default();
                let files: Vec<IncomingFile> = intro
                    .file_metadata
                    .iter()
                    .map(|f| IncomingFile {
                        payload_id: f.payload_id.unwrap_or(0),
                        name: f.name.clone().unwrap_or_else(|| "file".into()),
                        size: f.size.unwrap_or(0),
                        mime_type: f.mime_type.clone().unwrap_or_default(),
                    })
                    .collect();
                let texts: Vec<IncomingText> = intro
                    .text_metadata
                    .iter()
                    .map(|t| IncomingText {
                        payload_id: t.payload_id.unwrap_or(0),
                        title: t.text_title.clone().unwrap_or_default(),
                        kind: text_kind(t.r#type),
                        size: t.size.unwrap_or(0),
                    })
                    .collect();
                for f in &files {
                    self.files.insert(f.payload_id, FileState { file: f.clone(), received: 0, opened: false });
                }
                for t in &texts {
                    self.texts.insert(t.payload_id, t.clone());
                }
                self.remaining = files.len() + texts.len();
                self.state = State::AwaitingDecision;
                Ok(vec![Action::Event(Event::Introduction { files, texts })])
            }
            T::Cancel => Ok(self.cancel()),
            // Response (phone echoes), ProgressUpdate, Bindings, ...: nothing to do.
            _ => Ok(vec![]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{bytes_payload, offline, sharing_frame};

    fn phone_sharing(v: sh::V1Frame) -> Vec<nc::OfflineFrame> {
        bytes_payload(99, &sharing_frame(v)).to_vec()
    }

    fn feed(s: &mut ReceiveSession, frames: Vec<nc::OfflineFrame>) -> Vec<Action> {
        frames.iter().flat_map(|f| s.on_frame(f).unwrap()).collect()
    }

    fn intro(files: &[(i64, &str, i64)], texts: &[(i64, &str)]) -> Vec<nc::OfflineFrame> {
        phone_sharing(sh::V1Frame {
            r#type: Some(sh::v1_frame::FrameType::Introduction as i32),
            introduction: Some(sh::IntroductionFrame {
                file_metadata: files
                    .iter()
                    .map(|(id, name, size)| sh::FileMetadata {
                        payload_id: Some(*id),
                        name: Some((*name).into()),
                        size: Some(*size),
                        ..Default::default()
                    })
                    .collect(),
                text_metadata: texts
                    .iter()
                    .map(|(id, title)| sh::TextMetadata {
                        payload_id: Some(*id),
                        text_title: Some((*title).into()),
                        r#type: Some(sh::text_metadata::Type::Url as i32),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn file_chunk(id: i64, offset: i64, body: &[u8], last: bool) -> nc::OfflineFrame {
        use nc::payload_transfer_frame as ptf;
        offline(nc::V1Frame {
            r#type: Some(nc::v1_frame::FrameType::PayloadTransfer as i32),
            payload_transfer: Some(nc::PayloadTransferFrame {
                packet_type: Some(ptf::PacketType::Data as i32),
                payload_header: Some(ptf::PayloadHeader {
                    id: Some(id),
                    r#type: Some(ptf::payload_header::PayloadType::File as i32),
                    ..Default::default()
                }),
                payload_chunk: Some(ptf::PayloadChunk {
                    flags: Some(if last { 1 } else { 0 }),
                    offset: Some(offset),
                    body: Some(body.to_vec()),
                    index: None,
                }),
                control_message: None,
            }),
            ..Default::default()
        })
    }

    fn sent_sharing_types(actions: &[Action]) -> Vec<sh::v1_frame::FrameType> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(f) => {
                    let p = f.v1.as_ref()?.payload_transfer.as_ref()?;
                    let body = p.payload_chunk.as_ref()?.body.as_ref()?;
                    if body.is_empty() {
                        return None;
                    }
                    let fr = sh::Frame::decode(body.as_slice()).ok()?;
                    sh::v1_frame::FrameType::try_from(fr.v1?.r#type?).ok()
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn handshake_replies() {
        use sh::v1_frame::FrameType as T;
        let mut s = ReceiveSession::new();
        let a = feed(&mut s, phone_sharing(sh::V1Frame { r#type: Some(T::PairedKeyEncryption as i32), ..Default::default() }));
        assert_eq!(sent_sharing_types(&a), vec![T::PairedKeyEncryption]);
        let a = feed(&mut s, phone_sharing(sh::V1Frame { r#type: Some(T::PairedKeyResult as i32), ..Default::default() }));
        assert_eq!(sent_sharing_types(&a), vec![T::PairedKeyResult]);
        assert_eq!(s.state(), State::Handshaking);
    }

    #[test]
    fn full_file_transfer() {
        let mut s = ReceiveSession::new();
        let a = feed(&mut s, intro(&[(7, "a.jpg", 5)], &[]));
        assert!(matches!(&a[..], [Action::Event(Event::Introduction { files, .. })] if files[0].name == "a.jpg"));
        assert_eq!(s.state(), State::AwaitingDecision);

        let a = s.accept();
        assert_eq!(sent_sharing_types(&a), vec![sh::v1_frame::FrameType::Response]);

        let a = s.on_frame(&file_chunk(7, 0, b"abc", false)).unwrap();
        assert!(matches!(a[0], Action::FileOpen(_)));
        assert!(matches!(&a[1], Action::FileWrite { offset: 0, data, .. } if data == b"abc"));

        let a = s.on_frame(&file_chunk(7, 3, b"de", false)).unwrap();
        assert!(matches!(&a[0], Action::FileWrite { offset: 3, .. }));
        let a = s.on_frame(&file_chunk(7, 5, b"", true)).unwrap();
        assert_eq!(a, vec![Action::FileClose { payload_id: 7 }, Action::Event(Event::Completed), Action::Disconnect]);
        assert_eq!(s.state(), State::Done);
    }

    #[test]
    fn text_item_and_completion() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[], &[(11, "example.com")]));
        s.accept();
        let a = feed(&mut s, bytes_payload(11, b"https://example.com").to_vec());
        assert!(matches!(&a[0], Action::Event(Event::Text { content, item }) if content == "https://example.com" && item.kind == TextKind::Url));
        assert_eq!(&a[1..], &[Action::Event(Event::Completed), Action::Disconnect]);
    }

    #[test]
    fn reject_disconnects() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 1)], &[]));
        let a = s.reject();
        assert_eq!(sent_sharing_types(&a), vec![sh::v1_frame::FrameType::Response]);
        assert_eq!(a.last(), Some(&Action::Disconnect));
        assert_eq!(s.state(), State::Rejected);
    }

    #[test]
    fn file_before_accept_is_an_error() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 3)], &[]));
        assert_eq!(s.on_frame(&file_chunk(7, 0, b"abc", true)), Err(SessionError::NotAccepted(7)));
    }

    #[test]
    fn unknown_and_misordered_payloads_rejected() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 6)], &[]));
        s.accept();
        assert_eq!(s.on_frame(&file_chunk(8, 0, b"x", false)), Err(SessionError::UnknownPayload(8)));
        s.on_frame(&file_chunk(7, 0, b"abc", false)).unwrap();
        assert_eq!(
            s.on_frame(&file_chunk(7, 5, b"x", false)),
            Err(SessionError::OutOfOrder { id: 7, offset: 5, expected: 3 })
        );
    }

    #[test]
    fn oversize_rejected() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 2)], &[]));
        s.accept();
        assert_eq!(s.on_frame(&file_chunk(7, 0, b"abc", false)), Err(SessionError::Oversize { id: 7, size: 2 }));
    }

    #[test]
    fn short_file_is_not_complete() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 100)], &[]));
        s.accept();
        assert_eq!(s.on_frame(&file_chunk(7, 0, b"abc", true)), Err(SessionError::Truncated { id: 7, received: 3, size: 100 }));
    }

    #[test]
    fn phone_cancel_aborts_open_files() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[(7, "a", 10)], &[]));
        s.accept();
        s.on_frame(&file_chunk(7, 0, b"abc", false)).unwrap();
        let a = feed(&mut s, phone_sharing(sh::V1Frame { r#type: Some(sh::v1_frame::FrameType::Cancel as i32), ..Default::default() }));
        assert_eq!(a, vec![Action::FileAbort { payload_id: 7 }, Action::Event(Event::Cancelled)]);
        assert_eq!(s.state(), State::Cancelled);
    }

    #[test]
    fn disconnect_after_done_is_quiet() {
        let mut s = ReceiveSession::new();
        feed(&mut s, intro(&[], &[(11, "t")]));
        s.accept();
        feed(&mut s, bytes_payload(11, b"hi").to_vec());
        assert_eq!(s.on_frame(&payload::disconnection()).unwrap(), vec![]);
    }
}
