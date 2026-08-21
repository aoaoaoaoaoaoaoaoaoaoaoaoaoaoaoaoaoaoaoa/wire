use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, StatusCode, multipart};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

const DEFAULT_URL: &str = "http://127.0.0.1:8065/api/v4";
const ERROR_BODY_LIMIT: usize = 2_000;

#[derive(Clone)]
pub(crate) struct Mattermost {
    base: String,
    client: Client,
    token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) username: String,
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
pub(crate) struct FileInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) mime_type: String,
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
    #[serde(default)]
    pub(crate) file_ids: Vec<String>,
    #[serde(default)]
    pub(crate) metadata: PostMetadata,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct PostMetadata {
    #[serde(default)]
    pub(crate) files: Vec<FileInfo>,
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
    pub(crate) files: Vec<FileInfo>,
}

#[derive(Clone, Debug)]
pub(crate) struct DownloadedFile {
    pub(crate) info: FileInfo,
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
}

#[derive(Debug, Deserialize)]
struct PostList {
    order: Vec<String>,
    posts: HashMap<String, Post>,
}

#[derive(Debug, Deserialize)]
struct UploadResponse {
    file_infos: Vec<FileInfo>,
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
    #[error("local file operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("keyring lookup failed: {0}")]
    Keyring(String),
}

impl Mattermost {
    pub(crate) fn load() -> Result<Self, WireError> {
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
        let token = env::var("WIRE_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map_or_else(load_keyring_token, Ok)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_mins(5))
            .user_agent(concat!("wire/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client,
            token,
        })
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
            WireError::Input(format!("channel `{selector}` is not visible to the agent"))
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
        posts.reverse();
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
        selector: &str,
        message: &str,
        reply_to: Option<&str>,
        attachments: &[PathBuf],
    ) -> Result<CreatedPost, WireError> {
        let channel = self.resolve_channel(selector).await?;
        let files = if attachments.is_empty() {
            Vec::new()
        } else {
            self.upload(&channel.channel.id, attachments).await?
        };
        let post = self
            .post_json(
                "/posts",
                &serde_json::json!({
                    "channel_id": channel.channel.id,
                    "message": message,
                    "root_id": reply_to.unwrap_or_default(),
                    "file_ids": files.iter().map(|file| &file.id).collect::<Vec<_>>(),
                }),
            )
            .await?;
        Ok(CreatedPost {
            channel,
            post,
            files,
        })
    }

    pub(crate) async fn download(
        &self,
        file_id: &str,
        destination_dir: &Path,
    ) -> Result<DownloadedFile, WireError> {
        let info: FileInfo = self.get(&format!("/files/{file_id}/info")).await?;
        let directory = destination_dir.canonicalize()?;
        if !directory.is_dir() {
            return Err(WireError::Input(format!(
                "destination is not a directory: {}",
                directory.display()
            )));
        }
        let name = Path::new(&info.name)
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("attachment");
        let destination = directory.join(format!("{}-{name}", info.id));
        let response = self
            .checked(self.request(reqwest::Method::GET, &format!("/files/{file_id}")))
            .await?;
        let temporary = NamedTempFile::new_in(&directory)?;
        let clone = temporary.as_file().try_clone()?;
        let mut output = tokio::fs::File::from_std(clone);
        let mut stream = response.bytes_stream();
        let mut bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            output.write_all(&chunk).await?;
            bytes = bytes
                .checked_add(
                    u64::try_from(chunk.len())
                        .map_err(|_| WireError::Input("attachment size exceeds u64".to_owned()))?,
                )
                .ok_or_else(|| WireError::Input("attachment size exceeds u64".to_owned()))?;
        }
        output.flush().await?;
        drop(output);
        if bytes != info.size {
            return Err(WireError::Response {
                status: StatusCode::OK,
                body: format!(
                    "attachment length mismatch: expected {}, received {bytes}",
                    info.size
                ),
            });
        }
        let _file = temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        Ok(DownloadedFile {
            info,
            path: destination,
            bytes,
        })
    }

    async fn upload(
        &self,
        channel_id: &str,
        attachments: &[PathBuf],
    ) -> Result<Vec<FileInfo>, WireError> {
        let mut form = multipart::Form::new().text("channel_id", channel_id.to_owned());
        for path in attachments {
            form = form.file("files", path).await?;
        }
        let response: UploadResponse = self
            .decode(
                self.checked(
                    self.request(reqwest::Method::POST, "/files")
                        .multipart(form),
                )
                .await?,
            )
            .await?;
        Ok(response.file_infos)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, WireError> {
        let response = self
            .checked(self.request(reqwest::Method::GET, path))
            .await?;
        self.decode(response).await
    }

    async fn post_json<I: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        input: &I,
    ) -> Result<O, WireError> {
        let response = self
            .checked(self.request(reqwest::Method::POST, path).json(input))
            .await?;
        self.decode(response).await
    }

    fn request(&self, method: reqwest::Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    async fn checked(&self, request: RequestBuilder) -> Result<reqwest::Response, WireError> {
        let response = request.send().await?;
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

fn load_keyring_token() -> Result<String, WireError> {
    let output = Command::new("secret-tool")
        .args([
            OsString::from("lookup"),
            OsString::from("application"),
            OsString::from("wire"),
            OsString::from("service"),
            OsString::from("mattermost"),
            OsString::from("account"),
            OsString::from("codex"),
        ])
        .output()
        .map_err(|error| WireError::Keyring(error.to_string()))?;
    if !output.status.success() {
        return Err(WireError::Keyring(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let token = String::from_utf8(output.stdout)
        .map_err(|error| WireError::Keyring(error.to_string()))?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err(WireError::Keyring("token is empty".to_owned()));
    }
    Ok(token)
}

fn name_matches(selector: &str, name: &str, display_name: &str) -> bool {
    selector.eq_ignore_ascii_case(name) || selector.eq_ignore_ascii_case(display_name)
}
