use std::{
    env,
    fs::{DirBuilder, File, OpenOptions},
    io::Read as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::PathBuf,
};

use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tokio::task;
use uuid::Uuid;

use crate::{
    api::{AgentIdentity, Mattermost, Session, WireError},
    appserver::{self, AppServerError},
};

const MIN_BIOGRAPHY_CHARS: usize = 120;
const MAX_BIOGRAPHY_CHARS: usize = 700;
const PROMPT: &str = include_str!("../assets/identity-prompt.md");
const OUTPUT_SCHEMA: &str = include_str!("../assets/identity.schema.json");

#[derive(Debug, Error)]
pub(crate) enum IdentityError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error("identity forge I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity lock task failed: {0}")]
    Task(#[from] task::JoinError),
    #[error("identity forge returned invalid JSON: {0}")]
    ForgeDecode(serde_json::Error),
    #[error("invalid Codex hook event: {0}")]
    HookDecode(serde_json::Error),
    #[error("identity forge returned an unlawful biography: {0}")]
    Biography(String),
    #[error("CODEX_HOME must resolve to an absolute path for identity serialization")]
    Runtime,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    biography: String,
}

#[derive(Debug, Deserialize)]
struct HookEvent {
    session_id: Uuid,
    hook_event_name: String,
}

struct ForgeGuard {
    _file: File,
}

pub(crate) async fn update(
    api: &Mattermost,
    session: &Session,
) -> Result<AgentIdentity, IdentityError> {
    let _guard = ForgeGuard::acquire(session).await?;
    let incumbent = api.agent_identity(session).await?;
    let biography = forge(session, incumbent.biography.as_deref()).await?;
    Ok(api.publish_biography(session, &biography).await?)
}

pub(crate) async fn update_hook(api: &Mattermost) -> Result<(), IdentityError> {
    let mut input = String::new();
    let _bytes = std::io::stdin().read_to_string(&mut input)?;
    let event: HookEvent = serde_json::from_str(&input).map_err(IdentityError::HookDecode)?;
    if event.hook_event_name != "PostCompact" {
        return Err(IdentityError::Biography(format!(
            "identity hook received {} instead of PostCompact",
            event.hook_event_name
        )));
    }
    let _identity = update(api, &Session::for_id(event.session_id)).await?;
    Ok(())
}

async fn forge(session: &Session, incumbent: Option<&str>) -> Result<String, IdentityError> {
    let metadata = json!({
        "thread_name": session.name(),
        "previous_biography": incumbent,
    });
    let prompt = format!("{PROMPT}\n\n# Identity Metadata\n\n{metadata}");
    let schema = serde_json::from_str(OUTPUT_SCHEMA).map_err(IdentityError::ForgeDecode)?;
    let output = appserver::forge_identity(session.id(), &prompt, schema).await?;
    let draft: Draft = serde_json::from_str(&output).map_err(IdentityError::ForgeDecode)?;
    validate(&draft.biography)
}

fn validate(biography: &str) -> Result<String, IdentityError> {
    let biography = biography.trim();
    let length = biography.chars().count();
    if !(MIN_BIOGRAPHY_CHARS..=MAX_BIOGRAPHY_CHARS).contains(&length) {
        return Err(IdentityError::Biography(format!(
            "expected {MIN_BIOGRAPHY_CHARS}..={MAX_BIOGRAPHY_CHARS} characters, received {length}"
        )));
    }
    if biography.contains(['\r', '\n']) {
        return Err(IdentityError::Biography(
            "the paragraph contains a line break".to_owned(),
        ));
    }
    if biography.chars().any(char::is_control) {
        return Err(IdentityError::Biography(
            "the paragraph contains a control character".to_owned(),
        ));
    }
    Ok(biography.to_owned())
}

impl ForgeGuard {
    async fn acquire(session: &Session) -> Result<Self, IdentityError> {
        let path = lock_path(session)?;
        let file = task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.lock()?;
            Ok::<_, std::io::Error>(file)
        })
        .await??;
        Ok(Self { _file: file })
    }
}

fn lock_path(session: &Session) -> Result<PathBuf, IdentityError> {
    let codex_home = env::var_os("CODEX_HOME").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".codex")
        },
        PathBuf::from,
    );
    if !codex_home.is_absolute() {
        return Err(IdentityError::Runtime);
    }
    let locks = codex_home.join("tmp/wire/identity");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&locks)?;
    Ok(locks.join(format!("{}.lock", session.id())))
}
