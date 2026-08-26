use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    path::PathBuf,
    process::{Command as StdCommand, Stdio},
    sync::Arc,
    time::Duration,
};

use reqwest::{Client, RequestBuilder, StatusCode};
use rmcp::model::RequestMetaObject;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{Mutex, OnceCell},
};
use uuid::Uuid;

use crate::config::ChannelBroadcast;

const DEFAULT_URL: &str = "http://127.0.0.1:8065/api/v4";
const ERROR_BODY_LIMIT: usize = 2_000;
const ADMIN_ACCOUNT: &str = "admin";
const SESSION_ACCOUNT: &str = "session";
const OPERATOR_USERNAME: &str = "main";
const SUBSCRIPTION_CATEGORY: &str = "wire_subscription";
const SESSION_PROPERTY: &str = "wire_session";

#[derive(Clone)]
pub(crate) struct Mattermost {
    base: String,
    client: Client,
    admin_token: String,
    identities: Arc<IdentityRegistry>,
    channel_broadcast: ChannelBroadcast,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) username: String,
    #[serde(default)]
    props: Option<HashMap<String, String>>,
    #[serde(default)]
    delete_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Team {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) display_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Channel {
    pub(crate) id: String,
    pub(crate) team_id: String,
    pub(crate) name: String,
    pub(crate) display_name: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) delete_at: i64,
    #[serde(default)]
    pub(crate) total_msg_count: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct BoundChannel {
    pub(crate) team: Team,
    pub(crate) channel: Channel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Post {
    pub(crate) id: String,
    pub(crate) create_at: i64,
    pub(crate) update_at: i64,
    pub(crate) user_id: String,
    pub(crate) channel_id: String,
    pub(crate) root_id: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Timeline {
    pub(crate) channel: BoundChannel,
    pub(crate) posts: Vec<Post>,
    pub(crate) users: HashMap<String, User>,
}

#[derive(Clone, Debug)]
pub(crate) struct CreatedPost {
    pub(crate) channel: BoundChannel,
    pub(crate) post: Post,
    pub(crate) sender: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CreatedDirectMessage {
    pub(crate) post: Post,
    pub(crate) sender: String,
    pub(crate) recipient: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSubscription {
    pub(crate) channel: BoundChannel,
    pub(crate) state: SubscriptionState,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SubscriptionState {
    Subscribed,
    Unsubscribed,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelAudience {
    pub(crate) channel: BoundChannel,
    pub(crate) sender: WireBot,
    pub(crate) recipients: Vec<WireBot>,
}

#[derive(Clone, Debug)]
pub(crate) struct WireBot {
    pub(crate) session: Uuid,
    pub(crate) username: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentIdentity {
    pub(crate) session: Uuid,
    pub(crate) name: String,
    pub(crate) biography: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Session {
    id: Uuid,
    title: Option<String>,
}

#[derive(Debug)]
struct Identity {
    user_id: String,
    token: String,
    profile: Mutex<Profile>,
}

type IdentitySlot = OnceCell<Arc<Identity>>;

#[derive(Debug, Default)]
struct IdentityRegistry {
    slots: Mutex<HashMap<Uuid, Arc<IdentitySlot>>>,
}

impl IdentityRegistry {
    async fn slot(&self, session: Uuid) -> Arc<IdentitySlot> {
        let mut slots = self.slots.lock().await;
        Arc::clone(
            slots
                .entry(session)
                .or_insert_with(|| Arc::new(OnceCell::new())),
        )
    }
}

#[derive(Clone, Debug)]
struct Profile {
    title: Option<String>,
    username: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Bot {
    user_id: String,
    username: String,
    display_name: String,
    description: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProvisionalUser {
    id: String,
    email: String,
    #[serde(default)]
    props: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize)]
struct ChannelMember {
    user_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Preference {
    user_id: String,
    category: String,
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct SessionIndexRow {
    id: Uuid,
    thread_name: String,
}

#[derive(Debug, Deserialize)]
struct AccessToken {
    id: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct PostList {
    order: Vec<String>,
    posts: HashMap<String, Post>,
}

#[derive(Debug, Error)]
pub(crate) enum WireError {
    #[error("invalid configuration: {0}")]
    Configuration(String),
    #[error("invalid input: {0}")]
    Input(String),
    #[error("Mattermost request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Mattermost returned HTTP {status}: {body}")]
    Response { status: StatusCode, body: String },
    #[error("Mattermost response was invalid: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("local credential registry failed: {0}")]
    Keyring(String),
}

impl Session {
    pub(crate) fn from_request(meta: &RequestMetaObject) -> Result<Self, WireError> {
        if let Some(value) = meta.get("threadId") {
            let raw = value.as_str().ok_or_else(|| {
                WireError::Configuration("request threadId metadata is not a string".to_owned())
            })?;
            let id = Uuid::parse_str(raw).map_err(|error| {
                WireError::Configuration(format!(
                    "request threadId metadata is not a UUID: {error}"
                ))
            })?;
            return Ok(Self::for_id(id));
        }
        Self::load()?.ok_or_else(|| {
            WireError::Configuration(
                "writes require Codex threadId request metadata, CODEX_THREAD_ID, or WIRE_SESSION_ID"
                    .to_owned(),
            )
        })
    }

    pub(crate) fn for_id(id: Uuid) -> Self {
        Self {
            id,
            title: indexed_title(id),
        }
    }

    pub(crate) const fn id(&self) -> Uuid {
        self.id
    }

    fn load() -> Result<Option<Self>, WireError> {
        let raw = ["CODEX_THREAD_ID", "WIRE_SESSION_ID"]
            .into_iter()
            .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()));
        raw.map(|raw| {
            let id = Uuid::parse_str(raw.trim()).map_err(|error| {
                WireError::Configuration(format!("session identity is not a UUID: {error}"))
            })?;
            let title = ["CODEX_THREAD_NAME", "WIRE_SESSION_NAME"]
                .into_iter()
                .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
                .or_else(|| indexed_title(id));
            Ok(Self { id, title })
        })
        .transpose()
    }

    pub(crate) fn name(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.display_name())
    }

    fn provisional_email(&self) -> String {
        format!("wire-{}@localhost", self.id.simple())
    }

    fn display_name(&self) -> String {
        let compact = self.id.simple().to_string();
        self.title.as_ref().map_or_else(
            || format!("Codex {}", &compact[..8]),
            |title| {
                let title = truncate_chars(title.trim(), 55);
                format!("{title} · {}", &compact[..4])
            },
        )
    }

    fn username_candidates(&self) -> Vec<String> {
        let compact = self.id.simple().to_string();
        let mut candidates = self.title.as_deref().map_or_else(Vec::new, |title| {
            let slug = session_slug(title);
            if slug.is_empty() {
                Vec::new()
            } else {
                vec![
                    format!("codex-{}", truncate_chars(&slug, 16)),
                    format!("codex-{}-{}", truncate_chars(&slug, 11), &compact[..4]),
                    format!("codex-{}-{}", truncate_chars(&slug, 7), &compact[..8]),
                ]
            }
        });
        candidates.push(self.legacy_username());
        candidates.dedup();
        candidates
    }

    fn legacy_username(&self) -> String {
        let compact = self.id.simple().to_string();
        format!("codex-{}", &compact[..16])
    }
}

impl Mattermost {
    pub(crate) fn load() -> Result<Self, WireError> {
        let channel_broadcast = ChannelBroadcast::load().map_err(WireError::Configuration)?;
        let base = env::var("WIRE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
        let url = reqwest::Url::parse(&base)
            .map_err(|error| WireError::Configuration(format!("WIRE_URL: {error}")))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.cannot_be_a_base()
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(WireError::Configuration(
                "WIRE_URL must be an HTTP(S) API root without credentials, query, or fragment"
                    .to_owned(),
            ));
        }
        let admin_token = env::var("WIRE_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map_or_else(
                || {
                    lookup_secret_sync(ADMIN_ACCOUNT, None)?.ok_or_else(|| {
                        WireError::Keyring("Mattermost administrator token is absent".to_owned())
                    })
                },
                Ok,
            )?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_mins(5))
            .user_agent(concat!("wire/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client,
            admin_token,
            identities: Arc::new(IdentityRegistry::default()),
            channel_broadcast,
        })
    }

    pub(crate) const fn channel_broadcast_enabled(&self) -> bool {
        self.channel_broadcast.enabled()
    }

    pub(crate) async fn channels(&self) -> Result<Vec<BoundChannel>, WireError> {
        let teams: Vec<Team> = self.get("/users/me/teams").await?;
        let mut channels = Vec::new();
        for team in teams {
            let path = format!("/users/me/teams/{}/channels", team.id);
            let team_channels: Vec<Channel> = self.get(&path).await?;
            channels.extend(
                team_channels
                    .into_iter()
                    .filter(|channel| {
                        channel.delete_at == 0 && matches!(channel.kind.as_str(), "O" | "P")
                    })
                    .map(|channel| BoundChannel {
                        team: team.clone(),
                        channel,
                    }),
            );
        }
        channels.sort_by(|left, right| {
            (&left.team.name, &left.channel.name).cmp(&(&right.team.name, &right.channel.name))
        });
        Ok(channels)
    }

    pub(crate) fn websocket_url(&self) -> Result<String, WireError> {
        let mut url = reqwest::Url::parse(&self.base)
            .map_err(|error| WireError::Configuration(format!("WIRE_URL: {error}")))?;
        let websocket_scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            _ => {
                return Err(WireError::Configuration(
                    "WIRE_URL does not project to a WebSocket URL".to_owned(),
                ));
            }
        };
        url.set_scheme(websocket_scheme).map_err(|()| {
            WireError::Configuration("WIRE_URL does not admit a WebSocket scheme".to_owned())
        })?;
        let path = format!("{}/websocket", url.path().trim_end_matches('/'));
        url.set_path(&path);
        Ok(url.to_string())
    }

    pub(crate) fn admin_token(&self) -> &str {
        &self.admin_token
    }

    pub(crate) async fn operator(&self) -> Result<User, WireError> {
        self.get(&format!("/users/username/{OPERATOR_USERNAME}"))
            .await
    }

    pub(crate) async fn direct_session(
        &self,
        channel_id: &str,
        operator_id: &str,
    ) -> Result<Option<Uuid>, WireError> {
        let channel: Channel = self.get(&format!("/channels/{channel_id}")).await?;
        if channel.kind != "D" {
            return Ok(None);
        }
        let members: Vec<ChannelMember> =
            self.get(&format!("/channels/{channel_id}/members")).await?;
        let mut recipients = members
            .into_iter()
            .filter(|member| member.user_id != operator_id);
        let Some(recipient) = recipients.next() else {
            return Ok(None);
        };
        if recipients.next().is_some() {
            return Ok(None);
        }
        let user: User = self.get(&format!("/users/{}", recipient.user_id)).await?;
        if user.delete_at != 0 {
            return Ok(None);
        }
        if let Some(session) = session_from_properties(user.props.as_ref()) {
            return Ok(Some(session));
        }
        let bot = self
            .get_optional::<Bot>(&format!("/bots/{}", recipient.user_id))
            .await?;
        Ok(bot.and_then(|bot| bot.legacy_session()))
    }

    pub(crate) async fn resolve_channel(&self, selector: &str) -> Result<BoundChannel, WireError> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(WireError::Input("channel is empty".to_owned()));
        }
        let channels = self.channels().await?;
        if let Some(channel) = channels
            .iter()
            .find(|bound| bound.channel.id == selector)
            .cloned()
        {
            return Ok(channel);
        }
        let (team_selector, channel_selector) = selector
            .split_once(':')
            .map_or((None, selector), |(team, channel)| (Some(team), channel));
        let mut matches = channels.into_iter().filter(|bound| {
            let channel_matches = name_matches(
                channel_selector,
                &bound.channel.name,
                &bound.channel.display_name,
            );
            let team_matches = team_selector
                .is_none_or(|team| name_matches(team, &bound.team.name, &bound.team.display_name));
            channel_matches && team_matches
        });
        let first = matches.next().ok_or_else(|| {
            WireError::Input(format!(
                "channel `{selector}` is not visible to the administrator"
            ))
        })?;
        if matches.next().is_some() {
            return Err(WireError::Input(format!(
                "channel `{selector}` is ambiguous; use team:channel"
            )));
        }
        Ok(first)
    }

    pub(crate) async fn read(
        &self,
        selector: &str,
        thread: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Timeline, WireError> {
        let channel = self.resolve_channel(selector).await?;
        let path = if let Some(root) = thread {
            format!("/posts/{root}/thread?per_page={limit}")
        } else {
            let before = before.map_or_else(String::new, |id| format!("&before={id}"));
            format!(
                "/channels/{}/posts?page=0&per_page={limit}{before}",
                channel.channel.id
            )
        };
        let list: PostList = self.get(&path).await?;
        let mut posts = list
            .order
            .into_iter()
            .filter_map(|id| list.posts.get(&id).cloned())
            .collect::<Vec<_>>();
        posts.sort_by_key(|post| post.create_at);
        let user_ids = posts
            .iter()
            .map(|post| post.user_id.clone())
            .collect::<HashSet<_>>();
        let users = if user_ids.is_empty() {
            HashMap::new()
        } else {
            let users: Vec<User> = self
                .post_json("/users/ids", &user_ids.into_iter().collect::<Vec<_>>())
                .await?;
            users
                .into_iter()
                .map(|user| (user.id.clone(), user))
                .collect()
        };
        Ok(Timeline {
            channel,
            posts,
            users,
        })
    }

    pub(crate) async fn post(
        &self,
        session: &Session,
        selector: &str,
        message: &str,
        reply_to: Option<&str>,
    ) -> Result<CreatedPost, WireError> {
        let channel = self.resolve_channel(selector).await?;
        let identity = self.identity(session).await?;
        let sender = self.sync_profile(&identity, session).await?;
        let _member_added = self.ensure_membership(&identity, &channel).await?;
        self.set_subscription(
            &identity.user_id,
            &channel.channel.id,
            SubscriptionState::Subscribed,
        )
        .await?;
        let post = self
            .post_json_as(
                &identity.token,
                "/posts",
                &serde_json::json!({
                    "channel_id": channel.channel.id,
                    "message": message,
                    "root_id": reply_to.unwrap_or_default(),
                }),
            )
            .await?;
        Ok(CreatedPost {
            channel,
            post,
            sender,
        })
    }

    pub(crate) async fn subscribe(
        &self,
        session: &Session,
        selector: &str,
    ) -> Result<ChannelSubscription, WireError> {
        let channel = self.resolve_channel(selector).await?;
        let identity = self.identity(session).await?;
        let _sender = self.sync_profile(&identity, session).await?;
        let _member_added = self.ensure_membership(&identity, &channel).await?;
        self.set_subscription(
            &identity.user_id,
            &channel.channel.id,
            SubscriptionState::Subscribed,
        )
        .await?;
        Ok(ChannelSubscription {
            channel,
            state: SubscriptionState::Subscribed,
        })
    }

    pub(crate) async fn unsubscribe(
        &self,
        session: &Session,
        selector: &str,
    ) -> Result<ChannelSubscription, WireError> {
        let channel = self.resolve_channel(selector).await?;
        let Some(bot) = self.find_bot(session).await? else {
            return Ok(ChannelSubscription {
                channel,
                state: SubscriptionState::Unsubscribed,
            });
        };
        self.set_subscription(
            &bot.user_id,
            &channel.channel.id,
            SubscriptionState::Unsubscribed,
        )
        .await?;
        Ok(ChannelSubscription {
            channel,
            state: SubscriptionState::Unsubscribed,
        })
    }

    pub(crate) async fn channel_audience(
        &self,
        post: &Post,
    ) -> Result<Option<ChannelAudience>, WireError> {
        let channel: Channel = self.get(&format!("/channels/{}", post.channel_id)).await?;
        if !matches!(channel.kind.as_str(), "O" | "P") {
            return Ok(None);
        }
        let bots = self.wire_bots().await?;
        let Some(sender) = bots.get(&post.user_id).cloned() else {
            // Human and foreign-bot channel posts are deliberately inert.
            return Ok(None);
        };
        let members: Vec<ChannelMember> = self
            .get(&format!("/channels/{}/members", channel.id))
            .await?;
        let mut recipients = Vec::new();
        for member in members
            .into_iter()
            .filter(|member| member.user_id != post.user_id)
        {
            let Some(bot) = bots.get(&member.user_id) else {
                continue;
            };
            if self
                .subscription(&member.user_id, &channel.id)
                .await?
                .is_some()
            {
                recipients.push(bot.clone());
            }
        }
        let team: Team = self.get(&format!("/teams/{}", channel.team_id)).await?;
        Ok(Some(ChannelAudience {
            channel: BoundChannel { team, channel },
            sender,
            recipients,
        }))
    }

    pub(crate) async fn direct_message(
        &self,
        session: &Session,
        message: &str,
        reply_to: Option<&str>,
    ) -> Result<CreatedDirectMessage, WireError> {
        let identity = self.identity(session).await?;
        let sender = self.sync_profile(&identity, session).await?;
        let recipient: User = self
            .get(&format!("/users/username/{OPERATOR_USERNAME}"))
            .await?;
        let channel: Channel = self
            .post_json_as(
                &identity.token,
                "/channels/direct",
                &[identity.user_id.as_str(), recipient.id.as_str()],
            )
            .await?;
        let post = self
            .post_json_as(
                &identity.token,
                "/posts",
                &serde_json::json!({
                    "channel_id": channel.id,
                    "message": message,
                    "root_id": reply_to.unwrap_or_default(),
                }),
            )
            .await?;
        Ok(CreatedDirectMessage {
            post,
            sender,
            recipient: recipient.username,
        })
    }

    pub(crate) async fn peer_direct_message(
        &self,
        session: &Session,
        recipient_id: Uuid,
        message: &str,
        reply_to: Option<&str>,
    ) -> Result<CreatedDirectMessage, WireError> {
        let identity = self.identity(session).await?;
        let sender = self.sync_profile(&identity, session).await?;
        let recipient = self.ensure_bot(&Session::for_id(recipient_id)).await?;
        let channel: Channel = self
            .post_json_as(
                &identity.token,
                "/channels/direct",
                &[identity.user_id.as_str(), recipient.user_id.as_str()],
            )
            .await?;
        let post = self
            .post_json_as(
                &identity.token,
                "/posts",
                &serde_json::json!({
                    "channel_id": channel.id,
                    "message": message,
                    "root_id": reply_to.unwrap_or_default(),
                }),
            )
            .await?;
        Ok(CreatedDirectMessage {
            post,
            sender,
            recipient: recipient.username,
        })
    }

    pub(crate) async fn agent_identity(
        &self,
        session: &Session,
    ) -> Result<AgentIdentity, WireError> {
        let biography = self.find_bot(session).await?.and_then(Bot::biography);
        Ok(AgentIdentity {
            session: session.id,
            name: session.name(),
            biography,
        })
    }

    pub(crate) async fn publish_biography(
        &self,
        session: &Session,
        biography: &str,
    ) -> Result<AgentIdentity, WireError> {
        let identity = self.identity(session).await?;
        let _username = self.sync_profile(&identity, session).await?;
        let bot: Bot = self.get(&format!("/bots/{}", identity.user_id)).await?;
        let updated: Bot = self
            .put_json(
                &format!("/bots/{}", identity.user_id),
                &serde_json::json!({
                    "username": bot.username,
                    "display_name": session.display_name(),
                    "description": biography,
                }),
            )
            .await?;
        Ok(AgentIdentity {
            session: session.id,
            name: session.name(),
            biography: updated.biography(),
        })
    }

    async fn identity(&self, session: &Session) -> Result<Arc<Identity>, WireError> {
        let slot = self.identities.slot(session.id).await;
        let identity = slot
            .get_or_try_init(|| async {
                let bot = self.ensure_bot(session).await?;
                let token = self.identity_token(session, &bot).await?;
                Ok::<_, WireError>(Arc::new(Identity {
                    user_id: bot.user_id,
                    token,
                    profile: Mutex::new(Profile {
                        title: session.title.clone(),
                        username: bot.username,
                    }),
                }))
            })
            .await?;
        Ok(Arc::clone(identity))
    }

    async fn sync_profile(
        &self,
        identity: &Identity,
        session: &Session,
    ) -> Result<String, WireError> {
        let mut profile = identity.profile.lock().await;
        if profile.title == session.title {
            return Ok(profile.username.clone());
        }
        let bot: Bot = self.get(&format!("/bots/{}", identity.user_id)).await?;
        let bot = self.sync_bot_profile(session, bot).await?;
        *profile = Profile {
            title: session.title.clone(),
            username: bot.username.clone(),
        };
        Ok(bot.username)
    }

    async fn ensure_bot(&self, session: &Session) -> Result<Bot, WireError> {
        if let Some(bot) = self.find_bot(session).await? {
            return self.sync_bot_profile(session, bot).await;
        }
        let email = session.provisional_email();
        for username in session.username_candidates() {
            let provisional = match self
                .get_optional::<ProvisionalUser>(&format!("/users/username/{username}"))
                .await?
            {
                Some(user) => user,
                None => match self
                    .create_provisional_user(session, &username, &email)
                    .await
                {
                    Ok(user) => user,
                    // Another worker may have won the same provisioning race.
                    Err(WireError::Response {
                        status: StatusCode::BAD_REQUEST,
                        ..
                    }) => {
                        let Some(user) = self
                            .get_optional::<ProvisionalUser>(&format!("/users/username/{username}"))
                            .await?
                        else {
                            continue;
                        };
                        user
                    }
                    Err(error) => return Err(error),
                },
            };
            if provisional.email != email
                && session_from_properties(provisional.props.as_ref()) != Some(session.id)
            {
                continue;
            }
            return self.convert_provisional_user(session, &provisional).await;
        }
        Err(WireError::Configuration(
            "no available Mattermost username for this session".to_owned(),
        ))
    }

    async fn create_provisional_user(
        &self,
        session: &Session,
        username: &str,
        email: &str,
    ) -> Result<ProvisionalUser, WireError> {
        self.post_json(
            "/users",
            &serde_json::json!({
                "username": username,
                "email": email,
                "password": format!("wire-{}", Uuid::new_v4().simple()),
                "first_name": session.display_name(),
                "props": {(SESSION_PROPERTY): session.id},
                "email_verified": true,
                "disable_welcome_email": true,
            }),
        )
        .await
    }

    async fn convert_provisional_user(
        &self,
        session: &Session,
        user: &ProvisionalUser,
    ) -> Result<Bot, WireError> {
        if let Some(bot) = self
            .get_optional::<Bot>(&format!("/bots/{}", user.id))
            .await?
        {
            return self.sync_bot_profile(session, bot).await;
        }

        // Mattermost's bot-creation endpoint unconditionally DMs its caller.
        // User conversion produces the same identity without that side effect.
        let converted = self
            .post_json(
                &format!("/users/{}/convert_to_bot", user.id),
                &serde_json::json!({}),
            )
            .await;
        match converted {
            Ok(bot) => self.sync_bot_profile(session, bot).await,
            Err(error) => {
                if let Some(bot) = self
                    .get_optional::<Bot>(&format!("/bots/{}", user.id))
                    .await?
                {
                    self.sync_bot_profile(session, bot).await
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn find_bot(&self, session: &Session) -> Result<Option<Bot>, WireError> {
        let bots = self.bots().await?;
        if bots.is_empty() {
            return Ok(None);
        }
        let user_ids = bots
            .iter()
            .map(|bot| bot.user_id.clone())
            .collect::<Vec<_>>();
        let users: Vec<User> = self.post_json("/users/ids", &user_ids).await?;
        let principals = users
            .into_iter()
            .filter(|user| user.delete_at == 0)
            .map(|user| {
                let session = session_from_properties(user.props.as_ref());
                (user.id, session)
            })
            .collect::<HashMap<_, _>>();
        Ok(bots.into_iter().find(|bot| {
            principals.get(&bot.user_id).is_some_and(|principal| {
                *principal == Some(session.id) || bot.legacy_session() == Some(session.id)
            })
        }))
    }

    async fn wire_bots(&self) -> Result<HashMap<String, WireBot>, WireError> {
        let bots = self.bots().await?;
        if bots.is_empty() {
            return Ok(HashMap::new());
        }
        let user_ids = bots
            .iter()
            .map(|bot| bot.user_id.clone())
            .collect::<Vec<_>>();
        let users: Vec<User> = self.post_json("/users/ids", &user_ids).await?;
        let principals = users
            .into_iter()
            .filter(|user| user.delete_at == 0)
            .map(|user| {
                let session = session_from_properties(user.props.as_ref());
                (user.id, session)
            })
            .collect::<HashMap<_, _>>();
        Ok(bots
            .into_iter()
            .filter_map(|bot| {
                let session = principals
                    .get(&bot.user_id)
                    .copied()?
                    .or_else(|| bot.legacy_session())?;
                Some((
                    bot.user_id.clone(),
                    WireBot {
                        session,
                        username: bot.username,
                    },
                ))
            })
            .collect())
    }

    async fn bots(&self) -> Result<Vec<Bot>, WireError> {
        let mut bots = Vec::new();
        for page in 0.. {
            let page_bots: Vec<Bot> = self.get(&format!("/bots?page={page}&per_page=200")).await?;
            let count = page_bots.len();
            bots.extend(page_bots);
            if count < 200 {
                return Ok(bots);
            }
        }
        unreachable!()
    }

    async fn sync_bot_profile(&self, session: &Session, bot: Bot) -> Result<Bot, WireError> {
        self.bind_bot_user(session, &bot).await?;
        let display_name = session.display_name();
        let candidates = session.username_candidates();
        if bot.display_name == display_name && candidates.contains(&bot.username) {
            return Ok(bot);
        }
        let username = if candidates.contains(&bot.username) {
            bot.username.clone()
        } else {
            let mut available = None;
            for candidate in candidates {
                if self
                    .username_available(&candidate, Some(&bot.user_id))
                    .await?
                {
                    available = Some(candidate);
                    break;
                }
            }
            available.ok_or_else(|| {
                WireError::Configuration(
                    "no available Mattermost username for this session".to_owned(),
                )
            })?
        };
        self.put_json(
            &format!("/bots/{}", bot.user_id),
            &serde_json::json!({
                "username": username,
                "display_name": display_name,
                "description": bot.description.unwrap_or_default(),
            }),
        )
        .await
    }

    async fn bind_bot_user(&self, session: &Session, bot: &Bot) -> Result<(), WireError> {
        let user: User = self.get(&format!("/users/{}", bot.user_id)).await?;
        let mut props = user.props.unwrap_or_default();
        let session_id = session.id.to_string();
        if props.get(SESSION_PROPERTY) != Some(&session_id) {
            let _previous = props.insert(SESSION_PROPERTY.to_owned(), session_id);
            let _updated: User = self
                .put_json(
                    &format!("/users/{}/patch", bot.user_id),
                    &serde_json::json!({"props": props}),
                )
                .await?;
        }
        Ok(())
    }

    async fn username_available(
        &self,
        username: &str,
        bot_user_id: Option<&str>,
    ) -> Result<bool, WireError> {
        Ok(self
            .get_optional::<User>(&format!("/users/username/{username}"))
            .await?
            .is_none_or(|user| bot_user_id == Some(user.id.as_str())))
    }

    async fn identity_token(&self, session: &Session, bot: &Bot) -> Result<String, WireError> {
        if let Some(token) = lookup_secret(SESSION_ACCOUNT, Some(session.id)).await?
            && self.token_belongs_to(&token, &bot.user_id).await?
        {
            return Ok(token);
        }
        for account in session.username_candidates() {
            if let Some(token) = lookup_secret(&account, Some(session.id)).await?
                && self.token_belongs_to(&token, &bot.user_id).await?
            {
                store_secret(SESSION_ACCOUNT, session.id, &session.display_name(), &token).await?;
                let _cleared = clear_secret(&account, session.id).await;
                return Ok(token);
            }
        }
        self.mint_token(session, &bot.user_id).await
    }

    async fn token_belongs_to(&self, token: &str, user_id: &str) -> Result<bool, WireError> {
        match self.get_as::<User>(token, "/users/me").await {
            Ok(user) => Ok(user.id == user_id),
            Err(WireError::Response {
                status: StatusCode::UNAUTHORIZED,
                ..
            }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn mint_token(&self, session: &Session, user_id: &str) -> Result<String, WireError> {
        let access: AccessToken = self
            .post_json(
                &format!("/users/{user_id}/tokens"),
                &serde_json::json!({"description": format!("wire {}", session.id)}),
            )
            .await?;
        if let Err(error) = store_secret(
            SESSION_ACCOUNT,
            session.id,
            &session.display_name(),
            &access.token,
        )
        .await
        {
            let _revoked: Result<serde_json::Value, WireError> = self
                .post_json(
                    "/users/tokens/revoke",
                    &serde_json::json!({"token_id": access.id}),
                )
                .await;
            return Err(error);
        }
        Ok(access.token)
    }

    async fn ensure_membership(
        &self,
        identity: &Identity,
        channel: &BoundChannel,
    ) -> Result<bool, WireError> {
        let team_member = format!("/teams/{}/members/{}", channel.team.id, identity.user_id);
        let _team_added = self
            .ensure_relation(
                &team_member,
                &format!("/teams/{}/members", channel.team.id),
                &serde_json::json!({
                    "team_id": channel.team.id,
                    "user_id": identity.user_id,
                }),
            )
            .await?;
        let channel_member = format!(
            "/channels/{}/members/{}",
            channel.channel.id, identity.user_id
        );
        self.ensure_relation(
            &channel_member,
            &format!("/channels/{}/members", channel.channel.id),
            &serde_json::json!({
                "channel_id": channel.channel.id,
                "user_id": identity.user_id,
            }),
        )
        .await
    }

    async fn set_subscription(
        &self,
        user_id: &str,
        channel_id: &str,
        state: SubscriptionState,
    ) -> Result<(), WireError> {
        let preference = Preference::subscription(user_id, channel_id);
        let _: serde_json::Value = match state {
            SubscriptionState::Subscribed => {
                self.put_json(&format!("/users/{user_id}/preferences"), &[preference])
                    .await?
            }
            SubscriptionState::Unsubscribed => {
                self.post_json(
                    &format!("/users/{user_id}/preferences/delete"),
                    &[preference],
                )
                .await?
            }
        };
        Ok(())
    }

    async fn subscription(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Option<Preference>, WireError> {
        self.get_optional(&format!(
            "/users/{user_id}/preferences/{SUBSCRIPTION_CATEGORY}/name/{channel_id}"
        ))
        .await
        .map(|preference: Option<Preference>| {
            preference.filter(|preference| preference.value == "1")
        })
    }

    async fn ensure_relation<I: Serialize + ?Sized>(
        &self,
        probe: &str,
        create: &str,
        input: &I,
    ) -> Result<bool, WireError> {
        if self
            .get_optional::<serde_json::Value>(probe)
            .await?
            .is_some()
        {
            return Ok(false);
        }
        let created: Result<serde_json::Value, WireError> = self.post_json(create, input).await;
        match created {
            Ok(_) => Ok(true),
            Err(_error)
                if self
                    .get_optional::<serde_json::Value>(probe)
                    .await?
                    .is_some() =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, WireError> {
        self.get_as(&self.admin_token, path).await
    }

    async fn get_as<T: DeserializeOwned>(&self, token: &str, path: &str) -> Result<T, WireError> {
        let response = self
            .checked(self.request_as(token, reqwest::Method::GET, path))
            .await?;
        self.decode(response).await
    }

    async fn get_optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, WireError> {
        let response = self
            .request_as(&self.admin_token, reqwest::Method::GET, path)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        self.decode(self.checked_response(response).await?)
            .await
            .map(Some)
    }

    async fn post_json<I: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        input: &I,
    ) -> Result<O, WireError> {
        self.post_json_as(&self.admin_token, path, input).await
    }

    async fn put_json<I: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        input: &I,
    ) -> Result<O, WireError> {
        let response = self
            .checked(
                self.request_as(&self.admin_token, reqwest::Method::PUT, path)
                    .json(input),
            )
            .await?;
        self.decode(response).await
    }

    async fn post_json_as<I: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
        input: &I,
    ) -> Result<O, WireError> {
        let response = self
            .checked(
                self.request_as(token, reqwest::Method::POST, path)
                    .json(input),
            )
            .await?;
        self.decode(response).await
    }

    fn request_as(&self, token: &str, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
    }

    async fn checked(&self, request: RequestBuilder) -> Result<reqwest::Response, WireError> {
        self.checked_response(request.send().await?).await
    }

    async fn checked_response(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, WireError> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let mut body = response.text().await.unwrap_or_default();
        body.truncate(body.floor_char_boundary(ERROR_BODY_LIMIT));
        Err(WireError::Response { status, body })
    }

    async fn decode<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, WireError> {
        let bytes = response.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

impl Preference {
    fn subscription(user_id: &str, channel_id: &str) -> Self {
        Self {
            user_id: user_id.to_owned(),
            category: SUBSCRIPTION_CATEGORY.to_owned(),
            name: channel_id.to_owned(),
            value: "1".to_owned(),
        }
    }
}

impl Bot {
    fn legacy_session(&self) -> Option<Uuid> {
        self.description
            .as_deref()?
            .strip_prefix("Codex session ")?
            .parse()
            .ok()
    }

    fn biography(self) -> Option<String> {
        let legacy = self.legacy_session().is_some();
        self.description
            .filter(|description| !description.trim().is_empty())
            .filter(|_| !legacy)
    }
}

fn lookup_secret_sync(account: &str, session: Option<Uuid>) -> Result<Option<String>, WireError> {
    let output = StdCommand::new("secret-tool")
        .args(secret_args("lookup", account, session))
        .output()
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    secret_output(output)
}

async fn lookup_secret(account: &str, session: Option<Uuid>) -> Result<Option<String>, WireError> {
    let output = Command::new("secret-tool")
        .args(secret_args("lookup", account, session))
        .output()
        .await
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    secret_output(output)
}

async fn clear_secret(account: &str, session: Uuid) -> Result<bool, WireError> {
    let output = Command::new("secret-tool")
        .args(secret_args("clear", account, Some(session)))
        .output()
        .await
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    if output.status.success() {
        Ok(true)
    } else if output.status.code() == Some(1) && output.stderr.is_empty() {
        Ok(false)
    } else {
        Err(WireError::Keyring(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

async fn store_secret(
    account: &str,
    session: Uuid,
    display_name: &str,
    token: &str,
) -> Result<(), WireError> {
    let mut child = Command::new("secret-tool")
        .args([
            OsString::from("store"),
            OsString::from(format!("--label=Wire {display_name}")),
        ])
        .args(secret_attributes(account, Some(session)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| WireError::Keyring("secret-tool stdin is unavailable".to_owned()))?;
    input
        .write_all(token.as_bytes())
        .await
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    drop(input);
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WireError::Keyring(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn secret_args(command: &str, account: &str, session: Option<Uuid>) -> Vec<OsString> {
    let mut args = vec![OsString::from(command)];
    args.extend(secret_attributes(account, session));
    args
}

fn secret_attributes(account: &str, session: Option<Uuid>) -> Vec<OsString> {
    let mut attributes = vec![
        OsString::from("application"),
        OsString::from("wire"),
        OsString::from("service"),
        OsString::from("mattermost"),
        OsString::from("account"),
        OsString::from(account),
    ];
    if let Some(session) = session {
        attributes.extend([
            OsString::from("session"),
            OsString::from(session.to_string()),
        ]);
    }
    attributes
}

fn secret_output(output: std::process::Output) -> Result<Option<String>, WireError> {
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.is_empty() {
            return Ok(None);
        }
        return Err(WireError::Keyring(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|error| WireError::Keyring(error.to_string()))?
        .trim()
        .to_owned();
    Ok((!token.is_empty()).then_some(token))
}

pub(crate) fn indexed_title(id: Uuid) -> Option<String> {
    let path = env::var_os("CODEX_HOME").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".codex"))
                .unwrap_or_default()
        },
        PathBuf::from,
    );
    fs::read_to_string(path.join("session_index.jsonl"))
        .ok()?
        .lines()
        .filter_map(|line| serde_json::from_str::<SessionIndexRow>(line).ok())
        .filter(|row| row.id == id && !row.thread_name.trim().is_empty())
        .map(|row| row.thread_name)
        .next_back()
}

fn session_slug(title: &str) -> String {
    let title = title.trim();
    let folded = title.to_ascii_lowercase();
    let title = ["coder_", "coder-", "codex_", "codex-"]
        .into_iter()
        .find_map(|prefix| folded.starts_with(prefix).then(|| &title[prefix.len()..]))
        .unwrap_or(title);
    let mut slug = String::new();
    let mut separated = false;
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separated = false;
        } else if !slug.is_empty() && !separated {
            slug.push('-');
            separated = true;
        }
    }
    if separated {
        let _last = slug.pop();
    }
    slug
}

fn session_from_properties(properties: Option<&HashMap<String, String>>) -> Option<Uuid> {
    properties?.get(SESSION_PROPERTY)?.parse().ok()
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn name_matches(selector: &str, name: &str, display_name: &str) -> bool {
    selector.eq_ignore_ascii_case(name) || selector.eq_ignore_ascii_case(display_name)
}
