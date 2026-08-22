use std::sync::Arc;

use libmcp::{DetailLevel, RenderMode};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ContentBlock, Implementation, MetaObject, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::api::{BoundChannel, CreatedPost, Mattermost, Timeline, WireError};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
const MAX_MESSAGE_CHARS: usize = 16_383;
const CONCISE_BODY_CHARS: usize = 1_000;
const FULL_BODY_CHARS: usize = 12_000;
const MAX_BODY_CHARS: usize = 100_000;

#[derive(Clone)]
pub(crate) struct WireServer {
    api: Mattermost,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ViewArgs {
    #[serde(default)]
    render: RenderMode,
    #[serde(default)]
    detail: DetailLevel,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[schemars(description = "Channel ID, channel name, or team:channel.")]
    channel: String,
    #[schemars(description = "Optional thread root post ID.")]
    thread: Option<String>,
    #[schemars(description = "Exclusive post cursor for older channel history.")]
    before: Option<String>,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    limit: usize,
    #[schemars(
        description = "Per-message body character cap; overrides the detail default.",
        range(min = 1, max = 100_000)
    )]
    body_chars: Option<usize>,
    #[serde(default)]
    render: RenderMode,
    #[serde(default)]
    detail: DetailLevel,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PostArgs {
    #[schemars(description = "Channel ID, channel name, or team:channel.")]
    channel: String,
    #[serde(default)]
    message: String,
    #[schemars(description = "Optional thread root post ID.")]
    reply_to: Option<String>,
    #[serde(default)]
    render: RenderMode,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ChannelOutput {
    id: String,
    team: String,
    team_display: String,
    name: String,
    display_name: String,
    message_count: u64,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ChannelsOutput {
    channels: Vec<ChannelOutput>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct MessageOutput {
    id: String,
    created_at: String,
    updated_at: Option<String>,
    sender: String,
    reply_to: Option<String>,
    body: String,
    body_truncated: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct ReadOutput {
    channel: ChannelOutput,
    messages: Vec<MessageOutput>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
struct PostOutput {
    channel: String,
    id: String,
    sender: String,
    reply_to: Option<String>,
}

#[tool_router]
impl WireServer {
    fn new(api: Mattermost) -> Self {
        let mut tool_router = Self::tool_router();
        for (name, route) in &mut tool_router.map {
            let recovery = if name.as_ref() == "chat.post" {
                "at_most_once"
            } else {
                "replay_safe"
            };
            let mut meta = MetaObject::new();
            let _old = meta.0.insert(
                "io.libmcp/effect".to_owned(),
                json!({
                    "recovery": {"kind": recovery},
                    "state": {"kind": "stateless"}
                }),
            );
            route.attr.meta = Some(meta);
        }
        Self { api, tool_router }
    }

    #[tool(
        name = "chat.channels",
        description = "List Mattermost channels visible to the agent.",
        annotations(
            title = "List chat channels",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        output_schema = output_schema::<ChannelsOutput>()
    )]
    async fn channels(
        &self,
        Parameters(args): Parameters<ViewArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.api.channels().await {
            Ok(channels) => render(
                ChannelsOutput {
                    channels: channels.iter().map(ChannelOutput::from).collect(),
                },
                args.render,
                args.detail,
            ),
            Err(error) => Ok(tool_error(&error)),
        }
    }

    #[tool(
        name = "chat.read",
        description = "Read bounded channel history or one thread, oldest to newest.",
        annotations(
            title = "Read chat",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        output_schema = output_schema::<ReadOutput>()
    )]
    async fn read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        validate_limit(args.limit)?;
        validate_optional_id(args.thread.as_deref(), "thread")?;
        validate_optional_id(args.before.as_deref(), "before")?;
        let body_chars = args.body_chars.unwrap_or(match args.detail {
            DetailLevel::Concise => CONCISE_BODY_CHARS,
            DetailLevel::Full => FULL_BODY_CHARS,
        });
        if !(1..=MAX_BODY_CHARS).contains(&body_chars) {
            return Err(invalid(format!(
                "body_chars must be between 1 and {MAX_BODY_CHARS}"
            )));
        }
        match self
            .api
            .read(
                &args.channel,
                args.thread.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await
        {
            Ok(timeline) => render(
                ReadOutput::from_timeline(timeline, body_chars, args.detail),
                args.render,
                args.detail,
            ),
            Err(error) => Ok(tool_error(&error)),
        }
    }

    #[tool(
        name = "chat.post",
        description = "Post freeform text as this Codex session to a channel or thread.",
        annotations(
            title = "Post chat message",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        ),
        output_schema = output_schema::<PostOutput>()
    )]
    async fn post(
        &self,
        Parameters(args): Parameters<PostArgs>,
    ) -> Result<CallToolResult, McpError> {
        validate_post(&args)?;
        match self
            .api
            .post(&args.channel, &args.message, args.reply_to.as_deref())
            .await
        {
            Ok(created) => render(PostOutput::from(created), args.render, DetailLevel::Concise),
            Err(error) => Ok(tool_error(&error)),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WireServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Use chat.channels to discover administrator-created channels. Read with chat.read. Post useful freeform coordination with chat.post. Each Codex session has one stable Mattermost bot identity."
            )
            .with_server_info(Implementation::new("wire", env!("CARGO_PKG_VERSION")))
    }
}

pub(crate) async fn serve(api: Mattermost) -> Result<(), Box<dyn std::error::Error>> {
    let service = WireServer::new(api).serve(stdio()).await?;
    let _reason = service.waiting().await?;
    Ok(())
}

impl From<&BoundChannel> for ChannelOutput {
    fn from(value: &BoundChannel) -> Self {
        Self {
            id: value.channel.id.clone(),
            team: value.team.name.clone(),
            team_display: value.team.display_name.clone(),
            name: value.channel.name.clone(),
            display_name: value.channel.display_name.clone(),
            message_count: value.channel.total_msg_count,
        }
    }
}

impl ReadOutput {
    fn from_timeline(timeline: Timeline, body_chars: usize, detail: DetailLevel) -> Self {
        let Timeline {
            channel,
            posts,
            users,
        } = timeline;
        let messages = posts
            .into_iter()
            .map(|post| {
                let sender = users
                    .get(&post.user_id)
                    .map_or_else(|| post.user_id.clone(), |user| user.username.clone());
                let (body, body_truncated) = truncate(&post.message, body_chars);
                MessageOutput {
                    id: post.id,
                    created_at: timestamp(post.create_at),
                    updated_at: (matches!(detail, DetailLevel::Full)
                        && post.update_at != post.create_at)
                        .then(|| timestamp(post.update_at)),
                    sender,
                    reply_to: (!post.root_id.is_empty()).then_some(post.root_id),
                    body,
                    body_truncated,
                }
            })
            .collect();
        Self {
            channel: ChannelOutput::from(&channel),
            messages,
        }
    }
}

impl From<CreatedPost> for PostOutput {
    fn from(value: CreatedPost) -> Self {
        Self {
            channel: format!("{}:{}", value.channel.team.name, value.channel.channel.name),
            id: value.post.id,
            sender: value.sender,
            reply_to: (!value.post.root_id.is_empty()).then_some(value.post.root_id),
        }
    }
}

trait Porcelain {
    fn porcelain(&self, detail: DetailLevel) -> String;
}

impl Porcelain for ChannelsOutput {
    fn porcelain(&self, _detail: DetailLevel) -> String {
        let mut lines = vec!["channel | id | display | messages".to_owned()];
        lines.extend(self.channels.iter().map(|channel| {
            format!(
                "{}:{} | {} | {} | {}",
                channel.team, channel.name, channel.id, channel.display_name, channel.message_count
            )
        }));
        lines.join("\n")
    }
}

impl Porcelain for ReadOutput {
    fn porcelain(&self, detail: DetailLevel) -> String {
        let mut lines = vec![format!(
            "{}:{} | {} message(s)",
            self.channel.team,
            self.channel.name,
            self.messages.len()
        )];
        for message in &self.messages {
            let reply = message.reply_to.as_deref().unwrap_or("-");
            lines.push(format!(
                "{} | {} | @{} | reply={}",
                message.created_at, message.id, message.sender, reply
            ));
            let body = match detail {
                DetailLevel::Concise => collapse(&message.body),
                DetailLevel::Full => message.body.replace('\n', "\n  "),
            };
            lines.push(format!(
                "  {body}{}",
                if message.body_truncated {
                    " …[truncated]"
                } else {
                    ""
                }
            ));
        }
        lines.join("\n")
    }
}

impl Porcelain for PostOutput {
    fn porcelain(&self, _detail: DetailLevel) -> String {
        format!("posted {} | {} | @{}", self.id, self.channel, self.sender)
    }
}

fn render<T: Porcelain + Serialize>(
    output: T,
    mode: RenderMode,
    detail: DetailLevel,
) -> Result<CallToolResult, McpError> {
    match mode {
        RenderMode::Porcelain => Ok(CallToolResult::success(vec![ContentBlock::text(
            output.porcelain(detail),
        )])),
        RenderMode::Json => serde_json::to_value(output).map_err(internal).map(|value| {
            let mut result = CallToolResult::default();
            result.structured_content = Some(value);
            result.is_error = Some(false);
            result
        }),
    }
}

fn tool_error(error: &WireError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

fn validate_limit(limit: usize) -> Result<(), McpError> {
    if (1..=MAX_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}")))
    }
}

fn validate_post(args: &PostArgs) -> Result<(), McpError> {
    if args.message.is_empty() {
        return Err(invalid("message must be present"));
    }
    if args.message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(invalid(format!(
            "message exceeds {MAX_MESSAGE_CHARS} characters"
        )));
    }
    validate_optional_id(args.reply_to.as_deref(), "reply_to")?;
    Ok(())
}

fn validate_optional_id(value: Option<&str>, name: &str) -> Result<(), McpError> {
    value.map_or(Ok(()), |value| validate_id(value, name))
}

fn validate_id(value: &str, name: &str) -> Result<(), McpError> {
    if value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name} must be a 26-character Mattermost ID"
        )))
    }
}

fn truncate(value: &str, limit: usize) -> (String, bool) {
    let mut chars = value.chars();
    let output = chars.by_ref().take(limit).collect::<String>();
    (output, chars.next().is_some())
}

fn collapse(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn timestamp(milliseconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| milliseconds.to_string())
}

fn output_schema<T: JsonSchema + 'static>() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<T>()
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn internal(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(format!("wire output failed: {error}"), None)
}

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}
