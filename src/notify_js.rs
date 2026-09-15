//! The JavaScript notification channel: an operator's own script, run in an
//! embedded engine.
//!
//! Ported from komari's `utils/messageSender/javascript`. That one stands on
//! goja plus goja_nodejs -- `require`, `fs`, `crypto`, `process`, an event loop
//! -- while this hub ships `FROM scratch`. The engine here is QuickJS, through
//! `rquickjs`: one C library the `cc` crate builds from sources vendored in the
//! binding crate, so the release image gains a C compiler at build time and
//! nothing at run time. A script written for the reference implementation finds
//! the same family of JavaScript -- `fetch`, `Buffer`, `crypto`, `process`,
//! timers -- and none of the parts of Node that reach outside the process: no
//! file, no child process, no socket.
//!
//! Its own module because of the byte budget rather than the subject: `notify`
//! owns the configuration, the channel enum and the dispatch, and this is the
//! one channel that is an interpreter instead of a request. The two contracts
//! they share -- the event object and the rendered template -- stay in `notify`,
//! so a script and a webhook never disagree about what an event looks like.
//!
//! Four things shape the code below:
//!
//! * `rquickjs::Runtime` and `Context` are `!Send`, so one evaluation owns one
//!   thread. That thread is a blocking-pool one, never a tokio worker: the
//!   workers are shared with the panel and every agent socket, and a script is
//!   untrusted text.
//! * A script gets deterministic ceilings rather than a watchdog. The engine's
//!   own memory and stack limits bound what it can allocate, and an interrupt
//!   handler bounds what it can burn, so `while (true) {}` ends as a channel
//!   error instead of as a thread that never comes back.
//! * `fetch` is the one place a script reaches the network, and the only place
//!   the evaluation crosses back into the async world. It is synchronous to the
//!   script, so a plain `const r = fetch(...)` works, and it is what the clocks
//!   in [`Clock`] are calibrated around.
//! * What a script can reach is what the submodules below install, and nothing
//!   else: no file, no process, no socket. `require` answers for a handful of
//!   modules that only compute and format -- and for the `crypto` and `buffer`
//!   globals under the names Node gives them -- and refuses everything else,
//!   because the reason to want `fs` in a notification script is the reason not
//!   to have it.

mod binary;
mod bridge;
mod crypto;
mod globals;
mod node;
mod timers;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use reqwest::Client;
use rquickjs::context::EvalOptions;
use rquickjs::function::{Args, IntoJsFunc};
use rquickjs::promise::PromiseState;
use rquickjs::{CaughtError, Context, Ctx, Exception, Function, IntoJs, Promise, Runtime, Value};
use serde_json::Value as Json;
use tokio::runtime::Handle;

use crate::notify::{event_object, render, Config, EventMessage};
use bridge::Bridge;

/// How long one evaluation may take, network waits included.
///
/// A script can spend this on fetching, on waiting for its own timers, or on
/// running; it cannot spend more than this on all three together.
const BUDGET: Duration = Duration::from_secs(20);

/// How long one uninterrupted stretch of script may run, as the hub runs it.
///
/// The budget above bounds the evaluation; this bounds a single turn inside it,
/// which is what makes `while (true) {}` fail in seconds rather than in twenty.
/// It is pushed forward every time control comes back to the host, so a script
/// that waits on the network is not charged for the wait.
#[cfg(not(test))]
const BURST: Duration = Duration::from_secs(3);

/// The same ceiling under `cargo test`, deliberately much shorter.
///
/// What a test has to prove about this figure is that a loop which never gives
/// control back is interrupted at all: the arithmetic that turns the figure into
/// a deadline is the same at either value, and the error it produces is spelled
/// with `BURST` rather than with a number, so it adapts. What a test must not do
/// is spend the production figure proving it. The two runaway-script tests below
/// burn CPU for exactly as long as this allows, so at the production value they
/// are six seconds of the suite's own wall time and about a third of its CPU;
/// they run beside `api::tests`, whose two history-gate tests already race each
/// other for the one process-wide `HISTORY_GATE` about one run in three, and a
/// suite that is already timing-sensitive is the last place to add load for no
/// extra coverage. Two hundred milliseconds is still thousands of interrupt
/// polls -- the interrupt needs one -- so the semantics under test are the same
/// and the burn is a fraction of a second.
///
/// Only a test build gets this value. The hub always compiles the figure above,
/// so nothing an operator can see changes.
#[cfg(test)]
const BURST: Duration = Duration::from_millis(200);

/// The most heap one evaluation may allocate.
///
/// QuickJS refuses an allocation past this with an out-of-memory error, which is
/// a catchable channel error rather than an abort. The figure is far above what
/// a message costs -- templates and payloads are kilobytes -- and far below what
/// would matter to the hub's own memory.
const MEMORY: usize = 64 * 1024 * 1024;

/// How deep a script may nest before the engine refuses to grow its stack.
///
/// The engine's own default is 256 KiB, which a debug build reaches after about
/// fifty frames of ordinary script. That is thin for a script that walks a list
/// of clients by recursion, and raising it costs nothing: the interrupt handler
/// and the memory limit are what bound a runaway script, not the stack.
const STACK: usize = 512 * 1024;

/// The name every position in a script error is reported under.
///
/// The operator pasted the script into a textarea, so a position without a name
/// says nothing about where it came from.
const SCRIPT: &str = "notify.js";

/// What the engine calls the source it evaluates.
///
/// `Ctx::eval` does not let a caller name its source, and QuickJS puts whatever
/// name it was given into every stack frame. Errors are therefore rewritten on
/// the way out -- see [`failure`] -- so that `notify.js:12:3` is what an
/// operator reads instead of the engine's own placeholder.
const EVAL_NAME: &str = "eval_script";

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
    let http = http.clone();
    // Off the runtime's worker threads. `spawn_blocking` also gives the
    // evaluation a thread of its own, which is what a `!Send` engine, an
    // untrusted script and the engine's own stack limit all need.
    //
    // The clocks and the bridge are built on that thread rather than here: the
    // clock is shared between the engine's interrupt handler and the natives
    // through an `Rc`, which cannot cross a thread at all.
    tokio::task::spawn_blocking(move || evaluate(&call, http, Handle::current(), BUDGET))
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
    event: Json,
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

/// The clocks an evaluation runs under, shared with the engine's interrupt
/// handler.
///
/// Two deadlines rather than one, because an operator's script should be able to
/// wait twenty seconds for an endpoint and still be thrown out after a few
/// seconds of looping. `budget` is the evaluation's and covers everything;
/// `turn` is the currently running stretch of script, and is what the handler
/// compares against. It is pushed forward whenever control returns to the host,
/// so time spent waiting on the network, or sleeping until a timer is due, is
/// not charged to the script's own execution.
///
/// A `Cell` in an `Rc` rather than a lock: one evaluation owns one thread, and
/// the handler runs inside the engine, where taking a lock the installer might
/// hold would deadlock.
struct Clock {
    /// How long the evaluation was given, so that a refusal can name the figure
    /// it ran out of rather than the one the hub happens to use in production.
    span: Duration,
    budget: Cell<Instant>,
    turn: Cell<Instant>,
    /// Which clock tripped, for the error the operator reads.
    tripped: Cell<Option<&'static str>>,
}

impl Clock {
    fn new(span: Duration) -> Rc<Self> {
        let now = Instant::now();
        Rc::new(Self {
            span,
            budget: Cell::new(now + span),
            turn: Cell::new(now + BURST.min(span)),
            tripped: Cell::new(None),
        })
    }

    /// Starts the clock on a new turn: one entry into script from the host.
    fn turn(&self) {
        self.turn.set((Instant::now() + BURST).min(self.budget.get()));
    }

    /// What is left of the whole evaluation's budget.
    fn remaining(&self) -> Duration {
        self.budget.get().saturating_duration_since(Instant::now())
    }

    /// Whether the evaluation's budget is spent.
    fn expired(&self) -> bool {
        Instant::now() >= self.budget.get()
    }

    /// The handler the engine calls while it is executing code.
    ///
    /// QuickJS polls it on every backward branch and function entry, so a loop
    /// cannot leave it behind; returning `true` raises an exception the script
    /// cannot catch, which surfaces as an ordinary channel error.
    fn interrupt(self: &Rc<Self>) -> Box<dyn FnMut() -> bool + 'static> {
        let clock = Rc::clone(self);
        Box::new(move || {
            let now = Instant::now();
            let over_turn = now >= clock.turn.get();
            let over_budget = now >= clock.budget.get();
            // The worse of the two is the one worth reporting: a script that ran
            // past both ran past its budget first.
            if over_budget {
                clock.tripped.set(Some("budget"));
            } else if over_turn {
                clock.tripped.set(Some("turn"));
            }
            over_turn || over_budget
        })
    }

    /// The reason the engine was interrupted, if it was.
    fn stopped(&self) -> Option<&'static str> {
        self.tripped.get()
    }

    /// How long the evaluation was given, for a refusal that has to name it.
    fn span(&self) -> Duration {
        self.span
    }

    /// What running out of time is called, for the operator.
    ///
    /// The two are worth telling apart: a script that looped without yielding
    /// and one whose fetch never answered need different fixes, and the engine's
    /// own word for both is "interrupted".
    fn out_of_time(&self) -> String {
        match self.stopped() {
            Some("turn") => format!("the script ran {BURST:?} without giving control back"),
            _ => format!("the script did not finish within its {:?} budget", self.span),
        }
    }
}

// SAFETY: a clock holds no JavaScript value at all -- a duration, two instants
// and a word -- so there is no `'js` lifetime inside it for `Changed` to
// rewrite.
unsafe impl<'js> rquickjs::JsLifetime<'js> for Clock {
    type Changed<'to> = Clock;
}

/// Builds an engine, runs the script in it, calls the one function that sends.
///
/// The engine is built per event and dropped with it: a script cannot leave state
/// behind for the next event, and no `Runtime` or `Context` ever crosses a
/// thread.
fn evaluate(call: &Call, http: Client, handle: Handle, budget: Duration) -> Result<()> {
    let clock = Clock::new(budget);
    let bridge = Bridge::new(&http, handle, Rc::clone(&clock));
    let runtime = Runtime::new().map_err(|e| anyhow!("the JavaScript engine could not start: {e}"))?;
    runtime.set_memory_limit(MEMORY);
    runtime.set_max_stack_size(STACK);
    runtime.set_interrupt_handler(Some(clock.interrupt()));
    let context =
        Context::full(&runtime).map_err(|e| anyhow!("the JavaScript engine could not start: {e}"))?;
    // Everything the evaluation touches -- the engine, its globals, the values
    // it left behind -- is created and dropped inside this scope, on this
    // thread, before either the context or the runtime goes away.
    context.with(|ctx| run(&ctx, call, bridge, &clock))
}

/// One evaluation, from installed globals to the called function's outcome.
fn run<'js>(ctx: &Ctx<'js>, call: &Call, bridge: Bridge, clock: &Rc<Clock>) -> Result<()> {
    install(ctx, bridge)?;

    // The script, handed to the engine as one program: an operator's syntax
    // error is reported with its position, which is what makes it actionable.
    //
    // Sloppy mode, which is what a script written for the reference
    // implementation expects and what a Node module gets without a `'use
    // strict'` of its own: the engine would otherwise refuse an undeclared
    // assignment that the same script makes happily in goja.
    clock.turn();
    let mut options = EvalOptions::default();
    options.strict = false;
    ctx.eval_with_options::<Value, _>(call.script.as_bytes(), options)
        .map_err(|e| anyhow!("the script could not be loaded: {}", failure(ctx, clock, e)))?;

    // `sendEvent` wins when it is there, as in the reference: a script defining
    // both wants the structured event, and `sendMessage` is the older form.
    let (name, arguments) = if callable(ctx, "sendEvent") {
        let event = ctx
            .json_parse(call.event.to_string())
            .map_err(|e| anyhow!("the event could not be handed to sendEvent: {}", failure(ctx, clock, e)))?;
        ("sendEvent", vec![event])
    } else if callable(ctx, "sendMessage") {
        let message = call.message.as_str().into_js(ctx).map_err(|e| {
            anyhow!("the message could not be handed to sendMessage: {}", failure(ctx, clock, e))
        })?;
        let title = call.title.as_str().into_js(ctx).map_err(|e| {
            anyhow!("the title could not be handed to sendMessage: {}", failure(ctx, clock, e))
        })?;
        ("sendMessage", vec![message, title])
    } else {
        anyhow::bail!("the script defines neither sendEvent nor sendMessage");
    };

    let function: Function = ctx
        .globals()
        .get(name)
        .map_err(|e| anyhow!("{name} could not be read: {}", failure(ctx, clock, e)))?;
    let mut args = Args::new(ctx.clone(), arguments.len());
    for argument in arguments {
        args.push_arg(argument)
            .map_err(|e| anyhow!("{name} could not be called: {}", failure(ctx, clock, e)))?;
    }
    clock.turn();
    let returned: Value =
        function.call_arg(args).map_err(|e| anyhow!("{name} failed: {}", failure(ctx, clock, e)))?;

    settle(ctx, clock, name, returned)
}

/// Hands the script the globals it may have.
///
/// This is the whole capability list, so what a script can do is what these
/// modules write here. Their own natives are named with a leading `__` and are
/// deleted again by the bootstrap in [`globals`], so the operator's script sees
/// the finished objects and not the primitives they are built from.
fn install<'js>(ctx: &Ctx<'js>, bridge: Bridge) -> Result<()> {
    // Data the natives cannot capture: they are plain function pointers, and
    // host data is the only way for them to reach the client, the clocks and the
    // tables of pending work they have to borrow.
    ctx.store_userdata(bridge).map_err(|_| anyhow!("the network bridge could not be installed"))?;
    ctx.store_userdata(timers::Timers::default())
        .map_err(|_| anyhow!("the timer table could not be installed"))?;
    ctx.store_userdata(crypto::Hashes::default())
        .map_err(|_| anyhow!("the digest table could not be installed"))?;

    bridge::install(ctx)?;
    binary::install(ctx)?;
    crypto::install(ctx)?;
    node::install(ctx)?;
    timers::install(ctx)?;
    globals::install(ctx)?;
    Ok(())
}

/// Whether the script defined a callable global with this name.
fn callable(ctx: &Ctx<'_>, name: &str) -> bool {
    ctx.globals().get::<_, Value>(name).is_ok_and(|value| value.is_function())
}

/// Registers one native under the name the bootstrap looks it up by.
///
/// Every module installs through here so that the naming convention lives in one
/// place: a primitive is a global the bootstrap reads once and then deletes, so
/// what it is called only has to agree with the one list in [`globals`].
fn native<'js, P, F>(ctx: &Ctx<'js>, name: &str, function: F) -> Result<()>
where
    F: IntoJsFunc<'js, P> + 'js,
{
    let function =
        Function::new(ctx.clone(), function).map_err(|e| anyhow!("{name} could not be installed: {e}"))?;
    ctx.globals().set(name, function).map_err(|e| anyhow!("{name} could not be installed: {e}"))
}

/// A refusal a script can catch, as `fetch` in a browser rejects.
///
/// The exception is thrown into the context rather than returned as a host
/// error, so that a script's own `try`/`catch` sees it and so that a script that
/// does not catch it still gets the message and the position.
fn refuse<'js>(ctx: &Ctx<'js>, message: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(exception) => ctx.throw(exception.into_value()),
        // Only reachable if the engine cannot allocate an error at all, which is
        // itself the answer worth reporting.
        Err(error) => error,
    }
}

/// What the called function's return value means for the event.
///
/// A script written for the reference implementation may be `async`, and `await`
/// -- on our `fetch` included -- suspends into the microtask queue, so the queue
/// and the script's timers are both drained before the promise is judged. A
/// promise still pending afterwards never finished: reporting a send that has
/// not happened would be worse than reporting the error, because nobody looks at
/// a message that did arrive.
fn settle<'js>(ctx: &Ctx<'js>, clock: &Rc<Clock>, name: &str, returned: Value<'js>) -> Result<()> {
    let promise = returned.into_promise();
    timers::drain(ctx, clock, promise.as_ref())?;
    let Some(promise) = promise else { return Ok(()) };
    match promise.state() {
        PromiseState::Resolved => Ok(()),
        PromiseState::Rejected => Err(anyhow!("{name} rejected: {}", reason(ctx, &promise))),
        PromiseState::Pending => Err(anyhow!("{name} returned a promise that never settled")),
    }
}

/// Why a promise was rejected, as text an operator can act on.
///
/// `Promise::result` hands a rejection back the way the engine does everything
/// else: it throws the rejected value into the context and reports
/// `Error::Exception`, so reading it means catching it here and letting the
/// engine's own rendering of an `Error` -- message and stack -- do the work.
fn reason(ctx: &Ctx<'_>, promise: &Promise<'_>) -> String {
    match promise.result::<Value>() {
        Some(Err(error)) => tidy(&CaughtError::from_error(ctx, error).to_string()),
        Some(Ok(_)) => "the promise resolved after all".to_owned(),
        None => "the promise had not settled".to_owned(),
    }
}

/// The engine's error as the operator's text.
///
/// Three things are folded together here. A thrown error carries a message and a
/// stack, and the stack is where the position lives. Anything the host refuses
/// -- a value that will not convert -- carries a readable message of its own.
/// And an interrupt is the engine saying only "interrupted", which says nothing
/// about which ceiling was reached, so the clock answers for it instead.
fn failure(ctx: &Ctx<'_>, clock: &Clock, error: rquickjs::Error) -> String {
    let caught = CaughtError::from_error(ctx, error);
    if let CaughtError::Exception(exception) = &caught {
        if exception.message().as_deref() == Some("interrupted") {
            return clock.out_of_time();
        }
    }
    tidy(&caught.to_string())
}

/// The engine's error as text, without a clock's opinion about why it stopped.
///
/// For the failures that are the hub's own -- the bootstrap that builds the
/// script's world -- where there is no script time to have run out of.
fn plain(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    tidy(&CaughtError::from_error(ctx, error).to_string())
}

/// Rewrites the engine's source name in an error or a stack.
fn tidy(text: &str) -> String {
    text.replace(EVAL_NAME, SCRIPT)
}
