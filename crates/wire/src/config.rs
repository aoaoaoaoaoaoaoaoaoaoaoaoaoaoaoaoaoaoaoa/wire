use std::{env, fs, io, path::PathBuf};

const BROADCAST_ENV: &str = "WIRE_CHANNEL_BROADCAST";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChannelBroadcast {
    Disabled,
    Enabled,
}

impl ChannelBroadcast {
    pub(crate) fn load() -> Result<Self, String> {
        match env::var(BROADCAST_ENV) {
            Ok(value) => return Self::parse(&value),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(format!("{BROADCAST_ENV} is not UTF-8"));
            }
            Err(env::VarError::NotPresent) => {}
        }
        let Some(path) = codex_config() else {
            return Ok(Self::Disabled);
        };
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Disabled),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let document = toml::from_str::<toml::Table>(&source)
            .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
        let Some(value) = document
            .get("mcp_servers")
            .and_then(|value| value.get("wire"))
            .and_then(|value| value.get("env"))
            .and_then(|value| value.get(BROADCAST_ENV))
        else {
            return Ok(Self::Disabled);
        };
        let value = value
            .as_str()
            .ok_or_else(|| format!("mcp_servers.wire.env.{BROADCAST_ENV} must be a string"))?;
        Self::parse(value)
    }

    pub(crate) const fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "false" => Ok(Self::Disabled),
            "true" => Ok(Self::Enabled),
            _ => Err(format!("{BROADCAST_ENV} must be `true` or `false`")),
        }
    }
}

fn codex_config() -> Option<PathBuf> {
    absolute_env("CODEX_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".codex")))
        .map(|home| home.join("config.toml"))
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}
