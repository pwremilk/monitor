//! Tests for the JavaScript channel, in the module's own tree so that the
//! engine's behaviour and the tests that pin it down are read together.
//!
//! Two helpers carry most of them: `reason` asks for the text of the channel
//! error a script produces, and `run` asks whether it produced one at all. A
//! script's only output is its behaviour, so a test says what it expects by
//! throwing when it does not see it -- and a script that sends nothing at all
//! looks exactly like a channel that works, which is why the failure paths are
//! tested as carefully as the happy one.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Instant;

use super::*;
use crate::notify::{offline_event, Provider};
use reqwest::Client;

/// The channel's configuration for one script, with everything else default.
fn config(script: &str) -> Config {
    let mut config = Config::load(|_| None);
    config.provider = Provider::JavaScript;
    config.javascript_script = script.to_owned();
    config
}

/// Runs one script the way the hub runs it, and answers whether it sent.
async fn run(script: &str) -> Result<()> {
    send(&Client::new(), &config(script), &offline_event("vps-1")).await
}

/// The reason the channel gives for a script, which is what the panel shows.
async fn reason(script: &str) -> String {
    run(script).await.unwrap_err().to_string()
}

/// Evaluates one script with a budget of its own instead of the channel's.
///
/// The clocks are per-evaluation, so this is what lets a test about running out
/// of time take milliseconds rather than twenty seconds. It runs on the calling
/// thread: `evaluate` is the whole of one evaluation, and a test may as well
/// borrow its own.
fn evaluate_with(script: &str, budget: Duration) -> Result<()> {
    let config = config(script);
    let event = offline_event("vps-1");
    let call = Call::for_event(&config, &event);
    evaluate(&call, Client::new(), Handle::current(), budget)
}

/// A script is an operator's text, and every way it can be wrong has to come
/// back as the reason rather than as silence or a panic: a message nobody
/// received looks exactly like a channel that works.
#[tokio::test]
async fn a_broken_script_is_reported_with_its_reason() {
    assert_eq!(
        reason("   ").await,
        "the JavaScript notification script is not configured",
        "a script that was never written is its own kind of error"
    );

    // A syntax error has to say where it is: the operator is looking at a
    // textarea, and a position without a name would not tell them which setting
    // it came from.
    let syntax = reason("function sendMessage( {").await;
    assert!(syntax.contains("could not be loaded"), "{syntax}");
    assert!(syntax.contains("notify.js:1:"), "the position is missing: {syntax}");

    let missing = reason("function anythingElse() {}").await;
    assert!(missing.contains("neither sendEvent nor sendMessage"), "{missing}");

    let threw = reason(r#"function sendMessage() { throw new Error("no token") }"#).await;
    assert!(threw.contains("sendMessage failed") && threw.contains("no token"), "{threw}");
    assert!(threw.contains("notify.js:1:"), "the position is missing: {threw}");

    let deep = reason("function sendMessage() { (function again() { again() })() }").await;
    assert!(deep.contains("sendMessage failed"), "{deep}");
    assert!(deep.to_lowercase().contains("stack"), "{deep}");
}

/// The ceilings are the engine's own, and each of them is a channel error rather
/// than a panic or a thread that never comes back.
#[tokio::test]
async fn the_engine_holds_a_runaway_script_within_its_limits() {
    // A loop with nothing to allocate is stopped by the interrupt handler, which
    // is the only one of the three ceilings that a loop can reach.
    let started = Instant::now();
    let runaway = reason("function sendMessage() { while (true) {} }").await;
    let took = started.elapsed();
    assert!(runaway.contains("sendMessage failed") || runaway.contains("did not finish"), "{runaway}");
    assert!(took < Duration::from_secs(15), "a runaway script held the thread for {took:?}");

    // The memory limit is reachable from a script that allocates instead.
    let memory =
        reason("function sendMessage() { const held = []; while (true) held.push(new Array(4096).fill(1)) }")
            .await;
    assert!(memory.contains("sendMessage failed") || memory.contains("did not finish"), "{memory}");
}

/// A script that never finishes must not hold the hub: it runs beside the
/// runtime, so timers and sockets keep being served while it burns its budget,
/// and it ends as an error.
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
    assert!(took < Duration::from_secs(15), "abandoned only after {took:?}");
    eprintln!("runaway script abandoned after {took:?}: {}", outcome.unwrap_err());
}

/// The capability list, end to end: what a notification script may reach, and
/// what it must not, in one script that fails loudly on either mistake.
#[tokio::test]
async fn the_script_reaches_the_node_style_surface_and_nothing_more() {
    let script = r#"
        function sendMessage() {
            const provided = [
                ['fetch', typeof fetch, 'function'], ['console.log', typeof console.log, 'function'],
                ['console.warn', typeof console.warn, 'function'], ['console.error', typeof console.error, 'function'],
                ['console.info', typeof console.info, 'function'], ['console.debug', typeof console.debug, 'function'],
                ['setTimeout', typeof setTimeout, 'function'], ['clearTimeout', typeof clearTimeout, 'function'],
                ['setInterval', typeof setInterval, 'function'], ['clearInterval', typeof clearInterval, 'function'],
                ['atob', typeof atob, 'function'], ['btoa', typeof btoa, 'function'],
                ['Buffer', typeof Buffer, 'function'], ['crypto', typeof crypto, 'object'],
                ['crypto.subtle', typeof crypto.subtle, 'object'],
                ['process', typeof process, 'object'], ['require', typeof require, 'function'],
            ];
            for (const [name, kind, expected] of provided) {
                if (kind !== expected) throw new Error(name + ' is ' + kind);
            }
            if (typeof crypto.subtle.digest !== 'function') throw new Error('crypto.subtle.digest');
            for (const name of ['randomUUID', 'getRandomValues', 'createHash', 'createHmac']) {
                if (typeof crypto[name] !== 'function') throw new Error('crypto.' + name);
            }
            for (const name of ['env', 'platform', 'arch', 'version', 'versions', 'argv']) {
                if (!(name in process)) throw new Error('process.' + name + ' is missing');
            }

            // What a notification script has no business having. Each of these
            // is one file, one process or one socket away from the hub.
            for (const specifier of [
                'fs', 'node:fs', 'fs/promises', 'node:fs/promises',
                'child_process', 'node:child_process', 'net', 'node:net',
                'http', 'node:http', 'https', 'node:https', 'dgram', 'node:dgram',
                'worker_threads', 'node:worker_threads', 'os/anything', 'node:module',
            ]) {
                let message = null;
                try { require(specifier) } catch (error) { message = String(error && error.message) }
                if (message === null || message.indexOf('Cannot find module') !== 0) {
                    throw new Error(specifier + ' was loadable: ' + message);
                }
            }
            for (const name of ['XMLHttpRequest', 'WebSocket', 'fs', 'child_process', 'net', 'http']) {
                if (typeof globalThis[name] !== 'undefined') throw new Error(name + ' exists');
            }
            if (typeof process.exit !== 'undefined') throw new Error('process.exit exists');
            if (typeof process.binding !== 'undefined') throw new Error('process.binding exists');
            if (typeof process.kill !== 'undefined') throw new Error('process.kill exists');
            if (typeof process.chdir !== 'undefined') throw new Error('process.chdir exists');
            if (typeof process.env === 'object' && !Object.isFrozen(process.env)) throw new Error('process.env is writable');

            // A module the engine could load would be a module the hub did not
            // put there, so a dynamic import has to fail like a static one.
            return import('node:fs').then(
                () => { throw new Error('a dynamic import loaded') },
                (error) => {
                    // The engine reports a module it cannot load as a
                    // `ReferenceError` with no `message` of its own, so what is
                    // checked is the text it does carry.
                    if (String(error).indexOf('could not load module') < 0) {
                        throw new Error('the import failed oddly: ' + error);
                    }
                },
            );
        }
    "#;
    run(script).await.unwrap();
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
    run(script).await.unwrap();
}

/// The `sendMessage` form gets the rendered template and the event name, and an
/// `async` one still sends: `await` suspends into the microtask queue, so the
/// queue has to be drained before the promise is judged.
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
    run(script).await.unwrap();

    // A promise that never settles is a script that never sent anything, and
    // saying so is the point of the check.
    let pending = reason("function sendMessage() { return new Promise(() => {}) }").await;
    assert!(pending.contains("never settled"), "{pending}");

    let rejected = reason(r#"async function sendMessage() { throw new Error("nope") }"#).await;
    assert!(rejected.contains("rejected") && rejected.contains("nope"), "{rejected}");

    // An `await` on nothing at all also has to be waited out.
    let delayed = reason(r#"async function sendMessage() { await null; throw new Error("later") }"#).await;
    assert!(delayed.contains("later"), "{delayed}");
}

/// Timers are drained after the synchronous part of a script has finished, and
/// they are what the loop waits for when nothing else is left.
#[tokio::test]
async fn the_timers_a_script_sets_are_drained_after_it_returns() {
    let script = r#"
        function sendMessage() {
            let finished = false
            setTimeout(() => {
                if (!finished) throw new Error("the timer ran before the synchronous part ended")
                if (globalThis.__trace !== "sync,microtask") throw new Error("trace: " + globalThis.__trace)
            }, 5)
            Promise.resolve().then(() => { globalThis.__trace += ",microtask" })
            globalThis.__trace = "sync"
            finished = true
        }
    "#;
    run(script).await.unwrap();

    // A promise a timer settles is a promise the loop waits for.
    let settled = r#"
        function sendMessage() {
            let early = false
            setTimeout(() => { early = true }, 1)
            return new Promise((resolve, reject) => {
                setTimeout(() => {
                    if (!early) reject(new Error("the later timer ran first"))
                    else resolve()
                }, 5)
            })
        }
    "#;
    run(settled).await.unwrap();

    // And a cleared timer is one the loop does not wait for: without the clear,
    // this script's callback would throw.
    let cleared = r#"
        function sendMessage() {
            const id = setTimeout(() => { throw new Error("a cleared timer ran") }, 1)
            clearTimeout(id)
            const interval = setInterval(() => { throw new Error("a cleared interval ran") }, 1)
            clearInterval(interval)
        }
    "#;
    run(cleared).await.unwrap();

    // An interval that clears itself when it is done is the shape a script
    // actually needs: one that never clears it runs until the budget is spent,
    // which the short budget here would turn into a failure.
    let repeating = r#"
        function sendMessage() {
            return new Promise((resolve, reject) => {
                let ticks = 0
                const id = setInterval(() => {
                    ticks += 1
                    if (ticks === 3) { clearInterval(id); resolve() }
                    if (ticks > 3) reject(new Error("the interval kept running"))
                }, 1)
            })
        }
    "#;
    evaluate_with(repeating, Duration::from_secs(5)).unwrap();
}

/// What a timer callback throws is the channel's answer, not a log line: a send
/// that failed inside a timer did not happen, and saying otherwise would be
/// worse than the error.
#[tokio::test]
async fn a_timer_that_throws_is_a_channel_error() {
    let script = r#"function sendMessage() { setTimeout(() => { throw new Error("in the timer") }, 1) }"#;
    let reason = reason(script).await;
    assert!(reason.contains("timer callback failed") && reason.contains("in the timer"), "{reason}");
    assert!(reason.contains("notify.js:1:"), "the position is missing: {reason}");
}

/// `Buffer` in the four shapes a script uses it in, against the reference
/// implementation's own vectors.
#[tokio::test]
async fn buffer_encodes_and_decodes_the_way_node_does() {
    let script = r#"
        function sendMessage() {
            const roundTrip = Buffer.from("aGVsbG8sIHdvcmxk", "base64").toString("utf8")
            if (roundTrip !== "hello, world") throw new Error("base64: " + roundTrip)
            if (Buffer.from("hello, world").toString("base64") !== "aGVsbG8sIHdvcmxk") throw new Error("encode")
            if (Buffer.from("68656c6c6f", "hex").toString("utf8") !== "hello") throw new Error("hex")
            if (Buffer.from("hello").toString("hex") !== "68656c6c6f") throw new Error("hex out")
            // The URL-safe spelling, which is the same encoding with two
            // characters exchanged and no padding.
            if (Buffer.from("hello").toString("base64url") !== "aGVsbG8") throw new Error("base64url out")
            if (Buffer.from("aGVsbG8", "base64url").toString("utf8") !== "hello") throw new Error("base64url in")
            if (Buffer.from([251, 255]).toString("base64") !== "+/8=") throw new Error("the standard alphabet")
            if (Buffer.from([251, 255]).toString("base64url") !== "-_8") throw new Error("the url-safe alphabet")

            if (Buffer.byteLength("hello") !== 5) throw new Error("byteLength")
            if (Buffer.byteLength("68656c6c6f", "hex") !== 5) throw new Error("byteLength hex")
            if (!Buffer.isBuffer(Buffer.from("x"))) throw new Error("isBuffer")
            if (Buffer.isBuffer(new Uint8Array(1))) throw new Error("isBuffer says yes to a Uint8Array")

            const allocated = Buffer.alloc(4, 7)
            if (allocated.length !== 4 || allocated[3] !== 7) throw new Error("alloc")
            if (allocated.toString("hex") !== "07070707") throw new Error("alloc hex")

            const joined = Buffer.concat([Buffer.from("ab"), Buffer.from("cd")], 4)
            if (joined.toString("utf8") !== "abcd") throw new Error("concat: " + joined.toString("utf8"))
            if (Buffer.concat([Buffer.from("ab"), Buffer.from("cd")], 1).toString("utf8") !== "a") {
                throw new Error("concat with a total length")
            }

            // It is a Uint8Array, which is what makes it usable as bytes.
            const bytes = Buffer.from("hi")
            if (!(bytes instanceof Uint8Array) || !ArrayBuffer.isView(bytes)) throw new Error("not a Uint8Array")
            if (bytes.buffer.byteLength !== 2) throw new Error("no backing buffer")
        }
    "#;
    run(script).await.unwrap();

    let unknown = reason(r#"function sendMessage() { Buffer.from("x").toString("rot13") }"#).await;
    assert!(unknown.contains("Unknown encoding"), "{unknown}");
    // The runtime's own frames are named apart from the script's. An operator
    // sent to `notify.js:112` for a five-line script would go looking for a line
    // that is not theirs.
    assert!(unknown.contains("runtime.js:"), "a runtime frame lost its name: {unknown}");
    assert!(unknown.contains("notify.js:1:"), "the script's own frame lost its name: {unknown}");
}

/// `atob` and `btoa` are the browser pair, over the same encodings.
#[tokio::test]
async fn atob_and_btoa_round_trip_binary_strings() {
    let script = r#"
        function sendMessage() {
            if (btoa("hello") !== "aGVsbG8=") throw new Error("btoa: " + btoa("hello"))
            if (atob("aGVsbG8=") !== "hello") throw new Error("atob: " + atob("aGVsbG8="))
            if (atob("aGVs bG8=\n") !== "hello") throw new Error("atob with whitespace")
            // Every byte, including the ones that are not text: this is what a
            // binary string is for.
            let binary = ""
            for (let byte = 0; byte < 256; byte++) binary += String.fromCharCode(byte)
            const roundTrip = atob(btoa(binary))
            if (roundTrip.length !== 256) throw new Error("length: " + roundTrip.length)
            for (let byte = 0; byte < 256; byte++) {
                if (roundTrip.charCodeAt(byte) !== byte) throw new Error("byte " + byte)
            }
            try { btoa("\u{1F600}") } catch (error) { return }
            throw new Error("btoa accepted a character above U+00FF")
        }
    "#;
    run(script).await.unwrap();
}

/// `crypto` against the published vectors, so that a digest is the digest the
/// endpoint on the other side expects rather than merely a stable one.
#[tokio::test]
async fn crypto_matches_the_published_digests_and_hmacs() {
    let script = r#"
        async function sendMessage() {
            const vectors = {
                sha1: "a9993e364706816aba3e25717850c26c9cd0d89d",
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                sha512: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a" +
                        "2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            }
            for (const [algorithm, expected] of Object.entries(vectors)) {
                const hex = crypto.createHash(algorithm).update("abc").digest("hex")
                if (hex !== expected) throw new Error(algorithm + ": " + hex)
            }
            // The same name in the other spellings Node and WebCrypto accept.
            if (crypto.createHash("SHA256").update("abc").digest("hex") !== vectors.sha256) {
                throw new Error("the upper-case name was refused")
            }

            // RFC 4231 test case 2.
            const hmac = crypto.createHmac("sha256", "Jefe").update("what do ya want for nothing?").digest("hex")
            if (hmac !== "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843") {
                throw new Error("hmac: " + hmac)
            }
            // Chained updates are the same message as one update.
            const split = crypto.createHmac("sha256", "Jefe")
            if (split.update("what do ya ").update("want for nothing?").digest("hex") !== hmac) {
                throw new Error("a chained update is not the same message")
            }
            // An algorithm nobody supports is refused where it is asked for,
            // rather than quietly computing something else.
            let refusal = null
            try { crypto.createHmac("md5", "k") } catch (error) { refusal = String(error && error.message) }
            if (refusal !== "Digest method not supported") throw new Error("md5: " + refusal)

            // A digest is a Buffer unless an encoding asks otherwise.
            const digest = crypto.createHash("sha256").update("abc").digest()
            if (!Buffer.isBuffer(digest) || digest.length !== 32) throw new Error("digest() is not a Buffer")
            if (digest.toString("base64") !== crypto.createHash("sha256").update("abc").digest("base64")) {
                throw new Error("base64 digest")
            }
            // Bytes as readily as text.
            if (crypto.createHash("sha256").update(Buffer.from("abc")).digest("hex") !== vectors.sha256) {
                throw new Error("a Buffer input")
            }

            // WebCrypto's own spelling, and the ArrayBuffer it resolves with.
            const subtle = await crypto.subtle.digest("SHA-256", Buffer.from("abc"))
            if (Buffer.from(new Uint8Array(subtle)).toString("hex") !== vectors.sha256) {
                throw new Error("subtle.digest")
            }
            const named = await crypto.subtle.digest({ name: "SHA-512" }, Buffer.from("abc"))
            if (Buffer.from(new Uint8Array(named)).toString("hex") !== vectors.sha512) {
                throw new Error("subtle.digest with a name object")
            }

            // A version 4 UUID, and a different one every time.
            const seen = {}
            for (let index = 0; index < 32; index++) {
                const uuid = crypto.randomUUID()
                if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid)) {
                    throw new Error("uuid: " + uuid)
                }
                if (seen[uuid]) throw new Error("the same uuid twice")
                seen[uuid] = true
            }

            const filled = new Uint8Array(32)
            if (crypto.getRandomValues(filled) !== filled) throw new Error("getRandomValues returned another array")
            if (filled.every((byte) => byte === 0)) throw new Error("getRandomValues filled nothing")
            const alsoFilled = new Uint32Array(4)
            crypto.getRandomValues(alsoFilled)
            if (alsoFilled.every((word) => word === 0)) throw new Error("getRandomValues left a Uint32Array alone")
            try { crypto.getRandomValues(new Float32Array(1)) } catch (error) { return }
            throw new Error("getRandomValues accepted a float array")
        }
    "#;
    run(script).await.unwrap();
}

/// `require` answers for the few modules that compute and format, and for
/// nothing that could reach outside the hub.
#[tokio::test]
async fn require_answers_for_the_modules_a_script_may_have() {
    let script = r#"
        function sendMessage() {
            const path = require("node:path")
            if (path.join("a", "b", "..", "c") !== "a/c") throw new Error("join: " + path.join("a", "b", "..", "c"))
            if (path.join("/a", "b/") !== "/a/b/") throw new Error("join keeps a trailing slash: " + path.join("/a", "b/"))
            if (path.join() !== ".") throw new Error("join of nothing")
            if (path.basename("/a/b.txt") !== "b.txt") throw new Error("basename")
            if (path.basename("/a/b.txt", ".txt") !== "b") throw new Error("basename with a suffix")
            if (path.basename("/") !== "") throw new Error("basename of the root")
            if (path.dirname("/a/b.txt") !== "/a") throw new Error("dirname: " + path.dirname("/a/b.txt"))
            if (path.dirname("a") !== ".") throw new Error("dirname of a bare name")
            if (path.extname("a/b.txt") !== ".txt") throw new Error("extname")
            if (path.extname(".bashrc") !== "") throw new Error("extname of a dotfile")
            if (path.sep !== "/" || path.delimiter !== ":") throw new Error("the separators")
            if (path.isAbsolute("/a") !== true || path.isAbsolute("a") !== false) throw new Error("isAbsolute")
            if (path.resolve("a", "b") !== "/a/b") throw new Error("resolve: " + path.resolve("a", "b"))
            if (path.normalize("/a/./b/../c") !== "/a/c") throw new Error("normalize: " + path.normalize("/a/./b/../c"))
            if (require("path") !== require("node:path")) throw new Error("the two spellings disagree")

            const os = require("node:os")
            if (os.platform() !== process.platform) throw new Error("platform")
            if (os.arch() !== process.arch) throw new Error("arch")
            if (typeof os.hostname() !== "string") throw new Error("hostname: " + typeof os.hostname())
            if (typeof os.tmpdir() !== "string" || os.tmpdir().length === 0) throw new Error("tmpdir")
            if (os.EOL !== "\n") throw new Error("EOL")

            const util = require("node:util")
            if (util.format("%s=%d", "a", 2) !== "a=2") throw new Error("format: " + util.format("%s=%d", "a", 2))
            if (util.format("%j", { a: 1 }) !== '{"a":1}') throw new Error("format %j")
            if (util.format("100%") !== "100%") throw new Error("format with no placeholder")
            if (util.inspect({ a: 1 }) !== '{"a":1}') throw new Error("inspect")
        }
    "#;
    run(script).await.unwrap();

    // Nothing else is a module, and the refusal names what was asked for.
    for specifier in ["node:fs", "node:child_process", "node:net", "node:http"] {
        let refused = reason(&format!(r#"function sendMessage() {{ require("{specifier}") }}"#)).await;
        assert!(refused.contains(&format!("Cannot find module '{specifier}'")), "{refused}");
    }
}

/// `require('node:crypto')` and `require('node:buffer')` are the globals a script
/// moved here from Node expects, and they are *those* objects rather than copies
/// of them: a key hashed through one name has to be the key the other computes
/// with, and `Buffer.isBuffer` has to say yes to a buffer built either way.
#[tokio::test]
async fn require_hands_back_the_crypto_and_buffer_globals() {
    let script = r#"
        async function sendMessage() {
            // Node spells both of these with and without the prefix, and every
            // spelling has to be the one object the globals hold.
            if (require("node:crypto") !== crypto) throw new Error("node:crypto is not the global crypto")
            if (require("crypto") !== crypto) throw new Error("crypto is not the global crypto")
            if (require("crypto") !== require("node:crypto")) throw new Error("the two spellings disagree")
            if (require("node:crypto").subtle !== crypto.subtle) throw new Error("subtle is not the same object")

            const buffer = require("node:buffer")
            if (buffer.Buffer !== Buffer) throw new Error("node:buffer.Buffer is not the global Buffer")
            if (require("buffer") !== buffer) throw new Error("the two spellings disagree")
            if (!buffer.Buffer.isBuffer(buffer.Buffer.from("x"))) throw new Error("isBuffer across the two names")

            // The vectors, read through the module this time: a script that takes
            // its digest from `require` gets the published one.
            const sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            const hash = require("node:crypto").createHash("sha256").update("abc").digest("hex")
            if (hash !== sha256) throw new Error("sha256: " + hash)
            if (require("node:crypto").createHash("sha256").update("abc").digest("hex") !==
                crypto.createHash("sha256").update("abc").digest("hex")) {
                throw new Error("the two names compute different digests")
            }
            // RFC 4231 test case 2, through the unprefixed name.
            const hmac = require("crypto").createHmac("sha256", "Jefe").update("what do ya want for nothing?")
            if (hmac.digest("hex") !== "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843") {
                throw new Error("hmac")
            }
            // And the other half of the interface: a key generated through the
            // module is one the global can hash.
            const key = require("node:crypto").randomUUID()
            if (crypto.createHash("sha256").update(key).digest().length !== 32) throw new Error("uuid digest")
            const filled = new Uint8Array(8)
            if (require("node:crypto").getRandomValues(filled) !== filled) throw new Error("getRandomValues")
            // `subtle` is reached through the module and its `ArrayBuffer` read
            // back by the global `Buffer`, which is the pair a script uses.
            const subtle = await require("node:crypto").subtle.digest("SHA-256", Buffer.from("abc"))
            if (Buffer.from(new Uint8Array(subtle)).toString("hex") !== sha256) {
                throw new Error("subtle.digest through require: " + Buffer.from(new Uint8Array(subtle)).toString("hex"))
            }

            if (require("node:buffer").Buffer.from("aGVsbG8=", "base64").toString("utf8") !== "hello") {
                throw new Error("base64 through require")
            }
            if (require("buffer").Buffer.byteLength("68656c6c6f", "hex") !== 5) throw new Error("byteLength")
            // A buffer is a buffer whichever name built it, which is what makes
            // the two interchangeable in a script.
            if (!Buffer.isBuffer(require("node:buffer").Buffer.alloc(2))) throw new Error("alloc")
        }
    "#;
    run(script).await.unwrap();
}

/// `process` reports facts and can change nothing.
#[tokio::test]
async fn process_reports_facts_and_holds_no_way_out() {
    let script = r#"
        function sendMessage() {
            if (typeof process.platform !== "string" || process.platform.length === 0) throw new Error("platform")
            if (typeof process.arch !== "string" || process.arch.length === 0) throw new Error("arch")
            if (!/^v[0-9]+\./.test(process.version)) throw new Error("version: " + process.version)
            if (typeof process.versions.monitor !== "string") throw new Error("versions")
            if (process.argv.length !== 2) throw new Error("argv: " + JSON.stringify(process.argv))
            if (typeof process.env !== "object") throw new Error("env")
            for (const [name, value] of Object.entries(process.env)) {
                if (typeof value !== "string") throw new Error("env." + name + " is not a string")
            }
            // A snapshot, and frozen: writing to it is a mistake the script finds
            // out about here rather than one that misleads it later.
            const before = process.env.A_REAL_VARIABLE_IS_NOT_NEEDED
            process.env.A_REAL_VARIABLE_IS_NOT_NEEDED = "written"
            if (process.env.A_REAL_VARIABLE_IS_NOT_NEEDED !== before) throw new Error("process.env is writable")
        }
    "#;
    run(script).await.unwrap();
}

/// Echoes each request back as JSON, so a script can check everything the bridge
/// put on the wire.
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
        async function sendMessage(message, title) {{
            const plain = fetch("{url}")
            if (plain.status !== 200 || plain.ok !== true) throw new Error("GET: " + JSON.stringify(plain))
            if (!JSON.parse(plain.body).line.startsWith("GET /sink")) throw new Error("GET line: " + plain.body)

            const sent = fetch("{url}", {{
                method: "post",
                headers: {{ "X-Script": "yes" }},
                body: JSON.stringify({{ message: message, title: title }})
            }})
            if (!sent.ok) throw new Error("POST status: " + sent.status)
            const echoed = JSON.parse(sent.body)
            if (!echoed.line.startsWith("POST /sink")) throw new Error("POST line: " + echoed.line)
            if (!/^x-script: yes$/i.test(echoed.header)) throw new Error("header: " + echoed.header)
            if (!echoed.body.includes("vps-1") || !echoed.body.includes("Offline")) throw new Error("body: " + echoed.body)

            // The awaited form is the same request: `fetch` is synchronous, so
            // both have to work and neither may need the other.
            const awaited = await fetch("{url}")
            if (awaited.status !== 200) throw new Error("await status: " + awaited.status)
        }}
    "#
    );
    run(&script).await.unwrap();

    // A refusal is a refusal, and the URL it was asked for does not travel with
    // it: an address a script sends to is often the whole credential.
    let refused = format!(r#"function sendMessage() {{ fetch("{url}", {{ method: "NOT A METHOD" }}) }}"#);
    let refused = reason(&refused).await;
    assert!(refused.contains("is not an HTTP method"), "{refused}");
    assert!(!refused.contains("127.0.0.1"), "the URL leaked: {refused}");

    // A script's own refusal is catchable, the way it would be in a browser.
    let caught = format!(
        r#"
        function sendMessage() {{
            try {{ fetch("{url}", {{ method: "NOT A METHOD" }}) }}
            catch (error) {{ if (String(error.message).includes("is not an HTTP method")) return; throw error }}
            throw new Error("the refusal was not thrown")
        }}
    "#
    );
    run(&caught).await.unwrap();

    let bad_url = reason(r#"function sendMessage() { fetch("not a url") }"#).await;
    assert!(bad_url.contains("the URL cannot be used"), "{bad_url}");
}

/// The budget bounds what a script may wait for. Without it a script that keeps
/// fetching is bounded only by the number of requests it can make.
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
    let started = Instant::now();
    let error = evaluate_with(&script, Duration::from_millis(300)).unwrap_err().to_string();
    assert!(error.contains("budget"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());

    // A script whose timers outlast the budget is the same kind of failure, and
    // it is the timers that say so rather than a sleep that was never going to
    // end in time.
    let slow = r#"function sendMessage() { setTimeout(() => {}, 3600000) }"#;
    let started = Instant::now();
    let error = evaluate_with(slow, Duration::from_millis(300)).unwrap_err().to_string();
    assert!(error.contains("budget"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1), "waited {:?}", started.elapsed());
}

/// `console` writes into the hub's log rather than into the void, which is what
/// makes it worth having.
#[tokio::test]
async fn console_writes_to_the_log() {
    let script = r#"
        function sendMessage() {
            console.log("plain", 1, { a: [1, 2] })
            console.info("info")
            console.warn("warn")
            console.error("error")
            console.debug("debug")
        }
    "#;
    run(script).await.unwrap();
}
