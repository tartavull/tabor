use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use crate::cli::AgentBrowserOptions;
use crate::cli::WindowOptions;
use crate::ipc::{
    self, IpcAction, IpcErrorCode, IpcRequest, SocketReply, TabSelection, UrlTarget,
    WebNetworkAction, WebNetworkEntry,
};
use crate::web_url::normalize_web_url;
use crate::window_kind::WindowKind;

const HELP_TEXT: &str = r#"agent-browser - fast browser automation CLI for AI agents

Usage: agent-browser <command> [args] [options]

Core Commands:
  open <url>                 Navigate to URL
  click <sel>                Click element (or @ref)
  dblclick <sel>             Double-click element
  type <sel> <text>          Type into element
  fill <sel> <text>          Clear and fill
  press <key>                Press key (Enter, Tab, Control+a)
  hover <sel>                Hover element
  focus <sel>                Focus element
  check <sel>                Check checkbox
  uncheck <sel>              Uncheck checkbox
  select <sel> <val...>      Select dropdown option
  drag <src> <dst>           Drag and drop
  upload <sel> <files...>    Upload files
  scroll <dir> [px]          Scroll (up/down/left/right)
  scrollintoview <sel>       Scroll element into view
  wait <sel|ms>              Wait for element or time
  screenshot [path]          Take screenshot
  pdf <path>                 Save as PDF
  snapshot                   Accessibility tree with refs (for AI)
  eval <js>                  Run JavaScript
  connect <port>             Connect to browser via CDP (e.g., connect 9222)
  close                      Close browser

Navigation:
  back                       Go back
  forward                    Go forward
  reload                     Reload page

Get Info:  agent-browser get <what> [selector]
  text, html, value, attr <name>, title, url, count, box, styles

Check State:  agent-browser is <what> <selector>
  visible, enabled, checked

Find Elements:  agent-browser find <locator> <value> <action> [text]
  role, text, label, placeholder, alt, title, testid, first, last, nth

Mouse:  agent-browser mouse <action> [args]
  move <x> <y>, down [btn], up [btn], wheel <dy> [dx]

Browser Settings:  agent-browser set <setting> [value]
  viewport <w> <h>, device <name>, geo <lat> <lng>
  offline [on|off], headers <json>, credentials <user> <pass>
  media [dark|light] [reduced-motion]

Network:  agent-browser network <action>
  route <url> [--abort|--body <json>]
  unroute [url]
  requests [--clear] [--filter <pattern>]

Storage:
  cookies [get|set|clear]    Manage cookies
  storage <local|session>    Manage web storage
  state save|load <path>     Save or restore storage state

Tabs:
  tab [new|list|close|<n>]   Manage tabs
Frames:
  frame <selector|main>      Switch active iframe

Debug:
  trace start|stop [path]    Record trace
  record start <path> [url]  Start video recording (WebM)
  record stop                Stop and save video
  record restart <path>      Restart recording
  console [--clear]          View console logs
  errors [--clear]           View page errors
  highlight <sel>            Highlight element
  dialog accept|dismiss      Handle dialogs

Sessions:
  session                    Show current session name
  session list               List active sessions

Setup:
  install                    Install browser binaries
  install --with-deps        Also install system dependencies (Linux)

Snapshot Options:
  -i, --interactive          Only interactive elements
  -c, --compact              Remove empty structural elements
  -d, --depth <n>            Limit tree depth
  -s, --selector <sel>       Scope to CSS selector

Options:
  --session <name>           Isolated session (or AGENT_BROWSER_SESSION env)
  --headers <json>           HTTP headers scoped to URL's origin (for auth)
  --executable-path <path>   Custom browser executable (or AGENT_BROWSER_EXECUTABLE_PATH)
  --extension <path>         Load browser extensions (repeatable).
  --proxy <url>              Proxy server (http://[user:pass@]host:port)
  --json                     JSON output
  --full, -f                 Full page screenshot
  --headed                   Show browser window (not headless)
  --cdp <port>               Connect via CDP (Chrome DevTools Protocol)
  --debug                    Debug output
  --version, -V              Show version

Environment:
  AGENT_BROWSER_SESSION          Session name (default: "default")
  AGENT_BROWSER_EXECUTABLE_PATH  Custom browser executable path
  AGENT_BROWSER_STREAM_PORT      Enable WebSocket streaming on port (e.g., 9223)

Examples:
  agent-browser open example.com
  agent-browser snapshot -i              # Interactive elements only
  agent-browser click @e2                # Click by ref from snapshot
  agent-browser fill @e3 "test@example.com"
  agent-browser find role button click --name Submit
  agent-browser get text @e1
  agent-browser screenshot --full
  agent-browser --cdp 9222 snapshot      # Connect via CDP port
"#;

const JS_HELPER: &str = r#"(() => {
  if (window.__taborAB) return;

  const now = () => (performance && performance.now) ? performance.now() : Date.now();
  const normalize = (text) => (text || "").trim();
  const state = {
    frameSelector: null,
    console: [],
    errors: [],
    dialogs: [],
    dialogResponse: null,
    network: {
      entries: [],
      routes: [],
      clearTime: 0,
      active: 0,
      lastActivity: 0,
      offline: false,
      headers: {},
      auth: null,
      installed: false,
    },
    media: {
      colorScheme: null,
      reducedMotion: false,
    },
    viewport: null,
    device: null,
    geo: null,
    mediaListeners: [],
  };

  const updateActivity = () => {
    state.network.lastActivity = now();
  };

  const matchPattern = (pattern, url) => {
    if (!pattern) return false;
    if (pattern.indexOf("*") >= 0) {
      const parts = pattern.split("*");
      let pos = 0;
      for (let i = 0; i < parts.length; i += 1) {
        const part = parts[i];
        if (!part) continue;
        const idx = url.indexOf(part, pos);
        if (idx === -1) return false;
        if (i === 0 && !url.startsWith(part)) return false;
        pos = idx + part.length;
      }
      if (pattern.endsWith("*")) return true;
      const last = parts[parts.length - 1] || "";
      return last === "" || url.endsWith(last);
    }
    return url === pattern;
  };

  const currentDocument = () => {
    if (state.frameSelector) {
      try {
        const frame = document.querySelector(state.frameSelector);
        if (frame && frame.contentDocument) return frame.contentDocument;
      } catch (e) {}
    }
    return document;
  };

  const isVisible = (el) => {
    if (!el) return false;
    const style = window.getComputedStyle(el);
    if (!style || style.visibility === "hidden" || style.display === "none") return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };

  const elementName = (el) => {
    if (!el) return "";
    return normalize(
      el.getAttribute("aria-label") ||
      el.getAttribute("alt") ||
      el.getAttribute("title") ||
      el.value ||
      el.innerText ||
      el.textContent ||
      ""
    );
  };

  const elementRole = (el) => {
    if (!el) return "element";
    const role = el.getAttribute("role");
    if (role) return role;
    const tag = el.tagName.toLowerCase();
    if (tag === "a") return "link";
    if (tag === "button") return "button";
    if (tag === "input") {
      const type = (el.getAttribute("type") || "text").toLowerCase();
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (type === "submit" || type === "button") return "button";
      return "textbox";
    }
    if (tag === "select") return "combobox";
    if (tag === "textarea") return "textbox";
    return tag;
  };

  const isInteractive = (el) => {
    if (!el) return false;
    const tag = el.tagName.toLowerCase();
    if (tag === "a" || tag === "button" || tag === "select" || tag === "textarea") return true;
    if (tag === "input") return true;
    if (el.hasAttribute("role")) return true;
    if (el.hasAttribute("onclick")) return true;
    if (el.hasAttribute("tabindex")) return true;
    return false;
  };

  const resolveSelector = (sel) => {
    if (!sel) return null;
    const doc = currentDocument();
    if (sel.startsWith("@")) {
      const ref = sel.slice(1);
      return doc.querySelector(`[data-tabor-ref="${ref}"]`);
    }
    return doc.querySelector(sel);
  };

  const resolveAll = (sel) => {
    if (!sel) return [];
    const doc = currentDocument();
    if (sel.startsWith("@")) {
      const el = resolveSelector(sel);
      return el ? [el] : [];
    }
    return Array.from(doc.querySelectorAll(sel));
  };

  const findElement = (locator, value, options) => {
    const exact = options && options.exact;
    const name = options && options.name;
    const doc = currentDocument();
    const textMatches = (el) => {
      const text = normalize(el.innerText || el.textContent || "");
      return exact ? text === value : text.includes(value);
    };
    if (locator === "text") {
      return Array.from(doc.querySelectorAll("*"))
        .find((el) => textMatches(el));
    }
    if (locator === "label") {
      const labels = Array.from(doc.querySelectorAll("label"));
      const label = labels.find((el) => textMatches(el));
      if (!label) return null;
      const forId = label.getAttribute("for");
      if (forId) return doc.getElementById(forId);
      return label.querySelector("input, textarea, select");
    }
    if (locator === "placeholder") {
      return Array.from(doc.querySelectorAll("[placeholder]"))
        .find((el) => {
          const ph = normalize(el.getAttribute("placeholder") || "");
          return exact ? ph === value : ph.includes(value);
        });
    }
    if (locator === "alt" || locator === "title" || locator === "testid") {
      const attr = locator === "testid" ? "data-testid" : locator;
      return Array.from(doc.querySelectorAll(`[${attr}]`))
        .find((el) => {
          const v = normalize(el.getAttribute(attr) || "");
          return exact ? v === value : v.includes(value);
        });
    }
    if (locator === "role") {
      const role = value.toLowerCase();
      const selectors = [`[role="${role}"]`];
      if (role === "button") selectors.push("button", "input[type=button]", "input[type=submit]");
      if (role === "link") selectors.push("a[href]");
      const candidates = selectors.flatMap((sel) => Array.from(doc.querySelectorAll(sel)));
      if (!name) return candidates[0] || null;
      return candidates.find((el) => {
        const label = elementName(el);
        return exact ? label === name : label.includes(name);
      }) || null;
    }
    if (locator === "first" || locator === "last" || locator === "nth") {
      const sel = value;
      const list = Array.from(doc.querySelectorAll(sel));
      if (locator === "first") return list[0] || null;
      if (locator === "last") return list[list.length - 1] || null;
      const index = options && typeof options.index === "number" ? options.index : 0;
      return list[index] || null;
    }
    return null;
  };

  const ensureHighlightStyle = () => {
    if (state._highlightStyle) return;
    state._highlightStyle = true;
    const style = document.createElement("style");
    style.textContent = ".tabor-highlight{outline:2px solid #ff3366 !important; outline-offset:2px !important;}";
    document.head.appendChild(style);
  };

  const highlight = (sel) => {
    ensureHighlightStyle();
    const existing = document.querySelector(".tabor-highlight");
    if (existing) existing.classList.remove("tabor-highlight");
    const el = resolveSelector(sel);
    if (!el) return false;
    el.classList.add("tabor-highlight");
    return true;
  };

  const captureConsole = () => {
    if (state._consoleInstalled) return;
    state._consoleInstalled = true;
    const methods = ["log", "info", "warn", "error", "debug"];
    methods.forEach((method) => {
      const original = console[method];
      console[method] = (...args) => {
        state.console.push({
          type: method,
          args: args.map((arg) => {
            try { return typeof arg === "string" ? arg : JSON.stringify(arg); } catch (e) { return String(arg); }
          }),
          ts: Date.now(),
        });
        try { original.apply(console, args); } catch (e) {}
      };
    });
  };

  const captureErrors = () => {
    if (state._errorsInstalled) return;
    state._errorsInstalled = true;
    window.addEventListener("error", (event) => {
      state.errors.push({
        message: String(event.message || event.error || ""),
        filename: event.filename || "",
        lineno: event.lineno || 0,
        colno: event.colno || 0,
        ts: Date.now(),
      });
    });
    window.addEventListener("unhandledrejection", (event) => {
      state.errors.push({
        message: String(event.reason || ""),
        filename: "",
        lineno: 0,
        colno: 0,
        ts: Date.now(),
      });
    });
  };

  const captureDialogs = () => {
    if (state._dialogsInstalled) return;
    state._dialogsInstalled = true;
    const consumeResponse = () => {
      const response = state.dialogResponse;
      state.dialogResponse = null;
      return response;
    };
    window.alert = (msg) => {
      state.dialogs.push({ type: "alert", message: String(msg || ""), defaultValue: "" });
      consumeResponse();
    };
    window.confirm = (msg) => {
      state.dialogs.push({ type: "confirm", message: String(msg || ""), defaultValue: "" });
      const response = consumeResponse();
      return response ? !!response.accept : true;
    };
    window.prompt = (msg, defaultValue) => {
      state.dialogs.push({
        type: "prompt",
        message: String(msg || ""),
        defaultValue: defaultValue == null ? "" : String(defaultValue),
      });
      const response = consumeResponse();
      if (!response) return defaultValue == null ? "" : String(defaultValue);
      return response.accept ? (response.text == null ? "" : String(response.text)) : null;
    };
  };

  const setDialogResponse = (accept, text) => {
    state.dialogResponse = { accept: !!accept, text: text == null ? "" : String(text) };
    return true;
  };

  const consumeDialogs = (clear) => {
    const items = state.dialogs.slice();
    if (clear) state.dialogs = [];
    return items;
  };

  const applyGeolocation = () => {
    if (!state.geo) return;
    if (!navigator.geolocation || state._geoInstalled) return;
    state._geoInstalled = true;
    const makeCoords = () => ({
      latitude: state.geo.lat,
      longitude: state.geo.lng,
      accuracy: 1,
      altitude: null,
      altitudeAccuracy: null,
      heading: null,
      speed: null,
    });
    navigator.geolocation.getCurrentPosition = (success, error) => {
      if (typeof success === "function") {
        success({ coords: makeCoords(), timestamp: Date.now() });
      } else if (typeof error === "function") {
        error({ code: 1, message: "Position unavailable" });
      }
    };
    navigator.geolocation.watchPosition = (success, _error) => {
      const id = Math.floor(Math.random() * 1000000);
      if (typeof success === "function") {
        success({ coords: makeCoords(), timestamp: Date.now() });
      }
      return id;
    };
    navigator.geolocation.clearWatch = (_id) => {};
  };

  const applyUserAgent = (ua, platform) => {
    try {
      if (ua) Object.defineProperty(navigator, "userAgent", { get: () => ua, configurable: true });
      if (platform) Object.defineProperty(navigator, "platform", { get: () => platform, configurable: true });
    } catch (e) {}
  };

  const applyViewport = () => {
    if (!state.viewport) return;
    const width = state.viewport.width;
    const height = state.viewport.height;
    const doc = document;
    if (doc && doc.head) {
      let meta = doc.querySelector("meta[name=viewport]");
      if (!meta) {
        meta = doc.createElement("meta");
        meta.setAttribute("name", "viewport");
        doc.head.appendChild(meta);
      }
      meta.setAttribute("content", `width=${width}, height=${height}`);
    }
    if (doc && doc.documentElement) {
      doc.documentElement.style.width = `${width}px`;
      doc.documentElement.style.height = `${height}px`;
      doc.documentElement.style.overflow = "auto";
    }
    if (doc && doc.body) {
      doc.body.style.width = `${width}px`;
      doc.body.style.height = `${height}px`;
      doc.body.style.overflow = "auto";
    }
    try {
      Object.defineProperty(window, "innerWidth", { get: () => width, configurable: true });
      Object.defineProperty(window, "innerHeight", { get: () => height, configurable: true });
    } catch (e) {}
  };

  const applyMedia = () => {
    if (state._mediaInstalled) return;
    state._mediaInstalled = true;
    const original = window.matchMedia ? window.matchMedia.bind(window) : null;
    window.matchMedia = (query) => {
      const q = String(query || "");
      const list = {
        media: q,
        matches: false,
        addListener: function() {},
        removeListener: function() {},
        addEventListener: function() {},
        removeEventListener: function() {},
        onchange: null,
      };
      if (q.includes("prefers-color-scheme")) {
        const isDark = state.media.colorScheme === "dark";
        list.matches = q.includes("dark") ? isDark : !isDark;
      } else if (q.includes("prefers-reduced-motion")) {
        list.matches = !!state.media.reducedMotion && q.includes("reduce");
      } else if (original) {
        return original(query);
      }
      return list;
    };
  };

  const setViewport = (width, height) => {
    const w = Number(width);
    const h = Number(height);
    if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return false;
    state.viewport = { width: w, height: h };
    applyViewport();
    return true;
  };

  const devices = {
    "iphone 14": {
      width: 390,
      height: 844,
      userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1",
      platform: "iPhone",
    },
    "iphone 14 pro": {
      width: 393,
      height: 852,
      userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 16_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1",
      platform: "iPhone",
    },
    "pixel 5": {
      width: 393,
      height: 851,
      userAgent: "Mozilla/5.0 (Linux; Android 13; Pixel 5) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
      platform: "Android",
    },
  };

  const setDevice = (name) => {
    const key = String(name || "").toLowerCase();
    const device = devices[key];
    if (!device) return false;
    state.device = key;
    applyUserAgent(device.userAgent, device.platform);
    setViewport(device.width, device.height);
    return true;
  };

  const setGeo = (lat, lng) => {
    const latitude = Number(lat);
    const longitude = Number(lng);
    if (!Number.isFinite(latitude) || !Number.isFinite(longitude)) return false;
    state.geo = { lat: latitude, lng: longitude };
    applyGeolocation();
    return true;
  };

  const setOffline = (offline) => {
    state.network.offline = !!offline;
    try {
      Object.defineProperty(navigator, "onLine", { get: () => !state.network.offline, configurable: true });
    } catch (e) {}
    window.dispatchEvent(new Event(state.network.offline ? "offline" : "online"));
    return true;
  };

  const setHeaders = (headers) => {
    state.network.headers = headers || {};
    return true;
  };

  const setAuth = (user, pass) => {
    if (user == null && pass == null) {
      state.network.auth = null;
      return true;
    }
    state.network.auth = { user: String(user || ""), pass: String(pass || "") };
    return true;
  };

  const setMedia = (scheme, reduced) => {
    state.media.colorScheme = scheme || null;
    state.media.reducedMotion = !!reduced;
    applyMedia();
    return true;
  };

  const ensureNetwork = () => {
    if (state.network.installed) return;
    state.network.installed = true;
    captureConsole();
    captureErrors();
    captureDialogs();

    if (typeof fetch === "function") {
      const originalFetch = fetch.bind(window);
      window.fetch = async (...args) => {
        const startTime = now();
        let method = "GET";
        let url = "";
        let req = null;
        try {
          req = args[0] instanceof Request ? args[0] : new Request(...args);
          method = req.method || method;
          url = req.url || url;
        } catch (e) {
          try { url = String(args[0] || ""); } catch (e2) { url = ""; }
        }
        const entry = {
          request_id: `fetch:${url}:${startTime}`,
          url,
          method,
          status: null,
          resource_type: "fetch",
          start_time: startTime,
          end_time: null,
          error_text: null,
        };

        if (state.network.offline) {
          entry.end_time = now();
          entry.error_text = "Offline";
          state.network.entries.push(entry);
          updateActivity();
          return Promise.reject(new Error("Offline"));
        }

        const route = state.network.routes.find((r) => matchPattern(r.pattern, url));
        if (route) {
          if (route.action === "abort") {
            entry.end_time = now();
            entry.error_text = "Aborted";
            state.network.entries.push(entry);
            updateActivity();
            return Promise.reject(new Error("Aborted"));
          }
          if (route.action === "fulfill") {
            const body = route.body || "";
            const headers = new Headers(route.headers || {});
            if (!headers.has("content-type")) {
              headers.set("content-type", route.contentType || "application/json");
            }
            const response = new Response(body, { status: route.status || 200, headers });
            entry.status = response.status;
            entry.end_time = now();
            state.network.entries.push(entry);
            updateActivity();
            return response;
          }
        }

        let headers = new Headers();
        try {
          const base = req ? req.headers : (args[1] && args[1].headers);
          headers = new Headers(base || undefined);
        } catch (e) {}
        Object.entries(state.network.headers || {}).forEach(([key, value]) => {
          try { headers.set(key, value); } catch (e) {}
        });
        if (state.network.auth) {
          const token = btoa(`${state.network.auth.user}:${state.network.auth.pass}`);
          headers.set("Authorization", `Basic ${token}`);
        }

        let request = null;
        try {
          if (req) {
            request = new Request(req, { headers });
          } else {
            const init = args[1] || {};
            request = new Request(args[0], Object.assign({}, init, { headers }));
          }
        } catch (e) {
          request = req || args[0];
        }

        state.network.active += 1;
        updateActivity();
        try {
          const response = await originalFetch(request);
          entry.status = response && typeof response.status === "number" ? response.status : null;
          entry.end_time = now();
          state.network.entries.push(entry);
          return response;
        } catch (error) {
          entry.end_time = now();
          entry.error_text = String(error);
          state.network.entries.push(entry);
          throw error;
        } finally {
          state.network.active = Math.max(0, state.network.active - 1);
          updateActivity();
        }
      };
    }

    if (typeof XMLHttpRequest !== "undefined") {
      const originalOpen = XMLHttpRequest.prototype.open;
      const originalSend = XMLHttpRequest.prototype.send;
      XMLHttpRequest.prototype.open = function(method, url, ...rest) {
        try {
          this.__taborMethod = method ? String(method) : "GET";
          this.__taborUrl = url ? String(url) : "";
        } catch (e) {
          this.__taborMethod = "GET";
          this.__taborUrl = "";
        }
        return originalOpen.call(this, method, url, ...rest);
      };
      XMLHttpRequest.prototype.send = function(...args) {
        const startTime = now();
        const method = this.__taborMethod || "GET";
        const url = this.__taborUrl || "";
        const entry = {
          request_id: `xhr:${url}:${startTime}`,
          url,
          method,
          status: null,
          resource_type: "xhr",
          start_time: startTime,
          end_time: null,
          error_text: null,
        };

        if (state.network.offline) {
          entry.end_time = now();
          entry.error_text = "Offline";
          state.network.entries.push(entry);
          updateActivity();
          this.abort();
          return;
        }

        const route = state.network.routes.find((r) => matchPattern(r.pattern, url));
        if (route && route.action === "abort") {
          entry.end_time = now();
          entry.error_text = "Aborted";
          state.network.entries.push(entry);
          updateActivity();
          this.abort();
          return;
        }
        if (route && route.action === "fulfill") {
          entry.status = route.status || 200;
          entry.end_time = now();
          state.network.entries.push(entry);
          updateActivity();
          try {
            Object.defineProperty(this, "status", { value: entry.status, configurable: true });
            Object.defineProperty(this, "responseText", { value: route.body || "", configurable: true });
            Object.defineProperty(this, "response", { value: route.body || "", configurable: true });
          } catch (e) {}
          setTimeout(() => {
            this.dispatchEvent(new Event("readystatechange"));
            this.dispatchEvent(new Event("load"));
            this.dispatchEvent(new Event("loadend"));
          }, 0);
          return;
        }

        try {
          Object.entries(state.network.headers || {}).forEach(([key, value]) => {
            try { this.setRequestHeader(key, value); } catch (e) {}
          });
          if (state.network.auth) {
            const token = btoa(`${state.network.auth.user}:${state.network.auth.pass}`);
            this.setRequestHeader("Authorization", `Basic ${token}`);
          }
        } catch (e) {}

        const finalize = () => {
          entry.end_time = now();
          entry.status = typeof this.status === "number" && this.status > 0 ? this.status : null;
          entry.url = this.responseURL || url;
          if (entry.status == null && !this.responseURL) {
            entry.error_text = "Network error";
          }
          state.network.entries.push(entry);
          state.network.active = Math.max(0, state.network.active - 1);
          updateActivity();
        };

        state.network.active += 1;
        updateActivity();
        this.addEventListener("loadend", finalize, { once: true });
        return originalSend.apply(this, args);
      };
    }
  };

  const getNetworkEntries = (filter, clear) => {
    ensureNetwork();
    let entries = state.network.entries.slice();
    if (performance && performance.getEntriesByType) {
      const resources = performance.getEntriesByType("resource") || [];
      const cutoff = state.network.clearTime || 0;
      resources.forEach((res) => {
        if (cutoff && res.startTime < cutoff) return;
        entries.push({
          request_id: `res:${res.name}:${res.startTime}`,
          url: res.name,
          method: null,
          status: null,
          resource_type: res.initiatorType || null,
          start_time: res.startTime,
          end_time: res.responseEnd || null,
          error_text: null,
        });
      });
    }
    if (filter) {
      entries = entries.filter((entry) => entry.url && entry.url.includes(filter));
    }
    if (clear) {
      state.network.entries = [];
      state.network.clearTime = now();
      if (performance && performance.clearResourceTimings) {
        performance.clearResourceTimings();
      }
    }
    return entries;
  };

  const isNetworkIdle = (idleMs) => {
    ensureNetwork();
    const idleFor = now() - (state.network.lastActivity || state.network.clearTime || 0);
    return state.network.active === 0 && idleFor >= idleMs;
  };

  const addRoute = (pattern, action) => {
    ensureNetwork();
    if (!pattern) return false;
    const entry = Object.assign({}, action || {}, { pattern: String(pattern) });
    state.network.routes.push(entry);
    return true;
  };

  const removeRoute = (pattern) => {
    ensureNetwork();
    if (!pattern) {
      state.network.routes = [];
      return true;
    }
    state.network.routes = state.network.routes.filter((route) => route.pattern !== pattern);
    return true;
  };

  const getConsole = (clear) => {
    captureConsole();
    const items = state.console.slice();
    if (clear) state.console = [];
    return items;
  };

  const getErrors = (clear) => {
    captureErrors();
    const items = state.errors.slice();
    if (clear) state.errors = [];
    return items;
  };

  const setFrame = (selector) => {
    if (!selector || selector === "main") {
      state.frameSelector = null;
      return true;
    }
    const frame = document.querySelector(selector);
    if (!frame || !(frame.tagName === "IFRAME" || frame.tagName === "FRAME")) return false;
    try {
      if (!frame.contentDocument) return false;
    } catch (e) {
      return false;
    }
    state.frameSelector = selector;
    return true;
  };

  captureConsole();
  captureErrors();
  captureDialogs();
  applyMedia();
  applyViewport();
  applyGeolocation();

  window.__taborAB = {
    normalize,
    isVisible,
    elementName,
    elementRole,
    isInteractive,
    resolveSelector,
    resolveAll,
    findElement,
    highlight,
    getConsole,
    getErrors,
    consumeDialogs,
    setDialogResponse,
    setOffline,
    setHeaders,
    setAuth,
    setGeo,
    setMedia,
    setViewport,
    setDevice,
    addRoute,
    removeRoute,
    getNetworkEntries,
    isNetworkIdle,
    setFrame,
    doc: currentDocument,
  };
})();"#;

#[derive(Default)]
struct GlobalOptions {
    json: bool,
    full: bool,
    session: Option<String>,
    headers: Option<String>,
    headed: bool,
    cdp: Option<u16>,
    proxy: Option<String>,
    executable_path: Option<String>,
    extensions: Vec<String>,
    debug: bool,
}

pub fn run(options: AgentBrowserOptions) -> Result<(), Box<dyn Error>> {
    let args =
        options.args.into_iter().map(|arg| arg.to_string_lossy().to_string()).collect::<Vec<_>>();

    let (globals, command, command_args) = parse_args(args)?;
    if let Some(command) = command {
        if command.is_empty() {
            return Ok(());
        }
        execute_command(&globals, &command, &command_args)
    } else {
        print_help();
        Ok(())
    }
}

type ParsedArgs = (GlobalOptions, Option<String>, Vec<String>);

fn parse_args(args: Vec<String>) -> Result<ParsedArgs, Box<dyn Error>> {
    let mut globals = GlobalOptions::default();
    let mut command: Option<String> = None;
    let mut command_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if command.is_none() {
            match arg.as_str() {
                "--help" | "-h" => {
                    print_help();
                    return Ok((globals, Some(String::new()), Vec::new()));
                },
                "--version" | "-V" => {
                    println!("tabor agent-browser {}", env!("CARGO_PKG_VERSION"));
                    return Ok((globals, Some(String::new()), Vec::new()));
                },
                "--json" => globals.json = true,
                "--full" | "-f" => globals.full = true,
                "--headed" => globals.headed = true,
                "--debug" => globals.debug = true,
                "--session" => {
                    i += 1;
                    globals.session = args.get(i).cloned();
                },
                "--headers" => {
                    i += 1;
                    globals.headers = args.get(i).cloned();
                },
                "--cdp" => {
                    i += 1;
                    globals.cdp = args.get(i).and_then(|value| value.parse::<u16>().ok());
                },
                "--proxy" => {
                    i += 1;
                    globals.proxy = args.get(i).cloned();
                },
                "--executable-path" => {
                    i += 1;
                    globals.executable_path = args.get(i).cloned();
                },
                "--extension" => {
                    i += 1;
                    if let Some(value) = args.get(i) {
                        globals.extensions.push(value.clone());
                    }
                },
                _ if arg.starts_with('-') => {
                    return Err(shim_error(format!("unknown option '{arg}'")));
                },
                _ => {
                    command = Some(arg.clone());
                    command_args = args[i + 1..].to_vec();
                    break;
                },
            }
        } else {
            command_args.push(arg.clone());
        }
        i += 1;
    }

    Ok((globals, command, command_args))
}

fn execute_command(
    globals: &GlobalOptions,
    command: &str,
    args: &[String],
) -> Result<(), Box<dyn Error>> {
    match command {
        "open" | "goto" | "navigate" => cmd_open(globals, args),
        "back" => cmd_simple_js(globals, "history.back();"),
        "forward" => cmd_simple_js(globals, "history.forward();"),
        "reload" => cmd_simple_js(globals, "location.reload();"),
        "close" | "quit" | "exit" => cmd_close(globals, args),
        "snapshot" => cmd_snapshot(globals, args),
        "click" => cmd_click(globals, args),
        "dblclick" => cmd_dblclick(globals, args),
        "focus" => cmd_focus(globals, args),
        "fill" => cmd_fill(globals, args),
        "type" => cmd_type(globals, args),
        "press" => cmd_press(globals, args, PressEvent::Both),
        "keydown" => cmd_press(globals, args, PressEvent::Down),
        "keyup" => cmd_press(globals, args, PressEvent::Up),
        "hover" => cmd_hover(globals, args),
        "check" => cmd_check(globals, args, true),
        "uncheck" => cmd_check(globals, args, false),
        "select" => cmd_select(globals, args),
        "scroll" => cmd_scroll(globals, args),
        "scrollintoview" | "scrollinto" => cmd_scroll_into(globals, args),
        "drag" => cmd_drag(globals, args),
        "upload" => cmd_upload(globals, args),
        "screenshot" => cmd_screenshot(globals, args),
        "pdf" => cmd_pdf(globals, args),
        "eval" => cmd_eval(globals, args),
        "get" => cmd_get(globals, args),
        "is" => cmd_is(globals, args),
        "find" => cmd_find(globals, args),
        "wait" => cmd_wait(globals, args),
        "mouse" => cmd_mouse(globals, args),
        "tab" => cmd_tab(globals, args),
        "window" => cmd_window(globals, args),
        "cookies" => cmd_cookies(globals, args),
        "storage" => cmd_storage(globals, args),
        "session" => cmd_session(globals, args),
        "network" => cmd_network(globals, args),
        "state" => cmd_state(globals, args),
        "install" => Ok(()),
        "connect" => cmd_connect(globals, args),
        "set" => cmd_set(globals, args),
        "record" => cmd_record(globals, args),
        "trace" => cmd_trace(globals, args),
        "console" => cmd_console(globals, args),
        "errors" => cmd_errors(globals, args),
        "highlight" => cmd_highlight(globals, args),
        "frame" => cmd_frame(globals, args),
        "dialog" => cmd_dialog(globals, args),
        "__record-worker" => cmd_record_worker(globals, args),
        _ => Err(shim_error(format!("unknown agent-browser command '{command}'"))),
    }
}

fn cmd_open(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("open requires a URL"));
    }
    let url = normalize_web_url(&args.join(" "));
    if url.is_empty() {
        return Err(shim_error("open requires a URL"));
    }

    let reply = send_request(IpcRequest::OpenUrl { url, target: UrlTarget::Current })?;
    expect_ok(reply)?;
    if let Some(headers) = &globals.headers {
        cmd_set(globals, &[String::from("headers"), headers.clone()])?;
    }
    Ok(())
}

fn cmd_close(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if !args.is_empty() {
        return Err(shim_error("close does not accept arguments"));
    }
    let reply = send_request(IpcRequest::CloseTab { tab_id: None })?;
    expect_ok(reply)
}

fn cmd_snapshot(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut interactive = false;
    let mut compact = false;
    let mut depth: Option<usize> = None;
    let mut selector: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--interactive" => interactive = true,
            "-c" | "--compact" => compact = true,
            "-d" | "--depth" => {
                i += 1;
                depth = args.get(i).and_then(|v| v.parse::<usize>().ok());
            },
            "-s" | "--selector" => {
                i += 1;
                selector = args.get(i).cloned();
            },
            _ => return Err(shim_error("invalid snapshot option")),
        }
        i += 1;
    }

    let opts = json!({
        "interactive": interactive,
        "compact": compact,
        "depth": depth,
        "selector": selector,
    });
    let script = js_wrap(&format!(
        "const opts = {opts};\nconst doc = window.__taborAB.doc();\nconst root = opts.selector ? doc.querySelector(opts.selector) : doc.body;\nif (!root) return JSON.stringify({{ error: 'selector not found' }});\nconst existing = new Set();\nArray.from(doc.querySelectorAll('[data-tabor-ref]')).forEach((el) => existing.add(el.getAttribute('data-tabor-ref')));\nlet refIndex = 1;\nconst makeRef = () => {{ while (existing.has('e' + refIndex)) refIndex += 1; const ref = 'e' + refIndex; existing.add(ref); refIndex += 1; return ref; }};\nconst depthOf = (el) => {{ let d = 0; let cur = el; while (cur && cur !== root) {{ d += 1; cur = cur.parentElement; }} return d; }};\nconst elements = [];\nArray.from(root.querySelectorAll('*')).forEach((el) => {{\n  if (opts.interactive && !window.__taborAB.isInteractive(el)) return;\n  if (opts.depth !== null && opts.depth !== undefined && depthOf(el) > opts.depth) return;\n  let ref = el.getAttribute('data-tabor-ref');\n  if (!ref) {{ ref = makeRef(); el.setAttribute('data-tabor-ref', ref); }}\n  const role = window.__taborAB.elementRole(el);\n  const name = window.__taborAB.elementName(el);\n  elements.push({{ ref, role, name }});\n}});\nreturn JSON.stringify({{ elements }});"
    ));

    let result = web_eval_json(script)?;
    if globals.json {
        print_json(result)
    } else {
        let elements = result
            .get("elements")
            .and_then(|v| v.as_array())
            .ok_or_else(|| shim_error("snapshot returned no elements"))?;
        for element in elements {
            let role = element.get("role").and_then(|v| v.as_str()).unwrap_or("element");
            let name = element.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let reference = element.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                println!("{} [ref={}]", role, reference);
            } else {
                println!("{} \"{}\" [ref={}]", role, name, reference);
            }
        }
        Ok(())
    }
}

fn cmd_click(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("click requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nel.dispatchEvent(new MouseEvent('click', {{ bubbles: true, cancelable: true }}));\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_dblclick(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("dblclick requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nel.dispatchEvent(new MouseEvent('dblclick', {{ bubbles: true, cancelable: true }}));\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_focus(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("focus requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nel.focus();\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_fill(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("fill requires a selector and text"));
    }
    let selector = &args[0];
    let text = args[1..].join(" ");
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nif ('value' in el) {{ el.value = ''; el.value = {}; el.dispatchEvent(new Event('input', {{ bubbles: true }})); el.dispatchEvent(new Event('change', {{ bubbles: true }})); }} else if (el.isContentEditable) {{ const doc = window.__taborAB.doc(); el.textContent = ''; doc.execCommand('insertText', false, {}); }}\nreturn JSON.stringify({{ ok: true }});",
        js_string(selector),
        js_string(&text),
        js_string(&text)
    ));
    eval_ok(script, globals)
}

fn cmd_type(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("type requires a selector and text"));
    }
    let selector = &args[0];
    let text = args[1..].join(" ");
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nif ('value' in el) {{ el.value = (el.value || '') + {}; el.dispatchEvent(new Event('input', {{ bubbles: true }})); }} else if (el.isContentEditable) {{ const doc = window.__taborAB.doc(); doc.execCommand('insertText', false, {}); }}\nreturn JSON.stringify({{ ok: true }});",
        js_string(selector),
        js_string(&text),
        js_string(&text)
    ));
    eval_ok(script, globals)
}

enum PressEvent {
    Down,
    Up,
    Both,
}

fn cmd_press(
    globals: &GlobalOptions,
    args: &[String],
    event: PressEvent,
) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("press requires a key"));
    }
    let key = &args[0];
    let key_info = parse_key(key);
    if matches!(event, PressEvent::Both) && key_info.meta && !key_info.ctrl && !key_info.alt {
        let action_name = match key_info.key.as_str() {
            "c" => Some("copy"),
            "v" => Some("paste"),
            _ => None,
        };
        if let Some(action_name) = action_name {
            let reply = send_request(IpcRequest::DispatchAction {
                tab_id: None,
                action: IpcAction::Action { name: action_name.to_string() },
            })?;
            return expect_ok(reply);
        }
    }
    let info_json = serde_json::to_string(&key_info)?;
    let event_type = match event {
        PressEvent::Down => "keydown",
        PressEvent::Up => "keyup",
        PressEvent::Both => "press",
    };
    let script = js_wrap(&format!(
        "const info = {info_json};\nconst doc = window.__taborAB.doc();\nconst target = doc.activeElement || doc.body;\nconst init = {{ key: info.key, ctrlKey: info.ctrl, altKey: info.alt, shiftKey: info.shift, metaKey: info.meta, bubbles: true, cancelable: true }};\nif ('{event_type}' === 'keydown') {{ target.dispatchEvent(new KeyboardEvent('keydown', init)); }} else if ('{event_type}' === 'keyup') {{ target.dispatchEvent(new KeyboardEvent('keyup', init)); }} else {{ target.dispatchEvent(new KeyboardEvent('keydown', init)); target.dispatchEvent(new KeyboardEvent('keyup', init)); }}\nreturn JSON.stringify({{ ok: true }});"
    ));
    eval_ok(script, globals)
}

fn cmd_hover(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("hover requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nel.dispatchEvent(new MouseEvent('mouseover', {{ bubbles: true, cancelable: true }}));\nel.dispatchEvent(new MouseEvent('mousemove', {{ bubbles: true, cancelable: true }}));\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_check(
    globals: &GlobalOptions,
    args: &[String],
    checked: bool,
) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("check requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nif ('checked' in el) {{ el.checked = {}; el.dispatchEvent(new Event('change', {{ bubbles: true }})); }}\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0]),
        if checked { "true" } else { "false" }
    ));
    eval_ok(script, globals)
}

fn cmd_select(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("select requires a selector and values"));
    }
    let selector = &args[0];
    let values = args[1..].to_vec();
    let values_json = serde_json::to_string(&values)?;
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nconst values = {values_json};\nif (el.options) {{\n  for (const opt of el.options) {{ opt.selected = values.includes(opt.value); }}\n  el.dispatchEvent(new Event('change', {{ bubbles: true }}));\n}}\nreturn JSON.stringify({{ ok: true }});",
        js_string(selector)
    ));
    eval_ok(script, globals)
}

fn cmd_scroll(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("scroll requires direction"));
    }
    let direction = args[0].as_str();
    let amount = args.get(1).and_then(|v| v.parse::<i32>().ok()).unwrap_or(300);
    let (dx, dy) = match direction {
        "up" => (0, -amount),
        "down" => (0, amount),
        "left" => (-amount, 0),
        "right" => (amount, 0),
        _ => return Err(shim_error("scroll direction must be up/down/left/right")),
    };
    let script =
        js_wrap(&format!("window.scrollBy({dx}, {dy});\nreturn JSON.stringify({{ ok: true }});"));
    eval_ok(script, globals)
}

fn cmd_scroll_into(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("scrollintoview requires a selector"));
    }
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nel.scrollIntoView({{ block: 'center', inline: 'center' }});\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_drag(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 2 {
        return Err(shim_error("drag requires source and target selectors"));
    }
    let script = js_wrap(&format!(
        "const src = window.__taborAB.resolveSelector({});\nconst dst = window.__taborAB.resolveSelector({});\nif (!src || !dst) return JSON.stringify({{ error: 'element not found' }});\nconst data = new DataTransfer();\nsrc.dispatchEvent(new DragEvent('dragstart', {{ dataTransfer: data, bubbles: true }}));\ndst.dispatchEvent(new DragEvent('dragover', {{ dataTransfer: data, bubbles: true }}));\ndst.dispatchEvent(new DragEvent('drop', {{ dataTransfer: data, bubbles: true }}));\nsrc.dispatchEvent(new DragEvent('dragend', {{ dataTransfer: data, bubbles: true }}));\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0]),
        js_string(&args[1])
    ));
    eval_ok(script, globals)
}

fn cmd_upload(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("upload requires a selector and file paths"));
    }
    let selector = &args[0];
    #[derive(serde::Serialize)]
    struct UploadFile {
        name: String,
        data: String,
        mime: String,
    }
    let mut files = Vec::new();
    for path in &args[1..] {
        let data = fs::read(path).map_err(|_| shim_error(format!("failed to read file {path}")))?;
        let name = Path::new(path)
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| shim_error("invalid file name"))?
            .to_string();
        let mime = "";
        files.push(UploadFile { name, data: BASE64.encode(data), mime: mime.to_string() });
    }
    let payload = serde_json::to_string(&files)?;
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nconst payload = {};\nconst files = payload.map((file) => {{\n  const binary = atob(file.data || '');\n  const bytes = new Uint8Array(binary.length);\n  for (let i = 0; i < binary.length; i += 1) {{ bytes[i] = binary.charCodeAt(i); }}\n  return new File([bytes], file.name || 'file', {{ type: file.mime || '' }});\n}});\nconst dt = new DataTransfer();\nfiles.forEach((f) => dt.items.add(f));\ntry {{ el.files = dt.files; }} catch (e) {{ return JSON.stringify({{ error: 'failed to set files' }}); }}\nel.dispatchEvent(new Event('input', {{ bubbles: true }}));\nel.dispatchEvent(new Event('change', {{ bubbles: true }}));\nreturn JSON.stringify({{ ok: true }});",
        js_string(selector),
        payload
    ));
    eval_ok(script, globals)
}

fn cmd_screenshot(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() > 1 {
        return Err(shim_error("screenshot accepts at most one path"));
    }
    let reply = send_request(IpcRequest::WebSnapshot { tab_id: None, full: globals.full })?;
    let data = match reply {
        Some(SocketReply::WebSnapshot { data }) => BASE64.decode(data)?,
        Some(SocketReply::Error { error }) => return Err(shim_error(error.message)),
        _ => return Err(shim_error("unexpected IPC reply for screenshot")),
    };

    if let Some(path) = args.first() {
        std::fs::write(path, data)?;
    } else {
        let mut stdout = io::stdout();
        stdout.write_all(&data)?;
        stdout.flush()?;
    }
    Ok(())
}

fn cmd_pdf(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let Some(path) = args.first() else {
        return Err(shim_error("pdf requires a path"));
    };
    let reply = send_request(IpcRequest::WebPdf { tab_id: None })?;
    let data = match reply {
        Some(SocketReply::WebPdf { data }) => BASE64.decode(data)?,
        Some(SocketReply::Error { error }) => return Err(shim_error(error.message)),
        _ => return Err(shim_error("unexpected IPC reply for pdf")),
    };
    std::fs::write(path, data)?;
    Ok(())
}

fn cmd_eval(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("eval requires a script"));
    }
    let script = args.join(" ");
    let wrapped = js_wrap(&format!(
        "const result = (function() {{ return eval({}); }})();\nreturn JSON.stringify({{ value: result }});",
        js_string(&script)
    ));
    let value = web_eval_json(wrapped)?;
    if globals.json {
        print_json(value)
    } else {
        if let Some(value) = value.get("value") {
            println!("{}", value_to_string(value));
        }
        Ok(())
    }
}

fn cmd_get(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("get requires a subcommand"));
    }
    let kind = args[0].as_str();
    match kind {
        "title" => {
            let script = js_wrap(
                "const doc = window.__taborAB.doc(); return JSON.stringify({ value: doc.title || '' });",
            );
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        "url" => {
            let script = js_wrap("return JSON.stringify({ value: window.location.href });");
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        "count" => {
            let selector = args.get(1).ok_or_else(|| shim_error("count requires selector"))?;
            let script = js_wrap(&format!(
                "return JSON.stringify({{ value: window.__taborAB.resolveAll({}).length }});",
                js_string(selector)
            ));
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        "text" | "html" | "value" | "box" | "styles" | "attr" => {
            let selector = args.get(1).ok_or_else(|| shim_error("get requires selector"))?;
            let attr = if kind == "attr" {
                args.get(2).ok_or_else(|| shim_error("get attr requires name"))?
            } else {
                ""
            };
            let script = js_wrap(&format!(
                "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nlet value;\nif ('{kind}' === 'text') value = el.innerText || el.textContent || '';\nif ('{kind}' === 'html') value = el.innerHTML || '';\nif ('{kind}' === 'value') value = (el.value !== undefined ? el.value : (el.getAttribute('value') || ''));\nif ('{kind}' === 'attr') value = el.getAttribute({}) || '';\nif ('{kind}' === 'box') {{ const r = el.getBoundingClientRect(); value = {{ x: r.x, y: r.y, width: r.width, height: r.height }}; }}\nif ('{kind}' === 'styles') {{ const style = window.getComputedStyle(el); value = {{ font: style.font, color: style.color, backgroundColor: style.backgroundColor }}; }}\nreturn JSON.stringify({{ value }});",
                js_string(selector),
                js_string(attr)
            ));
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        _ => Err(shim_error("unknown get field")),
    }
}

fn cmd_is(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("is requires a field and selector"));
    }
    let kind = args[0].as_str();
    if !matches!(kind, "visible" | "enabled" | "checked") {
        return Err(shim_error("is requires visible, enabled, or checked"));
    }
    let selector = &args[1];
    let script = js_wrap(&format!(
        "const el = window.__taborAB.resolveSelector({});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nlet value = false;\nif ('{kind}' === 'visible') value = window.__taborAB.isVisible(el);\nif ('{kind}' === 'enabled') value = !el.disabled;\nif ('{kind}' === 'checked') value = !!el.checked;\nreturn JSON.stringify({{ value }});",
        js_string(selector)
    ));
    let value = web_eval_json(script)?;
    print_value(globals, &value)
}

fn cmd_find(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 3 {
        return Err(shim_error("find requires locator, value, and action"));
    }
    let locator = args[0].clone();
    let mut offset = 1;
    let mut value = args[offset].clone();
    let mut options = json!({});

    if locator == "nth" {
        let index = value.parse::<usize>().map_err(|_| shim_error("nth requires index"))?;
        offset += 1;
        value = args.get(offset).ok_or_else(|| shim_error("nth requires selector"))?.clone();
        options["index"] = json!(index);
    }

    offset += 1;
    let action = args.get(offset).ok_or_else(|| shim_error("find requires action"))?.clone();
    let mut action_text: Option<String> = None;
    if let Some(next) = args.get(offset + 1) {
        if !next.starts_with("--") {
            action_text = Some(next.clone());
            offset += 1;
        }
    }

    let mut exact = false;
    let mut name: Option<String> = None;
    let mut i = offset + 1;
    while i < args.len() {
        match args[i].as_str() {
            "--exact" => exact = true,
            "--name" => {
                i += 1;
                name = args.get(i).cloned();
            },
            _ => (),
        }
        i += 1;
    }

    options["exact"] = json!(exact);
    if let Some(name) = name {
        options["name"] = json!(name);
    }

    let script = js_wrap(&format!(
        "const el = window.__taborAB.findElement({}, {}, {});\nif (!el) return JSON.stringify({{ error: 'element not found' }});\nconst action = {};\nif (action === 'click') {{ el.dispatchEvent(new MouseEvent('click', {{ bubbles: true, cancelable: true }})); }}\nelse if (action === 'hover') {{ el.dispatchEvent(new MouseEvent('mouseover', {{ bubbles: true, cancelable: true }})); }}\nelse if (action === 'fill' || action === 'type') {{ const text = {}; if ('value' in el) {{ if (action === 'fill') el.value = ''; el.value = (action === 'fill') ? text : (el.value || '') + text; el.dispatchEvent(new Event('input', {{ bubbles: true }})); }} else if (el.isContentEditable) {{ const doc = window.__taborAB.doc(); if (action === 'fill') el.textContent = ''; doc.execCommand('insertText', false, text); }} }}\nreturn JSON.stringify({{ ok: true }});",
        js_string(&locator),
        js_string(&value),
        serde_json::to_string(&options)?,
        js_string(&action),
        js_string(action_text.as_deref().unwrap_or(""))
    ));
    eval_ok(script, globals)
}

fn cmd_wait(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("wait requires a selector, text, or duration"));
    }

    let mut selector: Option<String> = None;
    let mut text: Option<String> = None;
    let mut url: Option<String> = None;
    let mut fn_expr: Option<String> = None;
    let mut load: Option<String> = None;
    let mut duration_ms: Option<u64> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--text" | "-t" => {
                i += 1;
                text = args.get(i).cloned();
            },
            "--url" | "-u" => {
                i += 1;
                url = args.get(i).cloned();
            },
            "--fn" | "-f" => {
                i += 1;
                fn_expr = args.get(i).cloned();
            },
            "--load" | "-l" => {
                i += 1;
                load = args.get(i).cloned().or_else(|| Some(String::from("load")));
            },
            value => {
                if let Ok(ms) = value.parse::<u64>() {
                    duration_ms = Some(ms);
                } else {
                    selector = Some(value.to_string());
                }
            },
        }
        i += 1;
    }

    if let Some(ms) = duration_ms {
        thread::sleep(Duration::from_millis(ms));
        return Ok(());
    }

    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(shim_error("wait timed out"));
        }

        let ready = if let Some(selector) = &selector {
            let script = js_wrap(&format!(
                "const el = window.__taborAB.resolveSelector({});\nreturn JSON.stringify({{ value: !!el }});",
                js_string(selector)
            ));
            let result = web_eval_json(script)?;
            result.get("value").and_then(|v| v.as_bool()).unwrap_or(false)
        } else if let Some(text) = &text {
            let script = js_wrap(&format!(
                "const doc = window.__taborAB.doc();\nconst body = doc.body ? doc.body.innerText || '' : '';\nreturn JSON.stringify({{ value: body.includes({}) }});",
                js_string(text)
            ));
            let result = web_eval_json(script)?;
            result.get("value").and_then(|v| v.as_bool()).unwrap_or(false)
        } else if let Some(url) = &url {
            let script = js_wrap("return JSON.stringify({ value: window.location.href });");
            let result = web_eval_json(script)?;
            let value = result.get("value").and_then(|v| v.as_str()).unwrap_or("");
            url_matches(url, value)
        } else if let Some(expr) = &fn_expr {
            let script = js_wrap(&format!(
                "const value = (function() {{ return !!({}); }})();\nreturn JSON.stringify({{ value }});",
                expr
            ));
            let result = web_eval_json(script)?;
            result.get("value").and_then(|v| v.as_bool()).unwrap_or(false)
        } else if let Some(load) = &load {
            let mode = load.to_lowercase();
            let script = js_wrap(&format!(
                "const doc = window.__taborAB.doc();\nlet ok = false;\nif ({mode:?} === 'domcontentloaded' || {mode:?} === 'domcontent') {{ ok = doc.readyState === 'interactive' || doc.readyState === 'complete'; }}\nelse if ({mode:?} === 'networkidle' || {mode:?} === 'idle') {{ ok = window.__taborAB.isNetworkIdle(500); }}\nelse {{ ok = doc.readyState === 'complete'; }}\nreturn JSON.stringify({{ value: ok }});"
            ));
            let result = web_eval_json(script)?;
            result.get("value").and_then(|v| v.as_bool()).unwrap_or(false)
        } else {
            return Err(shim_error("wait requires a selector, text, url, fn, load, or duration"));
        };

        if ready {
            return Ok(());
        }

        thread::sleep(Duration::from_millis(200));
    }
}

fn cmd_mouse(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("mouse requires an action"));
    }
    let action = args[0].as_str();
    let script = match action {
        "move" => {
            if args.len() < 3 {
                return Err(shim_error("mouse move requires x y"));
            }
            let x = args[1].parse::<f64>().map_err(|_| shim_error("invalid x"))?;
            let y = args[2].parse::<f64>().map_err(|_| shim_error("invalid y"))?;
            js_wrap(&format!(
                "const doc = window.__taborAB.doc();\nconst el = doc.elementFromPoint({}, {});\nif (el) el.dispatchEvent(new MouseEvent('mousemove', {{ bubbles: true, clientX: {}, clientY: {} }}));\nreturn JSON.stringify({{ ok: true }});",
                x, y, x, y
            ))
        },
        "down" | "up" => {
            let button = args.get(1).map(|v| v.as_str()).unwrap_or("left");
            let button_code = match button {
                "left" => 0,
                "middle" => 1,
                "right" => 2,
                _ => 0,
            };
            let event = if action == "down" { "mousedown" } else { "mouseup" };
            js_wrap(&format!(
                "const doc = window.__taborAB.doc();\ndoc.dispatchEvent(new MouseEvent('{event}', {{ bubbles: true, button: {button_code} }}));\nreturn JSON.stringify({{ ok: true }});"
            ))
        },
        "wheel" => {
            if args.len() < 2 {
                return Err(shim_error("mouse wheel requires delta"));
            }
            let dy = args[1].parse::<f64>().map_err(|_| shim_error("invalid dy"))?;
            let dx = args.get(2).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
            js_wrap(&format!(
                "window.scrollBy({}, {});\nreturn JSON.stringify({{ ok: true }});",
                dx, dy
            ))
        },
        _ => return Err(shim_error("unknown mouse action")),
    };
    eval_ok(script, globals)
}

fn cmd_tab(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() || args[0] == "list" {
        let reply = send_request(IpcRequest::ListTabs)?;
        let groups = match reply {
            Some(SocketReply::TabList { groups }) => groups,
            Some(SocketReply::Error { error }) => return Err(shim_error(error.message)),
            _ => return Err(shim_error("unexpected IPC reply")),
        };
        if globals.json {
            print_json(json!({ "groups": groups }))
        } else {
            for group in groups {
                let name = group.name.as_deref().unwrap_or("unnamed");
                println!("group {} ({})", group.id, name);
                for tab in group.tabs {
                    let active = if tab.is_active { "*" } else { " " };
                    let url = match tab.kind {
                        crate::ipc::IpcTabKind::Web { url } => url,
                        crate::ipc::IpcTabKind::Terminal => String::new(),
                    };
                    if url.is_empty() {
                        println!("{} [{}] {}", active, tab.index, tab.title);
                    } else {
                        println!("{} [{}] {} {}", active, tab.index, tab.title, url);
                    }
                }
            }
            Ok(())
        }
    } else if args[0] == "new" {
        let url = args.get(1).map(|_| args[1..].join(" "));
        let mut options = WindowOptions::default();
        options.window_kind = match url {
            Some(url) => WindowKind::Web { url: normalize_web_url(&url) },
            None => WindowKind::Terminal,
        };
        let reply =
            send_request(IpcRequest::CreateTab { options, group_id: None, group_name: None })?;
        expect_ok(reply)
    } else if args[0] == "close" {
        if let Some(index) = args.get(1) {
            let index = index.parse::<usize>().map_err(|_| shim_error("invalid tab index"))?;
            let reply =
                send_request(IpcRequest::SelectTab { selection: TabSelection::ByIndex { index } })?;
            expect_ok(reply)?;
            let reply = send_request(IpcRequest::CloseTab { tab_id: None })?;
            expect_ok(reply)
        } else {
            let reply = send_request(IpcRequest::CloseTab { tab_id: None })?;
            expect_ok(reply)
        }
    } else {
        let index = args[0].parse::<usize>().map_err(|_| shim_error("invalid tab index"))?;
        let reply =
            send_request(IpcRequest::SelectTab { selection: TabSelection::ByIndex { index } })?;
        expect_ok(reply)
    }
}

fn cmd_window(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 || args[0] != "new" {
        return Err(shim_error("window only supports 'new'"));
    }
    let mut options = WindowOptions::default();
    options.window_kind = WindowKind::Web { url: String::from("about:blank") };
    let reply = send_request(IpcRequest::CreateTab { options, group_id: None, group_name: None })?;
    expect_ok(reply)
}

fn cmd_cookies(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let action = args.first().map(|v| v.as_str()).unwrap_or("get");
    match action {
        "get" => {
            let script = js_wrap("return JSON.stringify({ value: document.cookie || '' });");
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        "set" => {
            if args.len() < 3 {
                return Err(shim_error("cookies set requires name and value"));
            }
            let cookie = format!("{}={}", args[1], args[2..].join(" "));
            let script = js_wrap(&format!(
                "document.cookie = {};\nreturn JSON.stringify({{ ok: true }});",
                js_string(&cookie)
            ));
            eval_ok(script, globals)
        },
        "clear" => {
            let script = js_wrap(
                "document.cookie.split(';').forEach((c) => { const name = c.split('=')[0].trim(); document.cookie = name + '=; Max-Age=0; path=/'; });\nreturn JSON.stringify({ ok: true });",
            );
            eval_ok(script, globals)
        },
        _ => Err(shim_error("unknown cookies action")),
    }
}

fn cmd_storage(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("storage requires local or session"));
    }
    let store = match args[0].as_str() {
        "local" => "localStorage",
        "session" => "sessionStorage",
        _ => return Err(shim_error("storage requires local or session")),
    };
    let action = args.get(1).map(|v| v.as_str()).unwrap_or("get");
    match action {
        "get" => {
            let script = js_wrap(&format!(
                "const out = {{}}; for (let i = 0; i < {store}.length; i++) {{ const key = {store}.key(i); out[key] = {store}.getItem(key); }} return JSON.stringify({{ value: out }});"
            ));
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
        "set" => {
            if args.len() < 4 {
                return Err(shim_error("storage set requires key and value"));
            }
            let key = &args[2];
            let val = &args[3];
            let script = js_wrap(&format!(
                "{store}.setItem({}, {}); return JSON.stringify({{ ok: true }});",
                js_string(key),
                js_string(val)
            ));
            eval_ok(script, globals)
        },
        "clear" => {
            let script =
                js_wrap(&format!("{store}.clear(); return JSON.stringify({{ ok: true }});"));
            eval_ok(script, globals)
        },
        key => {
            let script = js_wrap(&format!(
                "return JSON.stringify({{ value: {store}.getItem({}) }});",
                js_string(key)
            ));
            let value = web_eval_json(script)?;
            print_value(globals, &value)
        },
    }
}

fn cmd_session(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.first().map(|v| v.as_str()) == Some("list") {
        if globals.json {
            print_json(json!({ "sessions": ["default"] }))
        } else {
            println!("default");
            Ok(())
        }
    } else if globals.json {
        print_json(
            json!({ "session": globals.session.clone().unwrap_or_else(|| "default".to_string()) }),
        )
    } else {
        println!("{}", globals.session.clone().unwrap_or_else(|| "default".to_string()));
        Ok(())
    }
}

fn cmd_network(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let Some(action) = args.first().map(|v| v.as_str()) else {
        return Err(shim_error("network requires an action"));
    };

    match action {
        "requests" => {
            let mut filter: Option<String> = None;
            let mut clear = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--filter" => {
                        i += 1;
                        filter = args.get(i).cloned();
                    },
                    "--clear" => clear = true,
                    _ => (),
                }
                i += 1;
            }

            let action = if clear {
                WebNetworkAction::Clear
            } else {
                WebNetworkAction::List { filter: filter.clone() }
            };

            let reply = send_request(IpcRequest::WebNetwork { tab_id: None, action })?;
            let mut entries = match reply {
                Some(SocketReply::WebNetwork { entries }) => entries,
                Some(SocketReply::Error { error }) => {
                    if matches!(
                        error.code,
                        IpcErrorCode::Timeout
                            | IpcErrorCode::PermissionDenied
                            | IpcErrorCode::Unsupported
                            | IpcErrorCode::Internal
                    ) {
                        network_fallback(globals, clear, filter.clone())?
                    } else {
                        return Err(shim_error(error.message));
                    }
                },
                _ => return Err(shim_error("unexpected IPC reply")),
            };
            if !clear {
                if let Ok(mut fallback_entries) = network_fallback(globals, false, filter.clone()) {
                    let mut seen: HashSet<String> =
                        entries.iter().map(|entry| entry.request_id.clone()).collect();
                    fallback_entries.retain(|entry| seen.insert(entry.request_id.clone()));
                    entries.extend(fallback_entries);
                }
            }

            if globals.json {
                print_json(json!({ "requests": entries }))
            } else {
                for entry in entries {
                    let status =
                        entry.status.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
                    let method = entry.method.unwrap_or_else(|| "-".to_string());
                    println!("{status} {method} {}", entry.url);
                }
                Ok(())
            }
        },
        "route" => {
            let pattern = args.get(1).ok_or_else(|| shim_error("route requires a url pattern"))?;
            let mut action = "continue".to_string();
            let mut body: Option<String> = None;
            let mut status: Option<u16> = None;
            let mut content_type: Option<String> = None;
            let mut headers: Option<Value> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--abort" => action = "abort".to_string(),
                    "--body" => {
                        i += 1;
                        body = args.get(i).cloned();
                        action = "fulfill".to_string();
                    },
                    "--status" => {
                        i += 1;
                        status = args.get(i).and_then(|v| v.parse::<u16>().ok());
                    },
                    "--content-type" => {
                        i += 1;
                        content_type = args.get(i).cloned();
                    },
                    "--headers" => {
                        i += 1;
                        if let Some(value) = args.get(i) {
                            headers = serde_json::from_str::<Value>(value).ok();
                        }
                    },
                    _ => (),
                }
                i += 1;
            }

            let route = json!({
                "action": action,
                "body": body,
                "status": status,
                "contentType": content_type,
                "headers": headers,
            });
            let script = js_wrap(&format!(
                "const ok = window.__taborAB.addRoute({}, {});\nif (!ok) return JSON.stringify({{ error: 'failed to add route' }});\nreturn JSON.stringify({{ ok: true }});",
                js_string(pattern),
                route
            ));
            eval_ok(script, globals)
        },
        "unroute" => {
            let pattern = args.get(1).map(|v| v.as_str()).unwrap_or("");
            let script = js_wrap(&format!(
                "const ok = window.__taborAB.removeRoute({});\nif (!ok) return JSON.stringify({{ error: 'failed to remove route' }});\nreturn JSON.stringify({{ ok: true }});",
                js_string(pattern)
            ));
            eval_ok(script, globals)
        },
        _ => Err(shim_error("unknown network action")),
    }
}

fn cmd_state(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("state requires save or load"));
    }
    match args[0].as_str() {
        "save" => {
            let path = args.get(1).ok_or_else(|| shim_error("state save requires a path"))?;
            let script = js_wrap(
                "const parseCookies = () => {\n  const out = [];\n  const raw = document.cookie || '';\n  raw.split(';').map((c) => c.trim()).filter(Boolean).forEach((pair) => {\n    const idx = pair.indexOf('=');\n    if (idx === -1) return;\n    out.push({ name: pair.slice(0, idx), value: pair.slice(idx + 1) });\n  });\n  return out;\n};\nconst store = (storage) => {\n  const out = {};\n  for (let i = 0; i < storage.length; i += 1) {\n    const key = storage.key(i);\n    out[key] = storage.getItem(key);\n  }\n  return out;\n};\nconst state = {\n  url: window.location.href,\n  cookies: parseCookies(),\n  localStorage: store(window.localStorage),\n  sessionStorage: store(window.sessionStorage),\n};\nreturn JSON.stringify({ state });",
            );
            let value = web_eval_json(script)?;
            let state = value.get("state").cloned().unwrap_or_else(|| json!({}));
            fs::write(path, serde_json::to_string_pretty(&state)?)?;
            Ok(())
        },
        "load" => {
            let path = args.get(1).ok_or_else(|| shim_error("state load requires a path"))?;
            let contents = fs::read_to_string(path)?;
            let state: Value = serde_json::from_str(&contents)?;
            let script = js_wrap(&format!(
                "const state = {};\nif (state.cookies) {{\n  state.cookies.forEach((cookie) => {{\n    if (!cookie || !cookie.name) return;\n    const value = cookie.value == null ? '' : cookie.value;\n    document.cookie = `${{cookie.name}}=${{value}}; path=/`;\n  }});\n}}\nif (state.localStorage) {{\n  Object.entries(state.localStorage).forEach(([key, value]) => {{\n    window.localStorage.setItem(key, value == null ? '' : value);\n  }});\n}}\nif (state.sessionStorage) {{\n  Object.entries(state.sessionStorage).forEach(([key, value]) => {{\n    window.sessionStorage.setItem(key, value == null ? '' : value);\n  }});\n}}\nreturn JSON.stringify({{ ok: true }});",
                state
            ));
            eval_ok(script, _globals)
        },
        _ => Err(shim_error("state requires save or load")),
    }
}

fn cmd_connect(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("connect requires a port"));
    }
    let _port = args[0].parse::<u16>().map_err(|_| shim_error("invalid port"))?;
    Ok(())
}

fn cmd_set(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("set requires a setting"));
    }
    match args[0].as_str() {
        "viewport" => {
            if args.len() < 3 {
                return Err(shim_error("set viewport requires width and height"));
            }
            let width = args[1].parse::<i32>().map_err(|_| shim_error("invalid width"))?;
            let height = args[2].parse::<i32>().map_err(|_| shim_error("invalid height"))?;
            let script = js_wrap(&format!(
                "const ok = window.__taborAB.setViewport({}, {});\nif (!ok) return JSON.stringify({{ error: 'invalid viewport' }});\nreturn JSON.stringify({{ ok: true }});",
                width, height
            ));
            eval_ok(script, _globals)
        },
        "device" => {
            if args.len() < 2 {
                return Err(shim_error("set device requires a name"));
            }
            let name = args[1..].join(" ");
            let script = js_wrap(&format!(
                "const ok = window.__taborAB.setDevice({});\nif (!ok) return JSON.stringify({{ error: 'unknown device' }});\nreturn JSON.stringify({{ ok: true }});",
                js_string(&name)
            ));
            eval_ok(script, _globals)
        },
        "geo" | "geolocation" => {
            if args.len() < 3 {
                return Err(shim_error("set geo requires latitude and longitude"));
            }
            let lat = args[1].parse::<f64>().map_err(|_| shim_error("invalid latitude"))?;
            let lng = args[2].parse::<f64>().map_err(|_| shim_error("invalid longitude"))?;
            let script = js_wrap(&format!(
                "const ok = window.__taborAB.setGeo({}, {});\nif (!ok) return JSON.stringify({{ error: 'invalid geolocation' }});\nreturn JSON.stringify({{ ok: true }});",
                lat, lng
            ));
            eval_ok(script, _globals)
        },
        "offline" => {
            let mode = args.get(1).map(|v| v.as_str()).unwrap_or("on");
            let offline = matches!(mode, "on" | "true" | "1");
            let script = js_wrap(&format!(
                "window.__taborAB.setOffline({});\nreturn JSON.stringify({{ ok: true }});",
                if offline { "true" } else { "false" }
            ));
            eval_ok(script, _globals)
        },
        "headers" => {
            if args.len() < 2 {
                return Err(shim_error("set headers requires JSON"));
            }
            let raw = args[1..].join(" ");
            let headers: Value =
                serde_json::from_str(&raw).map_err(|_| shim_error("invalid headers JSON"))?;
            if !headers.is_object() {
                return Err(shim_error("headers must be a JSON object"));
            }
            let script = js_wrap(&format!(
                "window.__taborAB.setHeaders({});\nreturn JSON.stringify({{ ok: true }});",
                headers
            ));
            eval_ok(script, _globals)
        },
        "credentials" | "auth" => {
            if args.len() < 3 {
                return Err(shim_error("set credentials requires user and pass"));
            }
            let user = &args[1];
            let pass = &args[2];
            let script = js_wrap(&format!(
                "window.__taborAB.setAuth({}, {});\nreturn JSON.stringify({{ ok: true }});",
                js_string(user),
                js_string(pass)
            ));
            eval_ok(script, _globals)
        },
        "media" => {
            if args.len() < 2 {
                return Err(shim_error("set media requires a mode"));
            }
            let mut scheme: Option<String> = None;
            let mut reduced = false;
            for value in args.iter().skip(1) {
                match value.to_lowercase().as_str() {
                    "dark" | "light" => scheme = Some(value.to_lowercase()),
                    "reduced-motion" | "reduced" => reduced = true,
                    _ => (),
                }
            }
            let script = js_wrap(&format!(
                "window.__taborAB.setMedia({}, {});\nreturn JSON.stringify({{ ok: true }});",
                scheme.as_deref().map(js_string).unwrap_or_else(|| "null".to_string()),
                if reduced { "true" } else { "false" }
            ));
            eval_ok(script, _globals)
        },
        _ => Err(shim_error("unknown set option")),
    }
}

fn cmd_console(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let clear = args.iter().any(|arg| arg == "--clear");
    let script = js_wrap(&format!(
        "const items = window.__taborAB.getConsole({});\nreturn JSON.stringify({{ items }});",
        if clear { "true" } else { "false" }
    ));
    let value = web_eval_json(script)?;
    if globals.json {
        print_json(value)
    } else {
        let items = value.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for item in items {
            let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("log");
            let args = item
                .get("args")
                .and_then(|v| v.as_array())
                .map(|vals| {
                    vals.iter()
                        .map(|v| {
                            v.as_str().map(|s| s.to_string()).unwrap_or_else(|| value_to_string(v))
                        })
                        .collect::<Vec<String>>()
                        .join(" ")
                })
                .unwrap_or_default();
            println!("[{}] {}", kind, args);
        }
        Ok(())
    }
}

fn cmd_errors(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let clear = args.iter().any(|arg| arg == "--clear");
    let script = js_wrap(&format!(
        "const items = window.__taborAB.getErrors({});\nreturn JSON.stringify({{ items }});",
        if clear { "true" } else { "false" }
    ));
    let value = web_eval_json(script)?;
    if globals.json {
        print_json(value)
    } else {
        let items = value.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        for item in items {
            let message = item.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let filename = item.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            let lineno = item.get("lineno").and_then(|v| v.as_u64()).unwrap_or(0);
            let colno = item.get("colno").and_then(|v| v.as_u64()).unwrap_or(0);
            if filename.is_empty() {
                println!("{message}");
            } else {
                println!("{message} ({filename}:{lineno}:{colno})");
            }
        }
        Ok(())
    }
}

fn cmd_highlight(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("highlight requires a selector"));
    }
    let script = js_wrap(&format!(
        "const ok = window.__taborAB.highlight({});\nif (!ok) return JSON.stringify({{ error: 'element not found' }});\nreturn JSON.stringify({{ ok: true }});",
        js_string(&args[0])
    ));
    eval_ok(script, globals)
}

fn cmd_frame(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 1 {
        return Err(shim_error("frame requires a selector or main"));
    }
    let selector = &args[0];
    let script = js_wrap(&format!(
        "const ok = window.__taborAB.setFrame({});\nif (!ok) return JSON.stringify({{ error: 'frame not found' }});\nreturn JSON.stringify({{ ok: true }});",
        js_string(selector)
    ));
    eval_ok(script, globals)
}

fn cmd_dialog(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("dialog requires accept or dismiss"));
    }
    match args[0].as_str() {
        "accept" => {
            let text = if args.len() > 1 { Some(args[1..].join(" ")) } else { None };
            let script = js_wrap(&format!(
                "window.__taborAB.setDialogResponse(true, {});\nreturn JSON.stringify({{ ok: true }});",
                text.as_deref().map(js_string).unwrap_or_else(|| "null".to_string())
            ));
            eval_ok(script, globals)
        },
        "dismiss" => {
            let script = js_wrap(
                "window.__taborAB.setDialogResponse(false, null);\nreturn JSON.stringify({ ok: true });",
            );
            eval_ok(script, globals)
        },
        _ => Err(shim_error("dialog requires accept or dismiss")),
    }
}

fn cmd_trace(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("trace requires start or stop"));
    }
    let session = session_name(globals);
    let state_path = trace_state_path(&session);
    match args[0].as_str() {
        "start" => {
            let script = js_wrap(
                "window.__taborAB.getConsole(true);\nwindow.__taborAB.getErrors(true);\nwindow.__taborAB.getNetworkEntries(null, true);\nreturn JSON.stringify({ ok: true });",
            );
            eval_ok(script, globals)?;
            let started_at = now_timestamp_ms();
            let state = json!({ "startedAt": started_at });
            fs::write(&state_path, serde_json::to_string(&state)?)?;
            Ok(())
        },
        "stop" => {
            let path = args.get(1).ok_or_else(|| shim_error("trace stop requires a path"))?;
            let started_at = fs::read_to_string(&state_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|value| value.get("startedAt").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let script = js_wrap(
                "const console = window.__taborAB.getConsole(false);\nconst errors = window.__taborAB.getErrors(false);\nconst network = window.__taborAB.getNetworkEntries(null, false);\nconst url = window.location.href;\nreturn JSON.stringify({ console, errors, network, url });",
            );
            let value = web_eval_json(script)?;
            let trace = json!({
                "startedAt": started_at,
                "endedAt": now_timestamp_ms(),
                "url": value.get("url").cloned().unwrap_or_else(|| json!("")),
                "console": value.get("console").cloned().unwrap_or_else(|| json!([])),
                "errors": value.get("errors").cloned().unwrap_or_else(|| json!([])),
                "network": value.get("network").cloned().unwrap_or_else(|| json!([])),
            });
            fs::write(path, serde_json::to_string_pretty(&trace)?)?;
            let _ = fs::remove_file(&state_path);
            Ok(())
        },
        _ => Err(shim_error("trace requires start or stop")),
    }
}

fn cmd_record(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err(shim_error("record requires start, stop, or restart"));
    }
    match args[0].as_str() {
        "start" => record_start(globals, &args[1..]),
        "restart" => {
            let _ = record_stop(globals);
            record_start(globals, &args[1..])
        },
        "stop" => record_stop(globals),
        _ => Err(shim_error("record requires start, stop, or restart")),
    }
}

fn cmd_record_worker(_globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(shim_error("record worker requires session and output path"));
    }
    let session = &args[0];
    let output = &args[1];
    let mut fps = 5.0;
    let mut full = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--fps" => {
                i += 1;
                if let Some(value) = args.get(i) {
                    fps = value.parse::<f64>().unwrap_or(5.0);
                }
            },
            "--full" => full = true,
            _ => (),
        }
        i += 1;
    }

    let (_, stop_path, done_path) = record_state_paths(session);
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("image2pipe")
        .arg("-r")
        .arg(format!("{fps}"))
        .arg("-i")
        .arg("-")
        .arg("-c:v")
        .arg("libvpx")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ffmpeg.spawn()?;
    let mut stdin = child.stdin.take().ok_or_else(|| shim_error("ffmpeg stdin unavailable"))?;

    let interval = if fps > 0.0 {
        Duration::from_millis((1000.0 / fps) as u64)
    } else {
        Duration::from_millis(200)
    };
    loop {
        if stop_path.exists() {
            break;
        }
        let reply = match send_request(IpcRequest::WebSnapshot { tab_id: None, full }) {
            Ok(reply) => reply,
            Err(_) => break,
        };
        let data = match reply {
            Some(SocketReply::WebSnapshot { data }) => BASE64.decode(data)?,
            Some(SocketReply::Error { .. }) => break,
            _ => break,
        };
        if stdin.write_all(&data).is_err() {
            break;
        }
        thread::sleep(interval);
    }

    let _ = stdin.flush();
    drop(stdin);
    let _ = child.wait();
    let _ = fs::write(&done_path, "done");
    Ok(())
}

fn record_start(globals: &GlobalOptions, args: &[String]) -> Result<(), Box<dyn Error>> {
    let path = args.first().ok_or_else(|| shim_error("record start requires a path"))?;
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if !command_exists("ffmpeg") {
        return Err(shim_error("ffmpeg is required for recording"));
    }
    if args.len() > 1 {
        let url = args[1..].join(" ");
        cmd_open(globals, &[url])?;
    }

    let session = session_name(globals);
    let (state_path, stop_path, done_path) = record_state_paths(&session);
    if state_path.exists() {
        return Err(shim_error("recording already in progress"));
    }
    let _ = fs::remove_file(&stop_path);
    let _ = fs::remove_file(&done_path);

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("agent-browser").arg("__record-worker").arg(&session).arg(path);
    if globals.full {
        cmd.arg("--full");
    }
    if let Ok(socket) = std::env::var("TABOR_SOCKET") {
        cmd.env("TABOR_SOCKET", socket);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn()?;

    let state = json!({ "pid": child.id(), "path": path });
    fs::write(&state_path, serde_json::to_string(&state)?)?;
    Ok(())
}

fn record_stop(globals: &GlobalOptions) -> Result<(), Box<dyn Error>> {
    let session = session_name(globals);
    let (state_path, stop_path, done_path) = record_state_paths(&session);
    if !state_path.exists() {
        return Err(shim_error("no recording in progress"));
    }
    fs::write(&stop_path, "stop")?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if done_path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(&stop_path);
    let _ = fs::remove_file(&done_path);
    let _ = fs::remove_file(&state_path);
    Ok(())
}

fn now_timestamp_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn session_name(globals: &GlobalOptions) -> String {
    globals.session.clone().unwrap_or_else(|| String::from("default"))
}

fn record_state_paths(session: &str) -> (PathBuf, PathBuf, PathBuf) {
    let mut base = std::env::temp_dir();
    base.push(format!("tabor-agent-browser-record-{session}"));
    let state = base.with_extension("json");
    let stop = base.with_extension("stop");
    let done = base.with_extension("done");
    (state, stop, done)
}

fn trace_state_path(session: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("tabor-agent-browser-trace-{session}.json"));
    path
}

fn network_fallback(
    _globals: &GlobalOptions,
    clear: bool,
    filter: Option<String>,
) -> Result<Vec<WebNetworkEntry>, Box<dyn Error>> {
    let script = js_wrap(&format!(
        "const entries = window.__taborAB.getNetworkEntries({}, {});\nreturn JSON.stringify({{ entries }});",
        match filter.as_deref() {
            Some(value) => js_string(value),
            None => String::from("null"),
        },
        if clear { "true" } else { "false" }
    ));
    let value = web_eval_json(script)?;
    if clear {
        return Ok(Vec::new());
    }
    let entries_value = value.get("entries").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    let mut entries: Vec<WebNetworkEntry> = serde_json::from_value(entries_value)?;
    if let Some(filter) = filter {
        entries.retain(|entry| entry.url.contains(&filter));
    }
    Ok(entries)
}

fn cmd_simple_js(globals: &GlobalOptions, script: &str) -> Result<(), Box<dyn Error>> {
    let script = js_wrap(&format!("{script}\nreturn JSON.stringify({{ ok: true }});"));
    eval_ok(script, globals)
}

fn eval_ok(script: String, _globals: &GlobalOptions) -> Result<(), Box<dyn Error>> {
    let value = web_eval_json(script)?;
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(shim_error(error));
    }
    Ok(())
}

fn web_eval_json(script: String) -> Result<Value, Box<dyn Error>> {
    let reply = send_request(IpcRequest::WebEval { tab_id: None, script })?;
    match reply {
        Some(SocketReply::WebEval { result }) => {
            let Some(result) = result else {
                return Err(shim_error("web eval returned no result"));
            };
            let value: Value =
                serde_json::from_str(&result).map_err(|_| shim_error("invalid JS result"))?;
            if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
                return Err(shim_error(error));
            }
            Ok(value)
        },
        Some(SocketReply::Error { error }) => Err(shim_error(error.message)),
        _ => Err(shim_error("unexpected IPC reply")),
    }
}

fn send_request(request: IpcRequest) -> Result<Option<SocketReply>, Box<dyn Error>> {
    Ok(ipc::send_message(None, request)?)
}

fn expect_ok(reply: Option<SocketReply>) -> Result<(), Box<dyn Error>> {
    if let Some(SocketReply::Error { error }) = reply {
        return Err(shim_error(error.message));
    }
    Ok(())
}

fn print_help() {
    println!("{HELP_TEXT}");
}

fn print_json(value: Value) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

fn print_value(globals: &GlobalOptions, value: &Value) -> Result<(), Box<dyn Error>> {
    if globals.json {
        print_json(value.clone())
    } else if let Some(value) = value.get("value") {
        println!("{}", value_to_string(value));
        Ok(())
    } else {
        Ok(())
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

fn js_wrap(body: &str) -> String {
    format!(
        "{JS_HELPER}\n(() => {{ try {{ {body} }} catch (e) {{ return JSON.stringify({{ error: String(e) }}); }} }})()"
    )
}

fn url_matches(pattern: &str, url: &str) -> bool {
    if pattern.contains('*') {
        let mut parts = pattern.split('*');
        if let Some(first) = parts.next() {
            if !url.starts_with(first) {
                return false;
            }
            let mut remaining = url[first.len()..].to_string();
            for part in parts {
                if let Some(index) = remaining.find(part) {
                    remaining = remaining[index + part.len()..].to_string();
                } else {
                    return false;
                }
            }
            return true;
        }
    }
    url == pattern
}

#[derive(serde::Serialize)]
struct KeyInfo {
    key: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

fn parse_key(input: &str) -> KeyInfo {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut key = String::new();

    for part in input.split('+') {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" => meta = true,
            other => key = other.to_string(),
        }
    }

    if key.is_empty() {
        key = input.to_string();
    }

    KeyInfo { key, ctrl, alt, shift, meta }
}

fn shim_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}
