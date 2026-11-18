// Oak's browser extension (Brave, Chrome, Edge): connects to the Oak running
// on this computer (ws://127.0.0.1:47615/oak; Oak accepts only this
// extension's origin) and, for a tab the user allowed in Oak, passes Oak's
// DevTools commands to it through chrome.debugger, and its events back.
// The browser shows that a tab is being debugged while Oak has it. Oak's
// src/browser/bridge.rs is the other end.

const OAK = "ws://127.0.0.1:47615/oak";
let ws = null;
const attached = new Set();

function send(o) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(o));
}

// Brave's user agent is Chrome's; its brands say Brave.
function browserName() {
  const brands = ((navigator.userAgentData && navigator.userAgentData.brands) || []).map((b) => b.brand);
  if (navigator.brave || brands.includes("Brave")) return "Brave";
  if (brands.includes("Microsoft Edge") || navigator.userAgent.includes("Edg/")) return "Edge";
  if (brands.includes("Google Chrome")) return "Chrome";
  return "your browser";
}

function connect() {
  if (ws && (ws.readyState === WebSocket.CONNECTING || ws.readyState === WebSocket.OPEN)) return;
  try {
    ws = new WebSocket(OAK);
  } catch (e) {
    ws = null;
    return;
  }
  ws.onopen = () => send({ op: "hello", version: chrome.runtime.getManifest().version, browser: browserName() });
  ws.onmessage = (m) => {
    let msg;
    try {
      msg = JSON.parse(m.data);
    } catch (e) {
      return;
    }
    handle(msg);
  };
  ws.onclose = () => {
    ws = null;
    // Oak is gone: let go of its tabs, and look for it again.
    for (const tabId of attached) chrome.debugger.detach({ tabId }).catch(() => {});
    attached.clear();
    setTimeout(connect, 3000);
  };
  ws.onerror = () => {};
}

// Tabs the debugger may attach to (not the browser's own pages).
function usable(t) {
  return t.url && /^(https?:|about:blank)/.test(t.url);
}

async function handle(msg) {
  const reply = (result) => send({ id: msg.id, result: result === undefined ? {} : result });
  const fail = (e) => send({ id: msg.id, error: String((e && e.message) || e) });
  try {
    switch (msg.op) {
      case "ping":
        return reply({});
      case "tabs": {
        const tabs = await chrome.tabs.query({});
        return reply(tabs.filter(usable).map((t) => ({ id: t.id, title: t.title || "", url: t.url, active: t.active, window: t.windowId })));
      }
      case "attach":
        if (!attached.has(msg.tab)) {
          await chrome.debugger.attach({ tabId: msg.tab }, "1.3");
          attached.add(msg.tab);
        }
        return reply({});
      case "detach":
        if (attached.has(msg.tab)) {
          attached.delete(msg.tab);
          await chrome.debugger.detach({ tabId: msg.tab });
        }
        return reply({});
      case "cdp": {
        const r = await chrome.debugger.sendCommand({ tabId: msg.tab }, msg.method, msg.params || {});
        return reply(r || {});
      }
      default:
        return fail("unknown op " + msg.op);
    }
  } catch (e) {
    fail(e);
  }
}

chrome.debugger.onEvent.addListener((source, method, params) => {
  if (source.tabId !== undefined) send({ op: "event", tab: source.tabId, method, params: params || {} });
});

chrome.debugger.onDetach.addListener((source, reason) => {
  attached.delete(source.tabId);
  send({ op: "detached", tab: source.tabId, reason });
});

// The service worker sleeps when idle: an alarm wakes it to look for Oak.
chrome.alarms.create("oak", { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(connect);
chrome.runtime.onStartup.addListener(connect);
chrome.runtime.onInstalled.addListener(connect);
connect();
