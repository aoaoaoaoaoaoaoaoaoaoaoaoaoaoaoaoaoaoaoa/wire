use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs, io,
    os::unix::{
        fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
        net::UnixStream as StdUnixStream,
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

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
use uuid::Uuid;

use crate::{
    api::{Mattermost, Post, WireError},
    appserver::{self, AdvisorySource, DeliveryPermit, HumanMessage, Injection, PeerMessage},
};

const RELAY_SOCKET: &str = "wire/relay.sock";
const MAX_LOCAL_FRAME: u64 = 128 * 1024;
const LOCAL_TIMEOUT: Duration = Duration::from_secs(10);
const RESERVATION_TIMEOUT: Duration = Duration::from_mins(5);
const COALESCE_FOR: Duration = Duration::from_millis(750);
const MAX_LOCAL_CLIENTS: usize = 64;
const MAX_BATCH_MESSAGES: usize = 8;
const RECENT_POSTS: usize = 1_024;

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

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalRequest {
    Reserve {
        target: Uuid,
    },
    Enqueue {
        target: Uuid,
        post_id: String,
        sender_session: Uuid,
        sender: String,
        body: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalResponse {
    Ready,
    Queued,
    Error { message: String },
}

pub(crate) struct Reservation {
    target: Uuid,
    stream: BufStream<UnixStream>,
}

impl Reservation {
    pub(crate) async fn open(target: Uuid) -> Result<Self, RelayError> {
        let stream = timeout(LOCAL_TIMEOUT, UnixStream::connect(socket_path()?))
            .await
            .map_err(|_| RelayError::Protocol("relay connection timed out".to_owned()))??;
        let mut stream = BufStream::new(stream);
        write_frame(&mut stream, &LocalRequest::Reserve { target }).await?;
        let response = timeout(LOCAL_TIMEOUT, read_frame::<LocalResponse, _>(&mut stream))
            .await
            .map_err(|_| RelayError::Protocol("relay preflight timed out".to_owned()))??;
        match response {
            LocalResponse::Ready => Ok(Self { target, stream }),
            LocalResponse::Error { message } => Err(RelayError::Protocol(message)),
            LocalResponse::Queued => Err(RelayError::Protocol(
                "relay returned an impossible preflight response".to_owned(),
            )),
        }
    }

    pub(crate) async fn enqueue(
        mut self,
        post_id: String,
        sender_session: Uuid,
        sender: String,
        body: String,
    ) -> Result<(), RelayError> {
        write_frame(
            &mut self.stream,
            &LocalRequest::Enqueue {
                target: self.target,
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
            LocalResponse::Ready => Err(RelayError::Protocol(
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
    target: Uuid,
    permit: DeliveryPermit,
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
        self.target == other.target && self.permit == other.permit && self.kind() == other.kind()
    }
}

pub(crate) async fn serve(api: Mattermost) -> Result<(), RelayError> {
    let (listener, _socket) = bind_socket()?;
    let (deliveries, receiver) = mpsc::channel(256);
    let peer = serve_local(api.clone(), listener, deliveries.clone());
    let mattermost = serve_mattermost(api, deliveries);
    let dispatch = dispatch(receiver);
    tokio::select! {
        result = peer => result,
        result = mattermost => result,
        () = dispatch => Err(RelayError::Protocol("delivery dispatcher stopped".to_owned())),
    }
}

async fn serve_local(
    api: Mattermost,
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
        let api = api.clone();
        let deliveries = deliveries.clone();
        let _task = tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = handle_local(&api, stream, deliveries).await {
                eprintln!("wire relay: {error}");
            }
        });
    }
}

async fn handle_local(
    api: &Mattermost,
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
    let established = match api.established_sessions().await {
        Ok(sessions) => sessions.contains(&target),
        Err(error) => {
            return write_protocol_error(
                &mut stream,
                &format!("Wire identity discovery failed: {error}"),
            )
            .await;
        }
    };
    let permit = match appserver::admit_peer(target, established).await {
        Ok(Some(permit)) => permit,
        Ok(None) => {
            return write_protocol_error(&mut stream, "that Codex session cannot accept Wire")
                .await;
        }
        Err(error) => {
            return write_protocol_error(
                &mut stream,
                &format!("shared Codex app server is unavailable: {error}"),
            )
            .await;
        }
    };
    write_frame(&mut stream, &LocalResponse::Ready).await?;
    let request = timeout(
        RESERVATION_TIMEOUT,
        read_frame::<LocalRequest, _>(&mut stream),
    )
    .await
    .map_err(|_| RelayError::Protocol("reservation expired".to_owned()))??;
    let LocalRequest::Enqueue {
        target: submitted_target,
        post_id,
        sender_session,
        sender,
        body,
    } = request
    else {
        return write_protocol_error(&mut stream, "reservation already exists").await;
    };
    if submitted_target != target {
        return write_protocol_error(&mut stream, "reservation identity changed").await;
    }
    deliveries
        .send(Delivery {
            target,
            permit,
            payload: Payload::Peer(PeerMessage {
                post_id,
                sender_session,
                sender,
                source: AdvisorySource::Direct,
                body,
            }),
        })
        .await
        .map_err(|_| RelayError::Protocol("delivery dispatcher is unavailable".to_owned()))?;
    write_frame(&mut stream, &LocalResponse::Queued).await
}

async fn serve_mattermost(
    api: Mattermost,
    deliveries: mpsc::Sender<Delivery>,
) -> Result<(), RelayError> {
    let operator = api.operator().await?;
    let mut seen = SeenPosts::default();
    let mut delay = Duration::from_secs(1);
    loop {
        if let Err(error) = mattermost_connection(&api, &operator.id, &deliveries, &mut seen).await
        {
            eprintln!("wire relay: Mattermost intake disconnected: {error}");
        }
        sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn mattermost_connection(
    api: &Mattermost,
    operator_id: &str,
    deliveries: &mpsc::Sender<Delivery>,
    seen: &mut SeenPosts,
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
                admit_event(api, operator_id, deliveries, seen, &event).await?;
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

async fn admit_event(
    api: &Mattermost,
    operator_id: &str,
    deliveries: &mpsc::Sender<Delivery>,
    seen: &mut SeenPosts,
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
    if post.message.is_empty() || !seen.admit(&post.id) {
        return Ok(());
    }
    if post.user_id == operator_id {
        return admit_human_post(api, operator_id, deliveries, post).await;
    }
    if !api.channel_broadcast_enabled() {
        return Ok(());
    }
    let Some(audience) = api.channel_audience(&post).await? else {
        return Ok(());
    };
    let established = audience
        .recipients
        .iter()
        .filter(|recipient| recipient.established)
        .map(|recipient| recipient.session)
        .collect();
    let targets = match appserver::delivery_targets(&established).await {
        Ok(targets) => targets
            .into_iter()
            .map(|target| (target.id, target.permit))
            .collect::<HashMap<_, _>>(),
        Err(error) => {
            eprintln!("wire relay: dropped channel post {}: {error}", post.id);
            return Ok(());
        }
    };
    let channel = format!(
        "{}:{}",
        audience.channel.team.name, audience.channel.channel.display_name
    );
    for recipient in audience.recipients {
        let Some(permit) = targets.get(&recipient.session).copied() else {
            continue;
        };
        let delivery = Delivery {
            target: recipient.session,
            permit,
            payload: Payload::Peer(PeerMessage {
                post_id: post.id.clone(),
                sender_session: audience.sender.session,
                sender: audience.sender.username.clone(),
                source: AdvisorySource::Channel {
                    channel: channel.clone(),
                },
                body: post.message.clone(),
            }),
        };
        if let Err(error) = deliveries.try_send(delivery) {
            eprintln!(
                "wire relay: dropped channel delivery to {}: {error}",
                recipient.session
            );
        }
    }
    Ok(())
}

async fn admit_human_post(
    api: &Mattermost,
    operator_id: &str,
    deliveries: &mpsc::Sender<Delivery>,
    post: Post,
) -> Result<(), RelayError> {
    let Some(target) = api.direct_session(&post.channel_id, operator_id).await? else {
        // Human posts in ordinary channels are deliberately inert.
        return Ok(());
    };
    let established = api.established_sessions().await?.contains(&target);
    let permit = match appserver::admit_human(target, established).await {
        Ok(Some(permit)) => permit,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("wire relay: dropped human delivery to {target}: {error}");
            return Ok(());
        }
    };
    let _accepted = deliveries
        .send(Delivery {
            target,
            permit,
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
                Ok(Some(delivery))
                    if batch.len() < MAX_BATCH_MESSAGES && batch[0].shares_batch(&delivery) =>
                {
                    batch.push(delivery);
                }
                Ok(Some(delivery)) => waiting.push_back(delivery),
                Ok(None) | Err(_) => break,
            }
        }
        deliver(batch).await;
    }
}

#[derive(Default)]
struct SeenPosts {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenPosts {
    fn admit(&mut self, post: &str) -> bool {
        if !self.ids.insert(post.to_owned()) {
            return false;
        }
        self.order.push_back(post.to_owned());
        if self.order.len() > RECENT_POSTS
            && let Some(evicted) = self.order.pop_front()
        {
            let _removed = self.ids.remove(&evicted);
        }
        true
    }
}

async fn deliver(batch: Vec<Delivery>) {
    let target = batch[0].target;
    let permit = batch[0].permit;
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
    if let Err(error) = appserver::inject(target, injection, permit).await {
        eprintln!("wire relay: dropped delivery to {target}: {error}");
    }
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
