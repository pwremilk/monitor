//! `fetch`: the one global a script reaches the network with.
//!
//! Synchronous on purpose. The reference implementation has an event loop and
//! hands back a promise; here the request is waited for by the thread the script
//! already owns, so `const r = fetch(...)` and `await fetch(...)` both work and
//! nothing has to be pumped between them. What that costs is a blocking thread
//! for the length of the request, which is why the wait is bounded by the same
//! budget as the rest of the evaluation.
//!
//! The script runs on a blocking thread and the request has to go out on the
//! runtime, so the two hand off over a channel. That handoff is also the only
//! place the evaluation crosses back into the async world, and the only place
//! the script's own clock is deliberately paused.

use std::rc::Rc;
use std::sync::mpsc::{sync_channel, RecvTimeoutError};

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Client, Method, Url};
use rquickjs::{Ctx, Function, IntoJs, Object};
use serde::Deserialize;
use tokio::runtime::Handle;

use super::{refuse, Clock};

/// One request a script asked for.
///
/// Built by the bootstrap, which is where the four optional shapes of a `fetch`
/// call are flattened into one JSON argument. Deserializing that here is what
/// keeps the optionality in the one place that can express it, and rejecting a
/// header value that is not a string in the one place that knows the header's
/// name.
#[derive(Deserialize)]
pub(super) struct Ask {
    url: String,
    method: String,
    body: Option<String>,
    headers: std::collections::BTreeMap<String, String>,
}

/// What came back: what `fetch` resolves to, minus the fields a script has no
/// use for.
struct Response {
    status: u16,
    body: String,
}

/// The evaluation's route to the network.
pub(super) struct Bridge {
    http: Client,
    handle: Handle,
    /// The evaluation's clocks. The network wait counts against the budget but
    /// not against the turn, so this is also what tells the engine that the
    /// script's own execution starts again when the request is done.
    clock: Rc<Clock>,
}

impl Bridge {
    pub(super) fn new(http: &Client, handle: Handle, clock: Rc<Clock>) -> Self {
        Self { http: http.clone(), handle, clock }
    }

    /// Performs one request, waiting at most what the evaluation has left.
    fn request(&self, ask: Ask) -> std::result::Result<Response, String> {
        let left = self.clock.remaining();
        if left.is_zero() {
            return Err(format!("the script's {:?} budget ran out", self.clock.span()));
        }
        let (tx, rx) = sync_channel(1);
        let http = self.http.clone();
        self.handle.spawn(async move {
            // A closed receiver means the evaluation is already over; its error
            // is the one the caller sees, so this one has nowhere to go.
            let _ = tx.send(send(&http, ask).await);
        });
        match rx.recv_timeout(left) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(format!("the request did not answer within the script's {:?} budget", self.clock.span()))
            }
            Err(RecvTimeoutError::Disconnected) => Err("the request was dropped".into()),
        }
    }
}

// SAFETY: the bridge holds no JavaScript value -- an HTTP client, a runtime
// handle and the evaluation's clock -- so there is no `'js` lifetime inside it
// for `Changed` to rewrite.
unsafe impl<'js> rquickjs::JsLifetime<'js> for Bridge {
    type Changed<'to> = Bridge;
}

/// Installs `fetch` under the name the bootstrap picks it up from.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    let fetch = Function::new(ctx.clone(), fetch)
        .map_err(|e| anyhow::anyhow!("fetch could not be installed: {e}"))?;
    ctx.globals().set("__fetch", fetch).map_err(|e| anyhow::anyhow!("fetch could not be installed: {e}"))
}

/// `fetch(url, options)`, as the bootstrap hands it over: one JSON argument.
fn fetch<'js>(ctx: Ctx<'js>, ask: String) -> rquickjs::Result<Object<'js>> {
    let ask: Ask = match serde_json::from_str(&ask) {
        Ok(ask) => ask,
        Err(error) => return Err(refuse(&ctx, &format!("the request could not be read: {error}"))),
    };
    let response = {
        // The borrow ends before the response object -- which needs the context
        // -- is built, and before anything else can read the host data.
        let Some(bridge) = ctx.userdata::<Bridge>() else {
            return Err(refuse(&ctx, "this context has no network access"));
        };
        let outcome = bridge.request(ask);
        // Whatever happened, the script's own execution starts again here: the
        // wait was the host's, and the interrupt handler must not charge it to
        // the script. When the budget itself ran out this sets the turn to the
        // budget, so the script is stopped as soon as it runs again.
        bridge.clock.turn();
        outcome
    }
    .map_err(|message| refuse(&ctx, &message))?;

    // What `fetch` resolves to: the three fields a script has any use for, and
    // nothing of the request that produced them.
    let value = Object::new(ctx.clone())?;
    value.set("status", f64::from(response.status))?;
    value.set("ok", (200..300).contains(&response.status))?;
    value.set("body", response.body.as_str().into_js(&ctx)?)?;
    Ok(value)
}

/// Performs one request, reporting the failures a script can act on.
///
/// `without_url` for the same reason `notify`'s own errors use it: the address a
/// script sends to is often the whole credential, and this message reaches the
/// log and the panel.
async fn send(http: &Client, ask: Ask) -> std::result::Result<Response, String> {
    let url = Url::parse(ask.url.trim()).map_err(|e| format!("the URL cannot be used: {e}"))?;
    let method = Method::from_bytes(ask.method.as_bytes())
        .map_err(|_| format!("{} is not an HTTP method", ask.method))?;
    let mut build = http.request(method, url);
    for (name, value) in &ask.headers {
        // Checked because `RequestBuilder::header` panics on either, and a panic
        // in a release build of this hub is an abort.
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("{name:?} is not a header name: {e}"))?;
        let value = HeaderValue::from_str(value).map_err(|_| format!("the header {name} cannot be sent"))?;
        build = build.header(name, value);
    }
    if let Some(body) = ask.body {
        build = build.body(body);
    }
    let response = build.send().await.map_err(|e| format!("{}", e.without_url()))?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| format!("{}", e.without_url()))?;
    Ok(Response { status, body })
}
