// SPDX-License-Identifier: GPL-3.0-or-later
//
// AI subscription usage for the bar.
//
// These endpoints need bearer credentials and do not permit a file:// shell
// to call them cross-origin. Keep both the credentials and the network wait on
// this worker; the shell receives only normalized percentages or dollars.

use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use viewport_ipc::event::AiUsage as Usage;

const REFRESH: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone)]
pub struct Account {
    provider: crate::config::AiProvider,
    token_env: Option<String>,
    account_id_env: Option<String>,
}

pub enum Message {
    Usage(Vec<Usage>),
    Auth {
        state: &'static str,
        url: Option<String>,
        code: Option<String>,
        message: Option<String>,
    },
}

#[derive(Default)]
pub struct AiUsage {
    worker: Option<mpsc::Sender<Command>>,
    accounts: Vec<Account>,
    events: Option<smithay::reexports::calloop::channel::Sender<Message>>,
}

impl AiUsage {
    pub fn attach(&mut self, events: smithay::reexports::calloop::channel::Sender<Message>) {
        self.events = Some(events);
        let accounts = std::mem::take(&mut self.accounts);
        self.configure(accounts);
    }

    /// Replace providers atomically on config reload. Empty disables all
    /// polling and clears any values already drawn by the shell.
    pub fn configure(&mut self, accounts: Vec<Account>) {
        let mut seen = std::collections::HashSet::new();
        self.accounts = accounts
            .into_iter()
            .filter(|account| seen.insert(account.provider))
            .collect();
        let Some(events) = self.events.clone() else {
            return;
        };
        if self.worker.is_none() {
            if self.accounts.is_empty() {
                return;
            }
            self.worker = start(events).ok();
        }
        if let Some(worker) = &self.worker {
            let _ = worker.send(Command::Configure(self.accounts.clone()));
        }
    }

    pub fn login_openai(&mut self) {
        let Some(events) = self.events.clone() else {
            return;
        };
        if self.worker.is_none() {
            self.worker = start(events).ok();
        }
        if let Some(worker) = &self.worker {
            let _ = worker.send(Command::LoginOpenai);
        }
    }

    pub fn replay(&self) {
        if let Some(worker) = &self.worker {
            let _ = worker.send(Command::Replay);
        }
    }
}

/// Retain only where credentials come from. Values are resolved on every poll
/// so OAuth rotation by Claude Code or Codex is picked up without a reload.
pub fn account(widget: &crate::config::BarWidgetConfig) -> Option<Account> {
    let crate::config::BarWidgetConfig::Ai {
        provider,
        token_env,
        account_id_env,
    } = widget
    else {
        return None;
    };
    Some(Account {
        provider: *provider,
        token_env: token_env.clone(),
        account_id_env: account_id_env.clone(),
    })
}

enum Command {
    Configure(Vec<Account>),
    Replay,
    LoginOpenai,
    LoginFinished(Result<(), String>),
}

fn start(
    events: smithay::reexports::calloop::channel::Sender<Message>,
) -> std::io::Result<mpsc::Sender<Command>> {
    let (commands, inbox) = mpsc::channel();
    let worker_commands = commands.clone();
    std::thread::Builder::new()
        .name("ai-usage".to_owned())
        .spawn(move || worker(inbox, worker_commands, events))?;
    Ok(commands)
}

fn worker(
    inbox: mpsc::Receiver<Command>,
    commands: mpsc::Sender<Command>,
    events: smithay::reexports::calloop::channel::Sender<Message>,
) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build();
    let mut accounts = Vec::new();
    let mut last = Vec::new();
    let mut login_active = false;

    loop {
        let configured = match inbox.recv_timeout(if accounts.is_empty() {
            Duration::from_secs(24 * 60 * 60)
        } else {
            REFRESH
        }) {
            Ok(Command::Configure(next)) => {
                accounts = next;
                true
            }
            Ok(Command::Replay) => {
                if events.send(Message::Usage(last.clone())).is_err() {
                    return;
                }
                continue;
            }
            Ok(Command::LoginOpenai) => {
                if !login_active {
                    let agent = agent.clone();
                    let login_events = events.clone();
                    let commands = commands.clone();
                    match std::thread::Builder::new()
                        .name("openai-login".to_owned())
                        .spawn(move || {
                            let result = openai_login(&agent, &login_events);
                            let _ = commands.send(Command::LoginFinished(result));
                        }) {
                        Ok(_) => login_active = true,
                        Err(error) => {
                            let _ = events.send(Message::Auth {
                                state: "error",
                                url: None,
                                code: None,
                                message: Some(format!("starting OpenAI login worker: {error}")),
                            });
                        }
                    }
                }
                false
            }
            Ok(Command::LoginFinished(result)) => {
                login_active = false;
                match result {
                    Ok(()) => {
                        let _ = events.send(Message::Auth {
                            state: "complete",
                            url: None,
                            code: None,
                            message: None,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(Message::Auth {
                            state: "error",
                            url: None,
                            code: None,
                            message: Some(error),
                        });
                    }
                }
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };

        let next = refresh(&agent, &accounts, &last, login_active);
        // Re-send on config even when unchanged: a shell may have reloaded and
        // lost its cache while this longer-lived worker kept its own.
        if configured || next != last {
            last = next;
            if events.send(Message::Usage(last.clone())).is_err() {
                return;
            }
        }
    }
}

/// Keep a provider's last good value through a transient API failure. A config
/// reload still removes providers immediately because only configured entries
/// are copied into the next snapshot.
fn refresh(
    agent: &ureq::Agent,
    accounts: &[Account],
    previous: &[Usage],
    openai_login_active: bool,
) -> Vec<Usage> {
    accounts
        .iter()
        .filter_map(|account| {
            if openai_login_active && account.provider == crate::config::AiProvider::Openai {
                return previous
                    .iter()
                    .find(|usage| usage.provider == account.provider.name())
                    .cloned();
            }
            match fetch(agent, account) {
                Ok(usage) => Some(usage),
                Err(error) => {
                    tracing::warn!(
                        "ai usage: {} refresh failed: {error}",
                        account.provider.name()
                    );
                    previous
                        .iter()
                        .find(|usage| usage.provider == account.provider.name())
                        .cloned()
                }
            }
        })
        .collect()
}

fn fetch(agent: &ureq::Agent, account: &Account) -> Result<Usage, String> {
    let (token, account_id, own_openai_auth) = credentials(agent, account)?;
    match fetch_with(agent, account, &token, account_id.as_deref()) {
        Err(FetchError::Http(error))
            if own_openai_auth && matches!(error.as_ref(), ureq::Error::Status(401, _)) =>
        {
            let auth = refresh_openai_auth(agent, &load_openai_auth()?)?;
            fetch_with(
                agent,
                account,
                &auth.access_token,
                auth.account_id.as_deref(),
            )
            .map_err(|error| fetch_error(account.provider.usage_url(), error))
        }
        result => result.map_err(|error| fetch_error(account.provider.usage_url(), error)),
    }
}

enum FetchError {
    Http(Box<ureq::Error>),
    Body(String),
}

fn fetch_error(url: &str, error: FetchError) -> String {
    match error {
        FetchError::Http(error) => format!("{url}: {error}"),
        FetchError::Body(error) => format!("{url}: {error}"),
    }
}

fn fetch_with(
    agent: &ureq::Agent,
    account: &Account,
    token: &str,
    account_id: Option<&str>,
) -> Result<Usage, FetchError> {
    use crate::config::AiProvider;
    let mut request = match account.provider {
        AiProvider::Claude => {
            let url = "https://api.anthropic.com/api/oauth/usage";
            agent.get(url).set("anthropic-beta", "oauth-2025-04-20")
        }
        AiProvider::Openai => {
            let url = "https://chatgpt.com/backend-api/wham/usage";
            agent.get(url)
        }
        AiProvider::Openrouter => {
            let url = "https://openrouter.ai/api/v1/credits";
            agent.get(url)
        }
    };
    request = request.set("Authorization", &format!("Bearer {token}"));
    if account.provider == AiProvider::Openai {
        if let Some(id) = account_id {
            request = request.set("ChatGPT-Account-Id", id);
        }
    }
    let response = request
        .call()
        .map_err(|error| FetchError::Http(Box::new(error)))?;
    let body = response
        .into_string()
        .map_err(|error| FetchError::Body(error.to_string()))?;
    parse(account.provider, &body).map_err(FetchError::Body)
}

fn credentials(
    agent: &ureq::Agent,
    account: &Account,
) -> Result<(String, Option<String>, bool), String> {
    use crate::config::AiProvider;
    let from_env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let file = || -> Option<Value> {
        let home = std::env::var_os("HOME")?;
        let relative = match account.provider {
            AiProvider::Claude => ".claude/.credentials.json",
            AiProvider::Openai => ".codex/auth.json",
            AiProvider::Openrouter => return None,
        };
        let text = std::fs::read_to_string(std::path::Path::new(&home).join(relative)).ok()?;
        serde_json::from_str(&text).ok()
    };

    let token_name = account
        .token_env
        .as_deref()
        .unwrap_or_else(|| account.provider.token_env());
    let env_token = from_env(token_name);
    let own = (env_token.is_none()
        && account.provider == AiProvider::Openai
        && account.token_env.is_none())
    .then(|| current_openai_auth(agent).ok())
    .flatten();
    let token = env_token
        .clone()
        .or_else(|| own.as_ref().map(|auth| auth.access_token.clone()))
        .or_else(|| {
            if account.token_env.is_some() {
                return None;
            }
            let value = file()?;
            match account.provider {
                AiProvider::Claude => value
                    .pointer("/claudeAiOauth/accessToken")
                    .and_then(Value::as_str),
                AiProvider::Openai => value
                    .pointer("/tokens/access_token")
                    .and_then(Value::as_str),
                AiProvider::Openrouter => None,
            }
            .map(str::to_owned)
        })
        .ok_or_else(|| format!("no token in ${token_name} or provider login file"))?;

    let account_id = if account.provider == AiProvider::Openai {
        let name = account
            .account_id_env
            .as_deref()
            .unwrap_or("OPENAI_ACCOUNT_ID");
        let env_account = from_env(name);
        if env_token.is_some() || account.account_id_env.is_some() {
            env_account
        } else {
            env_account
                .or_else(|| own.as_ref().and_then(|auth| auth.account_id.clone()))
                .or_else(|| {
                    file()?
                        .pointer("/tokens/account_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        }
    } else {
        None
    };
    let own_openai_auth = env_token.is_none() && own.is_some();
    Ok((token, account_id, own_openai_auth))
}

#[derive(Clone, Deserialize, Serialize)]
struct OpenAiAuth {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: Option<String>,
}

fn openai_auth_path() -> Result<std::path::PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".config"))
        })
        .ok_or_else(|| "neither $XDG_CONFIG_HOME nor $HOME is set".to_owned())?;
    Ok(base.join("viewport/openai-auth.json"))
}

fn load_openai_auth() -> Result<OpenAiAuth, String> {
    let path = openai_auth_path()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("reading {}: {error}", path.display()))
}

fn save_openai_auth(auth: &OpenAiAuth) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    let path = openai_auth_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "invalid auth path".to_owned())?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    let temporary = parent.join(format!(".openai-auth-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("creating {}: {error}", temporary.display()))?;
    let bytes = serde_json::to_vec(auth).map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("writing {}: {error}", temporary.display()));
    }
    match std::fs::rename(&temporary, &path) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
            // A file persisted by bind-mounting it cannot be replaced, but the
            // mounted inode can still be updated safely and kept private.
            let result = update_mounted_auth(&path, &bytes, &error);
            let _ = std::fs::remove_file(&temporary);
            result
        }
        Err(error) => Err(format!("saving {}: {error}", path.display())),
    }
}

fn update_mounted_auth(
    path: &std::path::Path,
    bytes: &[u8],
    replace_error: &std::io::Error,
) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            format!(
                "saving {}: {replace_error}; updating mount: {error}",
                path.display()
            )
        })?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            format!(
                "saving {}: {replace_error}; securing mount: {error}",
                path.display()
            )
        })?;
    file.set_len(0).map_err(|error| {
        format!(
            "saving {}: {replace_error}; truncating mount: {error}",
            path.display()
        )
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            format!(
                "saving {}: {replace_error}; updating mount: {error}",
                path.display()
            )
        })
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn account_id(id_token: &str) -> Option<String> {
    let claims = jwt_claims(id_token)?;
    claims
        .get("chatgpt_account_id")
        .or_else(|| claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn current_openai_auth(agent: &ureq::Agent) -> Result<OpenAiAuth, String> {
    let auth = load_openai_auth()?;
    let expiring = jwt_claims(&auth.access_token)
        .and_then(|claims| claims.get("exp").and_then(Value::as_u64))
        .is_some_and(|expires| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            expires <= now + 120
        });
    if !expiring {
        return Ok(auth);
    }
    refresh_openai_auth(agent, &auth)
}

fn refresh_openai_auth(agent: &ureq::Agent, prior: &OpenAiAuth) -> Result<OpenAiAuth, String> {
    let response = agent
        .post(&format!("{OPENAI_ISSUER}/oauth/token"))
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_CLIENT_ID),
            ("refresh_token", prior.refresh_token.as_str()),
        ])
        .map_err(|error| format!("refreshing OpenAI OAuth: {error}"))?;
    let value: Value = serde_json::from_str(
        &response
            .into_string()
            .map_err(|error| format!("refreshing OpenAI OAuth: {error}"))?,
    )
    .map_err(|error| format!("refreshing OpenAI OAuth: {error}"))?;
    let access_token = required_text(&value, "access_token")?;
    let id_token = value
        .get("id_token")
        .and_then(Value::as_str)
        .unwrap_or(&prior.id_token)
        .to_owned();
    let auth = OpenAiAuth {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or(&prior.refresh_token)
            .to_owned(),
        account_id: account_id(&id_token).or_else(|| prior.account_id.clone()),
        id_token,
    };
    save_openai_auth(&auth)?;
    Ok(auth)
}

fn openai_login(
    agent: &ureq::Agent,
    events: &smithay::reexports::calloop::channel::Sender<Message>,
) -> Result<(), String> {
    let response = agent
        .post(&format!("{OPENAI_ISSUER}/api/accounts/deviceauth/usercode"))
        .set("Content-Type", "application/json")
        .send_string(&format!(r#"{{"client_id":"{OPENAI_CLIENT_ID}"}}"#))
        .map_err(|error| format!("starting OpenAI OAuth: {error}"))?;
    let value: Value = serde_json::from_str(
        &response
            .into_string()
            .map_err(|error| format!("starting OpenAI OAuth: {error}"))?,
    )
    .map_err(|error| format!("starting OpenAI OAuth: {error}"))?;
    let device_auth_id = required_text(&value, "device_auth_id")?;
    let user_code = value
        .get("user_code")
        .or_else(|| value.get("usercode"))
        .and_then(Value::as_str)
        .ok_or_else(|| "OpenAI OAuth response has no user code".to_owned())?
        .to_owned();
    let interval = value
        .get("interval")
        .and_then(|value| {
            value
                .as_str()
                .and_then(|text| text.parse().ok())
                .or_else(|| value.as_u64())
        })
        .unwrap_or(5)
        .max(1);
    let verification_url = format!("{OPENAI_ISSUER}/codex/device");
    events
        .send(Message::Auth {
            state: "pending",
            url: Some(verification_url),
            code: Some(user_code.clone()),
            message: None,
        })
        .map_err(|_| "the compositor stopped during OpenAI login".to_owned())?;

    let started = std::time::Instant::now();
    let code = loop {
        let response = agent
            .post(&format!("{OPENAI_ISSUER}/api/accounts/deviceauth/token"))
            .set("Content-Type", "application/json")
            .send_string(&format!(
                r#"{{"device_auth_id":{},"user_code":{}}}"#,
                serde_json::to_string(&device_auth_id).unwrap(),
                serde_json::to_string(&user_code).unwrap()
            ));
        match response {
            Ok(response) => {
                let body = response.into_string().map_err(|error| error.to_string())?;
                break serde_json::from_str::<Value>(&body)
                    .map_err(|error| format!("finishing OpenAI OAuth: {error}"))?;
            }
            Err(ureq::Error::Status(403 | 404, _))
                if started.elapsed() < Duration::from_secs(15 * 60) =>
            {
                std::thread::sleep(Duration::from_secs(interval));
            }
            Err(error) => return Err(format!("finishing OpenAI OAuth: {error}")),
        }
    };

    let response = agent
        .post(&format!("{OPENAI_ISSUER}/oauth/token"))
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("code", required_text(&code, "authorization_code")?.as_str()),
            (
                "redirect_uri",
                &format!("{OPENAI_ISSUER}/deviceauth/callback"),
            ),
            ("client_id", OPENAI_CLIENT_ID),
            (
                "code_verifier",
                required_text(&code, "code_verifier")?.as_str(),
            ),
        ])
        .map_err(|error| format!("exchanging OpenAI OAuth code: {error}"))?;
    let tokens: Value = serde_json::from_str(
        &response
            .into_string()
            .map_err(|error| format!("exchanging OpenAI OAuth code: {error}"))?,
    )
    .map_err(|error| format!("exchanging OpenAI OAuth code: {error}"))?;
    let id_token = required_text(&tokens, "id_token")?;
    save_openai_auth(&OpenAiAuth {
        access_token: required_text(&tokens, "access_token")?,
        refresh_token: required_text(&tokens, "refresh_token")?,
        account_id: account_id(&id_token),
        id_token,
    })
}

fn required_text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("OpenAI OAuth response has no {key}"))
}

fn parse(provider: crate::config::AiProvider, body: &str) -> Result<Usage, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    match provider {
        crate::config::AiProvider::Claude => {
            let primary = number(&value, &["five_hour", "utilization"]);
            let secondary = number(&value, &["seven_day", "utilization"]);
            if primary.is_none() && secondary.is_none() {
                return Err("Claude usage response has no usage windows".to_owned());
            }
            Ok(Usage {
                provider: provider.name().to_owned(),
                primary,
                secondary,
                remaining: None,
                primary_seconds: Some(5 * 60 * 60),
                secondary_seconds: Some(7 * 24 * 60 * 60),
                primary_reset: text(&value, &["five_hour", "resets_at"]),
                secondary_reset: text(&value, &["seven_day", "resets_at"]),
            })
        }
        crate::config::AiProvider::Openai => {
            let limits = value.get("rate_limit").unwrap_or(&value);
            let primary = number(limits, &["primary_window", "used_percent"]);
            let secondary = number(limits, &["secondary_window", "used_percent"]);
            if primary.is_none() && secondary.is_none() {
                return Err("OpenAI usage response has no usage windows".to_owned());
            }
            Ok(Usage {
                provider: provider.name().to_owned(),
                primary,
                secondary,
                remaining: None,
                primary_seconds: integer(limits, &["primary_window", "limit_window_seconds"]),
                secondary_seconds: integer(limits, &["secondary_window", "limit_window_seconds"]),
                primary_reset: text(limits, &["primary_window", "reset_at"]),
                secondary_reset: text(limits, &["secondary_window", "reset_at"]),
            })
        }
        crate::config::AiProvider::Openrouter => {
            let data = value.get("data").unwrap_or(&value);
            let total = number(data, &["total_credits"]);
            let used = number(data, &["total_usage"]);
            let remaining = total
                .zip(used)
                .map(|(total, used)| (total - used).max(0.0))
                .ok_or_else(|| "OpenRouter usage response has no credit totals".to_owned())?;
            Ok(Usage {
                provider: provider.name().to_owned(),
                primary: None,
                secondary: None,
                remaining: Some(remaining),
                primary_seconds: None,
                secondary_seconds: None,
                primary_reset: None,
                secondary_reset: None,
            })
        }
    }
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |value, key| value.get(key))
}

fn number(value: &Value, path: &[&str]) -> Option<f64> {
    at(value, path)?.as_f64().filter(|value| value.is_finite())
}

fn integer(value: &Value, path: &[&str]) -> Option<u64> {
    at(value, path)?.as_u64()
}

fn text(value: &Value, path: &[&str]) -> Option<String> {
    let value = at(value, path)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiProvider;

    #[test]
    fn mounted_openai_auth_is_updated_in_place_and_kept_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = std::env::temp_dir().join(format!(
            "viewport-mounted-openai-auth-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let replace_error = std::io::Error::from_raw_os_error(libc::EBUSY);

        update_mounted_auth(&path, br#"{"access_token":"token"}"#, &replace_error).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"access_token":"token"}"#
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn openai_account_id_comes_from_the_namespaced_id_token_claim() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-123" }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        assert_eq!(
            account_id(&format!("header.{payload}.signature")).as_deref(),
            Some("acct-123")
        );
    }

    #[test]
    fn claude_windows_are_normalized() {
        let usage = parse(
            AiProvider::Claude,
            r#"{"five_hour":{"utilization":42.5,"resets_at":"2026-08-26T12:00:00Z"},"seven_day":{"utilization":18}}"#,
        )
        .unwrap();
        assert_eq!(usage.primary, Some(42.5));
        assert_eq!(usage.secondary, Some(18.0));
        assert_eq!(usage.primary_reset.as_deref(), Some("2026-08-26T12:00:00Z"));
    }

    #[test]
    fn openai_windows_are_normalized() {
        let usage = parse(
            AiProvider::Openai,
            r#"{"rate_limit":{"primary_window":{"used_percent":12,"reset_at":1787745600},"secondary_window":{"used_percent":34}}}"#,
        )
        .unwrap();
        assert_eq!(usage.primary, Some(12.0));
        assert_eq!(usage.secondary, Some(34.0));
        assert_eq!(usage.primary_reset.as_deref(), Some("1787745600"));
    }

    #[test]
    fn openrouter_reports_credits_left() {
        let usage = parse(
            AiProvider::Openrouter,
            r#"{"data":{"total_credits":20,"total_usage":7.25}}"#,
        )
        .unwrap();
        assert_eq!(usage.remaining, Some(12.75));
    }

    #[test]
    fn successful_bodies_without_usage_are_not_good_samples() {
        for provider in [
            AiProvider::Claude,
            AiProvider::Openai,
            AiProvider::Openrouter,
        ] {
            assert!(parse(provider, r#"{"error":"schema changed"}"#).is_err());
        }
    }
}
