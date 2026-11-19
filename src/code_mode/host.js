// Code Mode's host (src/code_mode.rs): one conversation's JavaScript REPL,
// run by the user's own Node.js. The REPL is Node's (node:repl) on a sealed
// vm context: no require, import or process, only the globals the library
// below puts there, which reach the machine by asking Oak to run its tools.
// Oak talks to it in JSON lines: on stdin {t:"eval", code, tools},
// {t:"reply", id, ok, value} and {t:"interrupt"}; on stdout {t:"call", id,
// name, input} and {t:"done", text, error}. After each script the
// context's variables are saved to the snapshot file (argv[2]) and a new
// host restores them from it, so they outlast restarts.
'use strict';
const repl = require('node:repl');
const vm = require('node:vm');
const domain = require('node:domain');
const util = require('node:util');
const fs = require('node:fs');
const inspector = require('node:inspector');
const { PassThrough, Writable } = require('node:stream');

const SNAPSHOT = process.argv[2];
const SNAPSHOT_MAX = 64 * 1024 * 1024;

function send(m) {
  process.stdout.write(JSON.stringify(m) + '\n');
}

// ---------------------------------------------------------------- the REPL

const d = domain.create();
// The script running: its eval() settles through this.
let settle = null;
// (First, before the REPL's own listener rewrites the error's stack.)
d.on('error', (e) => {
  const s = settle;
  settle = null;
  if (s) s({ error: e });
});
const r = repl.start({
  input: new PassThrough(),
  output: new Writable({ write(_c, _e, cb) { cb(); } }),
  prompt: '',
  terminal: false,
  useGlobal: false,
  ignoreUndefined: true,
  preview: false,
  domain: d,
});
const ctx = r.context;
// Sealed: only what a bare V8 context has (no require, process, module).
const pure = new Set(Object.getOwnPropertyNames(vm.runInNewContext('globalThis')));
for (const n of Object.getOwnPropertyNames(ctx)) {
  if (!pure.has(n)) {
    try { delete ctx[n]; } catch { /* kept */ }
  }
}

function evaluate(code) {
  return new Promise((resolve) => {
    settle = resolve;
    try {
      r.eval(code + '\n', ctx, 'repl', (e, v) => {
        if (settle !== resolve) return;
        settle = null;
        resolve(e ? { error: e } : { value: v });
      });
    } catch (e) {
      if (settle === resolve) {
        settle = null;
        resolve({ error: e });
      }
    }
  });
}

// The names the REPL knows besides the context's own properties: its
// let, const and class declarations too (the REPL completes them).
function knownNames() {
  return new Promise((resolve) => {
    try {
      r.complete('', (e, v) => resolve(e || !v ? [] : v[0].filter((n) => /^[A-Za-z_$][\w$]*$/.test(n))));
    } catch {
      resolve([]);
    }
  });
}

// ---------------------------------------------------------------- the library

// Compiled inside the context (its objects are the context's own), so it
// sees no host variable: what it needs comes in `host`.
function library(host) {
  'use strict';
  const toolList = [];
  const late = [];
  const current = { logs: null, timers: new Set() };
  const RING = 1000;
  const ring = [];
  let dropped = 0;

  const show = (v) => (typeof v === 'string' ? v : host.inspect(v));
  const line = (args) => args.map(show).join(' ');

  function log(...args) {
    const l = line(args);
    if (current.logs) current.logs.push(l);
    else late.push(l);
  }
  function toRing(level, args) {
    ring.push((level === 'log' ? '' : `[${level}] `) + line(args));
    if (ring.length > RING) {
      ring.shift();
      dropped++;
    }
  }
  const console = {};
  for (const level of ['log', 'info', 'debug', 'warn', 'error', 'trace', 'dir']) {
    console[level] = (...args) => toRing(level, args);
  }
  console.tail = (n = 20) => ring.slice(-n).join('\n');
  console.buffer = () => ring.join('\n');
  console.dropped = () => dropped;

  function call(name, input) {
    return host.call(name, input).then((v) => JSON.parse(v));
  }
  class ToolError extends Error {}
  ToolError.prototype.name = 'ToolError';

  // ---- $
  const COUNTED = new Set(['grep', 'rg', 'egrep', 'fgrep', 'find', 'diff', 'test', '[']);
  const READINGS = { grep: 'no match', rg: 'no match', egrep: 'no match', fgrep: 'no match', find: 'some paths could not be read', diff: 'the files differ', test: 'the test is false', '[': 'the test is false', 'git diff': 'there are differences', 'git grep': 'no match' };
  function exitOneReading(command) {
    const segs = command.trim().split(/\|\||&&|[|;\n]/).map((s) => s.trim()).filter(Boolean);
    const last = (segs[segs.length - 1] || '').split(/\s+/).filter((w) => !/^\w+=/.test(w));
    if (last[0] === 'git' && (last[1] === 'diff' || last[1] === 'grep')) return READINGS['git ' + last[1]];
    return COUNTED.has(last[0]) ? READINGS[last[0]] : null;
  }
  class ShellOutput {
    text() {
      if (this.running) throw new ShellError(this, 'the command is still running in the background');
      return this.stdout;
    }
    lines() {
      const t = this.text();
      const ls = t.split(/\r?\n/);
      if (ls.length && ls[ls.length - 1] === '') ls.pop();
      return ls;
    }
    json() {
      return JSON.parse(this.text());
    }
  }
  class ShellError extends Error {
    constructor(out, why) {
      const what = why || out.error || (out.exitCode !== null ? `exit code ${out.exitCode}` : 'failed');
      const tail = (out.stdout || '').trimEnd().split('\n').slice(-20).join('\n');
      super(`Command failed (${what})${tail ? ':\n' + tail : ''}`);
      this.stdout = out.stdout;
      this.stderr = out.stderr;
      this.exitCode = out.exitCode;
      if (out.running) this.running = out.running;
      if (out.truncated) this.truncated = true;
    }
  }
  ShellError.prototype.name = 'ShellError';
  const quote1 = (s) => "'" + String(s).replace(/'/g, "'\\''") + "'";
  function quote(v) {
    if (Array.isArray(v)) return v.map(quote1).join(' ');
    if (v && typeof v === 'object' && 'raw' in v) return String(v.raw);
    return quote1(v);
  }
  function shellOutput(o, command) {
    const out = Object.assign(new ShellOutput(), {
      stdout: o.stdout || '',
      stderr: o.error || '',
      exitCode: o.exitCode === undefined ? null : o.exitCode,
      truncated: !!o.truncated,
      whole: !o.truncated,
    });
    if (o.file) {
      out.file = o.file;
      out.bytes = o.bytes;
    }
    if (o.running) out.running = o.running;
    if (o.error) out.error = o.error;
    if (out.exitCode === 1) {
      const reading = exitOneReading(command);
      if (reading) out.note = `exit 1: ${reading}`;
    }
    let shown = out.stdout;
    if (out.error) shown += `\n[error] ${out.error}`;
    if (out.exitCode) shown += `\n[exit ${out.exitCode}]`;
    out.output = shown;
    return out;
  }
  function shell(command, timeoutMs) {
    const opts = { command, timeout: timeoutMs, throws: null, quiet: false, cwd: null, env: null };
    let run = null;
    const start = () => {
      if (!run) {
        run = call('$', { command: opts.command, timeout_ms: opts.timeout, cwd: opts.cwd, env: opts.env, quiet: opts.quiet }).then((o) => {
          const out = shellOutput(o, opts.command);
          const fails = out.error || (out.exitCode !== 0 && out.exitCode !== null && !out.note);
          if (fails && opts.throws !== false) throw new ShellError(out);
          return out;
        });
      }
      return run;
    };
    const p = {
      then: (a, b) => start().then(a, b),
      catch: (b) => start().catch(b),
      finally: (f) => start().finally(f),
      nothrow() { opts.throws = false; return p; },
      throws(on) { opts.throws = !!on; return p; },
      quiet() { opts.quiet = true; return p; },
      timeout(ms) { opts.timeout = ms; return p; },
      cwd(dir) { opts.cwd = String(dir); return p; },
      env(vars) { opts.env = Object.assign({}, opts.env, vars); return p; },
      text: () => start().then((o) => o.text()),
      lines: () => start().then((o) => o.lines()),
      json: () => start().then((o) => o.json()),
    };
    Object.defineProperty(p, Symbol.toStringTag, { value: 'ShellPromise' });
    // It runs whether or not it is awaited.
    Promise.resolve().then(() => start().catch(() => {}));
    return p;
  }
  function $(strings, ...values) {
    if (typeof strings === 'string') return shell(strings, values[0]);
    if (!strings || !Array.isArray(strings.raw)) throw new TypeError('$ takes a template ($`cmd`) or a command string ($(cmd, ms))');
    const raw = strings.raw.map((s) => s.replace(/\\`/g, '`').replace(/\\\$\{/g, '${'));
    let command = raw[0];
    values.forEach((v, i) => {
      command += quote(v) + raw[i + 1];
    });
    return shell(command);
  }
  $.escape = quote1;
  $.ShellError = ShellError;

  // ---- files
  async function edit(path, old, next, opts) {
    const batch = Array.isArray(old);
    if (batch) opts = next;
    opts = opts || {};
    const edits = batch ? old.map(([o, n]) => ({ old_string: String(o), new_string: String(n) })) : [{ old_string: String(old), new_string: String(next) }];
    const r = await call('_edit', { path: String(path), edits, replace_all: !!opts.all });
    if (!r.ok && !opts.soft) throw new ToolError(r.error);
    return r.ok ? { ok: true, replacements: r.replacements } : { ok: false, replacements: 0, error: r.error };
  }
  async function write(path, content) {
    content = String(content);
    await tool('WRITE', { path: String(path), content });
    return host.bytes(content);
  }
  function file(path) {
    path = String(path);
    const read = async () => {
      const r = await call('_file', { path });
      if (r.error) throw new ToolError(r.error);
      return r.text;
    };
    return {
      name: path,
      text: read,
      json: async () => JSON.parse(await read()),
      lines: async () => {
        const ls = (await read()).split(/\r?\n/);
        if (ls.length && ls[ls.length - 1] === '') ls.pop();
        return ls;
      },
      exists: async () => (await call('_file', { path, exists: true })).exists,
      slice: async (offset = 1, limit) => {
        const ls = (await read()).split(/\r?\n/);
        const from = Math.max(1, offset) - 1;
        return ls.slice(from, limit === undefined ? undefined : from + limit).join('\n');
      },
    };
  }

  // ---- tools
  async function tool(name, input) {
    const r = await call('tool', { name, input: input === undefined ? {} : input });
    if (r.error) throw new ToolError(r.text);
    return r.text;
  }
  function tsType(s, depth = 0) {
    if (!s || typeof s !== 'object') return 'unknown';
    if (Array.isArray(s.enum)) return s.enum.map((v) => JSON.stringify(v)).join(' | ');
    if (Array.isArray(s.anyOf || s.oneOf)) return (s.anyOf || s.oneOf).map((x) => tsType(x, depth)).join(' | ');
    switch (s.type) {
      case 'string': return 'string';
      case 'integer': case 'number': return 'number';
      case 'boolean': return 'boolean';
      case 'array': return `${tsType(s.items, depth + 1)}[]`;
      case 'object': {
        const props = s.properties || {};
        const req = new Set(s.required || []);
        const keys = Object.keys(props);
        if (!keys.length || depth > 3) return 'Record<string, unknown>';
        return '{' + keys.map((k) => `${k}${req.has(k) ? '' : '?'}: ${tsType(props[k], depth + 1)}`).join(', ') + '}';
      }
      default: return 'unknown';
    }
  }
  const about = (t) => (t.description || '').split(/(?<=\.)\s/)[0];
  async function searchTools(query, max = 10) {
    const words = String(query || '').toLowerCase().split(/\W+/).filter(Boolean);
    if (!words.length) return { error: 'The query is empty: give words from a tool\'s name or what it does.' };
    const scored = toolList.map((t) => {
      const name = t.name.toLowerCase();
      const desc = (t.description || '').toLowerCase();
      let score = 0;
      for (const w of words) score += (name.includes(w) ? 3 : 0) + (desc.includes(w) ? 1 : 0);
      return [score, t];
    }).filter(([s]) => s > 0).sort((a, b) => b[0] - a[0]).slice(0, max);
    return scored.map(([, t]) => ({ name: t.name, about: about(t), input: tsType(t.input_schema) }));
  }
  async function describeTool(name) {
    const t = toolList.find((t) => t.name === name);
    return t ? { name: t.name, description: t.description || '', inputSchema: t.input_schema || {} } : { error: `No tool is named ${name}.` };
  }

  // ---- time
  function setTimeout(fn, ms, ...args) {
    const id = host.setTimeout(() => {
      current.timers.delete(id);
      fn(...args);
    }, ms);
    current.timers.add(id);
    return id;
  }
  function setInterval(fn, ms, ...args) {
    const id = host.setInterval(() => fn(...args), ms);
    current.timers.add(id);
    return id;
  }
  function clearTimeout(id) {
    current.timers.delete(id);
    host.clearTimer(id);
  }
  const sleep = (ms) => new Promise((resolve) => host.setTimeout(resolve, ms));

  function preview(value, n = 200) {
    let head = '';
    let text;
    if (value instanceof ShellOutput) {
      if (value.file) head = `[${value.bytes} bytes, whole output in ${value.file}]\n`;
      text = value.stdout;
    } else {
      text = show(value);
    }
    const cut = text.length > n ? `${text.slice(0, n)}… (${text.length - n} more characters)` : text;
    return head + cut;
  }

  return {
    globals: {
      $, edit, write, file, sleep, preview, log, console, searchTools, describeTool,
      setTimeout, setInterval, clearTimeout, clearInterval: clearTimeout,
      queueMicrotask: host.queueMicrotask, atob: host.atob, btoa: host.btoa,
      TextEncoder: host.TextEncoder, TextDecoder: host.TextDecoder, URL: host.URL, URLSearchParams: host.URLSearchParams,
    },
    classes: { ShellOutput, ShellError, ToolError },
    tool,
    current,
    late,
    setTools(list) {
      toolList.length = 0;
      toolList.push(...list);
    },
  };
}

// ---------------------------------------------------------------- snapshots

// Both compiled inside the context, like the library: what they make is
// the context's own (an array is its Array), and `instanceof` holds.
function encoder(lookup, libraryClasses, b64, closes) {
  'use strict';
  const seen = new Map();
  class Unsupported extends Error {}
  const no = (what) => {
    throw new Unsupported(what);
  };
  const fnSource = (f) => {
    const src = Function.prototype.toString.call(f);
    if (/\{\s*\[native code\]\s*\}\s*$/.test(src)) no('a built-in or bound function');
    if (closes(f)) no('a function that closes over local variables');
    return src;
  };
  function enc(v) {
    switch (typeof v) {
      case 'undefined': return { u: 1 };
      case 'number': return Number.isFinite(v) && !Object.is(v, -0) ? v : { n: Object.is(v, -0) ? '-0' : String(v) };
      case 'string': case 'boolean': return v;
      case 'bigint': return { b: String(v) };
      case 'symbol': return no('a Symbol');
    }
    if (v === null) return null;
    if (seen.has(v)) return { r: seen.get(v) };
    const i = seen.size;
    seen.set(v, i);
    if (typeof v === 'function') return { i, f: fnSource(v) };
    const tag = Object.prototype.toString.call(v);
    switch (tag) {
      case '[object Array]': return { i, a: Array.from(v, enc) };
      case '[object Date]': return { i, d: v.getTime() };
      case '[object RegExp]': return { i, re: v.source, fl: v.flags };
      case '[object Map]': return { i, m: Array.from(v, ([k, x]) => [enc(k), enc(x)]) };
      case '[object Set]': return { i, s: Array.from(v, enc) };
      case '[object Error]': return { i, e: { name: v.name, message: v.message, stack: v.stack }, o: props(v, ['stack', 'message']) };
      case '[object ArrayBuffer]': return { i, ab: b64(new Uint8Array(v)) };
    }
    if (ArrayBuffer.isView(v)) {
      const name = v.constructor && v.constructor.name;
      if (tag === '[object DataView]' || !/^(Big)?(Ui|I)nt(8|16|32|64)(Clamped)?Array$|^Float(32|64)Array$/.test(name)) no(`a ${name}`);
      return { i, ta: name, x: b64(new Uint8Array(v.buffer, v.byteOffset, v.byteLength)) };
    }
    if (tag !== '[object Object]') no(`a ${tag.slice(8, -1)}`);
    const proto = Object.getPrototypeOf(v);
    if (proto === Object.prototype) return { i, o: props(v) };
    if (proto === null) return { i, o: props(v), np: 1 };
    for (const [name, c] of Object.entries(libraryClasses)) {
      if (proto === c.prototype) return { i, o: props(v), lc: name };
    }
    const name = proto.constructor && proto.constructor.name;
    if (name && lookup(name) === proto.constructor) return { i, o: props(v), c: name };
    return no(`an instance of ${name || 'a class that is not a top-level name'}`);
  }
  function props(v, skip = []) {
    const out = {};
    for (const k of Object.keys(v)) {
      if (skip.includes(k)) continue;
      const desc = Object.getOwnPropertyDescriptor(v, k);
      if (desc.get || desc.set) no(`a getter or setter (${k})`);
      out[k] = enc(desc.value);
    }
    return out;
  }
  return {
    // One name's value, or {unsupported: why}. A failure takes back the
    // objects it had numbered, so later names don't point to them.
    encode(v) {
      const before = new Set(seen.keys());
      try {
        return { value: enc(v) };
      } catch (e) {
        for (const k of [...seen.keys()]) if (!before.has(k)) seen.delete(k);
        if (e instanceof Unsupported) return { unsupported: `holds ${e.message}` };
        return { unsupported: String(e && e.message) };
      }
    },
  };
}

function decoder(lookup, libraryClasses, unb64) {
  'use strict';
  const made = [];
  const evalFn = (src) => {
    const g = (0, eval);
    try {
      return g(`(${src})`);
    } catch {
      // A method written in an object or class (`name() {...}`).
      const o = g(`({${src}})`);
      return Object.values(o)[0];
    }
  };
  function dec(x) {
    if (x === null || typeof x !== 'object') return x;
    if ('u' in x) return undefined;
    if ('n' in x) return x.n === '-0' ? -0 : Number(x.n);
    if ('b' in x) return BigInt(x.b);
    if ('r' in x) {
      if (!(x.r in made)) throw new Error('it points to a value that was not saved');
      return made[x.r];
    }
    const at = x.i;
    const keep = (v) => {
      made[at] = v;
      return v;
    };
    if ('f' in x) return keep(evalFn(x.f));
    if ('a' in x) {
      const a = keep([]);
      for (const y of x.a) a.push(dec(y));
      return a;
    }
    if ('d' in x) return keep(new Date(x.d));
    if ('re' in x) return keep(new RegExp(x.re, x.fl));
    if ('m' in x) {
      const m = keep(new Map());
      for (const [k, v] of x.m) m.set(dec(k), dec(v));
      return m;
    }
    if ('s' in x) {
      const s = keep(new Set());
      for (const v of x.s) s.add(dec(v));
      return s;
    }
    if ('e' in x) {
      const Ctor = typeof globalThis[x.e.name] === 'function' && /Error$/.test(x.e.name) ? globalThis[x.e.name] : Error;
      const e = keep(new Ctor(x.e.message));
      if (e.name !== x.e.name) e.name = x.e.name;
      e.stack = x.e.stack;
      fill(e, x.o || {});
      return e;
    }
    if ('ab' in x) return keep(unb64(x.ab).buffer);
    if ('ta' in x) {
      const bytes = unb64(x.x);
      return keep(new globalThis[x.ta](bytes.buffer, bytes.byteOffset, bytes.byteLength / globalThis[x.ta].BYTES_PER_ELEMENT));
    }
    let proto = Object.prototype;
    if (x.np) proto = null;
    else if (x.lc) proto = libraryClasses[x.lc].prototype;
    else if (x.c) {
      const C = lookup(x.c);
      if (typeof C !== 'function') throw new Error(`its class ${x.c} wasn't restored`);
      proto = C.prototype;
    }
    const o = keep(Object.create(proto));
    fill(o, x.o);
    return o;
  }
  function fill(o, props) {
    for (const [k, v] of Object.entries(props)) {
      Object.defineProperty(o, k, { value: dec(v), writable: true, enumerable: true, configurable: true });
    }
  }
  return { decode: dec };
}

// Whether function `f` closes over variables of a function it was made in
// (its source alone would lose them), as V8's inspector sees its scopes.
let session = null;
function closes(f) {
  const post = (method, params) => {
    let out;
    let err;
    session.post(method, params, (e, r) => {
      err = e;
      out = r;
    });
    if (err) throw err;
    return out;
  };
  try {
    if (!session) {
      session = new inspector.Session();
      session.connect();
    }
    globalThis.__oak_probe = f;
    const { result } = post('Runtime.evaluate', { expression: 'globalThis.__oak_probe' });
    const props = post('Runtime.getProperties', { objectId: result.objectId, ownProperties: false });
    const scopes = (props.internalProperties || []).find((p) => p.name === '[[Scopes]]');
    if (!scopes) return false;
    const list = post('Runtime.getProperties', { objectId: scopes.value.objectId, ownProperties: true });
    post('Runtime.releaseObjectGroup', { objectGroup: '' });
    return list.result.some((x) => /^\d+$/.test(x.name) && /^Closure/.test(x.value.description || ''));
  } catch {
    return false;
  } finally {
    delete globalThis.__oak_probe;
  }
}

const inContext = (f) => vm.runInContext(`(${f.toString()})`, ctx);
// A top-level name's value, let and const ones too.
const lookup = vm.runInContext('(name) => { try { return (0, eval)(name); } catch { return undefined; } }', ctx);
const b64 = (bytes) => Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('base64');
const unb64 = (s) => {
  const b = Buffer.from(s, 'base64');
  const out = new (vm.runInContext('Uint8Array', ctx))(b.length);
  out.set(b);
  return out;
};

// ---------------------------------------------------------------- setup

let evals = 0;
const pending = new Map();
let nextCall = 1;
// The script was interrupted: its calls are refused from then on.
let stopped = false;

const lib = inContext(library)({
  call(name, input) {
    return new Promise((resolve, reject) => {
      if (stopped) return reject(new ctx.Error('Interrupted'));
      const id = nextCall++;
      pending.set(id, { resolve, reject });
      send({ t: 'call', id, name, input });
    });
  },
  inspect: (v) => util.inspect(v, { depth: 4, maxArrayLength: 100, maxStringLength: 20000, breakLength: 100 }),
  bytes: (s) => Buffer.byteLength(s),
  setTimeout: (f, ms) => timer(setTimeout(f, ms)),
  setInterval: (f, ms) => timer(setInterval(f, ms)),
  clearTimer: (id) => {
    const h = timers.get(id);
    if (h) {
      clearTimeout(h);
      clearInterval(h);
      timers.delete(id);
    }
  },
  queueMicrotask, atob, btoa, TextEncoder, TextDecoder, URL, URLSearchParams,
});
const timers = new Map();
let nextTimer = 1;
function timer(h) {
  const id = nextTimer++;
  timers.set(id, h);
  return id;
}

const fixed = { ...lib.globals, self: vm.runInContext('globalThis', ctx) };
for (const [k, v] of Object.entries(fixed)) {
  Object.defineProperty(ctx, k, { value: v, writable: false, enumerable: false, configurable: false });
}
let toolNames = new Set();
function setTools(list) {
  for (const n of toolNames) delete ctx[n];
  toolNames = new Set();
  lib.setTools(list);
  for (const t of list) {
    if (!/^[A-Za-z_$][\w$]*$/.test(t.name) || t.name in fixed || pure.has(t.name)) continue;
    const name = t.name;
    Object.defineProperty(ctx, name, { value: (input) => lib.tool(name, input), writable: true, enumerable: false, configurable: true });
    toolNames.add(name);
  }
}

// What the context had before any script: not the user's.
let builtin = new Set();
async function noteBuiltin() {
  builtin = new Set([...pure, ...Object.keys(fixed), ...(await knownNames())]);
}

// The user's top-level names: var and function ones are the context's
// properties, let, const and class ones the REPL knows.
async function userNames() {
  const names = new Set(await knownNames());
  for (const n of Object.getOwnPropertyNames(ctx)) names.add(n);
  return [...names].filter((n) => !builtin.has(n) && !toolNames.has(n) && /^[A-Za-z_$][\w$]*$/.test(n));
}

// ---------------------------------------------------------------- saving

async function save() {
  if (!SNAPSHOT) return;
  const { encode } = inContext(encoder)(lookup, lib.classes, b64, closes);
  const own = new Set(Object.getOwnPropertyNames(ctx));
  const names = [];
  const lost = [];
  // Functions and classes first: values made from them come after.
  const entries = [];
  for (const n of await userNames()) {
    let kind = own.has(n) ? 'var' : 'let';
    let value;
    try {
      value = lookup(n);
      vm.runInContext(`void ${n}`, ctx);
    } catch {
      continue; // declared, never set
    }
    if (kind === 'let') {
      try {
        vm.runInContext(`${n} = ${n}`, ctx);
      } catch (e) {
        if (/constant/.test(String(e && e.message))) kind = 'const';
        else continue;
      }
    }
    entries.push({ n, kind, value });
  }
  entries.sort((a, b) => (typeof b.value === 'function') - (typeof a.value === 'function'));
  for (const { n, kind, value } of entries) {
    const r = encode(value);
    if (r.unsupported) lost.push({ name: n, why: r.unsupported });
    else names.push({ name: n, kind, value: r.value });
  }
  let text = JSON.stringify({ version: 1, evals, saved: Date.now(), names, lost });
  if (text.length > SNAPSHOT_MAX) {
    // Too big to keep whole: the largest values go first.
    const sized = names.map((x) => [JSON.stringify(x).length, x]).sort((a, b) => b[0] - a[0]);
    let total = text.length;
    const drop = new Set();
    for (const [size, x] of sized) {
      if (total <= SNAPSHOT_MAX) break;
      drop.add(x);
      total -= size;
      lost.push({ name: x.name, why: 'too large to save' });
    }
    text = JSON.stringify({ version: 1, evals, saved: Date.now(), names: names.filter((x) => !drop.has(x)), lost });
  }
  const tmp = `${SNAPSHOT}.tmp`;
  fs.writeFileSync(tmp, text);
  fs.renameSync(tmp, SNAPSHOT);
}

// Puts a snapshot's names back: how many, and those that couldn't be.
function restore() {
  let snap;
  try {
    snap = JSON.parse(fs.readFileSync(SNAPSHOT, 'utf8'));
  } catch {
    return { restored: 0, lost: [] };
  }
  let restored = 0;
  evals = snap.evals || 0;
  const lost = (snap.lost || []).map((x) => `${x.name} (${x.why})`);
  const { decode } = inContext(decoder)(lookup, lib.classes, unb64);
  for (const { name, kind, value } of snap.names || []) {
    try {
      const v = decode(value);
      if (kind === 'var') {
        ctx[name] = v;
      } else {
        ctx.__oak_restoring = v;
        try {
          vm.runInContext(`${kind} ${name} = globalThis.__oak_restoring;`, ctx);
        } finally {
          delete ctx.__oak_restoring;
        }
      }
      restored++;
    } catch (e) {
      lost.push(`${name} (${String(e && e.message)})`);
    }
  }
  return { restored, lost, saved: snap.saved || 0 };
}

// ---------------------------------------------------------------- scripts

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const PROMISE_WARNING = "REPL: unawaited Promise coerced to string. A result used inline needs 'await' (as in `${await f()}`); only the top-level keys of a returned object are awaited for you.";

function isThenable(v) {
  return v !== null && (typeof v === 'object' || typeof v === 'function') && typeof v.then === 'function';
}

// Error text for the model: what kind of failure it was, and only the
// frames of its own code.
function describeError(e) {
  if (e instanceof repl.Recoverable) {
    return `Syntax error, so nothing ran: the code ends early (${(e.err && e.err.message) || 'unexpected end of input'})`;
  }
  if (e === null || (typeof e !== 'object' && typeof e !== 'function')) return `Uncaught ${util.inspect(e)}`;
  const message = String(e.message);
  const stack = typeof e.stack === 'string' ? e.stack : '';
  const frames = stack.split('\n').filter((l) => /^\s+at (?:.*\()?repl:\d+:\d+\)?$/.test(l)).slice(0, 6).map((l) => l.replace(/\(?repl:(\d+):(\d+)\)?/, 'line $1:$2').replace(/ \(line/, ', line'));
  if (e.name === 'SyntaxError' && !frames.length) {
    if (/already been declared/.test(message)) {
      return `Nothing ran: it would overwrite a name that is already taken here (${message}). Assign to it instead, or pick another name.`;
    }
    if (/import statement|Cannot use import/.test(message)) {
      return "Nothing ran: import and require aren't available here. The globals ($, file, edit, write and the tools) are how a script reaches the machine.";
    }
    const at = stack.match(/^repl:(\d+)/);
    return `Syntax error, so nothing ran: ${message}${at ? ` (line ${at[1]})` : ''}`;
  }
  let text = `${e.name || 'Error'}: ${message}`;
  if (/^(require|process|module|Buffer|__dirname|__filename) is not defined$/.test(message)) {
    text += ". The context is sealed: there is no require, import or process; use $, file, edit, write and the tools.";
  }
  if (e.exitCode !== undefined && e.name === 'ShellError') text += `\nexitCode: ${e.exitCode}`;
  if (frames.length) text += '\n' + frames.join('\n');
  return text;
}

let running = null;

async function runScript(msg) {
  stopped = false;
  if (Array.isArray(msg.tools)) setTools(msg.tools);
  const n = ++evals;
  const logs = lib.late.splice(0).map((l) => `[late] ${l}`);
  lib.current.logs = logs;
  let outcome;
  const interrupted = new Promise((resolve) => {
    running = { n, interrupt: (why) => resolve({ error: { oak: why } }) };
  });
  outcome = await Promise.race([evaluate(msg.code), interrupted]);
  if (!outcome.error) {
    // A promise returned whole, or as an object's top-level keys.
    try {
      let v = outcome.value;
      if (isThenable(v)) v = await Promise.race([v, interrupted.then((x) => { throw x; })]);
      else if (v && typeof v === 'object' && !Array.isArray(v) && Object.values(v).some(isThenable)) {
        const keys = Object.keys(v);
        const vals = await Promise.all(keys.map((k) => v[k]));
        v = Object.fromEntries(keys.map((k, i) => [k, vals[i]]));
      }
      outcome = { value: v };
    } catch (e) {
      outcome = e && e.error && e.error.oak ? e : { error: e };
    }
  }
  running = null;
  lib.current.logs = null;
  for (const id of lib.current.timers) {
    const h = timers.get(id);
    if (h) {
      clearTimeout(h);
      clearInterval(h);
      timers.delete(id);
    }
  }
  lib.current.timers.clear();
  const parts = [`<repl-output eval="${n}">`];
  if (msg.note) parts.push(`<note>${esc(msg.note)}</note>`);
  for (const l of logs) parts.push(`<log>${esc(l)}</log>`);
  let error = false;
  let shown = '';
  if (outcome.error) {
    error = true;
    shown = outcome.error.oak ? outcome.error.oak : describeError(outcome.error);
    parts.push(`<error>${esc(shown)}</error>`);
  } else if (outcome.value !== undefined) {
    shown = typeof outcome.value === 'string' ? outcome.value : util.inspect(outcome.value, { depth: 4, maxArrayLength: 200, maxStringLength: 100000, breakLength: 100 });
    parts.push(`<result>${esc(shown)}</result>`);
  }
  if ([...logs, shown].some((l) => l.includes('[object Promise]'))) parts.push(`<warning>${esc(PROMISE_WARNING)}</warning>`);
  parts.push('</repl-output>');
  let saveError = '';
  try {
    await save();
  } catch (e) {
    saveError = String(e && e.message);
  }
  send({ t: 'done', text: parts.join('\n'), error, saveError });
}

process.on('uncaughtException', (e) => {
  try {
    lib.globals.console.error('Uncaught', e);
  } catch { /* nothing to tell */ }
});
process.on('unhandledRejection', (e) => {
  try {
    lib.globals.console.error('Unhandled rejection', e);
  } catch { /* nothing to tell */ }
});

let queue = Promise.resolve();
async function main() {
  await noteBuiltin();
  send({ t: 'ready', ...restore(), evals });
  let buf = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => {
    buf += chunk;
    let nl;
    while ((nl = buf.indexOf('\n')) >= 0) {
      const lineText = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (!lineText.trim()) continue;
      let msg;
      try {
        msg = JSON.parse(lineText);
      } catch {
        continue;
      }
      if (msg.t === 'reply') {
        const p = pending.get(msg.id);
        if (!p) continue;
        pending.delete(msg.id);
        if (msg.ok) p.resolve(JSON.stringify(msg.value === undefined ? null : msg.value));
        else p.reject(new lib.classes.ToolError(String(msg.value)));
      } else if (msg.t === 'interrupt') {
        stopped = true;
        for (const [id, p] of pending) {
          pending.delete(id);
          p.reject(new ctx.Error('Interrupted'));
        }
        if (running) running.interrupt(msg.why || 'Interrupted');
      } else if (msg.t === 'eval') {
        queue = queue.then(() => runScript(msg)).catch((e) => send({ t: 'done', text: `<repl-output><error>${esc(String(e && e.stack))}</error></repl-output>`, error: true }));
      } else if (msg.t === 'save') {
        queue = queue.then(save).catch(() => {});
      }
    }
  });
  process.stdin.on('end', () => process.exit(0));
}
main();
