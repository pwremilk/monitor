//! What a script sees: the bootstrap that turns the host's primitives into the
//! JavaScript objects a Node script expects.
//!
//! The host installs primitives -- one function per thing that needs Rust -- and
//! this module writes the surface on top of them: `Buffer`, `crypto`, `process`,
//! `console`, `require`, and the encodings they share. The split is deliberate.
//! Arithmetic on bytes, digests and randomness belong in Rust, where a table is
//! written once and the compiler checks it; the *shape* of `Buffer` -- a
//! `Uint8Array` subclass with a static `from`, `alloc` and `concat` -- is a
//! JavaScript idea, and writing it as JavaScript is what keeps the Rust side
//! from growing an object model it would only ever use once.
//!
//! The bootstrap is also the capability boundary. It reads every `__`-prefixed
//! primitive the other modules installed, keeps them in its own scope, and
//! deletes them from the global object: from the operator's script's point of
//! view the primitives do not exist, and the closures below are the only things
//! that still hold them. A name that fails to arrive is a hard error here rather
//! than an `undefined` that shows up later as a confusing script error.

use rquickjs::{Ctx, Module, Promise, Value};
use tracing::{debug, error, info, warn};

use super::native;

/// What the engine calls the bootstrap's source.
///
/// A name of its own, and a *module* rather than a script, for one reason: the
/// engine names every source it evaluates and puts that name into every stack
/// frame, and a script error that reported a position inside the bootstrap as
/// `notify.js:112` would send an operator looking for a line their script does
/// not have. A module is the only way to hand the engine a name, and this one
/// needs nothing else a module offers: it imports nothing and exports nothing,
/// and its strict mode is what the bootstrap asks for anyway.
const MODULE: &str = "runtime.js";

/// The bootstrap, as one program.
///
/// One raw string rather than a file: it is compiled into the binary, and the
/// release image has no filesystem to fetch it from even if it were not.
const BOOTSTRAP: &str = r#"
(function () {
  'use strict';

  // ---- the host's primitives ----
  const natives = {};
  const names = [
    'fetch', 'console', 'setTimeout', 'setInterval', 'clearTimer',
    'bytesFromString', 'bytesToString', 'b64Encode', 'b64Decode',
    'randomUUID', 'randomBytes', 'hashNew', 'hashUpdate', 'hashDigest', 'digestBytes',
    'processInfo',
  ];
  for (const name of names) {
    const primitive = globalThis['__' + name];
    if (typeof primitive !== 'function') {
      throw new Error('the JavaScript runtime is incomplete: no ' + name);
    }
    natives[name] = primitive;
    delete globalThis['__' + name];
  }

  // ---- encodings ----
  // Node's names and aliases, mapped onto the seven the host knows. The host
  // refuses anything else, so this is the one place a spelling is allowed to
  // differ from what the primitive sees.
  const ENCODINGS = {
    'utf8': 'utf8', 'utf-8': 'utf8',
    'hex': 'hex',
    'base64': 'base64',
    'base64url': 'base64url',
    'latin1': 'latin1', 'binary': 'latin1',
    'ascii': 'ascii',
    'utf16le': 'utf16le', 'ucs2': 'utf16le', 'ucs-2': 'utf16le',
  };
  function encodingName(value, absent) {
    if (value === undefined || value === null || value === '') return absent || 'utf8';
    const name = ENCODINGS[String(value).toLowerCase()];
    if (name === undefined) throw new TypeError('Unknown encoding: ' + value);
    return name;
  }

  // Anything that is a sequence of bytes becomes a Uint8Array view of it, so
  // the host never has to reason about a Buffer, a DataView or an offset.
  function bytesOf(value, encoding) {
    if (typeof value === 'string') return natives.bytesFromString(value, encodingName(encoding));
    if (value instanceof ArrayBuffer) return new Uint8Array(value);
    if (ArrayBuffer.isView(value)) return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    throw new TypeError('the value must be a string or a BufferSource');
  }

  function describe(value) {
    if (typeof value === 'string') return value;
    try {
      const json = JSON.stringify(value);
      if (json !== undefined) return json;
    } catch (ignored) {}
    return String(value);
  }
  function join(values) { return values.map(describe).join(' '); }

  // ---- console ----
  // One host call per line, at the level the name asks for: the hub's log is
  // where a script's own words have to end up, and a `console.log` that merely
  // collected them somewhere would be worse than useless.
  function write(level, values) { natives.console(level, join(values)); }
  globalThis.console = {
    log: (...values) => write('info', values),
    info: (...values) => write('info', values),
    debug: (...values) => write('debug', values),
    warn: (...values) => write('warn', values),
    error: (...values) => write('error', values),
  };

  // ---- fetch ----
  // The four optional shapes of a `fetch` call are flattened here into the one
  // argument the host takes. That is what keeps the host and its deserialized
  // request the only place that has to know how a call is spelled, and it is
  // where a refusal becomes a `TypeError` a script can catch, the way a browser
  // would raise one.
  globalThis.fetch = function fetch(url, options) {
    if (url === undefined || url === null) throw new TypeError('fetch needs a URL');
    let method = 'GET';
    let body = null;
    let headers = null;
    if (options !== undefined && options !== null) {
      if (typeof options !== 'object') throw new TypeError('the second argument to fetch must be an object');
      if (options.method !== undefined && options.method !== null) method = String(options.method).toUpperCase();
      if (options.body !== undefined && options.body !== null) body = String(options.body);
      if (options.headers !== undefined && options.headers !== null) {
        if (typeof options.headers !== 'object') throw new TypeError("the fetch options' headers must be an object");
        headers = {};
        for (const name of Object.keys(options.headers)) {
          const value = options.headers[name];
          if (typeof value !== 'string') throw new TypeError('the header ' + name + ' must be a string');
          headers[name] = value;
        }
      }
    }
    return natives.fetch(JSON.stringify({ url: String(url), method: method, body: body, headers: headers || {} }));
  };

  // ---- Buffer ----
  // A Uint8Array subclass, which is what Node's Buffer is: every array method
  // comes with it, and `ArrayBuffer.isView` is true, so a script that treats it
  // as bytes behaves the same here as there. Only what an encoding needs is
  // overridden.
  class Buffer extends Uint8Array {
    toString(encoding) {
      return natives.bytesToString(bytesOf(this), encodingName(encoding, 'utf8'));
    }
    static from(value, encoding) {
      if (typeof value === 'string') return new Buffer(natives.bytesFromString(value, encodingName(encoding)));
      if (Array.isArray(value)) return new Buffer(value);
      return new Buffer(bytesOf(value));
    }
    static alloc(size, fill) {
      const length = Number(size);
      if (!Number.isInteger(length) || length < 0) {
        throw new TypeError('Buffer.alloc: the size must be a non-negative integer');
      }
      const buffer = new Buffer(length);
      if (fill !== undefined) {
        if (typeof fill === 'number') buffer.fill(fill & 0xff);
        else if (typeof fill === 'string') buffer.fill(fill.charCodeAt(0) & 0xff);
        else buffer.set(bytesOf(fill).subarray(0, length));
      }
      return buffer;
    }
    static concat(list, totalLength) {
      const parts = Array.from(list, (part) => bytesOf(part));
      const length = totalLength === undefined
        ? parts.reduce((sum, part) => sum + part.length, 0)
        : Number(totalLength);
      const buffer = new Buffer(length);
      let offset = 0;
      for (const part of parts) {
        if (offset >= length) break;
        buffer.set(part.subarray(0, length - offset), offset);
        offset += part.length;
      }
      return buffer;
    }
    static byteLength(value, encoding) {
      return typeof value === 'string' ? natives.bytesFromString(value, encodingName(encoding)).length : bytesOf(value).length;
    }
    static isBuffer(value) { return value instanceof Buffer; }
  }
  globalThis.Buffer = Buffer;

  // ---- atob / btoa ----
  // A binary string: one character per byte, which is what the browser pair
  // means and what makes them usable for data that is not text.
  globalThis.btoa = function btoa(data) {
    const text = String(data);
    const bytes = new Uint8Array(text.length);
    for (let index = 0; index < text.length; index++) {
      const code = text.charCodeAt(index);
      if (code > 0xff) throw new TypeError('btoa: the string has a character above U+00FF');
      bytes[index] = code;
    }
    return natives.b64Encode(bytes);
  };
  globalThis.atob = function atob(data) {
    const bytes = natives.b64Decode(String(data).replace(/[\s]+/g, ''));
    let text = '';
    for (let index = 0; index < bytes.length; index++) text += String.fromCharCode(bytes[index]);
    return text;
  };

  // ---- crypto ----
  // `createHash` and `createHmac` hand back a chainable handle over an id: the
  // host keeps the digest itself, so a long message never crosses the boundary
  // between them more than once.
  function chain(id) {
    const handle = {
      update(data, encoding) {
        natives.hashUpdate(id, bytesOf(data, encoding));
        return handle;
      },
      digest(encoding) {
        const hex = natives.hashDigest(id);
        if (encoding === undefined || encoding === null) return Buffer.from(hex, 'hex');
        const name = encodingName(encoding);
        return name === 'hex' ? hex : Buffer.from(hex, 'hex').toString(name);
      },
    };
    return handle;
  }
  const FLOAT_ARRAYS = [Float32Array, Float64Array];
  globalThis.crypto = {
    randomUUID() { return natives.randomUUID(); },
    getRandomValues(array) {
      if (!ArrayBuffer.isView(array) || array instanceof DataView || FLOAT_ARRAYS.some((type) => array instanceof type)) {
        throw new TypeError('getRandomValues: the argument must be an integer TypedArray');
      }
      // WebCrypto's quota, and Node's: a script that wants more than this has a
      // reason this API will not honour anyway.
      if (array.byteLength > 65536) throw new Error('getRandomValues: the array is larger than 65536 bytes');
      const bytes = new Uint8Array(array.buffer, array.byteOffset, array.byteLength);
      bytes.set(natives.randomBytes(bytes.length));
      return array;
    },
    createHash(algorithm) { return chain(natives.hashNew(String(algorithm), null)); },
    createHmac(algorithm, key) { return chain(natives.hashNew(String(algorithm), bytesOf(key))); },
    subtle: {
      digest(algorithm, data) {
        // WebCrypto takes either a name or `{ name: … }`, and spells digests
        // with a dash that everything else spells without.
        const raw = algorithm !== null && typeof algorithm === 'object' ? algorithm.name : algorithm;
        const name = String(raw).toLowerCase().replace(/[\-]/g, '');
        const bytes = bytesOf(data);
        // Deferred into a microtask rather than resolved straight away, so that
        // a script which awaits it sees the same ordering it would anywhere else.
        return Promise.resolve().then(() => {
          const digest = Buffer.from(natives.digestBytes(name, bytes));
          return digest.buffer.slice(digest.byteOffset, digest.byteOffset + digest.byteLength);
        });
      },
    },
  };

  // ---- timers ----
  // A delay is passed on as a number and clamped by the host; `undefined` is the
  // one case worth translating here, because `Number(undefined)` is not zero.
  globalThis.setTimeout = function setTimeout(callback, delay, ...rest) {
    if (typeof callback !== 'function') throw new TypeError('setTimeout: the callback must be a function');
    return natives.setTimeout(callback, Number(delay), ...rest);
  };
  globalThis.setInterval = function setInterval(callback, delay, ...rest) {
    if (typeof callback !== 'function') throw new TypeError('setInterval: the callback must be a function');
    return natives.setInterval(callback, Number(delay), ...rest);
  };
  function clearTimer(timer) { natives.clearTimer(Number(timer) | 0); }
  globalThis.clearTimeout = clearTimer;
  globalThis.clearInterval = clearTimer;

  // ---- process ----
  // A snapshot and frozen: what a script reads is the environment it was given,
  // and nothing it writes can reach back into the hub or mislead a later part of
  // the same script.
  const info = JSON.parse(natives.processInfo());
  globalThis.process = {
    env: Object.freeze(info.env),
    platform: info.platform,
    arch: info.arch,
    version: info.version,
    versions: Object.freeze(info.versions),
    argv: Object.freeze(info.argv),
  };

  // ---- require ----
  // Five modules, and not one of them is a capability: three that compute and
  // format, plus the two globals a script written for Node reaches for by name.
  // `path` is the posix spelling, because that is the only spelling this hub
  // runs on and because a script that branches on separators is better off not
  // having to.
  function pathNormalize(path) {
    const text = String(path);
    if (text.length === 0) return '.';
    const absolute = text.charCodeAt(0) === 47;
    const trailing = text.length > 1 && text.charCodeAt(text.length - 1) === 47;
    const parts = [];
    for (const part of text.split('/')) {
      if (part === '' || part === '.') continue;
      if (part === '..') {
        if (parts.length > 0 && parts[parts.length - 1] !== '..') parts.pop();
        else if (!absolute) parts.push('..');
        continue;
      }
      parts.push(part);
    }
    let out = (absolute ? '/' : '') + parts.join('/');
    if (out === '') out = '.';
    else if (trailing && out !== '/') out += '/';
    return out;
  }
  function pathJoin(...parts) {
    let joined = '';
    for (const part of parts) {
      const text = String(part);
      if (text === '') continue;
      joined = joined === '' ? text : joined + '/' + text;
    }
    return pathNormalize(joined === '' ? '.' : joined);
  }
  function pathResolve(...parts) {
    let resolved = '';
    for (let index = parts.length - 1; index >= 0; index--) {
      const text = String(parts[index]);
      if (text === '') continue;
      resolved = resolved === '' ? text : text + '/' + resolved;
      if (text.charCodeAt(0) === 47) break;
    }
    // Anchored at the filesystem root rather than at the hub's working
    // directory: where the hub was started is not something a notification has
    // any business knowing, and a script has no files to resolve against.
    return pathNormalize('/' + resolved);
  }
  function pathBasename(path, suffix) {
    const text = String(path);
    let end = text.length;
    while (end > 0 && text.charCodeAt(end - 1) === 47) end--;
    const start = text.lastIndexOf('/', end - 1) + 1;
    let base = text.slice(start, end);
    if (suffix !== undefined) {
      const tail = String(suffix);
      if (tail.length > 0 && base.length !== tail.length && base.endsWith(tail)) {
        base = base.slice(0, base.length - tail.length);
      }
    }
    return base;
  }
  function pathDirname(path) {
    const text = String(path);
    if (text.length === 0) return '.';
    const absolute = text.charCodeAt(0) === 47;
    let end = -1;
    let seen = false;
    for (let index = text.length - 1; index >= 1; index--) {
      if (text.charCodeAt(index) === 47) {
        if (seen) { end = index; break; }
      } else {
        seen = true;
      }
    }
    if (end === -1) return absolute ? '/' : '.';
    let stop = end;
    while (stop > 0 && text.charCodeAt(stop - 1) === 47) stop--;
    return stop === 0 ? (absolute ? '/' : '.') : text.slice(0, stop);
  }
  function pathExtname(path) {
    const base = pathBasename(path);
    const dot = base.lastIndexOf('.');
    if (dot <= 0 || dot === base.length - 1) return '';
    return base.slice(dot);
  }
  const pathModule = {
    sep: '/',
    delimiter: ':',
    normalize: pathNormalize,
    isAbsolute: (path) => String(path).charCodeAt(0) === 47,
    join: pathJoin,
    resolve: pathResolve,
    basename: pathBasename,
    dirname: pathDirname,
    extname: pathExtname,
  };
  const osModule = {
    platform: () => info.platform,
    arch: () => info.arch,
    hostname: () => info.hostname || '',
    tmpdir: () => info.tmpdir,
    EOL: info.eol,
  };
  function utilFormat(template, ...rest) {
    if (typeof template !== 'string') return join([template, ...rest]);
    let index = 0;
    let text = template.replace(/%[sdifjoO%]/g, (token) => {
      if (token === '%%') return '%';
      if (index >= rest.length) return token;
      const value = rest[index++];
      switch (token) {
        case '%s': return String(value);
        case '%d': return String(Number(value));
        case '%i': return String(parseInt(value, 10));
        case '%j': try { return JSON.stringify(value); } catch (ignored) { return '[Circular]'; }
        default: return describe(value);
      }
    });
    while (index < rest.length) text += ' ' + describe(rest[index++]);
    return text;
  }
  const utilModule = { format: utilFormat, inspect: (value) => describe(value) };
  // The last two entries are not new objects. Node spells `crypto` and `buffer`
  // with and without the `node:` prefix, and a script moved here from Node
  // writes `const crypto = require('node:crypto')` far more often than it writes
  // a bare `crypto` -- so the module and the global have to be *the same object*,
  // or a key hashed through one name is not the key the other computes with.
  // `buffer` has the shape Node's own module has: one `Buffer` property, over
  // the class that is already in the global scope.
  const MODULES = {
    path: pathModule,
    os: osModule,
    util: utilModule,
    crypto: globalThis.crypto,
    buffer: { Buffer: Buffer },
  };
  globalThis.require = function require(specifier) {
    const name = String(specifier).replace(/^node:/, '');
    // `Object.prototype.hasOwnProperty` rather than a plain lookup: a specifier
    // like `__proto__` or `constructor` must not find something that was never
    // in the table.
    if (Object.prototype.hasOwnProperty.call(MODULES, name)) return MODULES[name];
    throw new Error("Cannot find module '" + specifier + "'");
  };
})();
"#;

/// Installs `console`'s primitive and runs the bootstrap.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    native(ctx, "__console", console)?;
    let started = Module::evaluate(ctx.clone(), MODULE, BOOTSTRAP)
        .and_then(|module: Promise<'js>| module.finish::<Value>())
        .map(|_| ())
        .map_err(|error| {
            // The bootstrap is this hub's own code, so a failure here is a bug
            // in it rather than anything an operator did -- but it is still
            // reported as a channel error, because there is nothing better to do
            // with it.
            anyhow::anyhow!("the JavaScript runtime could not be started: {}", super::plain(ctx, error))
        });
    started
}

/// `console.log(…)`: one line into the hub's own log, at the level asked for.
fn console(level: String, text: String) {
    let message = format!("javascript notification script: {text}");
    match level.as_str() {
        "warn" => warn!("{message}"),
        "error" => error!("{message}"),
        "debug" => debug!("{message}"),
        _ => info!("{message}"),
    }
}
