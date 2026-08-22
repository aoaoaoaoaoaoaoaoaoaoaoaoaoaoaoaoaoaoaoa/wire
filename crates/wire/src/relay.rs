use std::{
    collections::VecDeque,
    env, fs, io,
    os::unix::{
        fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
        net::UnixStream as StdUnixStream,
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use codex_census::{Census, ProcessKey, SessionId};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufStream},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, mpsc},
    time::{Instant, sleep, timeout, timeout_at},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest as _,
        http::{HeaderValue, header::AUTHORIZATION},
    },
};

use crate::{
    api::{Mattermost, Post, WireError},
    appserver::{self, HumanMessage, Injection, PeerMessage},
};

const RELAY_SOCKET: &str = "wire/relay.sock";
const MAX_LOCAL_FRAME: u64 = 128 * 1024;
const LOCAL_TIMEOUT: Duration = Duration::from_secs(10);
const RESERVATION_TIMEOUT: Duration = Duration::from_mins(5);
const COALESCE_FOR: Duration = Duration::from_millis(750);
const MAX_LOCAL_CLIENTS: usize = 64;

#[derive(Debug, Error)]
pub(crate) enum RelayError {
    #[error("relay I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("relay protocol failed: {0}")]
    Protocol(String),
    #[error("relay JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Mattermost(#[from] WireError),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct SeatToken {
    pid: u32,
    start_ticks: u64,
}

impl From<ProcessKey> for SeatToken {
    fn from(process: ProcessKey) -> Self {
        Self {
            pid: process.pid,
            start_ticks: process.start_ticks(),
        }
    }
}

impl From<SeatToken> for ProcessKey {
    fn from(seat: SeatToken) -> Self {
        Self::from_parts(seat.pid, seat.start_ticks)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalRequest {
    Reserve {
        target: SessionId,
    },
    Enqueue {
        target: SessionId,
        seat: SeatToken,
        post_id: String,
        sender_session: SessionId,
        sender: String,
        body: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalResponse {
    Ready { seat: SeatToken },
    Queued,
    Error { message: String },
}

pub(crate) struct Reservation {
    target: SessionId,
    seat: SeatToken,
    stream: BufStream<UnixStream>,
}

impl Reservation {
    pub(crate) async fn open(target: SessionId) -> Result<Self, RelayError> {
        let stream = timeout(LOCAL_TIMEOUT, UnixStream::connect(socket_path()?))
            .await
            .map_err(|_| RelayError::Protocol("relay connection timed out".to_owned()))??;
        let mut stream = BufStream::new(stream);
        write_frame(&mut stream, &LocalRequest::Reserve { target }).await?;
        let response = timeout(LOCAL_TIMEOUT, read_frame::<LocalResponse, _>(&mut stream))
            .await
            .map_err(|_| RelayError::Protocol("relay preflight timed out".to_owned()))??;
        match response {
            LocalResponse::Ready { seat } => Ok(Self {
                target,
                seat,
                stream,
            }),
            LocalResponse::Error { message } => Err(RelayError::Protocol(message)),
            LocalResponse::Queued => Err(RelayError::Protocol(
                "relay returned an impossible preflight response".to_owned(),
            )),
        }
    }

    pub(crate) async fn enqueue(
        mut self,
        post_id: String,
        sender_session: SessionId,
        sender: String,
        body: String,
    ) -> Result<(), RelayError> {
        write_frame(
            &mut self.stream,
            &LocalRequest::Enqueue {
                target: self.target,
                seat: self.seat,
                post_id,
                sender_session,
                sender,
                body,
            },
        )
        .await?;
        let response = timeout(
            LOCAL_TIMEOUT,
            read_frame::<LocalResponse, _>(&mut self.stream),
        )
        .await
        .map_err(|_| RelayError::Protocol("relay enqueue timed out".to_owned()))??;
        match response {
            LocalResponse::Queued => Ok(()),
            LocalResponse::Error { message } => Err(RelayError::Protocol(message)),
            LocalResponse::Ready { .. } => Err(RelayError::Protocol(
                "relay returned an impossible enqueue response".to_owned(),
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Human,
    Peer,
}

#[derive(Clone, Debug)]
enum Payload {
    Human(HumanMessage),
    Peer(PeerMessage),
}

#[derive(Clone, Debug)]
struct Delivery {
    target: SessionId,
    process: ProcessKey,
    payload: Payload,
}

impl Delivery {
    fn kind(&self) -> Kind {
        match self.payload {
            Payload::Human(_) => Kind::Human,
            Payload::Peer(_) => Kind::Peer,
        }
    }

    fn shares_batch(&self, other: &Self) -> bool {
        self.target == other.target && self.process == other.process && self.kind() == other.kind()
    }
}

pub(crate) async fn serve(api: Mattermost) -> Result<(), RelayError> {
    let (listener, _socket) = bind_socket()?;
    let (deliveries, receiver) = mpsc::channel(256);
    let peer = serve_local(listener, deliveries.clone());
    let human = serve_human(api, deliveries);
    let dispatch = dispatch(receiver);
    tokio::select! {
        result = peer => result,
        result = human => result,
        () = dispatch => Err(RelayError::Protocol("delivery dispatcher stopped".to_owned())),
    }
}

async fn serve_local(
    listener: UnixListener,
    deliveries: mpsc::Sender<Delivery>,
) -> Result<(), RelayError> {
    let permits = Arc::new(Semaphore::new(MAX_LOCAL_CLIENTS));
    loop {
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|error| RelayError::Protocol(error.to_string()))?;
        let (stream, _address) = listener.accept().await?;
        let deliveries = deliveries.clone();
        let _task = tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_local(stream, deliveries).await {
                eprintln!("wire relay: {error}");
            }
        });
    }
}

async fn handle_local(
    stream: UnixStream,
    deliveries: mpsc::Sender<Delivery>,
) -> Result<(), RelayError> {
    let mut stream = BufStream::new(stream);
    let target = match read_frame::<LocalRequest, _>(&mut stream).await? {
        LocalRequest::Reserve { target } => target,
        LocalRequest::Enqueue { .. } => {
            return write_protocol_error(&mut stream, "reservation required").await;
        }
    };
    let Some(process) = live_process(target)? else {
        return write_protocol_error(&mut stream, "that Codex session is not live").await;
    };
    match appserver::loaded_sessions().await {
        Ok(sessions) if sessions.contains(&target) => {}
        Ok(_) => {
            return write_protocol_error(
                &mut stream,
                "that Codex session is not attached to the shared app server",
            )
            .await;
        }
        Err(error) => {
            return write_protocol_error(
                &mut stream,
                &format!("shared Codex app server is unavailable: {error}"),
            )
            .await;
        }
    }
    let seat = SeatToken::from(process);
    write_frame(&mut stream, &LocalResponse::Ready { seat }).await?;
    let request = timeout(
        RESERVATION_TIMEOUT,
        read_frame::<LocalRequest, _>(&mut stream),
    )
    .await
    .map_err(|_| RelayError::Protocol("reservation expired".to_owned()))??;
    let LocalRequest::Enqueue {
        target: submitted_target,
        seat: submitted_seat,
        post_id,
        sender_session,
        sender,
        body,
    } = request
    else {
        return write_protocol_error(&mut stream, "reservation already exists").await;
    };
    if submitted_target != target || submitted_seat != seat {
        return write_protocol_error(&mut stream, "reservation identity changed").await;
    }
    if live_process(target)? != Some(process) {
        return write_protocol_error(&mut stream, "target Codex process changed").await;
    }
    deliveries
        .send(Delivery {
            target,
            process,
            payload: Payload::Peer(PeerMessage {
                post_id,
                sender_session,
                sender,
                body,
            }),
        })
        .await
        .map_err(|_| RelayError::Protocol("delivery dispatcher is unavailable".to_owned()))?;
    write_frame(&mut stream, &LocalResponse::Queued).await
}

async fn serve_human(
    api: Mattermost,
    deliveries: mpsc::Sender<Delivery>,
) -> Result<(), RelayError> {
    let operator = api.operator().await?;
    let mut delay = Duration::from_secs(1);
    loop {
        if let Err(error) = human_connection(&api, &operator.id, &deliveries).await {
            eprintln!("wire relay: Mattermost intake disconnected: {error}");
        }
        sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn human_connection(
    api: &Mattermost,
    operator_id: &str,
    deliveries: &mpsc::Sender<Delivery>,
) -> Result<(), RelayError> {
    let mut request = api
        .websocket_url()?
        .into_client_request()
        .map_err(|error| RelayError::Protocol(error.to_string()))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", api.admin_token()))
        .map_err(|error| RelayError::Protocol(error.to_string()))?;
    let _old = request.headers_mut().insert(AUTHORIZATION, authorization);
    let (mut socket, _response) = connect_async(request)
        .await
        .map_err(|error| RelayError::Protocol(error.to_string()))?;
    let mut armed = false;
    while let Some(frame) = socket.next().await {
        match frame.map_err(|error| RelayError::Protocol(error.to_string()))? {
            Message::Text(text) => {
                let event: Value = serde_json::from_str(&text)?;
                if event.get("event").and_then(Value::as_str) == Some("hello") {
                    armed = true;
                    continue;
                }
                if !armed || event.get("event").and_then(Value::as_str) != Some("posted") {
                    continue;
                }
                admit_human_event(api, operator_id, deliveries, &event).await?;
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .await
                .map_err(|error| RelayError::Protocol(error.to_string()))?,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Err(RelayError::Protocol(
        "Mattermost WebSocket ended".to_owned(),
    ))
}

async fn admit_human_event(
    api: &Mattermost,
    operator_id: &str,
    deliveries: &mpsc::Sender<Delivery>,
    event: &Value,
) -> Result<(), RelayError> {
    let Some(post_json) = event
        .get("data")
        .and_then(|data| data.get("post"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let post: Post = serde_json::from_str(post_json)?;
    if post.user_id != operator_id || post.message.is_empty() {
        return Ok(());
    }
    let Some(target) = api.direct_session(&post.channel_id, operator_id).await? else {
        return Ok(());
    };
    let Some(process) = live_process(target)? else {
        return Ok(());
    };
    let _accepted = deliveries
        .send(Delivery {
            target,
            process,
            payload: Payload::Human(HumanMessage {
                post_id: post.id,
                body: post.message,
            }),
        })
        .await;
    Ok(())
}

async fn dispatch(mut receiver: mpsc::Receiver<Delivery>) {
    let mut waiting = VecDeque::new();
    loop {
        let first = if let Some(delivery) = waiting.pop_front() {
            Some(delivery)
        } else {
            receiver.recv().await
        };
        let Some(first) = first else {
            return;
        };
        let deadline = Instant::now() + COALESCE_FOR;
        let mut batch = vec![first];
        loop {
            match timeout_at(deadline, receiver.recv()).await {
                Ok(Some(delivery)) if batch[0].shares_batch(&delivery) => batch.push(delivery),
                Ok(Some(delivery)) => waiting.push_back(delivery),
                Ok(None) | Err(_) => break,
            }
        }
        deliver(batch).await;
    }
}

async fn deliver(batch: Vec<Delivery>) {
    let target = batch[0].target;
    let process = batch[0].process;
    let injection = match batch[0].kind() {
        Kind::Human => Injection::Human(
            batch
                .into_iter()
                .filter_map(|delivery| match delivery.payload {
                    Payload::Human(message) => Some(message),
                    Payload::Peer(_) => None,
                })
                .collect(),
        ),
        Kind::Peer => Injection::Peer(
            batch
                .into_iter()
                .filter_map(|delivery| match delivery.payload {
                    Payload::Peer(message) => Some(message),
                    Payload::Human(_) => None,
                })
                .collect(),
        ),
    };
    if let Err(error) = appserver::inject(target, process, injection).await {
        eprintln!("wire relay: dropped delivery to {target}: {error}");
    }
}

fn live_process(target: SessionId) -> Result<Option<ProcessKey>, RelayError> {
    let census = Census::scan().map_err(|error| RelayError::Protocol(error.to_string()))?;
    Ok(census.seat(&target).map(|seat| seat.process))
}

fn bind_socket() -> Result<(UnixListener, SocketGuard), RelayError> {
    let path = socket_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| RelayError::Protocol("relay socket has no parent".to_owned()))?;
    let mut builder = fs::DirBuilder::new();
    let _builder = builder.recursive(true).mode(0o700);
    builder.create(parent)?;
    if path.exists() {
        match StdUnixStream::connect(&path) {
            Ok(_) => {
                return Err(RelayError::Protocol(
                    "another Wire relay is already listening".to_owned(),
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let metadata = fs::metadata(&path)?;
    Ok((
        listener,
        SocketGuard {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _removed = fs::remove_file(&self.path);
        }
    }
}

fn socket_path() -> Result<PathBuf, RelayError> {
    env::var_os("WIRE_RELAY_SOCKET")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_RUNTIME_DIR")
                .map(|directory| PathBuf::from(directory).join(RELAY_SOCKET))
        })
        .ok_or_else(|| {
            RelayError::Protocol("XDG_RUNTIME_DIR or WIRE_RELAY_SOCKET is required".to_owned())
        })
}

async fn write_protocol_error<T>(stream: &mut BufStream<T>, message: &str) -> Result<(), RelayError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_frame(
        stream,
        &LocalResponse::Error {
            message: message.to_owned(),
        },
    )
    .await
}

async fn write_frame<T, V>(stream: &mut BufStream<T>, value: &V) -> Result<(), RelayError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    V: Serialize,
{
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_LOCAL_FRAME {
        return Err(RelayError::Protocol("local frame is too large".to_owned()));
    }
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame<V, T>(stream: &mut BufStream<T>) -> Result<V, RelayError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    V: for<'de> Deserialize<'de>,
{
    let mut bytes = Vec::new();
    loop {
        let available = stream.fill_buf().await?;
        if available.is_empty() {
            return Err(RelayError::Protocol("local connection ended".to_owned()));
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len() as u64 + consumed as u64 > MAX_LOCAL_FRAME {
            return Err(RelayError::Protocol("local frame is too large".to_owned()));
        }
        bytes.extend_from_slice(&available[..consumed]);
        let complete = bytes.last() == Some(&b'\n');
        stream.consume(consumed);
        if complete {
            break;
        }
    }
    Ok(serde_json::from_slice(&bytes[..bytes.len() - 1])?)
}
