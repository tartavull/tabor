use std::str::FromStr;

use winit::window::CursorIcon;

pub const WEB_CURSOR_BOOTSTRAP: &str = r#"(function() {
  if (window.__taborCursorProbe) return;
  const state = window.__taborCursorState || (window.__taborCursorState = {});
  if (!state.textTypes) {
    state.textTypes = new Set(["", "text", "search", "url", "email", "password", "tel", "number"]);
  }
  state.isTextInput = function(node) {
    if (node.isContentEditable) return true;
    const tag = node.tagName;
    if (tag === "TEXTAREA") return true;
    if (tag === "INPUT") {
      const type = (node.getAttribute("type") || "").toLowerCase();
      return state.textTypes.has(type);
    }
    if (node.getAttribute && node.getAttribute("role") === "textbox") return true;
    return false;
  };
  state.isPointerTarget = function(node) {
    if (node.matches && node.matches("a[href], area[href], button, summary, select, [role='button'], [role='link']")) {
      return true;
    }
    if (node.tagName === "INPUT") {
      const type = (node.getAttribute("type") || "").toLowerCase();
      return !state.textTypes.has(type);
    }
    return false;
  };
  window.__taborCursorProbe = function(x, y) {
    const el = document.elementFromPoint(x, y);
    if (!el) return "default";
    for (let node = el; node; node = node.parentElement) {
      const style = window.getComputedStyle(node);
      if (style && style.cursor && style.cursor !== "auto") {
        return style.cursor;
      }
      if (state.isTextInput(node)) return "text";
      if (state.isPointerTarget(node)) return "pointer";
    }
    return "default";
  };
})();"#;

pub fn web_cursor_script(x: f64, y: f64) -> String {
    format!(
        r#"(() => {{
  const probe = window.__taborCursorProbe;
  return probe ? probe({x}, {y}) : "default";
}})()"#,
        x = x,
        y = y
    )
}

pub fn web_cursor_from_css(value: &str) -> Option<CursorIcon> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() {
        return None;
    }

    let fallback = value.split(',').next_back().unwrap_or(value).trim();
    let fallback = fallback.trim_matches('"');
    let fallback = if fallback.is_empty() { "default" } else { fallback };
    let fallback = fallback.to_ascii_lowercase();

    if fallback == "auto" {
        return Some(CursorIcon::Default);
    }

    Some(CursorIcon::from_str(&fallback).unwrap_or(CursorIcon::Default))
}

#[cfg(test)]
mod tests {
    use super::web_cursor_from_css;
    use winit::window::CursorIcon;

    #[test]
    fn web_cursor_from_css_parses_common() {
        assert_eq!(web_cursor_from_css("pointer"), Some(CursorIcon::Pointer));
        assert_eq!(web_cursor_from_css("text"), Some(CursorIcon::Text));
        assert_eq!(web_cursor_from_css("auto"), Some(CursorIcon::Default));
        assert_eq!(web_cursor_from_css("url(test.cur), pointer"), Some(CursorIcon::Pointer));
    }
}
