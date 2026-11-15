//! Quick Share send session as a sans-IO state machine (the PC is the sender).
//!
//! Flow (both sides exchange the PairedKey frames first):
//!   us → PairedKeyEncryption, phone → PairedKeyEncryption, us → PairedKeyResult,
//!   phone → PairedKeyResult, us → Introduction, phone → Response(accept|reject),
//!   us → FILE payloads (streamed by the caller), phone disconnects when done.
//! Bandwidth-upgrade and keep-alive frames are handled by the connection layer.

use prost::Message;
use rand::RngCore;

use crate::payload::{self, BytesAssembler};
use crate::proto::connections as nc;
use crate::proto::sharing as sh;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingFile {
    pub payload_id: i64,
    pub name: String,
    pub size: i64,
    pub mime_type: String,
}

impl OutgoingFile {
    pub fn new(name: &str, size: i64, mime_type: &str) -> Self {
        Self { payload_id: random_id(), name: name.to_owned(), size, mime_type: mime_type.to_owned() }
    }
}

/// A link or some text to send (Quick Share TextMetadata + a BYTES payload with the body).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingText {
    pub payload_id: i64,
    pub kind: sh::text_metadata::Type,
    pub body: String,
}

impl OutgoingText {
    pub fn new(kind: sh::text_metadata::Type, body: &str) -> Self {
        Self { payload_id: random_id(), kind, body: body.to_owned() }
    }

    /// What the phone shows before accepting (Android uses the text itself, shortened).
    fn title(&self) -> String {
        let t: String = self.body.chars().take(64).collect();
        if self.body.chars().count() > 64 { format!("{t}…") } else { t }
    }
}

fn random_id() -> i64 {
    (rand::thread_rng().next_u64() >> 1) as i64
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Exchanging PairedKey frames.
    Handshaking,
    /// Introduction sent, waiting for the user on the phone.
    AwaitingResponse,
    /// Accepted: the caller streams the files.
    Streaming,
    /// Every file sent; waiting for the phone to disconnect.
    Sent,
    Done,
    Declined,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Send(nc::OfflineFrame),
    /// The phone accepted: start streaming the files, in order.
    StartStreaming,
    /// The phone declined (status says why, e.g. NOT_ENOUGH_SPACE).
    Declined(sh::connection_response_frame::Status),
    Cancelled,
    /// All files were sent and the phone hung up: success.
    Completed,
    Disconnect,
}

pub struct SendSession {
    trust: Option<(std::sync::Arc<crate::trust::DeviceKey>, [u8; 32], bool)>,
    peer_key: Option<crate::trust::PeerKey>,
    state: State,
    files: Vec<OutgoingFile>,
    texts: Vec<OutgoingText>,
    bytes: BytesAssembler,
    sent_pke: bool,
}

fn sharing(v: sh::V1Frame) -> Vec<Action> {
    payload::bytes_payload(random_id(), &payload::sharing_frame(v)).into_iter().map(Action::Send).collect()
}

impl SendSession {
    pub fn new(files: Vec<OutgoingFile>) -> Self {
        Self::with_texts(files, vec![])
    }

    pub fn with_texts(files: Vec<OutgoingFile>, texts: Vec<OutgoingText>) -> Self {
        Self { trust: None, peer_key: None, state: State::Handshaking, files, texts, bytes: BytesAssembler::default(), sent_pke: false }
    }

    /// Drift trust for this connection: our key, the auth string, whether the receiver is Drift.
    pub fn set_trust(&mut self, key: std::sync::Arc<crate::trust::DeviceKey>, auth: [u8; 32], peer_drift: bool) {
        self.trust = Some((key, auth, peer_drift));
    }

    /// The receiver's device key, if it proved one in this session.
    pub fn peer_key(&self) -> Option<&crate::trust::PeerKey> {
        self.peer_key.as_ref()
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn files(&self) -> &[OutgoingFile] {
        &self.files
    }

    /// Call once the encrypted channel is up.
    pub fn start(&mut self) -> Vec<Action> {
        self.sent_pke = true;
        let frame = match &self.trust {
            Some((key, auth, true)) => key.frame(auth, crate::trust::ROLE_SENDER),
            _ => crate::sharing::random_paired_key_frame(),
        };
        sharing(sh::V1Frame {
            r#type: Some(sh::v1_frame::FrameType::PairedKeyEncryption as i32),
            paired_key_encryption: Some(frame),
            ..Default::default()
        })
    }

    /// The caller finished streaming every file.
    pub fn all_sent(&mut self) {
        if self.state == State::Streaming {
            self.state = State::Sent;
        }
    }

    /// The user cancelled on the PC.
    pub fn cancel(&mut self) -> Vec<Action> {
        if matches!(self.state, State::Done | State::Declined | State::Cancelled) {
            return vec![];
        }
        self.state = State::Cancelled;
        let mut out = sharing(sh::V1Frame { r#type: Some(sh::v1_frame::FrameType::Cancel as i32), ..Default::default() });
        out.push(Action::Disconnect);
        out
    }

    pub fn on_frame(&mut self, frame: &nc::OfflineFrame) -> Vec<Action> {
        use nc::v1_frame::FrameType as T;
        match payload::frame_type(frame) {
            T::PayloadTransfer => {
                let p = frame.v1.as_ref().and_then(|v| v.payload_transfer.clone()).unwrap_or_default();
                let id = p.payload_header.as_ref().and_then(|h| h.id).unwrap_or(0);
                let is_bytes = p.payload_header.as_ref().and_then(|h| h.r#type)
                    == Some(nc::payload_transfer_frame::payload_header::PayloadType::Bytes as i32);
                match (is_bytes, p.payload_chunk) {
                    (true, Some(chunk)) => match self.bytes.push(id, &chunk) {
                        Some(data) => self.on_sharing(&data),
                        None => vec![],
                    },
                    _ => vec![],
                }
            }
            T::Disconnection => match self.state {
                State::Sent => {
                    self.state = State::Done;
                    vec![Action::Completed]
                }
                State::Done | State::Declined | State::Cancelled => vec![],
                _ => {
                    self.state = State::Cancelled;
                    vec![Action::Cancelled]
                }
            },
            _ => vec![],
        }
    }

    fn on_sharing(&mut self, data: &[u8]) -> Vec<Action> {
        let Ok(frame) = sh::Frame::decode(data) else { return vec![] };
        let v = frame.v1.unwrap_or_default();
        use sh::v1_frame::FrameType as T;
        match T::try_from(v.r#type.unwrap_or(0)).unwrap_or(T::UnknownFrameType) {
            T::PairedKeyEncryption if self.state == State::Handshaking => {
                if let (Some((_, auth, _)), Some(f)) = (&self.trust, v.paired_key_encryption.as_ref()) {
                    self.peer_key = crate::trust::verify(f, auth, crate::trust::ROLE_RECEIVER);
                }
                let mut out = if self.sent_pke { vec![] } else { self.start() };
                out.extend(sharing(sh::V1Frame {
                    r#type: Some(T::PairedKeyResult as i32),
                    paired_key_result: Some(sh::PairedKeyResultFrame {
                        status: Some(sh::paired_key_result_frame::Status::Unable as i32),
                        os_type: None,
                    }),
                    ..Default::default()
                }));
                out
            }
            T::PairedKeyResult if self.state == State::Handshaking => {
                self.state = State::AwaitingResponse;
                sharing(sh::V1Frame {
                    r#type: Some(T::Introduction as i32),
                    introduction: Some(sh::IntroductionFrame {
                        file_metadata: self
                            .files
                            .iter()
                            .map(|f| sh::FileMetadata {
                                name: Some(f.name.clone()),
                                r#type: Some(file_type(&f.mime_type) as i32),
                                payload_id: Some(f.payload_id),
                                size: Some(f.size),
                                mime_type: Some(f.mime_type.clone()),
                                id: Some(random_id()),
                                ..Default::default()
                            })
                            .collect(),
                        text_metadata: self
                            .texts
                            .iter()
                            .map(|t| sh::TextMetadata {
                                text_title: Some(t.title()),
                                r#type: Some(t.kind as i32),
                                payload_id: Some(t.payload_id),
                                size: Some(t.body.len() as i64),
                                id: Some(random_id()),
                                ..Default::default()
                            })
                            .collect(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            T::Response if self.state == State::AwaitingResponse => {
                use sh::connection_response_frame::Status as S;
                let status = v
                    .connection_response
                    .and_then(|r| r.status)
                    .and_then(|s| S::try_from(s).ok())
                    .unwrap_or(S::Unknown);
                if status == S::Accept {
                    self.state = State::Streaming;
                    // texts first (each one small BYTES payload), then the caller streams the files
                    let mut out: Vec<Action> = self
                        .texts
                        .iter()
                        .flat_map(|t| payload::bytes_payload(t.payload_id, t.body.as_bytes()))
                        .map(Action::Send)
                        .collect();
                    out.push(Action::StartStreaming);
                    out
                } else {
                    self.state = State::Declined;
                    vec![Action::Declined(status), Action::Disconnect]
                }
            }
            T::Cancel => {
                self.state = State::Cancelled;
                vec![Action::Cancelled, Action::Disconnect]
            }
            _ => vec![],
        }
    }
}

fn file_type(mime: &str) -> sh::file_metadata::Type {
    use sh::file_metadata::Type as F;
    match mime.split('/').next().unwrap_or("") {
        "image" => F::Image,
        "video" => F::Video,
        "audio" => F::Audio,
        _ if mime == "application/vnd.android.package-archive" => F::AndroidApp,
        _ => F::Unknown,
    }
}

/// Best-effort MIME type from a file extension.
pub fn mime_for(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("mp4") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("apk") => "application/vnd.android.package-archive",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{bytes_payload, sharing_frame};

    fn from_phone(v: sh::V1Frame) -> Vec<nc::OfflineFrame> {
        bytes_payload(5, &sharing_frame(v)).to_vec()
    }

    fn feed(s: &mut SendSession, frames: Vec<nc::OfflineFrame>) -> Vec<Action> {
        frames.iter().flat_map(|f| s.on_frame(f)).collect()
    }

    fn sent_types(actions: &[Action]) -> Vec<sh::v1_frame::FrameType> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(f) => {
                    let body = f.v1.as_ref()?.payload_transfer.as_ref()?.payload_chunk.as_ref()?.body.clone()?;
                    if body.is_empty() {
                        return None;
                    }
                    sh::v1_frame::FrameType::try_from(sh::Frame::decode(body.as_slice()).ok()?.v1?.r#type?).ok()
                }
                _ => None,
            })
            .collect()
    }

    fn typed(t: sh::v1_frame::FrameType) -> sh::V1Frame {
        sh::V1Frame { r#type: Some(t as i32), ..Default::default() }
    }

    fn respond(status: sh::connection_response_frame::Status) -> sh::V1Frame {
        sh::V1Frame {
            r#type: Some(sh::v1_frame::FrameType::Response as i32),
            connection_response: Some(sh::ConnectionResponseFrame { status: Some(status as i32), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn happy_path() {
        use sh::v1_frame::FrameType as T;
        let mut s = SendSession::new(vec![OutgoingFile::new("a.mp4", 10, "video/mp4")]);
        assert_eq!(sent_types(&s.start()), vec![T::PairedKeyEncryption]);
        assert_eq!(sent_types(&feed(&mut s, from_phone(typed(T::PairedKeyEncryption)))), vec![T::PairedKeyResult]);
        let a = feed(&mut s, from_phone(typed(T::PairedKeyResult)));
        assert_eq!(sent_types(&a), vec![T::Introduction]);
        assert_eq!(s.state(), State::AwaitingResponse);
        let a = feed(&mut s, from_phone(respond(sh::connection_response_frame::Status::Accept)));
        assert_eq!(a, vec![Action::StartStreaming]);
        s.all_sent();
        assert_eq!(s.on_frame(&payload::disconnection()), vec![Action::Completed]);
        assert_eq!(s.state(), State::Done);
    }

    #[test]
    fn introduction_describes_files() {
        use sh::v1_frame::FrameType as T;
        let f = OutgoingFile::new("pic.jpg", 1234, "image/jpeg");
        let id = f.payload_id;
        let mut s = SendSession::new(vec![f]);
        s.start();
        feed(&mut s, from_phone(typed(T::PairedKeyEncryption)));
        let a = feed(&mut s, from_phone(typed(T::PairedKeyResult)));
        let Action::Send(frame) = &a[0] else { panic!() };
        let body = frame.v1.clone().unwrap().payload_transfer.unwrap().payload_chunk.unwrap().body.unwrap();
        let intro = sh::Frame::decode(body.as_slice()).unwrap().v1.unwrap().introduction.unwrap();
        let fm = &intro.file_metadata[0];
        assert_eq!((fm.name.as_deref(), fm.size, fm.payload_id), (Some("pic.jpg"), Some(1234), Some(id)));
        assert_eq!(fm.r#type, Some(sh::file_metadata::Type::Image as i32));
    }

    #[test]
    fn decline_and_early_disconnect() {
        use sh::v1_frame::FrameType as T;
        let mut s = SendSession::new(vec![OutgoingFile::new("a", 1, "application/octet-stream")]);
        s.start();
        feed(&mut s, from_phone(typed(T::PairedKeyEncryption)));
        feed(&mut s, from_phone(typed(T::PairedKeyResult)));
        let a = feed(&mut s, from_phone(respond(sh::connection_response_frame::Status::NotEnoughSpace)));
        assert_eq!(a, vec![Action::Declined(sh::connection_response_frame::Status::NotEnoughSpace), Action::Disconnect]);

        let mut s = SendSession::new(vec![]);
        s.start();
        assert_eq!(s.on_frame(&payload::disconnection()), vec![Action::Cancelled]);
    }

    #[test]
    fn mime_guess() {
        assert_eq!(mime_for("x.JPG"), "image/jpeg");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }
}
