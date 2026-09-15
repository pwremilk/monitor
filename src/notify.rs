//! Notifications: one message per node going offline or coming back, through a
//! channel the operator picked in the panel.
//!
//! Ported from komari's `utils/messageSender` and `utils/notifier`, and
//! deliberately shaped like them -- an event, a template, one HTTP request per
//! channel -- so templates an operator already wrote keep working and so the
//! grace-period semantics stay the ones the reference implementation was tested
//! against.
//!
//! Four things live here: the event model and its template, the channels, the
//! per-node state that decides whether a disconnect is an outage, and the three
//! routes the panel's notification card talks to. The sockets only report
//! connects and teardowns; nothing in this module touches them.

use std::time::Duration;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Local, Utc};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::warn;

use crate::api::Admin;
use crate::{App, Shared};

// ---- settings ----

/// Every setting this subsystem reads or writes, the one vocabulary the reader,
/// the write path and the panel all check against: a key spelled differently in
/// two of the three is a setting that silently does nothing.
pub const KEYS: [&str; 19] = [
    "notify_enabled",
    "notify_provider",
    "notify_notify_on_online",
    "notify_grace_seconds",
    "notify_template",
    "notify_webhook_url",
    "notify_webhook_method",
    "notify_webhook_headers",
    "notify_webhook_username",
    "notify_webhook_password",
    "notify_telegram_token",
    "notify_telegram_chat",
    "notify_telegram_endpoint",
    "notify_bark_url",
    "notify_bark_key",
    "notify_bark_level",
    "notify_serverchan_key",
    "notify_serverchan_endpoint",
    "notify_javascript_script",
];

/// The keys holding a credential. Write-only, as `github_client_secret` is:
/// `settings` answers with `<key>_set` instead of the value, because a secret
/// the browser can read is a secret every XSS and every screenshot has.
pub const SECRETS: [&str; 5] = [
    "notify_webhook_url",
    "notify_webhook_password",
    "notify_telegram_token",
    "notify_bark_key",
    "notify_serverchan_key",
];

/// Default grace period, in seconds. Five minutes is komari's default and about
/// as long as a machine reboot takes, which is the case the period exists for.
const DEFAULT_GRACE_SECONDS: i64 = 300;

/// The longest grace period the panel offers. A day is far past the point where
/// "offline" has any meaning, and it is an upper bound on a task that sleeps.
const MAX_GRACE_SECONDS: i64 = 86_400;

const DEFAULT_TELEGRAM_ENDPOINT: &str = "https://api.telegram.org/bot";
const DEFAULT_BARK_URL: &str = "https://api.day.app";

/// Where ServerChan is reached by default. Configurable since the reference
/// implementation takes a complete interface address: a self-hosted mirror, and
/// any end-to-end test of this channel, needs somewhere else to send to.
const DEFAULT_SERVERCHAN_ENDPOINT: &str = "https://sctapi.ftqq.com";

/// The levels a Bark push may carry, from the Bark app's own vocabulary.
const BARK_LEVELS: [&str; 4] = ["active", "timeSensitive", "passive", "critical"];

/// The methods a webhook may use. The write path and the reader both check
/// against this pair, and it is the same pair the request builder understands.
const WEBHOOK_METHODS: [&str; 2] = ["GET", "POST"];

// ---- the event ----

/// What happened to a node.
///
/// The serialised names are a contract: they appear in the template, in the
/// title of every message and in the webhook body, so an operator's filter on
/// their side matches what the panel showed them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    #[serde(rename = "Offline")]
    Offline,
    #[serde(rename = "Online")]
    Online,
    #[serde(rename = "Test")]
    Test,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "Offline",
            Self::Online => "Online",
            Self::Test => "Test",
        }
    }

    /// The prefix on every message, and the fallback when the template has no
    /// `{{emoji}}` at all.
    fn emoji(self) -> &'static str {
        match self {
            // A red circle for a node that has gone, green for one that has
            // returned, a bell for a test that is neither.
            Self::Offline => "🔴",
            Self::Online => "🟢",
            Self::Test => "🔔",
        }
    }
}

/// One notification, before any channel has seen it.
#[derive(Debug, Clone)]
pub struct EventMessage {
    pub event: Event,
    /// The node's name, or the hub itself for a test.
    pub node: String,
    /// A free-form line. Empty for the offline and online events, whose meaning
    /// the event name already carries.
    pub message: String,
    pub time: DateTime<Utc>,
    pub emoji: String,
}

/// The template a hub uses until an operator writes another one. Word for word
/// komari's, so a message already arriving from one of these hubs does not
/// change the moment this one takes over.
pub const DEFAULT_TEMPLATE: &str =
    "{{emoji}}{{emoji}}\n事件: {{event}}\n节点: {{node}}\n信息: {{message}}\n时间: {{time}}";

pub fn offline_event(node: &str) -> EventMessage {
    EventMessage::new(Event::Offline, node, "")
}

pub fn online_event(node: &str) -> EventMessage {
    EventMessage::new(Event::Online, node, "")
}

/// The panel's test message. It says so in the body as well as in the event
/// name: the notification arrives on a phone with no context beside it.
pub fn test_event() -> EventMessage {
    EventMessage::new(Event::Test, "monitor-hub", "这是一条测试通知，收到即表示通知渠道可用")
}

impl EventMessage {
    fn new(event: Event, node: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            event,
            node: node.into(),
            message: message.into(),
            time: Utc::now(),
            emoji: event.emoji().to_owned(),
        }
    }
}

/// Substitutes the placeholders a template may carry.
///
/// A name this renderer does not know is left in place, braces and all: the
/// template is the operator's text, and a visible `{{typo}}` in the message is
/// a fault they can see and fix, while deleting the word silently is not.
pub fn render(template: &str, event: &EventMessage) -> String {
    let values = [
        ("event", event.event.as_str().to_owned()),
        ("node", event.node.clone()),
        ("message", event.message.clone()),
        ("time", format_time(event.time)),
        ("emoji", event.emoji.clone()),
    ];
    let mut out = template.to_owned();
    for (name, value) in values {
        out = out.replace(&format!("{{{{{name}}}}}"), &value);
    }
    out
}

/// Local time, as komari renders it. The operator reads the notification on a
/// phone in their own timezone, and the hub normally runs in it too; an ISO
/// instant would have them doing the arithmetic.
fn format_time(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---- the channel ----

/// The channels the panel offers, and the values `notify_provider` stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Nothing is sent. The state a hub is in before anyone chose a channel.
    None,
    Webhook,
    Telegram,
    Bark,
    ServerChan,
    /// An operator's own script, run in the embedded engine. See `notify_js`.
    JavaScript,
}

impl Provider {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "none" => Self::None,
            "webhook" => Self::Webhook,
            "telegram" => Self::Telegram,
            "bark" => Self::Bark,
            "serverchan" => Self::ServerChan,
            "javascript" => Self::JavaScript,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Webhook => "webhook",
            Self::Telegram => "telegram",
            Self::Bark => "bark",
            Self::ServerChan => "serverchan",
            Self::JavaScript => "javascript",
        }
    }
}

/// The notification settings as they are in effect, each default applied.
///
/// Gathered in one place so the automatic path, the panel's test button and the
/// form it renders all agree on what is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub provider: Provider,
    pub notify_on_online: bool,
    pub grace_seconds: i64,
    pub template: String,
    pub webhook_url: String,
    pub webhook_method: String,
    pub webhook_headers: String,
    pub webhook_username: String,
    pub webhook_password: String,
    pub telegram_token: String,
    pub telegram_chat: String,
    pub telegram_endpoint: String,
    pub bark_url: String,
    pub bark_key: String,
    pub bark_level: String,
    pub serverchan_key: String,
    pub serverchan_endpoint: String,
    /// The JavaScript channel's program, as the operator wrote it. Empty means
    /// the channel has nothing to run, which is reported like any other
    /// unconfigured channel rather than silently succeeding.
    pub javascript_script: String,
}

impl Config {
    /// Reads every setting through `get`, which is the settings table.
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Self {
        let text = |key: &str| get(key).unwrap_or_default();
        let flag = |key: &str, default: bool| match text(key).as_str() {
            "on" => true,
            "off" => false,
            _ => default,
        };
        let or_default = |key: &str, default: &str| {
            let value = text(key);
            if value.trim().is_empty() {
                default.to_owned()
            } else {
                value
            }
        };
        Self {
            enabled: flag("notify_enabled", false),
            // An unknown value is no channel rather than an arbitrary one: the
            // write path refuses it, so this is a row edited by hand.
            provider: Provider::parse(&text("notify_provider")).unwrap_or(Provider::None),
            notify_on_online: flag("notify_notify_on_online", true),
            grace_seconds: grace_seconds(&text("notify_grace_seconds")),
            template: or_default("notify_template", DEFAULT_TEMPLATE),
            webhook_url: text("notify_webhook_url"),
            // Out of the vocabulary the writer and the request builder both
            // understand, so a `post`, a `patch` or anything else lands on POST
            // instead of coming back to the panel as a value that cannot be
            // saved. `deliver` picks POST for everything but GET either way.
            webhook_method: one_of(&text("notify_webhook_method"), &WEBHOOK_METHODS, "POST"),
            webhook_headers: text("notify_webhook_headers"),
            webhook_username: text("notify_webhook_username"),
            webhook_password: text("notify_webhook_password"),
            telegram_token: text("notify_telegram_token"),
            telegram_chat: text("notify_telegram_chat"),
            telegram_endpoint: or_default("notify_telegram_endpoint", DEFAULT_TELEGRAM_ENDPOINT),
            bark_url: or_default("notify_bark_url", DEFAULT_BARK_URL),
            bark_key: text("notify_bark_key"),
            // The app's four levels or nothing at all: a stored value outside
            // them is one no push should carry, and one the panel's select
            // cannot even display.
            bark_level: one_of(&text("notify_bark_level"), &BARK_LEVELS, ""),
            serverchan_key: text("notify_serverchan_key"),
            serverchan_endpoint: or_default("notify_serverchan_endpoint", DEFAULT_SERVERCHAN_ENDPOINT),
            javascript_script: text("notify_javascript_script"),
        }
    }

    /// The settings as the panel reads them, defaults applied.
    ///
    /// Effective values rather than stored ones, because the form echoes back
    /// what it was given: an unset channel shown as an empty select asks the
    /// operator a question the hub has already answered, and an empty template
    /// would be saved as the empty string the reader treats as "use the
    /// default".
    pub fn readable(&self) -> Vec<(&'static str, String)> {
        let on_off = |value: bool| if value { "on".to_owned() } else { "off".to_owned() };
        vec![
            ("notify_enabled", on_off(self.enabled)),
            ("notify_provider", self.provider.as_str().to_owned()),
            ("notify_notify_on_online", on_off(self.notify_on_online)),
            ("notify_grace_seconds", self.grace_seconds.to_string()),
            ("notify_template", self.template.clone()),
            ("notify_webhook_method", self.webhook_method.clone()),
            ("notify_webhook_headers", self.webhook_headers.clone()),
            ("notify_webhook_username", self.webhook_username.clone()),
            ("notify_telegram_chat", self.telegram_chat.clone()),
            ("notify_telegram_endpoint", self.telegram_endpoint.clone()),
            ("notify_bark_url", self.bark_url.clone()),
            ("notify_bark_level", self.bark_level.clone()),
            ("notify_serverchan_endpoint", self.serverchan_endpoint.clone()),
            ("notify_javascript_script", self.javascript_script.clone()),
        ]
    }
}

pub fn config(app: &App) -> Config {
    Config::load(|key| app.db.get(key))
}

/// The grace period, clamped to the range the panel offers.
///
/// A stored value outside it means the row was written by hand or by an older
/// hub. Clamping keeps `0` meaningful -- notify at once -- and an absurd figure
/// from becoming a task that sleeps for years.
fn grace_seconds(value: &str) -> i64 {
    value.trim().parse::<i64>().map_or(DEFAULT_GRACE_SECONDS, |s| s.clamp(0, MAX_GRACE_SECONDS))
}

/// Reads a setting whose value has to come from a fixed list, falling back to
/// `default` for anything outside it.
///
/// The reader's output is what the panel shows and echoes back on the next save,
/// so a stored value the write path refuses is not merely a display problem: the
/// whole card becomes unsavable, and the operator meets a 400 naming a field they
/// never touched. Rows can arrive from a hand edit, an older hub or another
/// implementation, which is exactly why the reader has to answer with something
/// the writer accepts. The list's own spelling wins, so `post` reads back as
/// `POST` whether or not the row was written by the panel.
fn one_of(value: &str, allowed: &[&str], default: &str) -> String {
    let value = value.trim();
    allowed
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(value))
        .map_or_else(|| default.to_owned(), |found| (*found).to_owned())
}

/// Why one notification setting cannot be stored, or `None` when it can.
///
/// Beside the reader rather than in the handler, so a value that cannot be used
/// is refused where the values it must match are written down. A stored
/// mismatch -- a URL that does not parse, headers that are not JSON -- surfaces
/// only as a notification that never arrives, which is the one failure mode
/// nobody looks for.
pub fn setting_error(key: &str, value: &str) -> Option<String> {
    match key {
        "notify_enabled" | "notify_notify_on_online" if !matches!(value, "on" | "off") => {
            Some(format!("{key} must be on or off"))
        }
        "notify_provider" if Provider::parse(value).is_none() => Some(
            "the notification channel must be one of none, webhook, telegram, bark, serverchan, javascript"
                .into(),
        ),
        "notify_grace_seconds"
            if !value.trim().parse::<i64>().is_ok_and(|s| (0..=MAX_GRACE_SECONDS).contains(&s)) =>
        {
            Some("the notification grace period must be a whole number of seconds from 0 to 86400".into())
        }
        "notify_webhook_method"
            if !(value.trim().is_empty()
                || WEBHOOK_METHODS.iter().any(|method| method.eq_ignore_ascii_case(value.trim()))) =>
        {
            Some("the webhook method must be GET or POST".into())
        }
        "notify_webhook_headers" => parse_headers(value).err().map(|e| format!("{e:#}")),
        "notify_webhook_url"
        | "notify_telegram_endpoint"
        | "notify_bark_url"
        | "notify_serverchan_endpoint" => url_error(value).map(str::to_owned),
        "notify_bark_level" if !(value.trim().is_empty() || BARK_LEVELS.contains(&value.trim())) => {
            Some(format!("the Bark level must be empty or one of {}", BARK_LEVELS.join(", ")))
        }
        _ => None,
    }
}

/// Whether a URL the hub is asked to send to is one it can speak.
///
/// Empty is allowed: the channel then uses its default. Plain `http://` is
/// allowed too, unlike the GitHub proxy setting, because nothing is installed
/// from this address -- the hub only posts a message to it -- and a webhook or
/// a Bark server on a private network is an ordinary deployment.
fn url_error(value: &str) -> Option<&'static str> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if !(value.starts_with("http://") || value.starts_with("https://")) {
        return Some("the address must start with http:// or https://");
    }
    // Parsed as well: `https://` alone satisfies the prefix above and is not a
    // URL, and the failure would otherwise appear as a channel that never
    // delivers anything.
    Url::parse(value).err().map(|_| "the address must be a valid URL")
}

/// Reads the custom headers, which the panel takes as a JSON object.
///
/// Names and values are checked here rather than at the request, where
/// `reqwest` would reject them: the same parse runs when the setting is saved,
/// so an operator learns about a bad header from the form instead of from a
/// notification that never arrived.
pub fn parse_headers(raw: &str) -> Result<Vec<(HeaderName, HeaderValue)>> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let object: serde_json::Map<String, Value> = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("custom headers must be a JSON object: {e}"))?;
    let mut headers = Vec::with_capacity(object.len());
    for (name, value) in object {
        let value = match value {
            Value::String(value) => value,
            other => anyhow::bail!("custom header {name} must be a string, not {other}"),
        };
        headers.push((
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| anyhow::anyhow!("custom header {name} is not a valid header name: {e}"))?,
            HeaderValue::from_str(&value)
                .map_err(|_| anyhow::anyhow!("custom header {name} has a value HTTP cannot carry"))?,
        ));
    }
    Ok(headers)
}

// ---- sending ----

/// How many times a failing channel is tried. A channel down for a few seconds
/// -- a restarting bot, a webhook behind a reloading proxy -- is back within
/// them, and a notification nobody receives has no second chance.
const ATTEMPTS: usize = 3;

/// Runs `attempt` up to [`ATTEMPTS`] times, returning the last failure.
///
/// Nothing is delayed between attempts, as in the reference implementation: the
/// event has already happened, so holding the task only postpones a warning the
/// operator has not seen yet.
async fn retry<F, Fut>(mut attempt: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut last = Ok(());
    for _ in 0..ATTEMPTS {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(e) => last = Err(e),
        }
    }
    last
}

/// Sends one event through the configured channel, retrying a failure.
///
/// [`Config::enabled`] is the caller's business, not this function's: the
/// automatic path checks it before calling, while the panel's test button
/// deliberately ignores it -- a channel is tested before it is switched on.
pub async fn send(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    match retry(|| deliver(http, config, event)).await {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!(
                "notification {} for {} was not delivered after {ATTEMPTS} attempts: {e:#}",
                event.event.as_str(),
                event.node
            );
            Err(e)
        }
    }
}

/// One attempt, through whichever channel is configured.
async fn deliver(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    match config.provider {
        // Not a failure: a hub with no channel configured is the state every
        // hub starts in, and the events it would have sent are not errors.
        Provider::None => Ok(()),
        Provider::Webhook => webhook(http, config, event).await,
        Provider::Telegram => telegram(http, config, event).await,
        Provider::Bark => bark(http, config, event).await,
        Provider::ServerChan => serverchan(http, config, event).await,
        // The only channel that runs code rather than building a request, so it
        // lives in its own module: the engine, the limits it runs under and the
        // bridge to `fetch` are all its own business. See `notify_js`.
        Provider::JavaScript => crate::notify_js::send(http, config, event).await,
    }
}

/// The panel's test button: one `Test` event through the channel as configured.
pub async fn test(app: &App) -> Result<()> {
    let config = config(app);
    if config.provider == Provider::None {
        // The `none` channel accepts every event, which is right for a node
        // going offline and wrong for this button: a test that reports success
        // while no message was sent is worse than no button at all.
        anyhow::bail!("no notification channel is selected");
    }
    send(&app.http, &config, &test_event()).await
}

/// A request failure with the URL removed.
///
/// `reqwest`'s own message quotes the URL, and three of these channels carry a
/// credential inside it: Telegram's bot token, ServerChan's sendkey, a webhook
/// whose path is itself the secret. The log and the panel get the reason
/// without the secret.
fn http_error(context: &str, e: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!("{context}: {}", e.without_url())
}

/// Bounds what a failing endpoint can put into a log line and the panel's
/// toast: a misconfigured URL can answer with a whole HTML page.
fn truncate(body: &str) -> String {
    const LIMIT: usize = 200;
    let body = body.trim();
    match body.char_indices().nth(LIMIT) {
        Some((at, _)) => format!("{}…", &body[..at]),
        None => body.to_owned(),
    }
}

/// What a POST webhook receives, and the object a JavaScript channel's
/// `sendEvent` is handed.
///
/// One builder for both, so the fields an operator's own script reads are the
/// ones their webhook already receives. The names are the template's, so an
/// operator who has already wired up an endpoint for one of these hubs does not
/// meet a second vocabulary. `time` is the readable one the notification shows;
/// `timestamp` is the same instant for anything automated.
pub(crate) fn event_object(event: &EventMessage) -> Value {
    json!({
        "event": event.event.as_str(),
        "title": event.event.as_str(),
        "node": event.node,
        "message": event.message,
        "emoji": event.emoji,
        "time": format_time(event.time),
        "timestamp": event.time.timestamp(),
    })
}

/// The same fields as query parameters, for a GET webhook.
fn webhook_query(url: &str, event: &EventMessage) -> Result<Url> {
    let mut url =
        Url::parse(url.trim()).map_err(|e| anyhow::anyhow!("the webhook URL cannot be used: {e}"))?;
    url.query_pairs_mut().extend_pairs([
        ("event", event.event.as_str()),
        ("title", event.event.as_str()),
        ("node", event.node.as_str()),
        ("message", event.message.as_str()),
        ("emoji", event.emoji.as_str()),
        ("time", &format_time(event.time)),
    ]);
    Ok(url)
}

/// Builds the webhook request without sending it, so what goes on the wire can
/// be examined in a test.
fn webhook_request(http: &Client, config: &Config, event: &EventMessage) -> Result<RequestBuilder> {
    if config.webhook_url.trim().is_empty() {
        anyhow::bail!("the webhook URL is not configured");
    }
    let mut request = if config.webhook_method.trim().eq_ignore_ascii_case("GET") {
        http.get(webhook_query(&config.webhook_url, event)?)
    } else {
        // A POST when the method is empty or unreadable: it carries a body, and
        // the alternative silently drops the event into a query string.
        http.post(config.webhook_url.trim()).json(&event_object(event))
    };
    for (name, value) in parse_headers(&config.webhook_headers)? {
        request = request.header(name, value);
    }
    // The password may legitimately be empty -- some endpoints authenticate on
    // the username alone -- while a username that is absent means no auth.
    if !config.webhook_username.is_empty() {
        request = request.basic_auth(&config.webhook_username, Some(&config.webhook_password));
    }
    Ok(request)
}

async fn webhook(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    let response = webhook_request(http, config, event)?
        .send()
        .await
        .map_err(|e| http_error("the webhook is unreachable", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the webhook answered {status}: {}", truncate(&body));
    }
    Ok(())
}

/// The base URL of a channel, defaulted and without a trailing slash: the paths
/// below are appended with one, and `//sendMessage` is not the same URL to every
/// server.
fn base_url(value: &str, default: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        default.to_owned()
    } else {
        value.trim_end_matches('/').to_owned()
    }
}

async fn telegram(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    if config.telegram_token.trim().is_empty() {
        anyhow::bail!("the Telegram bot token is not configured");
    }
    if config.telegram_chat.trim().is_empty() {
        anyhow::bail!("the Telegram chat id is not configured");
    }
    let url = format!(
        "{}{}/sendMessage",
        base_url(&config.telegram_endpoint, DEFAULT_TELEGRAM_ENDPOINT),
        config.telegram_token.trim()
    );
    let response = http
        .post(url)
        .form(&[
            ("chat_id", config.telegram_chat.trim()),
            ("text", render(&config.template, event).as_str()),
            ("parse_mode", "HTML"),
        ])
        .send()
        .await
        .map_err(|e| http_error("the Telegram API is unreachable", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    // Telegram explains itself in `description` even when the status is 200, so
    // the body is read either way.
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let description = parsed.get("description").and_then(Value::as_str).unwrap_or(body.as_str());
    if !status.is_success() {
        anyhow::bail!("the Telegram API answered {status}: {}", truncate(description));
    }
    if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
        anyhow::bail!("the Telegram API rejected the message: {}", truncate(description));
    }
    Ok(())
}

async fn bark(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    if config.bark_key.trim().is_empty() {
        anyhow::bail!("the Bark device key is not configured");
    }
    let mut payload = json!({
        "body": render(&config.template, event),
        "device_key": config.bark_key.trim(),
        "title": event.event.as_str(),
    });
    if !config.bark_level.trim().is_empty() {
        payload["level"] = json!(config.bark_level.trim());
    }
    let url = format!("{}/push", base_url(&config.bark_url, DEFAULT_BARK_URL));
    let response = http
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| http_error("the Bark server is unreachable", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the Bark server answered {status}: {}", truncate(&body));
    }
    // A self-hosted Bark forwards to whatever answers on that path, and some
    // deployments reply with plain text. Only a body that parses and carries a
    // verdict is worth refusing.
    if let Ok(parsed) = serde_json::from_str::<Value>(&body) {
        if let Some(code) = parsed.get("code").and_then(Value::as_i64) {
            if code != 200 {
                let message = parsed.get("message").and_then(Value::as_str).unwrap_or_default();
                anyhow::bail!("the Bark server refused the push (code {code}): {}", truncate(message));
            }
        }
    }
    Ok(())
}

async fn serverchan(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    if config.serverchan_key.trim().is_empty() {
        anyhow::bail!("the ServerChan sendkey is not configured");
    }
    let url = format!(
        "{}/{}.send",
        base_url(&config.serverchan_endpoint, DEFAULT_SERVERCHAN_ENDPOINT),
        config.serverchan_key.trim()
    );
    let response = http
        .post(url)
        .form(&[("title", event.event.as_str()), ("desp", render(&config.template, event).as_str())])
        .send()
        .await
        .map_err(|e| http_error("the ServerChan API is unreachable", e))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the ServerChan API answered {status}: {}", truncate(&body));
    }
    // This API reports failures inside a 200: `code` 0 is success.
    if let Ok(parsed) = serde_json::from_str::<Value>(&body) {
        if let Some(code) = parsed.get("code").and_then(Value::as_i64) {
            if code != 0 {
                let message = parsed.get("message").and_then(Value::as_str).unwrap_or_default();
                anyhow::bail!("the ServerChan API refused the message (code {code}): {}", truncate(message));
            }
        }
    }
    Ok(())
}

// ---- offline and online ----

/// What the hub remembers about one node between its connections.
///
/// In memory, and reset by a hub restart: the reference implementation keeps
/// the same state in its process, and a restart is itself a gap that no
/// notification could describe accurately.
#[derive(Debug, Clone)]
pub struct NodeState {
    /// The session this state attributes the node to: the last one `connect`
    /// saw, or, once a teardown has armed the grace period, the session whose
    /// absence armed it. Numbered from `agent_ws::FIRST_SESSION`, so 0 is a node
    /// the hub has never met.
    pub connection_id: u64,
    /// The node has connected since the hub started. Its first connection is
    /// the hub meeting the node, not the node coming back.
    pub is_first_connection: bool,
    /// When a disconnect began its grace period, while one is running.
    pub pending_offline_since: Option<DateTime<Utc>>,
    /// Whether a connection is believed to exist. Cleared when an offline
    /// notification is sent, which is what makes the next connect a return
    /// rather than a repeat of something already reported.
    pub is_conn_exist: bool,
}

impl Default for NodeState {
    fn default() -> Self {
        Self {
            connection_id: 0,
            is_first_connection: true,
            pending_offline_since: None,
            is_conn_exist: false,
        }
    }
}

impl NodeState {
    /// Records a connection and reports whether it is a return worth
    /// announcing.
    ///
    /// Called for every connection whether or not notifications are enabled:
    /// this state is what tells an outage from a blip, and a hub that began
    /// tracking only once the feature was switched on would report the first
    /// reconnect afterwards as a return from an outage nobody was told about.
    pub fn connect(&mut self, connection_id: u64) -> bool {
        self.connection_id = connection_id;
        if self.is_first_connection {
            // Every node is new here: a hub announcing them all would send one
            // message per node on every restart.
            self.is_first_connection = false;
            self.pending_offline_since = None;
            self.is_conn_exist = true;
            return false;
        }
        // Back inside the grace period. The disconnect is cancelled, and the
        // node is not announced either: nothing happened worth a message.
        if self.pending_offline_since.take().is_some() {
            return false;
        }
        // Already connected -- a second socket for the same node, or a teardown
        // that was never reported.
        if self.is_conn_exist {
            return false;
        }
        self.is_conn_exist = true;
        true
    }

    /// Starts the grace period for a node that has gone, reporting whether this
    /// teardown is the one that owns it.
    ///
    /// The session is recorded rather than compared against the one `connect`
    /// last saw. Two sockets on one node can end in either order -- the newer
    /// one can go first, with the socket it replaced still reporting -- and the
    /// caller reports a teardown only once the node's last session has ended
    /// (see `agent_ws::release`), so the number arriving here is not always the
    /// newest. Comparing would drop the notification for a node that is
    /// genuinely gone. What such a comparison protected -- a node that is
    /// reporting again -- is decided in `disconnected` against the node's live
    /// entry, which is what the reconnect installs before this state is asked
    /// anything.
    ///
    /// The number is recorded by the teardown that arms the period and by no
    /// other. A teardown that finds one already running belongs to a different
    /// session -- the report of a socket the node's departure left behind --
    /// and taking the period over would leave it attributed to a session no
    /// task is waiting to ask about: `offline_due` for the session that armed
    /// it would then refuse a period that is genuinely owed. See the test
    /// `a_stale_teardown_leaves_a_running_grace_period_to_the_session_that_armed_it`.
    pub fn disconnect(&mut self, connection_id: u64, now: DateTime<Utc>) -> bool {
        // One period per absence: a second teardown for the same node finds one
        // already running, and adding to it would only move the deadline.
        if self.pending_offline_since.is_some() {
            return false;
        }
        self.connection_id = connection_id;
        self.pending_offline_since = Some(now);
        true
    }

    /// The grace period has elapsed: reports whether the offline notification
    /// is still owed, marking the node offline when it is.
    ///
    /// `false` means the node returned during the grace period, or the period
    /// belongs to a session this question is not about, and there is nothing to
    /// send.
    ///
    /// Either way the period is settled here, which is why the pending instant
    /// is taken before anything can refuse it. One left behind is left behind
    /// for good: `connect` would swallow it and report the node not at all,
    /// `disconnect` would arm nothing further, and the node would never be
    /// reported again. A question that has been asked is an answer.
    pub fn offline_due(&mut self, connection_id: u64) -> bool {
        if self.pending_offline_since.take().is_none() {
            return false;
        }
        if connection_id != self.connection_id {
            return false;
        }
        self.is_conn_exist = false;
        true
    }
}

/// The name the operator gave a node, falling back to its number: a
/// notification naming a node by id is still actionable, while one that fails
/// to send because the row has been deleted is not.
fn node_name(app: &App, node_id: i64) -> String {
    app.db
        .node(node_id)
        .ok()
        .flatten()
        .map(|node| node.name)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("node {node_id}"))
}

/// Records a node's connection and, when one is owed, announces it.
///
/// The state moves here, on the socket's own path, and only the sending is
/// spawned: the send reads settings from SQLite, which a restore or a vacuum
/// can hold for seconds, and a socket that waited on that would stop reading
/// its reports.
pub fn connected(app: &Shared, node_id: i64, connection_id: u64) {
    let announce = {
        let mut states = app.notify.lock().unwrap_or_else(|e| e.into_inner());
        states.entry(node_id).or_default().connect(connection_id)
    };
    if !announce {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let config = config(&app);
        if !config.enabled || !config.notify_on_online {
            return;
        }
        // A channel that fails is reported by `send` itself, naming the event and
        // the node; there is nothing this task could add to that.
        let _ = send(&app.http, &config, &online_event(&node_name(&app, node_id))).await;
    });
}

/// Arms the grace period for a node that has gone away, and sends the offline
/// notification once the period ends with the node still absent.
pub fn disconnected(app: &Shared, node_id: i64, connection_id: u64) {
    let app = app.clone();
    tokio::spawn(async move {
        // Read first, as the reference does: with notifications switched off
        // nothing is armed at all, so the reconnect that follows is judged
        // against the settings in effect when the node went away.
        let config = config(&app);
        if !config.enabled {
            return;
        }
        let armed = {
            let mut states = app.notify.lock().unwrap_or_else(|e| e.into_inner());
            // Read-only, so that a node the hub has never met has no state to
            // arm: created here it would be a node nobody has seen sending an
            // offline notification, and the number that arrives with a teardown
            // is no evidence of a connection -- 0 is what the state of a node
            // the hub has never met holds. See `agent_ws::FIRST_SESSION`.
            let Some(state) = states.get_mut(&node_id) else { return };
            // A teardown is reported once the node's last session has ended, but
            // it reaches the state only after a settings read, and a reconnect
            // can overtake it. An arm landing here would name a node that is
            // reporting again, so what is asked is the node's live entry, which
            // that reconnect installed before this point; an arm that lands
            // first is cancelled by the reconnect itself. See `connect`.
            if app.agents.read().unwrap_or_else(|e| e.into_inner()).contains_key(&node_id) {
                return;
            }
            state.disconnect(connection_id, Utc::now())
        };
        if !armed {
            return;
        }
        if config.grace_seconds > 0 {
            sleep(Duration::from_secs(config.grace_seconds as u64)).await;
        }
        let due = {
            let mut states = app.notify.lock().unwrap_or_else(|e| e.into_inner());
            // Read-only here too: the period this task armed belongs to a state
            // that exists, and asking for one that does not is not a reason to
            // create it -- there would be nothing left to send for it either.
            let Some(state) = states.get_mut(&node_id) else { return };
            state.offline_due(connection_id)
        };
        if !due {
            return;
        }
        // Logged by `send`, as above.
        let _ = send(&app.http, &config, &offline_event(&node_name(&app, node_id))).await;
    });
}

// ---- the panel's routes ----

/// The two refusals these routes answer with.
///
/// `api`'s own pair is private to that module, and these are the only two
/// responses a settings route gives.
fn bad(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

fn fail(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

/// Everything the notification card reads, as one document.
///
/// Its own route rather than a share of `api::settings`, because this module
/// owns the vocabulary: [`Config::readable`] decides which keys the form is
/// shown and with which defaults, [`setting_error`] decides which values it
/// accepts. A reader and a writer kept in two files is how those two lists
/// drift apart, and the form is the one caller that has to satisfy both.
pub async fn settings(_: Admin, State(app): State<Shared>) -> Json<Value> {
    let mut out = serde_json::Map::new();
    for (key, value) in config(&app).readable() {
        out.insert(key.to_owned(), json!(value));
    }
    // The credentials travel one way only: the form is told that one is stored,
    // so it can say "already set, leave blank to keep it", never what it is.
    // The same arrangement `github_client_secret` has in `api::settings`.
    for key in SECRETS {
        out.insert(format!("{key}_set"), json!(app.db.get(key).is_some_and(|value| !value.is_empty())));
    }
    Json(Value::Object(out))
}

/// Writes one card's worth of notification settings, or none of them.
///
/// Every key is checked before any is written, which is the semantics the rest
/// of the panel's settings have: the card is sent back whole on each save, so a
/// form that half-applied would leave a channel enabled with the address that
/// belongs to another one.
pub async fn save_settings(_: Admin, State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let Some(map) = body.as_object() else { return bad("expected an object") };
    // Two passes, and the second one cannot fail on a value the first accepted:
    // nothing reaches the table unless the whole body is storable.
    let mut accepted = Vec::with_capacity(map.len());
    for (key, value) in map {
        // A name from outside the vocabulary is a typo in the panel. Storing it
        // as any other row would make it look accepted while nothing reads it.
        if !KEYS.contains(&key.as_str()) {
            return bad(&format!("unknown setting: {key}"));
        }
        // Settings are stored as text, so the natural JSON type of a switch --
        // `{"notify_enabled": true}` -- is refused rather than skipped: a skipped
        // key reports success for a write that did not happen.
        let Some(value) = value.as_str() else { return bad(&format!("{key} must be a string")) };
        if let Some(message) = setting_error(key, value) {
            return bad(&message);
        }
        accepted.push((key.as_str(), value));
    }
    for (key, value) in accepted {
        if let Err(e) = app.db.set(key, value) {
            return fail(e);
        }
    }
    Json(json!({"ok": true})).into_response()
}

/// The panel's test button: one `Test` event through the channel as configured.
///
/// A 500 on a failure rather than a 400, because most of them are the channel's
/// own: a refused token, an unreachable host, a rejected message. The body
/// carries the reason it gave, which the panel shows as it is -- the only
/// feedback an operator gets before a node actually goes down. A hub with no
/// channel selected fails here too, instead of reporting a successful test of
/// nothing.
pub async fn notify_test(_: Admin, State(app): State<Shared>) -> Response {
    match test(&app).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => fail(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn app() -> Shared {
        std::sync::Arc::new(App::for_test(Db::open(":memory:").unwrap()))
    }

    /// A stored settings row, for the cases where the value matters.
    fn stored(key: &'static str, value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k: &str| (k == key).then(|| value.to_owned())
    }

    fn at(minute: u32) -> DateTime<Utc> {
        format!("2026-09-15T04:{minute:02}:00Z").parse().unwrap()
    }

    /// The template is the operator's text, so the two rules that decide what
    /// happens to a name the renderer does not know matter as much as the
    /// substitutions themselves.
    #[test]
    fn a_template_substitutes_its_placeholders_and_leaves_the_unknown_ones() {
        let mut event = online_event("vps-1");
        event.time = at(0);
        let time = format_time(event.time);

        assert_eq!(
            render(DEFAULT_TEMPLATE, &event),
            format!("🟢🟢\n事件: Online\n节点: vps-1\n信息: \n时间: {time}"),
            "the default template, with the empty message the online event carries"
        );
        assert!(!render(DEFAULT_TEMPLATE, &event).contains("{{"), "no known placeholder survives");

        // A name the renderer does not know is left as written: deleting the
        // operator's text silently would hide a typo they could have fixed.
        assert_eq!(render("{{event}}/{{client}}/{{node}}", &event), "Online/{{client}}/vps-1");
        // Nothing left to replace, so the literal braces are the template's.
        assert_eq!(render("no placeholders here", &event), "no placeholders here");

        // All five, in an order of the operator's choosing, and a repeated
        // placeholder is replaced every time.
        let offline = offline_event("vps-2");
        assert_eq!(
            render("{{emoji}} {{emoji}} {{message}} {{node}} {{time}}", &offline),
            format!("🔴 🔴  vps-2 {}", format_time(offline.time))
        );
    }

    /// The event name is the panel's, the template's and the webhook's shared
    /// vocabulary, so the serialised form is a contract rather than a detail.
    #[test]
    fn an_event_serialises_to_the_name_the_template_and_the_webhook_use() {
        for event in [Event::Offline, Event::Online, Event::Test] {
            let json = format!("\"{}\"", event.as_str());
            assert_eq!(serde_json::to_string(&event).unwrap(), json);
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    /// Every key the panel can read or write, so a name added to one list and
    /// forgotten in another is caught here instead of in production.
    #[test]
    fn every_notification_setting_is_either_readable_or_a_credential() {
        let readable: Vec<&str> = Config::load(|_| None).readable().into_iter().map(|(key, _)| key).collect();
        assert_eq!(readable.len() + SECRETS.len(), KEYS.len(), "a key is in neither list, or in both");
        for key in KEYS {
            assert!(key.starts_with("notify_"), "{key}");
            assert!(readable.contains(&key) || SECRETS.contains(&key), "{key} is not reachable by the panel");
        }
        // The credentials are exactly the ones the panel cannot read back.
        for key in SECRETS {
            assert!(!readable.contains(&key), "{key} must not be readable");
        }
    }

    /// A hub that was never configured still has to produce a usable
    /// configuration; the panel's form is built from it.
    #[test]
    fn an_unconfigured_hub_reads_defaults_that_send_nothing() {
        let config = Config::load(|_| None);
        assert!(!config.enabled);
        assert_eq!(config.provider, Provider::None);
        assert!(config.notify_on_online);
        assert_eq!(config.grace_seconds, 300);
        assert_eq!(config.template, DEFAULT_TEMPLATE);
        assert_eq!(config.webhook_method, "POST");
        assert_eq!(config.telegram_endpoint, DEFAULT_TELEGRAM_ENDPOINT);
        assert_eq!(config.bark_url, DEFAULT_BARK_URL);

        // Values outside what the panel offers still have to yield something
        // usable: the row can be edited by hand, and an older hub may have
        // written a shape this one does not know.
        assert_eq!(Config::load(stored("notify_grace_seconds", "999999")).grace_seconds, 86_400);
        assert_eq!(Config::load(stored("notify_grace_seconds", "-5")).grace_seconds, 0);
        assert_eq!(Config::load(stored("notify_grace_seconds", "abc")).grace_seconds, 300);
        assert_eq!(Config::load(stored("notify_provider", "sms")).provider, Provider::None);
        assert_eq!(Config::load(stored("notify_template", "  ")).template, DEFAULT_TEMPLATE);
        assert!(!Config::load(stored("notify_enabled", "off")).enabled);
        assert!(Config::load(stored("notify_enabled", "on")).enabled);
        // Upper case, so a value written by hand still matches the panel's own
        // select and the comparison that picks the request method.
        assert_eq!(Config::load(stored("notify_webhook_method", "post")).webhook_method, "POST");
    }

    /// Every value the reader hands the panel is one the write path accepts --
    /// for a row edited by hand as much as for a fresh hub.
    ///
    /// The panel shows what the reader returned and echoes it back on the next
    /// save, so a value the writer refuses makes the whole card unsavable: the
    /// operator meets a 400 naming a field they never touched, and every other
    /// edit on that card is lost with it.
    #[test]
    fn every_value_the_reader_hands_the_panel_is_one_the_writer_accepts() {
        let hand_edited = [
            // The two a hand edit can push outside their vocabulary.
            ("notify_bark_level", "loud"),
            ("notify_webhook_method", "PATCH"),
            // And the ones a hand edit only makes untidy or stale.
            ("notify_webhook_method", "post"),
            ("notify_telegram_endpoint", "https://api.telegram.org/bot/ "),
            ("notify_bark_url", "https://api.day.app/"),
            ("notify_grace_seconds", "999999"),
            ("notify_provider", "sms"),
            ("notify_webhook_headers", "{}"),
            ("notify_template", "  "),
            ("notify_serverchan_endpoint", "https://sctapi.ftqq.com/"),
            ("notify_webhook_url", "http://192.168.1.9:8080/push"),
            ("notify_javascript_script", "function sendMessage() {}"),
        ];
        for (key, value) in hand_edited {
            for (read_key, read_value) in Config::load(stored(key, value)).readable() {
                assert!(
                    setting_error(read_key, &read_value).is_none(),
                    "{key}={value:?} reads back as {read_key}={read_value:?}, which the same card cannot save"
                );
            }
        }
        // The reader's answer for the two closed vocabularies, spelled the way
        // the rest of the hub spells it.
        assert_eq!(Config::load(stored("notify_bark_level", "loud")).bark_level, "");
        assert_eq!(Config::load(stored("notify_bark_level", "timesensitive")).bark_level, "timeSensitive");
        assert_eq!(Config::load(stored("notify_webhook_method", "PATCH")).webhook_method, "POST");
        assert_eq!(Config::load(stored("notify_webhook_method", "get")).webhook_method, "GET");
    }

    #[test]
    fn an_unknown_channel_is_refused_rather_than_quietly_mapped_onto_one() {
        for provider in [
            Provider::None,
            Provider::Webhook,
            Provider::Telegram,
            Provider::Bark,
            Provider::ServerChan,
            Provider::JavaScript,
        ] {
            assert_eq!(Provider::parse(provider.as_str()), Some(provider), "{}", provider.as_str());
        }
        assert_eq!(Provider::parse("Webhook"), None, "the panel's values are the only ones");
        assert_eq!(Provider::parse(""), None);
    }

    /// The validation the write path runs, which is the only thing standing
    /// between a typo and a notification that never arrives.
    #[test]
    fn the_write_path_refuses_values_the_reader_cannot_use() {
        // Switches.
        assert!(setting_error("notify_enabled", "true").is_some());
        assert!(setting_error("notify_enabled", "").is_some());
        assert!(setting_error("notify_enabled", "on").is_none());
        assert!(setting_error("notify_notify_on_online", "off").is_none());
        // The channel, which decides which of the fields below are read at all.
        assert!(setting_error("notify_provider", "sms").is_some());
        assert!(setting_error("notify_provider", "none").is_none());
        // The grace period, inclusive at both ends because 0 is a choice.
        for bad in ["", "abc", "-1", "86401", "300.5"] {
            assert!(setting_error("notify_grace_seconds", bad).is_some(), "{bad:?}");
        }
        for good in ["0", "300", "86400"] {
            assert!(setting_error("notify_grace_seconds", good).is_none(), "{good:?}");
        }
        // Addresses the hub has to speak to.
        for bad in ["example.com/hook", "ftp://example.com", "https://", "//example.com"] {
            assert!(setting_error("notify_webhook_url", bad).is_some(), "{bad:?}");
        }
        for good in ["", "http://192.168.1.9:8080/push", "https://hook.example.com/a?b=c"] {
            assert!(setting_error("notify_webhook_url", good).is_none(), "{good:?}");
        }
        assert!(setting_error("notify_telegram_endpoint", "api.telegram.org").is_some());
        assert!(setting_error("notify_bark_url", "https://api.day.app").is_none());
        // The ServerChan interface address, validated like the other two: a
        // self-hosted mirror or a test double is an ordinary deployment.
        assert!(setting_error("notify_serverchan_endpoint", "https://sctapi.ftqq.com").is_none());
        assert!(setting_error("notify_serverchan_endpoint", "").is_none());
        assert!(setting_error("notify_serverchan_endpoint", "sctapi.ftqq.com").is_some());
        assert!(setting_error("notify_serverchan_endpoint", "ftp://sct.example.com").is_some());
        // The JavaScript channel: the provider name, and a script that is free
        // text -- what it says is checked by running it, not by looking at it.
        assert!(setting_error("notify_provider", "javascript").is_none());
        assert!(setting_error("notify_javascript_script", "function sendMessage() {}").is_none());
        assert!(setting_error("notify_javascript_script", "").is_none());
        // Headers have to be a JSON object of strings.
        for bad in ["[1]", "{\"x\": 1}", "{", "{\"X Token\": \"a\"}"] {
            assert!(setting_error("notify_webhook_headers", bad).is_some(), "{bad:?}");
        }
        assert!(setting_error("notify_webhook_headers", "").is_none());
        assert!(setting_error("notify_webhook_headers", "{\"Authorization\": \"Bearer t\"}").is_none());
        // The method, which decides where the event goes.
        assert!(setting_error("notify_webhook_method", "PUT").is_some());
        assert!(setting_error("notify_webhook_method", "post").is_none());
        assert!(setting_error("notify_webhook_method", "").is_none());
        // Bark's levels are the app's, not free text.
        assert!(setting_error("notify_bark_level", "loud").is_some());
        assert!(setting_error("notify_bark_level", "critical").is_none());
        assert!(setting_error("notify_bark_level", "").is_none());
        // A template is free text, placeholders and all.
        assert!(setting_error("notify_template", "{{typo}} {{node}}").is_none());
    }

    /// A header value is the one place a newline would let a caller append a
    /// header of their own.
    #[test]
    fn custom_headers_are_read_as_a_json_object_of_strings() {
        let headers = parse_headers("{\"X-Token\": \"a b\"}").unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0.as_str(), "x-token", "names are case-insensitive on the wire");
        assert_eq!(headers[0].1.to_str().unwrap(), "a b");
        assert!(parse_headers("").unwrap().is_empty());
        assert!(parse_headers("  ").unwrap().is_empty());
        assert!(parse_headers("{\"X-Token\": \"a\\r\\nX-Evil: 1\"}").is_err());
        assert!(parse_headers("{\"X Token\": \"a\"}").is_err());
        assert!(parse_headers("null").is_err());
    }

    /// A failing endpoint can answer with an entire HTML page; the log line and
    /// the panel's toast get a bounded prefix of it, cut on a character
    /// boundary.
    #[test]
    fn a_failure_body_is_bounded_and_never_split_mid_character() {
        let long = "错".repeat(500);
        let shown = truncate(&long);
        assert_eq!(shown.chars().count(), 201, "200 characters and the ellipsis");
        assert!(shown.ends_with('…'));
        assert_eq!(truncate("  short  "), "short");
        assert_eq!(truncate(""), "");
    }

    /// Three attempts, as the reference sends, and the reason survives to the
    /// caller: it is what the panel shows beside the test button.
    #[tokio::test]
    async fn a_failing_channel_is_tried_three_times_and_a_succeeding_one_stops_there() {
        let attempts = std::cell::Cell::new(0);
        let result = retry(|| {
            attempts.set(attempts.get() + 1);
            std::future::ready(Err::<(), _>(anyhow::anyhow!("connection refused")))
        })
        .await;
        assert_eq!(attempts.get(), 3);
        assert_eq!(result.unwrap_err().to_string(), "connection refused");

        let attempts = std::cell::Cell::new(0);
        retry(|| {
            let n = attempts.get() + 1;
            attempts.set(n);
            std::future::ready(if n == 2 { Ok(()) } else { Err(anyhow::anyhow!("not yet")) })
        })
        .await
        .unwrap();
        assert_eq!(attempts.get(), 2, "a channel that answers on the second try is not asked a third time");
    }

    /// The `none` channel is where every hub starts, and an event it drops is
    /// not an error: a node going offline with no channel configured is the
    /// expected state, and warning about it on every disconnect would train an
    /// operator to ignore the log.
    #[tokio::test]
    async fn the_none_channel_accepts_an_event_without_sending_anything() {
        let config = Config::load(|_| None);
        let client = Client::new();
        assert!(send(&client, &config, &offline_event("vps-1")).await.is_ok());
        assert!(deliver(&client, &config, &offline_event("vps-1")).await.is_ok());
    }

    /// The test button is the one place a missing channel is an error: the
    /// `none` channel would otherwise report success for a message that was
    /// never sent.
    #[tokio::test]
    async fn the_test_button_refuses_an_unconfigured_channel() {
        let app = app();
        let reason = test(&app).await.unwrap_err().to_string();
        assert!(reason.contains("channel"), "{reason}");

        app.db.set("notify_provider", "bark").unwrap();
        let reason = test(&app).await.unwrap_err().to_string();
        assert!(reason.contains("device key"), "{reason}");
        // And none of it depended on `notify_enabled`, which is still off: a
        // channel is tested before it is switched on.
    }

    /// What the hub actually puts on the wire, built but never sent.
    #[test]
    fn a_webhook_carries_the_event_in_the_body_or_the_query_with_its_own_headers() {
        let client = Client::new();
        let event = offline_event("vps-1");
        let mut config = Config::load(|_| None);
        config.provider = Provider::Webhook;
        config.webhook_url = "https://hook.example.com/monitor".into();

        // The default method is a POST carrying a JSON document.
        let request = webhook_request(&client, &config, &event).unwrap().build().unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        let body: Value = serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["event"], "Offline");
        assert_eq!(body["title"], "Offline");
        assert_eq!(body["node"], "vps-1");
        assert_eq!(body["timestamp"], event.time.timestamp());

        // A GET, with the same fields as query parameters instead.
        config.webhook_method = "get".into();
        let request = webhook_request(&client, &config, &event).unwrap().build().unwrap();
        assert_eq!(request.method(), reqwest::Method::GET);
        assert!(request.body().is_none(), "a GET carries no body");
        let query = request.url().query().unwrap();
        assert!(query.contains("event=Offline"), "{query}");
        assert!(query.contains("node=vps-1"), "{query}");

        // Anything else the panel does not offer -- a row edited by hand -- is a
        // POST rather than a GET, which would drop the event into a query
        // string nobody reads.
        config.webhook_method = "PATCH".into();
        let request = webhook_request(&client, &config, &event).unwrap().build().unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);

        // The operator's own headers and credentials, as they entered them.
        config.webhook_headers = r#"{"X-Token": "abc"}"#.into();
        config.webhook_username = "bot".into();
        config.webhook_password = "hunter2".into();
        let request = webhook_request(&client, &config, &event).unwrap().build().unwrap();
        assert_eq!(request.headers()["x-token"], "abc");
        assert!(request.headers()["authorization"].to_str().unwrap().starts_with("Basic "));

        // A webhook with no address says so rather than sending to nowhere.
        config.webhook_url = String::new();
        assert!(webhook_request(&client, &config, &event).is_err());
    }

    /// The base URLs the channels append their paths to, and the trailing slash
    /// that would make `//push` out of them.
    #[test]
    fn a_channel_endpoint_falls_back_to_its_default_without_a_trailing_slash() {
        assert_eq!(base_url("", DEFAULT_BARK_URL), DEFAULT_BARK_URL);
        assert_eq!(base_url("  ", DEFAULT_BARK_URL), DEFAULT_BARK_URL);
        assert_eq!(base_url("https://bark.example.com/", DEFAULT_BARK_URL), "https://bark.example.com");
        assert_eq!(
            base_url("https://api.telegram.org/bot", DEFAULT_TELEGRAM_ENDPOINT),
            DEFAULT_TELEGRAM_ENDPOINT
        );
        assert_eq!(
            base_url("https://api.telegram.org/bot/", DEFAULT_TELEGRAM_ENDPOINT),
            "https://api.telegram.org/bot"
        );
    }

    /// The first connection is the hub meeting the node, and a second socket
    /// for a node already on record is not a return either.
    #[test]
    fn the_first_connection_is_not_announced() {
        let mut state = NodeState::default();
        assert!(!state.connect(7));
        assert_eq!(state.connection_id, 7);
        assert!(!state.is_first_connection, "the connection is recorded");
        assert!(state.is_conn_exist);
        assert!(!state.connect(8));
    }

    /// A disconnect that heals inside the grace period was never reported, so
    /// there is nothing to report when it ends: no outage, and no return from
    /// one.
    #[test]
    fn a_disconnect_that_heals_inside_the_grace_period_reports_nothing() {
        let mut state = NodeState::default();
        state.connect(1);
        assert!(state.disconnect(1, at(0)), "the teardown that owns the node arms the grace period");
        assert_eq!(state.pending_offline_since, Some(at(0)), "the injected clock is what is stored");
        assert!(!state.disconnect(1, at(1)), "a second teardown for the same session arms nothing further");
        assert_eq!(state.pending_offline_since, Some(at(0)));

        assert!(!state.connect(2), "a reconnect inside the grace period is not a return from an outage");
        assert_eq!(state.pending_offline_since, None, "and it cancels the pending offline notification");
        assert!(!state.offline_due(1), "the grace period that expired owes nothing");
        assert!(!state.offline_due(2), "and neither does the live session's");
    }

    /// An outage that outlasts its grace period is the one worth a message, and
    /// the reconnect that follows is the end of it.
    #[test]
    fn an_outage_is_reported_once_and_the_return_is_reported_too() {
        let mut state = NodeState::default();
        state.connect(1);
        state.disconnect(1, at(0));
        assert!(state.offline_due(1), "the grace period ended with the node still gone");
        assert!(!state.is_conn_exist, "the node is now recorded as offline");
        assert_eq!(state.pending_offline_since, None);
        assert!(!state.offline_due(1), "the same outage cannot be reported twice");
        assert!(state.connect(2), "a node that comes back is the other half of the same outage");
        assert!(!state.connect(3), "and it is announced once");
    }

    /// A teardown can arrive after the session that owned the grace period has
    /// been replaced; it must arm nothing more, and above all must not report a
    /// node that is reporting again. Whether the node is really gone is decided
    /// where the sessions are counted, not by the number in the report; see
    /// `agent_ws::release` and the state's own `disconnect`.
    #[test]
    fn a_teardown_arms_once_and_a_replaced_session_keeps_its_period_to_itself() {
        let mut state = NodeState::default();
        state.connect(1);
        state.connect(2);
        assert!(state.disconnect(2, at(0)), "the teardown the sockets report arms the grace period");
        assert_eq!(state.connection_id, 2, "recorded as the session the period belongs to");
        assert_eq!(state.pending_offline_since, Some(at(0)));
        assert!(state.is_conn_exist, "the node is not marked offline before the period ends");

        // A teardown for the node that arrives while one is already running adds
        // nothing to it: the deadline stays where the departure set it.
        assert!(!state.disconnect(1, at(1)), "one period per absence");
        assert_eq!(state.pending_offline_since, Some(at(0)));

        // A session that took the node over while the period ran: the period is
        // not this node's outage, and marking it offline would report a node
        // that has been back for as long as the period was running.
        state.connect(3);
        assert!(!state.offline_due(2), "the expired period belongs to a replaced session");
        assert_eq!(state.connection_id, 3);
    }

    /// A teardown can reach the state while a grace period is already running,
    /// and the session it names is not the one that period belongs to: the
    /// socket a node's departure left behind, reporting its own end after the
    /// fact. It must leave the running period exactly as it found it -- not
    /// write its own session number into the state, and not arm anything -- or
    /// the period comes due attributed to a session nobody asks about, and the
    /// node that is genuinely gone is never reported.
    #[test]
    fn a_stale_teardown_leaves_a_running_grace_period_to_the_session_that_armed_it() {
        let mut state = NodeState::default();
        state.connect(1);
        assert!(state.disconnect(1, at(0)), "the departure arms the grace period");
        assert_eq!(state.connection_id, 1, "attributed to the session that went");

        // The stale teardown: the socket the departure left behind, ending.
        assert!(!state.disconnect(2, at(1)), "a teardown arriving mid-period arms nothing further");
        assert_eq!(state.connection_id, 1, "and does not take the period over");
        assert_eq!(state.pending_offline_since, Some(at(0)), "the deadline stays where the departure set it");

        // Which is what leaves the period collectable by the task that armed it.
        assert!(state.offline_due(1), "the period still comes due for the session that armed it");
        assert!(!state.is_conn_exist, "and the node is recorded offline");
        assert_eq!(state.pending_offline_since, None, "with nothing left pending behind it");
    }

    /// A period that has been asked about is settled, whatever the answer: the
    /// node must be able to report again afterwards. A pending instant that
    /// survives its own grace period would swallow the next connection and stop
    /// the next departure from arming one, leaving that node silent for good.
    #[test]
    fn a_settled_grace_period_leaves_the_node_free_to_report_again() {
        let mut state = NodeState::default();
        state.connect(1);
        assert!(state.disconnect(1, at(0)), "the departure arms the grace period");

        // The question arrives for a session the period is not the absence of,
        // so there is nothing to send -- and the period is over all the same.
        assert!(!state.offline_due(2), "a period for another session owes nothing");
        assert_eq!(state.pending_offline_since, None, "asked about is settled, not left behind");

        // Nothing is held back by it: the state takes the next departure and
        // reports it the way it reported the first.
        assert!(!state.connect(3), "the node was never marked offline, so it is not a return");
        assert!(state.disconnect(3, at(1)), "the next departure arms a period of its own");
        assert!(state.offline_due(3), "which is announced");
        assert!(!state.is_conn_exist);
        assert!(state.connect(4), "and the return from it is announced too");
        assert!(!state.connect(5), "once");
    }

    /// The two paths the sockets call: neither touches the network or the
    /// database on the socket's own thread, and a hub with notifications off
    /// records the connection anyway.
    #[tokio::test]
    async fn a_connection_is_recorded_even_with_notifications_switched_off() {
        let app = app();
        connected(&app, 1, 10);
        assert!(app.notify.lock().unwrap()[&1].is_conn_exist, "the state moves before any setting is read");
        assert!(!app.notify.lock().unwrap()[&1].is_first_connection);
        // Nothing was armed and nothing was sent: the offline event of a hub
        // that has no channel is not an error.
        disconnected(&app, 1, 10);
        tokio::task::yield_now().await;
        assert!(app.notify.lock().unwrap()[&1].pending_offline_since.is_none());
    }

    /// A teardown reaches the state only after a settings read, so a reconnect
    /// can overtake it. The node's live entry is what tells a departure from a
    /// node that is reporting again: an arm landing for a node that is held arms
    /// nothing, and one for a node nothing holds starts the period.
    #[tokio::test]
    async fn a_teardown_reaching_the_state_after_a_reconnect_arms_nothing() {
        let app = app();
        app.db.set("notify_enabled", "on").unwrap();
        connected(&app, 1, 10);
        // The socket the report names is gone, and a new one has taken the node
        // in the meantime -- which is what the panel sees as online.
        let (tx, _held) = tokio::sync::mpsc::channel::<String>(1);
        app.agents.write().unwrap().insert(1, crate::agent_ws::Agent::new(11, tx));
        disconnected(&app, 1, 10);
        // The armed state only exists once the spawned task has run.
        tokio::task::yield_now().await;
        {
            let states = app.notify.lock().unwrap();
            assert!(states[&1].pending_offline_since.is_none(), "a node that is held again arms nothing");
            assert_eq!(states[&1].connection_id, 10, "and the session the state holds is not disturbed");
        }

        // The departure itself: nothing holds the node, so the period starts.
        app.agents.write().unwrap().remove(&1);
        disconnected(&app, 1, 10);
        tokio::task::yield_now().await;
        assert!(app.notify.lock().unwrap()[&1].pending_offline_since.is_some());
    }

    /// A node the hub has never met has no state, and a teardown is no reason to
    /// give it one: the number that arrives with the report is not evidence that
    /// anything connected, and a state created here would send an offline
    /// notification for a node nobody has seen.
    #[tokio::test]
    async fn a_teardown_for_a_node_the_hub_never_met_creates_nothing() {
        let app = app();
        app.db.set("notify_enabled", "on").unwrap();
        disconnected(&app, 1, 0);
        // The session 0 of a state nothing has touched is the case the numbering
        // exists to rule out; the drawing the hub itself makes is the other.
        disconnected(&app, 1, crate::agent_ws::next_session());
        tokio::task::yield_now().await;
        assert!(
            app.notify.lock().unwrap().is_empty(),
            "no state is created for a node that has not connected, so nothing can be sent for it"
        );
    }

    /// 0 is what a node the hub has never met holds, so no session the hub hands
    /// out can equal it: the counter it draws from starts above it.
    #[test]
    fn the_never_connected_default_is_below_every_session_number() {
        assert_eq!(NodeState::default().connection_id, 0, "the state of a node nobody has seen");
        assert!(
            NodeState::default().connection_id < crate::agent_ws::FIRST_SESSION,
            "and the first number handed out is above it"
        );
        for _ in 0..3 {
            assert!(crate::agent_ws::next_session() >= crate::agent_ws::FIRST_SESSION);
        }
    }

    // ---- the panel's routes ----

    /// What a route says, which is where the reason shown to the operator lives.
    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The card is a form like any other: what `settings` returns has to be what
    /// the write path accepts, or the panel fails the entire save while naming a
    /// field nobody edited. The credentials travel the other way -- write-only,
    /// flagged with `_set`.
    #[tokio::test]
    async fn the_notification_settings_round_trip_with_every_credential_replaced_by_a_flag() {
        let app = app();
        app.db.set("notify_telegram_token", "123:hunter2").unwrap();
        app.db.set("notify_bark_key", "device-key").unwrap();

        let Json(read) = settings(Admin, State(app.clone())).await;
        // The defaults are in the answer rather than left to the panel: a form
        // showing an empty channel asks a question the hub has answered.
        assert_eq!(read["notify_enabled"], "off");
        assert_eq!(read["notify_provider"], "none");
        assert_eq!(read["notify_notify_on_online"], "on");
        assert_eq!(read["notify_grace_seconds"], "300");
        assert_eq!(read["notify_webhook_method"], "POST");
        assert_eq!(read["notify_telegram_endpoint"], "https://api.telegram.org/bot");
        assert_eq!(read["notify_bark_url"], "https://api.day.app");
        assert_eq!(read["notify_serverchan_endpoint"], "https://sctapi.ftqq.com");
        // The script is the operator's text and travels both ways: a form that
        // could not read its own channel back would erase it on the next save.
        assert_eq!(read["notify_javascript_script"], "");
        assert!(read["notify_template"].as_str().unwrap().contains("{{node}}"));
        assert_eq!(read["notify_telegram_token_set"], true);
        assert_eq!(read["notify_bark_key_set"], true);
        assert_eq!(read["notify_serverchan_key_set"], false);
        assert!(!read.to_string().contains("hunter2"), "a credential is write-only");

        // What the panel sends back: every readable key it was given, and the
        // credentials only where the operator typed a new one.
        let mut echoed: serde_json::Map<String, Value> = KEYS
            .iter()
            .filter_map(|key| read.get(key).map(|value| ((*key).to_owned(), value.clone())))
            .collect();
        assert_eq!(echoed.len(), 14, "each readable key, and none of the credentials");
        echoed.insert("notify_provider".into(), json!("bark"));
        echoed.insert("notify_bark_level".into(), json!("critical"));

        assert_eq!(
            save_settings(Admin, State(app.clone()), Json(Value::Object(echoed))).await.status(),
            StatusCode::OK,
            "the form's own answer must survive the write path"
        );
        assert_eq!(app.db.get("notify_provider").as_deref(), Some("bark"));
        assert_eq!(app.db.get("notify_grace_seconds").as_deref(), Some("300"));
        assert_eq!(app.db.get("notify_bark_key").as_deref(), Some("device-key"), "untouched");

        // A key beside the settings that nobody knows is still refused, and the
        // name it is refused by is the one the panel has to fix.
        let response =
            save_settings(Admin, State(app.clone()), Json(json!({"notify_channel": "bark"}))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await, "unknown setting: notify_channel");

        // A value of the wrong type is refused too. Silently reading it as text
        // would report a successful save that stored nothing.
        let before = app.db.get("notify_enabled");
        let response = save_settings(Admin, State(app.clone()), Json(json!({"notify_enabled": true}))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await, "notify_enabled must be a string");
        assert_eq!(app.db.get("notify_enabled"), before, "and nothing was written");

        assert_eq!(
            save_settings(Admin, State(app.clone()), Json(json!({"notify_grace_seconds": "5"})))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(app.db.get("notify_grace_seconds").as_deref(), Some("5"));

        // A good key beside a bad one is not written either: the card is
        // all-or-nothing, so a refused form leaves the hub as it was.
        let response = save_settings(
            Admin,
            State(app.clone()),
            Json(json!({"notify_grace_seconds": "1d", "notify_provider": "telegram"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "a value the reader cannot use");
        assert_eq!(app.db.get("notify_provider").as_deref(), Some("bark"), "and the good key with it");
        assert_eq!(app.db.get("notify_grace_seconds").as_deref(), Some("5"));
    }

    /// The button is behind the admin gate like every other panel route, and a
    /// hub with no channel says so instead of reporting a test it never sent.
    #[tokio::test]
    async fn the_test_route_answers_with_the_reason_a_channel_gave() {
        let app = app();
        let response = notify_test(Admin, State(app.clone())).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body(response).await, "no notification channel is selected");

        // A channel that is selected but not filled in names what is missing,
        // which is the whole point of the button.
        app.db.set("notify_provider", "webhook").unwrap();
        let response = notify_test(Admin, State(app.clone())).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body(response).await, "the webhook URL is not configured");
    }
}
