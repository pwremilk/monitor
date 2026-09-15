//! The JavaScript notification channel: an operator's own script, run in an
//! embedded engine.
//!
//! Ported from komari's `utils/messageSender/javascript`, without the layer it
//! stands on there. That one is goja plus goja_nodejs -- `require`, `fs`,
//! `crypto`, `process`, an event loop -- while this hub is pure Rust and ships
//! `FROM scratch`, so the engine is `boa_engine` and the only capability handed
//! to a script is `fetch`, beside `console`.
//!
//! Its own module because of the byte budget rather than the subject: `notify`
//! owns the configuration, the channel enum and the dispatch, and this is the
//! one channel that is an interpreter instead of a request. The two contracts
//! they share -- the event object and the rendered template -- stay in `notify`,
//! so a script and a webhook never disagree about what an event looks like.
//!
//! Three things shape the code below:
//!
//! * `boa_engine::Context` is `!Send`, so one evaluation owns one thread. That
//!   thread is a blocking-pool one, never a tokio worker: the workers are shared
//!   with the panel and every agent socket, and a script is untrusted text.
//! * A script gets a deterministic budget rather than a watchdog -- the engine's
//!   own loop and recursion limits -- so `while (true) {}` ends as a channel
//!   error instead of a thread that never comes back.
//! * `fetch` is the one place a script reaches the network, and the only place
//!   the evaluation crosses back into the async world. It is synchronous to the
//!   script, so a plain `const r = fetch(...)` works without an event loop.

use std::rc::Rc;
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use boa_engine::builtins::promise::PromiseState;
use boa_engine::context::ContextBuilder;
use boa_engine::module::IdleModuleLoader;
use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{
    js_string, Context, JsError, JsNativeError, JsString, JsValue, NativeFunction, Script, Source,
};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use tokio::runtime::Handle;
use tracing::{error, info, warn};

use crate::notify::{event_object, render, Config, EventMessage};

/// Loop iterations one evaluation may run in total.
///
/// The engine counts every iteration of every loop against one budget and throws
/// when it is spent, which is what bounds a runaway script. The figure is a
/// compromise between letting real work finish -- a script that assembles a long
/// message, or walks a small list -- and abandoning a stuck one quickly.
const LOOP_ITERATIONS: u64 = 1_000_000;

/// How deeply a script may call into itself. The engine's default is 512, which
/// is more rope than a notification script needs and a real stack overflow risk.
const RECURSION: usize = 64;

/// How long one evaluation may spend on the network, across every `fetch` it
/// makes.
///
/// The loop and recursion limits bound the CPU a script can burn; this bounds
/// what it can wait for. A script that keeps fetching in a loop ends here, and
/// the remaining time is also the deadline a single request waits under, so a
/// hanging endpoint does not hold its thread either.
const BUDGET: Duration = Duration::from_secs(20);

/// Runs the configured script for one event.
///
/// `Err` is one channel error like any other: a script that does not compile, a
/// script with no `sendMessage`, one that throws, and one that never finishes
/// are all reported to the test button and the log by the caller, which retries
/// them the same way it retries a refused token.
pub async fn send(http: &Client, config: &Config, event: &EventMessage) -> Result<()> {
    if config.javascript_script.trim().is_empty() {
        anyhow::bail!("the JavaScript notification script is not configured");
    }
    let call = Call::for_event(config, event);
    let bridge = Bridge::new(http, Handle::current());
    // Off the runtime's worker threads. `spawn_blocking` also gives the
    // evaluation a thread of its own, which is what a `!Send` engine and an
    // untrusted script both need.
    tokio::task::spawn_blocking(move || evaluate(&call, bridge))
        .await
        .map_err(|e| anyhow!("the JavaScript channel's thread did not finish: {e}"))?
}

/// What one evaluation is given: the script, and the arguments it is called with.
struct Call {
    script: String,
    /// The rendered template, for `sendMessage`.
    message: String,
    /// The event name, for `sendMessage`'s title.
    title: String,
    /// The structured event, for `sendEvent`.
    event: Value,
}

impl Call {
    fn for_event(config: &Config, event: &EventMessage) -> Self {
        Self {
            script: config.javascript_script.clone(),
            // The two arguments `sendMessage` gets are the two every other
            // channel sends: the rendered template, and the event name.
            message: render(&config.template, event),
            title: event.event.as_str().to_owned(),
            event: event_object(event),
        }
    }
}

/// Builds an engine, runs the script in it, calls the one function that sends.
///
/// The engine is built per event and dropped with it: a script cannot leave
/// state behind for the next event, and no `Context` ever crosses a thread.
fn evaluate(call: &Call, bridge: Bridge) -> Result<()> {
    let mut context = ContextBuilder::new()
        // Only the modules a script could import are closed off here. Nothing
        // else about the environment is a file, a process or a clock the script
        // can reach: `fetch` and `console` below are the whole surface.
        .module_loader(Rc::new(IdleModuleLoader))
        .build()
        .map_err(|e| anyhow!("the JavaScript engine could not start: {e}"))?;
    context.runtime_limits_mut().set_loop_iteration_limit(LOOP_ITERATIONS);
    context.runtime_limits_mut().set_recursion_limit(RECURSION);
    install(&mut context, bridge)?;
    // Given a path rather than left anonymous: every error the engine reports
    // carries a source and a position, and "notify.js:12" is a line an operator
    // can go and look at in the textarea they pasted it into.
    let source = Source::from_bytes(call.script.as_str()).with_path(std::path::Path::new("notify.js"));
    Script::parse(source, None, &mut context)
        // The engine's own parse errors carry no position here, so the name is
        // put back in: the operator is looking at a textarea called 脚本, and a
        // bare "SyntaxError: abrupt end" would say nothing about which of their
        // settings it came from.
        .map_err(|e| anyhow!("the script could not be loaded (notify.js): {e}"))?
        .evaluate(&mut context)
        .map_err(|e| anyhow!("the script could not be run: {e}"))?;

    // `sendEvent` wins when it is there, as in the reference: a script defining
    // both wants the structured event, and `sendMessage` is the older form.
    let (name, arguments) = if declared(&mut context, "sendEvent")? {
        let event = JsValue::from_json(&call.event, &mut context)
            .map_err(|e| anyhow!("the event could not be handed to sendEvent: {e}"))?;
        ("sendEvent", vec![event])
    } else if declared(&mut context, "sendMessage")? {
        (
            "sendMessage",
            vec![JsString::from(call.message.as_str()).into(), JsString::from(call.title.as_str()).into()],
        )
    } else {
        anyhow::bail!("the script defines neither sendEvent nor sendMessage");
    };

    let function = context
        .global_object()
        .get(JsString::from(name), &mut context)
        .map_err(|e| anyhow!("{name} could not be read: {e}"))?
        .as_callable()
        .ok_or_else(|| anyhow!("{name} is not a function"))?;
    match function.call(&JsValue::undefined(), &arguments, &mut context) {
        Ok(value) => settle(&mut context, name, value),
        // Everything the engine can refuse -- a thrown error, the loop and
        // recursion limits -- arrives here, and the reason is what the operator
        // needs to see. Nothing on this path unwinds out of the engine.
        Err(e) => Err(anyhow!("{name} failed: {e}")),
    }
}

/// What the called function's return value means for the event.
///
/// A script written for the reference implementation may be `async`, and `await`
/// -- on our `fetch` included -- suspends into the microtask queue, so the queue
/// is drained before the promise is judged. A promise still pending afterwards
/// never finished: reporting a send that has not happened would be worse than
/// reporting the error, because nobody looks at a message that did arrive.
fn settle(context: &mut Context, name: &str, value: JsValue) -> Result<()> {
    let Some(promise) = value.as_promise() else { return Ok(()) };
    context.run_jobs().map_err(|e| anyhow!("{name} left jobs that could not run: {e}"))?;
    match promise.state() {
        PromiseState::Fulfilled(_) => Ok(()),
        PromiseState::Rejected(reason) => Err(anyhow!("{name} rejected: {}", reason.display())),
        PromiseState::Pending => Err(anyhow!("{name} returned a promise that never settled")),
    }
}

/// Whether the script defined a callable global with this name.
fn declared(context: &mut Context, name: &str) -> Result<bool> {
    let value = context
        .global_object()
        .get(JsString::from(name), context)
        .map_err(|e| anyhow!("{name} could not be read: {e}"))?;
    Ok(value.as_callable().is_some())
}

/// Hands the script the two globals it may have.
///
/// Deliberately not `default_global_bindings`' full set: this is the whole
/// capability list, so what a script can do is what is written here. No
/// `require`, no `fs`, no `crypto`, no `process`, no timers -- a notification
/// script builds a request and sends it, and anything else it might want is
/// something the hub would have to be told about.
fn install(context: &mut Context, bridge: Bridge) -> Result<()> {
    // Held as host data rather than captured in a closure: the native functions
    // below are plain function pointers, and this is the only way they can reach
    // the client and the runtime they have to borrow.
    context.insert_data(bridge);
    context
        .register_global_callable(js_string!("fetch"), 2, NativeFunction::from_fn_ptr(fetch))
        .map_err(|e| anyhow!("fetch could not be installed: {e}"))?;
    let console = ObjectInitializer::new(context)
        .function(NativeFunction::from_fn_ptr(console_log), js_string!("log"), 0)
        .function(NativeFunction::from_fn_ptr(console_warn), js_string!("warn"), 0)
        .function(NativeFunction::from_fn_ptr(console_error), js_string!("error"), 0)
        .build();
    context
        .register_global_property(js_string!("console"), console, Attribute::READONLY)
        .map_err(|e| anyhow!("console could not be installed: {e}"))?;
    Ok(())
}

// ---- fetch ----

/// One request a script asked for.
struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

/// What came back: what `fetch` resolves to, minus the fields a script has no
/// use for.
struct Response {
    status: u16,
    body: String,
}

/// The evaluation's route to the network.
///
/// The script runs on a blocking thread and the request has to go out on the
/// runtime, so the two hand off over a channel. The wait is the same deadline as
/// the rest of the evaluation, which is what stops a hanging endpoint from
/// holding a blocking thread for the client's own timeout on every attempt.
struct Bridge {
    http: Client,
    handle: Handle,
    deadline: Instant,
}

impl Bridge {
    fn new(http: &Client, handle: Handle) -> Self {
        Self::with_budget(http, handle, BUDGET)
    }

    /// The budget is a field rather than a constant read here so a test can run
    /// out of it in milliseconds instead of twenty seconds.
    fn with_budget(http: &Client, handle: Handle, budget: Duration) -> Self {
        Self { http: http.clone(), handle, deadline: Instant::now() + budget }
    }

    fn request(&self, request: Request) -> std::result::Result<Response, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("the script's {}s budget ran out", BUDGET.as_secs()));
        }
        let (tx, rx) = sync_channel(1);
        let http = self.http.clone();
        self.handle.spawn(async move {
            // A closed receiver means the evaluation is already over; its error
            // is the one the caller sees, so this one has nowhere to go.
            let _ = tx.send(request_send(&http, request).await);
        });
        match rx.recv_timeout(left) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(format!("the request did not answer within the script's {}s budget", BUDGET.as_secs()))
            }
            Err(RecvTimeoutError::Disconnected) => Err("the request was dropped".into()),
        }
    }
}

/// Performs one request, reporting the failures a script can act on.
///
/// `without_url` for the same reason `notify`'s own errors use it: the address a
/// script sends to is often the whole credential, and this message reaches the
/// log and the panel.
async fn request_send(http: &Client, request: Request) -> std::result::Result<Response, String> {
    let url = Url::parse(request.url.trim()).map_err(|e| format!("the URL cannot be used: {e}"))?;
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("{} is not an HTTP method", request.method))?;
    let mut build = http.request(method, url);
    for (name, value) in request.headers {
        // Checked because `RequestBuilder::header` panics on either, and a panic
        // in a release build of this hub is an abort.
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("{name:?} is not a header name: {e}"))?;
        let value = HeaderValue::from_str(&value).map_err(|_| format!("the header {name} cannot be sent"))?;
        build = build.header(name, value);
    }
    if let Some(body) = request.body {
        build = build.body(body);
    }
    let response = build.send().await.map_err(|e| format!("{}", e.without_url()))?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| format!("{}", e.without_url()))?;
    Ok(Response { status, body })
}

/// `fetch(url, options)`: the only global a script can reach the network with.
///
/// Synchronous on purpose. The reference implementation has an event loop and
/// hands back a promise; here the request is waited for by the thread the script
/// already owns, so `const r = fetch(...)` and `await fetch(...)` both work and
/// nothing has to be pumped between them.
fn fetch(_: &JsValue, args: &[JsValue], context: &mut Context) -> boa_engine::JsResult<JsValue> {
    let url = match args.first() {
        Some(value) if !value.is_null_or_undefined() => value.to_string(context)?.to_std_string_escaped(),
        _ => return Err(type_error("fetch needs a URL")),
    };
    let mut request = Request { method: "GET".to_owned(), url, headers: Vec::new(), body: None };
    if let Some(options) = args.get(1).filter(|value| !value.is_null_or_undefined()) {
        let options = options
            .as_object()
            .ok_or_else(|| type_error("the second argument to fetch must be an object"))?;
        if let Some(method) = option_text(&options, "method", context)? {
            request.method = method.to_ascii_uppercase();
        }
        request.body = option_text(&options, "body", context)?;
        let headers = options.get(JsString::from("headers"), context)?;
        if !headers.is_null_or_undefined() {
            // Through JSON rather than over the property keys: a plain object of
            // strings is exactly what the options are, and anything else -- a
            // Map, a value that is not a string -- is refused here.
            let headers = headers.to_json(context)?.unwrap_or(Value::Null);
            let headers = headers
                .as_object()
                .ok_or_else(|| type_error("the fetch options' headers must be an object"))?;
            for (name, value) in headers {
                let value = value
                    .as_str()
                    .ok_or_else(|| type_error(&format!("the header {name} must be a string")))?;
                request.headers.push((name.clone(), value.to_owned()));
            }
        }
    }

    // The bridge is read out of host data, and the borrow ends before the
    // response -- which needs the context -- is built.
    let response = match context.get_data::<Bridge>() {
        Some(bridge) => bridge.request(request),
        None => return Err(type_error("this context has no network access")),
    }
    .map_err(|message| JsError::from(error_object(&message)))?;

    Ok(ObjectInitializer::new(context)
        .property(js_string!("status"), f64::from(response.status), Attribute::all())
        .property(js_string!("ok"), (200..300).contains(&response.status), Attribute::all())
        .property(js_string!("body"), JsString::from(response.body), Attribute::all())
        .build()
        .into())
}

/// Reads one property of the options object as text, `None` when it is absent.
fn option_text(
    options: &boa_engine::JsObject,
    name: &str,
    context: &mut Context,
) -> boa_engine::JsResult<Option<String>> {
    let value = options.get(JsString::from(name), context)?;
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    Ok(Some(value.to_string(context)?.to_std_string_escaped()))
}

// ---- console ----

fn console_log(_: &JsValue, args: &[JsValue], _: &mut Context) -> boa_engine::JsResult<JsValue> {
    info!("javascript notification script: {}", joined(args));
    Ok(JsValue::undefined())
}

fn console_warn(_: &JsValue, args: &[JsValue], _: &mut Context) -> boa_engine::JsResult<JsValue> {
    warn!("javascript notification script: {}", joined(args));
    Ok(JsValue::undefined())
}

fn console_error(_: &JsValue, args: &[JsValue], _: &mut Context) -> boa_engine::JsResult<JsValue> {
    error!("javascript notification script: {}", joined(args));
    Ok(JsValue::undefined())
}

/// The arguments as one line, the way a console writes them.
fn joined(args: &[JsValue]) -> String {
    args.iter().map(|value| value.display().to_string()).collect::<Vec<_>>().join(" ")
}

/// A refusal a script can catch, as `fetch` in a browser rejects.
fn error_object(message: &str) -> JsNativeError {
    JsNativeError::typ().with_message(message.to_owned())
}

fn type_error(message: &str) -> boa_engine::JsError {
    JsNativeError::typ().with_message(message.to_owned()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{offline_event, Provider};
    use reqwest::Client;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn config(script: &str) -> Config {
        let mut config = Config::load(|_| None);
        config.provider = Provider::JavaScript;
        config.javascript_script = script.to_owned();
        config
    }

    /// The reason the channel gives for a script, which is what the panel shows.
    async fn reason(script: &str) -> String {
        let client = Client::new();
        send(&client, &config(script), &offline_event("vps-1")).await.unwrap_err().to_string()
    }

    /// A script is an operator's text, and every way it can be wrong has to come
    /// back as the reason rather than as silence or a panic: a message nobody
    /// received looks exactly like a channel that works.
    #[tokio::test]
    async fn a_broken_script_is_reported_with_its_reason() {
        let client = Client::new();
        assert_eq!(
            send(&client, &config("   "), &offline_event("vps-1")).await.unwrap_err().to_string(),
            "the JavaScript notification script is not configured"
        );

        let syntax = reason("function sendMessage( {").await;
        assert!(syntax.contains("could not be loaded"), "{syntax}");

        let missing = reason("function anythingElse() {}").await;
        assert!(missing.contains("neither sendEvent nor sendMessage"), "{missing}");

        let threw = reason(r#"function sendMessage() { throw new Error("no token") }"#).await;
        assert!(threw.contains("sendMessage failed") && threw.contains("no token"), "{threw}");

        // A runtime limit is an error like any other, and the engine's own names
        // for them are what an operator gets to read.
        let runaway = reason("function sendMessage() { while (true) {} }").await;
        assert!(runaway.contains("sendMessage failed"), "{runaway}");
        let deep = reason("function sendMessage() { (function again() { again() })() }").await;
        assert!(deep.contains("sendMessage failed"), "{deep}");
    }

    /// The capabilities are exactly the two injected globals, and no more.
    #[tokio::test]
    async fn the_script_only_has_fetch_and_console() {
        let probe = "function sendMessage() { throw new Error([typeof require, typeof process, typeof crypto, typeof setTimeout, typeof fetch, typeof console.log, typeof console.error, typeof console.warn].join(\",\")) }";
        let surface = reason(probe).await;
        assert!(
            surface.contains("undefined,undefined,undefined,undefined,function,function,function,function"),
            "the globals a script can reach changed: {surface}"
        );
        assert!(surface.starts_with("sendMessage failed"), "{surface}");
    }

    /// `sendEvent` is preferred, and it is handed the event the webhook body is
    /// built from -- one vocabulary for a script and an endpoint alike.
    #[tokio::test]
    async fn send_event_is_preferred_and_carries_the_event_object() {
        let script = r#"
            function sendMessage() { throw new Error("sendMessage must not be reached") }
            function sendEvent(event) {
                const fields = ["event", "title", "node", "message", "emoji", "time", "timestamp"]
                for (const field of fields) if (!(field in event)) throw new Error("missing " + field)
                if (event.event !== "Offline" || event.node !== "vps-1") throw new Error("wrong event: " + JSON.stringify(event))
                if (typeof event.timestamp !== "number") throw new Error("timestamp is not a number")
            }
        "#;
        let client = Client::new();
        send(&client, &config(script), &offline_event("vps-1")).await.unwrap();
    }

    /// The `sendMessage` form gets the rendered template and the event name, and
    /// an `async` one still sends: `await` suspends into the microtask queue, so
    /// the queue has to be drained before the promise is judged.
    #[tokio::test]
    async fn a_message_script_is_called_with_the_rendered_template() {
        let script = r#"
            async function sendMessage(message, title) {
                await null
                const expected = "🔴🔴\n事件: Offline\n节点: vps-1\n信息: \n时间: "
                if (!message.startsWith(expected)) throw new Error("message: " + JSON.stringify(message))
                if (title !== "Offline") throw new Error("title: " + title)
            }
        "#;
        let client = Client::new();
        send(&client, &config(script), &offline_event("vps-1")).await.unwrap();

        // A promise that never settles is a script that never sent anything, and
        // saying so is the point of the check.
        let pending = reason("function sendMessage() { return new Promise(() => {}) }").await;
        assert!(pending.contains("never settled"), "{pending}");

        let rejected = reason(r#"async function sendMessage() { throw new Error("nope") }"#).await;
        assert!(rejected.contains("rejected") && rejected.contains("nope"), "{rejected}");
    }

    /// One echo server for the `fetch` tests: it answers with the request it was
    /// given, so a script can check everything the bridge put on the wire.
    fn echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = vec![0u8; 8192];
                let Ok(read) = stream.read(&mut buffer) else { continue };
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let mut lines = request.split("\r\n");
                let head = lines.next().unwrap_or_default().to_owned();
                let header = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("x-script:"))
                    .unwrap_or("x-script: none")
                    .to_owned();
                let body = request.split("\r\n\r\n").nth(1).unwrap_or_default().replace('"', "\\\"");
                let payload = format!("{{\"line\":\"{head}\",\"header\":\"{header}\",\"body\":\"{body}\"}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/sink")
    }

    /// `fetch` is what makes this channel useful, so the request it builds --
    /// method, headers, body -- and the response object it answers with are both
    /// checked against a server that echoes them back.
    #[tokio::test]
    async fn fetch_puts_the_request_on_the_wire_and_answers_with_the_response() {
        let url = echo_server();
        let script = format!(
            r#"
            function sendMessage(message, title) {{
                const plain = fetch("{url}")
                if (plain.status !== 200 || plain.ok !== true) throw new Error("GET: " + JSON.stringify(plain))
                if (!JSON.parse(plain.body).line.startsWith("GET /sink")) throw new Error("GET line: " + plain.body)

                const sent = fetch("{url}", {{
                    method: "POST",
                    headers: {{ "X-Script": "yes" }},
                    body: JSON.stringify({{ message: message, title: title }})
                }})
                if (!sent.ok) throw new Error("POST status: " + sent.status)
                const echoed = JSON.parse(sent.body)
                if (!echoed.line.startsWith("POST /sink")) throw new Error("POST line: " + echoed.line)
                if (!/^x-script: yes$/i.test(echoed.header)) throw new Error("header: " + echoed.header)
                if (!echoed.body.includes("vps-1") || !echoed.body.includes("Offline")) throw new Error("body: " + echoed.body)
            }}
        "#
        );
        let client = Client::new();
        send(&client, &config(&script), &offline_event("vps-1")).await.unwrap();

        // A refusal is a refusal, and the URL it was asked for does not travel
        // with it: an address a script sends to is often the whole credential.
        let refused = format!(r#"function sendMessage() {{ fetch("{url}", {{ method: "NOT A METHOD" }}) }}"#);
        let reason = reason(&refused).await;
        assert!(reason.contains("is not an HTTP method"), "{reason}");
        assert!(!reason.contains("127.0.0.1"), "the URL leaked: {reason}");
    }

    /// The loop and recursion limits bound the CPU a script may burn; the budget
    /// bounds what it may wait for. Without it a script that keeps fetching is
    /// bounded only by the number of requests it can make.
    #[tokio::test]
    async fn a_script_that_waits_for_the_network_runs_out_of_its_budget() {
        // Accepts and never answers, so the request is still in flight when the
        // budget expires.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                std::mem::forget(stream);
            }
        });

        let script = format!(r#"function sendMessage() {{ fetch("http://{address}/hang") }}"#);
        let config = config(&script);
        let event = offline_event("vps-1");
        let call = Call::for_event(&config, &event);
        let bridge = Bridge::with_budget(&Client::new(), Handle::current(), Duration::from_millis(200));
        let started = Instant::now();
        let error = evaluate(&call, bridge).unwrap_err().to_string();
        assert!(error.contains("budget"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }

    /// A script that never finishes must not hold the hub: it runs beside the
    /// runtime, so timers and sockets keep being served while it burns its
    /// budget, and it ends as an error.
    #[tokio::test]
    async fn a_runaway_script_does_not_hold_the_runtime() {
        let client = Client::new();
        let config = config("function sendMessage() { while (true) {} }");
        let event = offline_event("vps-1");
        let started = Instant::now();
        let mut running = tokio::spawn(async move { send(&client, &config, &event).await });

        // The runtime is not the thread the script is on: other work still lands.
        tokio::time::timeout(Duration::from_secs(2), tokio::time::sleep(Duration::from_millis(20)))
            .await
            .expect("the runtime kept running while the script looped");

        let outcome = tokio::time::timeout(Duration::from_secs(60), &mut running)
            .await
            .expect("the runaway script was abandoned")
            .expect("the evaluation did not panic");
        assert!(outcome.is_err(), "a runaway script must be a channel error");
        let took = started.elapsed();
        assert!(took < Duration::from_secs(60), "abandoned after {took:?}");
        eprintln!("runaway script abandoned after {took:?}: {}", outcome.unwrap_err());
    }
}
