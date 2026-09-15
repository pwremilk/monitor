//! Timers, and the event loop that drains them.
//!
//! A script gets `setTimeout`, `setInterval`, `clearTimeout` and
//! `clearInterval`, and one execution of it drains them: the microtask queue
//! first, then every timer that is due, then the microtasks those queued, and so
//! on until there is nothing left to run or the evaluation's budget is spent.
//!
//! The loop lives here rather than in the engine because QuickJS has no event
//! loop of its own -- its job queue holds promise reactions and nothing else --
//! and because the drain has to be bounded by something the hub controls.
//! `fetch` is synchronous, so a timer is the only thing a script can wait for,
//! and waiting is what the budget exists to bound. A script that leaves an
//! interval running never lets the loop go idle, so it ends as a channel error
//! once the budget is spent rather than holding the thread for ever -- which is
//! also what Node does with an interval nobody cleared, minus the process.
//!
//! Callbacks are held as [`Persistent`] values: the table lives in host data, so
//! it has to outlive the scope it was saved in, and `Persistent` is the
//! engine's own way of keeping a value alive across one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use rquickjs::function::{Args, Opt, Rest};
use rquickjs::promise::PromiseState;
use rquickjs::{Ctx, Function, Persistent, Promise, Value};

use super::{failure, native, refuse, Clock};

/// The longest a script may ask a timer to wait.
///
/// A day, which is far past any budget a notification has; it is here because
/// turning a script's number into a `Duration` panics on one that will not fit,
/// and a panic in a release build of this hub is an abort.
const LONGEST: f64 = 86_400_000.0;

/// The timers one evaluation has scheduled.
///
/// Host data rather than a Rust object on the call path: the natives are plain
/// function pointers, and this is the only thing they can borrow. Dropped with
/// the engine, so nothing a script scheduled outlives the event it was
/// scheduled for.
#[derive(Default)]
pub(super) struct Timers {
    entries: RefCell<Vec<Timer>>,
    next_id: Cell<u32>,
}

/// One scheduled callback.
struct Timer {
    id: u32,
    /// When it is next due. An interval moves this forward each time it fires;
    /// a one-shot is removed instead.
    due: Instant,
    /// `Some` for `setInterval`, the delay it repeats at.
    every: Option<Duration>,
    callback: Persistent<Function<'static>>,
    arguments: Vec<Persistent<Value<'static>>>,
    /// Set on a one-shot that has run, so the sweep below can drop it.
    fired: bool,
}

impl Timers {
    fn push(
        &self,
        callback: Persistent<Function<'static>>,
        arguments: Vec<Persistent<Value<'static>>>,
        due: Instant,
        every: Option<Duration>,
    ) -> u32 {
        let mut entries = self.entries.borrow_mut();
        // Ids start at one so that zero -- what a script gets from clearing
        // something that was never a timer -- never names one.
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        entries.push(Timer { id, due, every, callback, arguments, fired: false });
        id
    }

    fn next_due(&self) -> Option<Instant> {
        self.entries.borrow().iter().map(|timer| timer.due).min()
    }

    /// Removes a timer, whether or not it is waiting.
    fn clear(&self, id: u32) {
        self.entries.borrow_mut().retain(|timer| timer.id != id);
    }

    /// The callbacks that are due, with the repeating ones rescheduled in place.
    ///
    /// Rescheduled before they are run, and copied out rather than taken: a
    /// `clearInterval` inside one callback has to find its own timer still in
    /// the table, and an interval that fires every millisecond would otherwise
    /// be dropped and re-added on every pass.
    fn due(&self, now: Instant) -> Vec<(Persistent<Function<'static>>, Vec<Persistent<Value<'static>>>)> {
        let mut entries = self.entries.borrow_mut();
        let mut due = Vec::new();
        for timer in entries.iter_mut() {
            if timer.due <= now {
                due.push((timer.callback.clone(), timer.arguments.clone()));
                match timer.every {
                    // From now rather than from the missed time: a slow callback
                    // makes an interval later, not a burst of catch-up calls.
                    Some(every) => timer.due = now + every,
                    None => timer.fired = true,
                }
            }
        }
        entries.retain(|timer| !timer.fired);
        due
    }
}

// SAFETY: the table has no `'js` lifetime of its own. What it keeps of the
// engine is a `Persistent`, which is rooted in the runtime rather than borrowed
// from a context, and everything else in it is plain Rust data.
unsafe impl<'js> rquickjs::JsLifetime<'js> for Timers {
    type Changed<'to> = Timers;
}

/// Installs the timer primitives under the names the bootstrap picks up.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    native(ctx, "__setTimeout", set_timeout)?;
    native(ctx, "__setInterval", set_interval)?;
    native(ctx, "__clearTimer", clear_timer)
}

/// `setTimeout(callback, delay, ...args)`: runs once, `delay` ms from now.
fn set_timeout<'js>(
    ctx: Ctx<'js>,
    callback: Function<'js>,
    delay: Opt<f64>,
    arguments: Rest<Value<'js>>,
) -> rquickjs::Result<u32> {
    schedule(&ctx, callback, delay.0, arguments.0, None)
}

/// `setInterval(callback, delay, ...args)`: runs every `delay` ms from now.
fn set_interval<'js>(
    ctx: Ctx<'js>,
    callback: Function<'js>,
    delay: Opt<f64>,
    arguments: Rest<Value<'js>>,
) -> rquickjs::Result<u32> {
    let every = wait_for(delay.0);
    schedule(&ctx, callback, delay.0, arguments.0, Some(every))
}

/// `clearTimeout(timer)` and `clearInterval(timer)`: one table, so one function
/// answers for either, as in Node.
fn clear_timer<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<()> {
    if let Some(timers) = ctx.userdata::<Timers>() {
        timers.clear(id);
    }
    Ok(())
}

/// Saves one callback, and the arguments it will be called with.
fn schedule<'js>(
    ctx: &Ctx<'js>,
    callback: Function<'js>,
    millis: Option<f64>,
    arguments: Vec<Value<'js>>,
    every: Option<Duration>,
) -> rquickjs::Result<u32> {
    let Some(timers) = ctx.userdata::<Timers>() else {
        return Err(refuse(ctx, "this context has no timer table"));
    };
    let due = Instant::now() + wait_for(millis);
    Ok(timers.push(
        Persistent::save(ctx, callback),
        arguments.into_iter().map(|value| Persistent::save(ctx, value)).collect(),
        due,
        every,
    ))
}

/// What a script's delay means, clamped rather than refused.
///
/// Node reads anything that is not a usable number as `0`, and so does this: a
/// timer that fired as soon as it could is a bug the script's author can see,
/// while an exception about a delay would be a bug they have to catch. The one
/// exception is the infinite delay, which means "never" in every language that
/// has the word -- it becomes the ceiling instead of a timer that fires at once.
fn wait_for(millis: Option<f64>) -> Duration {
    let millis = match millis {
        None => 0.0,
        Some(value) if value.is_infinite() => LONGEST,
        Some(value) if value.is_finite() => value,
        // `NaN`, which is what a delay that was not a number at all becomes.
        Some(_) => 0.0,
    };
    Duration::from_secs_f64(millis.clamp(0.0, LONGEST) / 1000.0)
}

/// Runs the event loop to a standstill: microtasks, then the timers that are
/// due, until there is nothing left to run or the budget is spent.
pub(super) fn drain<'js>(ctx: &Ctx<'js>, clock: &Rc<Clock>, promise: Option<&Promise<'js>>) -> Result<()> {
    loop {
        // A rejection is the script's own answer and the one the operator needs
        // to read, so the loop stops here rather than running the rest of a
        // script that has already failed.
        if rejected(promise) {
            return Ok(());
        }
        if clock.expired() {
            return Err(anyhow!("{}", clock.out_of_time()));
        }
        // Microtasks first, the way an event loop would order them: `await` on
        // the synchronous `fetch` lands here, and a timer callback must not jump
        // ahead of the continuation it was meant to follow.
        clock.turn();
        while ctx.execute_pending_job() {}
        if rejected(promise) {
            return Ok(());
        }
        let Some(due) = next_due(ctx) else { return Ok(()) };

        let now = Instant::now();
        if due > now {
            let wait = due - now;
            // A timer that is not due before the budget is spent will never be
            // reached, so there is nothing to wait for: failing now rather than
            // sleeping first is the difference between a channel error in
            // milliseconds and one in twenty seconds.
            if wait > clock.remaining() {
                return Err(anyhow!("{}", clock.out_of_time()));
            }
            std::thread::sleep(wait);
        }
        fire(ctx, clock)?;
    }
}

/// Whether the script's promise has been rejected, which the caller reports.
fn rejected(promise: Option<&Promise<'_>>) -> bool {
    promise.is_some_and(|promise| promise.state() == PromiseState::Rejected)
}

/// When the next timer is due, if any is waiting.
fn next_due<'js>(ctx: &Ctx<'js>) -> Option<Instant> {
    ctx.userdata::<Timers>()?.next_due()
}

/// Runs every timer that is due, in the order they were scheduled.
fn fire<'js>(ctx: &Ctx<'js>, clock: &Rc<Clock>) -> Result<()> {
    let now = Instant::now();
    let due = match ctx.userdata::<Timers>() {
        Some(timers) => timers.due(now),
        None => return Ok(()),
    };
    for (callback, arguments) in due {
        let callback =
            callback.restore(ctx).map_err(|e| anyhow!("a timer's callback could not be read: {e}"))?;
        let mut values = Args::new(ctx.clone(), arguments.len());
        for argument in arguments {
            let value =
                argument.restore(ctx).map_err(|e| anyhow!("a timer's argument could not be read: {e}"))?;
            values.push_arg(value).map_err(|e| anyhow!("a timer's argument could not be passed: {e}"))?;
        }
        // The callback is a fresh turn: what it does with its own time is
        // bounded here, and so is what it spends on the network.
        clock.turn();
        callback
            .call_arg::<Value>(values)
            .map_err(|e| anyhow!("a timer callback failed: {}", failure(ctx, clock, e)))?;
    }
    Ok(())
}
