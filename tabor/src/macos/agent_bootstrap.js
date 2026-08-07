(() => {
  const VERSION = __TABOR_AGENT_RUNTIME_VERSION__;
  if (window.__taborAgent && window.__taborAgent.version === VERSION) {
    return;
  }

  const state = window.__taborAgentState || (window.__taborAgentState = {
    nextId: 1,
    revision: 1,
    inflight: 0,
    observersInstalled: false,
    networkInstalled: false,
    dialogsInstalled: false,
    nextDialogDecision: null
  });

  const bump = () => { state.revision += 1; };
  const doc = () => document;
  const round = (value) => Math.round(Number(value) || 0);

  if (!state.observersInstalled) {
    const observer = new MutationObserver(() => { bump(); });
    observer.observe(document, {
      subtree: true,
      childList: true,
      attributes: true,
      characterData: true
    });
    window.addEventListener("hashchange", bump);
    window.addEventListener("popstate", bump);
    state.observersInstalled = true;
  }

  if (!state.networkInstalled) {
    const finish = () => {
      state.inflight = Math.max(0, state.inflight - 1);
      bump();
    };
    if (typeof window.fetch === "function") {
      const originalFetch = window.fetch.bind(window);
      window.fetch = (...args) => {
        state.inflight += 1;
        bump();
        try {
          return Promise.resolve(originalFetch(...args)).finally(finish);
        } catch (error) {
          finish();
          throw error;
        }
      };
    }
    const originalSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.send = function(...args) {
      state.inflight += 1;
      bump();
      this.addEventListener("loadend", finish, { once: true });
      return originalSend.apply(this, args);
    };
    state.networkInstalled = true;
  }

  if (!state.dialogsInstalled) {
    const consumeDialogDecision = () => {
      const decision = state.nextDialogDecision || { accept: false, text: null };
      state.nextDialogDecision = null;
      bump();
      return decision;
    };

    window.alert = (message) => {
      state.lastDialog = {
        kind: "alert",
        message: String(message ?? ""),
        default_prompt_text: null
      };
      consumeDialogDecision();
    };

    window.confirm = (message) => {
      state.lastDialog = {
        kind: "confirm",
        message: String(message ?? ""),
        default_prompt_text: null
      };
      return !!consumeDialogDecision().accept;
    };

    window.prompt = (message, defaultText = "") => {
      state.lastDialog = {
        kind: "prompt",
        message: String(message ?? ""),
        default_prompt_text: defaultText == null ? null : String(defaultText)
      };
      const decision = consumeDialogDecision();
      if (!decision.accept) return null;
      if (decision.text != null) return String(decision.text);
      return defaultText == null ? "" : String(defaultText);
    };

    state.dialogsInstalled = true;
  }

  const visible = (el) => {
    if (!el) return false;
    const rect = el.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0) return false;
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    return rect.bottom >= 0 && rect.right >= 0 &&
      rect.top <= window.innerHeight && rect.left <= window.innerWidth;
  };

  const editable = (el) => {
    if (!el) return false;
    return !!el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName);
  };

  const interactive = (el) => {
    if (!visible(el)) return false;
    if (editable(el)) return true;
    if (el.tagName === "A" && el.href) return true;
    if (["BUTTON", "SUMMARY"].includes(el.tagName)) return true;
    if (el.onclick) return true;
    if (el.getAttribute("role")) return true;
    if (el.tabIndex >= 0) return true;
    return false;
  };

  const role = (el) => {
    return (
      el.getAttribute("role") ||
      (el.tagName || "").toLowerCase()
    );
  };

  const compactText = (value) => {
    if (value == null) return null;
    const text = String(value).replace(/\s+/g, " ").trim();
    if (!text) return null;
    return text.length > 96 ? text.slice(0, 96) : text;
  };

  const name = (el) => {
    return compactText(
      el.getAttribute("aria-label") ||
      el.getAttribute("placeholder") ||
      el.getAttribute("title") ||
      el.innerText ||
      el.textContent ||
      el.id ||
      el.value ||
      ""
    ) || "";
  };

  const value = (el) => {
    if (!editable(el)) return null;
    if ("value" in el) return compactText(el.value || "");
    return compactText(el.textContent || "");
  };

  const checked = (el) => {
    if ("checked" in el) return !!el.checked;
    return null;
  };

  const placeholder = (el) => compactText(el.getAttribute("placeholder") || "");

  const inputType = (el) => {
    if (!el || !el.tagName) return null;
    if (el.tagName === "INPUT") return String(el.getAttribute("type") || "text").toLowerCase();
    if (el.tagName === "TEXTAREA") return "textarea";
    if (el.tagName === "SELECT") return "select";
    if (el.isContentEditable) return "contenteditable";
    return null;
  };

  const bbox = (el) => {
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    if (!rect) return null;
    return {
      x: round(rect.left),
      y: round(rect.top),
      width: round(rect.width),
      height: round(rect.height)
    };
  };

  const center = (el) => {
    const rect = bbox(el);
    if (!rect) return null;
    return {
      x: rect.x + Math.floor(rect.width / 2),
      y: rect.y + Math.floor(rect.height / 2)
    };
  };

  const optionNames = (el) => {
    if (!el || el.tagName !== "SELECT" || !el.options) return null;
    const values = Array.from(el.options)
      .map((option) => compactText(option.label || option.textContent || option.value || ""))
      .filter(Boolean);
    return values.length ? values : null;
  };

  const idFor = (el) => {
    let id = el.getAttribute("data-tabor-agent-id");
    if (!id) {
      id = state.nextId.toString(36);
      state.nextId += 1;
      el.setAttribute("data-tabor-agent-id", id);
    }
    return id;
  };

  const resolve = (id) => {
    return Array.from(doc().querySelectorAll("[data-tabor-agent-id]"))
      .find((el) => el.getAttribute("data-tabor-agent-id") === id) || null;
  };

  const elements = () => {
    const out = [];
    const seen = new Set();
    const selector = [
      "a[href]",
      "button",
      "input",
      "textarea",
      "select",
      "summary",
      "[role]",
      "[contenteditable=\"true\"]",
      "[tabindex]"
    ].join(",");
    for (const el of Array.from(doc().querySelectorAll(selector))) {
      if (!interactive(el)) continue;
      const id = idFor(el);
      if (seen.has(id)) continue;
      seen.add(id);
      out.push({
        id,
        role: role(el),
        name: name(el),
        value: value(el),
        editable: editable(el),
        disabled: !!el.disabled,
        checked: checked(el)
      });
    }
    return out;
  };

  const observe = () => ({
    revision: state.revision,
    url: window.location.href,
    title: doc().title || "",
    ready_state: doc().readyState || "",
    pending_requests: state.inflight,
    elements: elements()
  });

  const inspect = (id) => {
    const el = resolve(id);
    if (!el) return { error: "element not found" };
    return {
      id,
      role: role(el),
      name: name(el),
      value: value(el),
      text: compactText(el.innerText || el.textContent || ""),
      href: el.getAttribute("href") || el.href || null,
      placeholder: placeholder(el),
      input_type: inputType(el),
      bbox: bbox(el),
      center: center(el),
      editable: editable(el),
      disabled: !!el.disabled,
      checked: checked(el),
      options: optionNames(el)
    };
  };

  const dispatchKeyboard = (type, key, modifiers) => {
    const target = doc().activeElement || doc().body;
    const init = {
      key,
      ctrlKey: !!modifiers.control,
      altKey: !!modifiers.alt,
      shiftKey: !!modifiers.shift,
      metaKey: !!modifiers.super_key,
      bubbles: true,
      cancelable: true
    };
    target.dispatchEvent(new KeyboardEvent(type, init));
  };

  const dispatchKey = (key, modifiers) => {
    dispatchKeyboard("keydown", key, modifiers);
    dispatchKeyboard("keyup", key, modifiers);
  };

  const fill = (el, text) => {
    el.focus();
    if ("value" in el) {
      el.value = text;
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
      return;
    }
    if (el.isContentEditable) {
      el.textContent = text;
      el.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
      return;
    }
    throw new Error("element is not editable");
  };

  const typeText = (text) => {
    const target = doc().activeElement || doc().body;
    if (!target) return;
    if ("value" in target) {
      const current = String(target.value || "");
      const next = `${current}${text}`;
      target.value = next;
      target.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    if (target.isContentEditable) {
      target.textContent = `${target.textContent || ""}${text}`;
      target.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
      return;
    }
    target.dispatchEvent(new InputEvent("input", { bubbles: true, data: text }));
  };

  const buttonCode = (button) => {
    const name = String(button || "left").toLowerCase();
    if (name === "right") return 2;
    if (name === "middle") return 1;
    return 0;
  };

  const targetAt = (x, y) => doc().elementFromPoint(x, y) || doc().body;

  const mouseInit = (x, y, button, clickCount = 1) => ({
    clientX: x,
    clientY: y,
    button: buttonCode(button),
    buttons: 1,
    detail: clickCount,
    bubbles: true,
    cancelable: true
  });

  const hoverAt = (x, y) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mouseover", mouseInit(x, y, "left")));
    target.dispatchEvent(new MouseEvent("mousemove", mouseInit(x, y, "left")));
  };

  const hoverElement = (el) => {
    const point = center(el);
    if (!point) throw new Error("element has no bounding box");
    el.dispatchEvent(new MouseEvent("mouseover", mouseInit(point.x, point.y, "left")));
    el.dispatchEvent(new MouseEvent("mousemove", mouseInit(point.x, point.y, "left")));
    return point;
  };

  const mouseDownAt = (x, y, button) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mousedown", mouseInit(x, y, button)));
  };

  const mouseUpAt = (x, y, button) => {
    const target = targetAt(x, y);
    target.dispatchEvent(new MouseEvent("mouseup", mouseInit(x, y, button)));
  };

  const clickAt = (x, y, button, clickCount = 1) => {
    const target = targetAt(x, y);
    hoverAt(x, y);
    target.dispatchEvent(new MouseEvent("mousedown", mouseInit(x, y, button, clickCount)));
    target.dispatchEvent(new MouseEvent("mouseup", mouseInit(x, y, button, clickCount)));
    target.dispatchEvent(new MouseEvent("click", mouseInit(x, y, button, clickCount)));
    if (clickCount >= 2) {
      target.dispatchEvent(new MouseEvent("dblclick", mouseInit(x, y, button, clickCount)));
    }
    if (buttonCode(button) === 0 && typeof target.click === "function") {
      target.click();
    }
  };

  const clickElement = (el, button, clickCount = 1) => {
    const point = hoverElement(el);
    el.dispatchEvent(new MouseEvent("mousedown", mouseInit(point.x, point.y, button, clickCount)));
    el.dispatchEvent(new MouseEvent("mouseup", mouseInit(point.x, point.y, button, clickCount)));
    if (buttonCode(button) === 0 && clickCount === 1 && typeof el.click === "function") {
      el.click();
      return;
    }
    el.dispatchEvent(new MouseEvent("click", mouseInit(point.x, point.y, button, clickCount)));
    if (clickCount >= 2) {
      el.dispatchEvent(new MouseEvent("dblclick", mouseInit(point.x, point.y, button, clickCount)));
    }
    if (buttonCode(button) === 0 && typeof el.click === "function") {
      el.click();
    }
  };

  const drag = (fromX, fromY, toX, toY) => {
    const source = targetAt(fromX, fromY);
    const destination = targetAt(toX, toY);
    const transfer = typeof DataTransfer === "function" ? new DataTransfer() : null;
    const eventInit = (x, y) => ({
      clientX: x,
      clientY: y,
      bubbles: true,
      cancelable: true,
      dataTransfer: transfer
    });
    source.dispatchEvent(new DragEvent("dragstart", eventInit(fromX, fromY)));
    destination.dispatchEvent(new DragEvent("dragenter", eventInit(toX, toY)));
    destination.dispatchEvent(new DragEvent("dragover", eventInit(toX, toY)));
    destination.dispatchEvent(new DragEvent("drop", eventInit(toX, toY)));
    source.dispatchEvent(new DragEvent("dragend", eventInit(toX, toY)));
  };

  const wheelAt = (dx, dy, x, y) => {
    const target = targetAt(x ?? Math.floor(window.innerWidth / 2), y ?? Math.floor(window.innerHeight / 2));
    target.dispatchEvent(new WheelEvent("wheel", {
      deltaX: dx,
      deltaY: dy,
      clientX: x ?? Math.floor(window.innerWidth / 2),
      clientY: y ?? Math.floor(window.innerHeight / 2),
      bubbles: true,
      cancelable: true
    }));
    window.scrollBy(dx, dy);
  };

  const upload = async (id) => {
    const el = resolve(id);
    if (!el) throw new Error("element not found");
    if (String(el.tagName || "").toLowerCase() !== "input" || inputType(el) !== "file") {
      throw new Error("element is not a file input");
    }
    if (typeof el.showPicker === "function") {
      try {
        el.showPicker();
      } catch (_error) {
        el.click();
      }
    } else {
      el.click();
    }
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline) {
      if (el.files && el.files.length > 0) return inspect(id);
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error("upload timed out");
  };

  const screenshotMeta = () => ({
    width: round(window.innerWidth),
    height: round(window.innerHeight),
    dpr: Number(window.devicePixelRatio || 1),
    scroll_x: round(window.scrollX || 0),
    scroll_y: round(window.scrollY || 0)
  });

  const waitFor = async (spec) => {
    const timeoutMs = spec.timeout_ms || 10000;
    if (spec.ms != null) {
      await new Promise((resolve) => setTimeout(resolve, spec.ms));
      return;
    }
    const deadline = Date.now() + timeoutMs;
    let stableAt = 0;
    while (Date.now() < deadline) {
      let ok = true;
      if (spec.id) ok = !!resolve(spec.id);
      if (ok && spec.text) {
        const body = doc().body ? (doc().body.innerText || doc().body.textContent || "") : "";
        ok = body.includes(spec.text);
      }
      if (ok && spec.url_contains) ok = window.location.href.includes(spec.url_contains);
      if (ok && spec.load) {
        const mode = String(spec.load).toLowerCase();
        if (mode === "domcontentloaded" || mode === "domcontent") {
          ok = doc().readyState === "interactive" || doc().readyState === "complete";
        } else if (mode === "networkidle" || mode === "network_idle") {
          const idle = state.inflight === 0 && doc().readyState === "complete";
          if (!idle) {
            stableAt = 0;
            ok = false;
          } else if (!stableAt) {
            stableAt = Date.now();
            ok = false;
          } else {
            ok = Date.now() - stableAt >= 500;
          }
        } else {
          ok = doc().readyState === "complete";
        }
      }
      if (ok) return;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error("wait timed out");
  };

  const act = async (actions, includeObservation) => {
    const results = [];
    const nextDialogDecision = (startIndex) => {
      for (let index = startIndex + 1; index < actions.length; index += 1) {
        const action = actions[index];
        if (action.__dialogConsumed) continue;
        if (action.type === "dialog_accept") {
          action.__dialogConsumed = true;
          return { accept: true, text: action.text ?? null };
        }
        if (action.type === "dialog_dismiss") {
          action.__dialogConsumed = true;
          return { accept: false, text: null };
        }
      }
      return { accept: false, text: null };
    };

    for (let index = 0; index < actions.length; index += 1) {
      const action = actions[index];
      try {
        if (action.__dialogConsumed) {
          results.push({ index, ok: true });
          continue;
        }
        state.nextDialogDecision = nextDialogDecision(index);
        if (action.type === "goto") {
          window.location.href = action.url;
        } else if (action.type === "click") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          el.scrollIntoView({ block: "center", inline: "center" });
          clickElement(el, "left", 1);
        } else if (action.type === "hover") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          hoverElement(el);
        } else if (action.type === "hover_at") {
          hoverAt(action.x, action.y);
        } else if (action.type === "click_at") {
          clickAt(action.x, action.y, action.button || "left", action.click_count || 1);
        } else if (action.type === "mouse_down") {
          mouseDownAt(action.x, action.y, action.button || "left");
        } else if (action.type === "mouse_up") {
          mouseUpAt(action.x, action.y, action.button || "left");
        } else if (action.type === "drag") {
          drag(action.from_x, action.from_y, action.to_x, action.to_y);
        } else if (action.type === "fill") {
          const el = resolve(action.id);
          if (!el) throw new Error("element not found");
          fill(el, action.text);
        } else if (action.type === "press") {
          dispatchKey(action.key, action.modifiers || {});
        } else if (action.type === "key_down") {
          dispatchKeyboard("keydown", action.key, action.modifiers || {});
        } else if (action.type === "key_up") {
          dispatchKeyboard("keyup", action.key, action.modifiers || {});
        } else if (action.type === "type") {
          typeText(action.text);
        } else if (action.type === "paste") {
          typeText(action.text);
        } else if (action.type === "scroll") {
          window.scrollBy(action.dx || 0, action.dy || 0);
        } else if (action.type === "wheel") {
          wheelAt(action.dx, action.dy, action.x, action.y);
        } else if (action.type === "dialog_accept" || action.type === "dialog_dismiss") {
          // Consumed by alert/confirm/prompt interception when needed.
        } else if (action.type === "wait") {
          await waitFor(action);
        } else {
          throw new Error("unsupported action");
        }
        results.push({ index, ok: true });
      } catch (error) {
        results.push({
          index,
          ok: false,
          error: error && error.message ? String(error.message) : String(error)
        });
        break;
      } finally {
        state.nextDialogDecision = null;
      }
    }
    return {
      results,
      observation: includeObservation ? observe() : null
    };
  };

  window.__taborAgent = {
    version: VERSION,
    observe,
    inspect,
    act,
    upload,
    screenshotMeta
  };
})()
