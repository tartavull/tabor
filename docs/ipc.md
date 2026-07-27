# IPC Protocol

Tabor exposes a local Unix socket for app control and stateful web automation.
Use `tabor msg` for one-shot app and inspector requests. Use `tabor agent` for
live tab takeover and repeated web actions against a running Tabor instance.

On macOS, launch the GUI app through LaunchServices, for example
`open -a /Applications/Tabor.app`. Direct execution of
`/Applications/Tabor.app/Contents/MacOS/tabor` is supported for CLI commands
such as `tabor msg`, `tabor agent`, and `tabor workspace`, but not for GUI
startup.

## Transport

- Socket discovery:
  - `TABOR_SOCKET` environment variable (preferred).
  - `tabor --socket <PATH>` when launching Tabor.
  - Fallback: the newest live Tabor socket in the temp directory.
- Terminal shells launched by Tabor receive `TABOR_TAB_ID=<index>:<generation>`.
  `tabor msg open-url --new-tab` uses it to place the new tab in the source
  terminal tab group.
- `tabor msg` opens one socket connection per request.
- `tabor agent attach` starts a per-socket local controller and keeps one
  persistent IPC connection open until `tabor agent close`.

## Common Types

`tab_id` is an object:

```json
{"index":1,"generation":1}
```

Tab ids are also accepted by typed CLI commands as `<index>:<generation>`.

`TabSelection`:
- `active`
- `next`
- `previous`
- `last`
- `by_index` with `index`
- `by_id` with `tab_id`

`UrlTarget`:
- `current`
- `new_tab`
- `new_tab_in_source_group` with `source_tab_id`
- `tab_id` with `tab_id`

Tab kind in tab state responses:
- `"terminal"`
- `{"web":{"url":"https://example.com"}}`

## App Control With `tabor msg`

Typed IPC commands map 1:1 to socket requests. Run `tabor msg --help` for the
full list and `tabor msg list-requests` for raw request names.

Common commands:
- `tabor msg list-tabs`
- `tabor msg get-tab-state --tab-id 1:1`
- `tabor msg create-tab --web https://example.com`
- `tabor msg open-url https://example.com --new-tab`
- `tabor msg select-tab --active`
- `tabor msg move-tab --tab-id 2:1 --target-group-id 1 --target-index 0`
- `tabor msg open-inspector --tab-id 2:1`
- `tabor msg terminal-observe --tab-id 1:1`
- `tabor msg terminal-read --tab-id 1:1 --scope buffer --max-lines 200`
- `tabor msg terminal-screenshot --tab-id 1:1 --path /tmp/terminal.png`

Raw JSON remains available through `tabor msg send`:

```json
{"type":"list_tabs"}
```

Terminal-specific typed IPC commands:
- `tabor msg terminal-observe [--tab-id 1:1]`
- `tabor msg terminal-read [--tab-id 1:1] [--scope viewport|buffer|selection] [--max-lines 200]`
- `tabor msg terminal-screenshot [--tab-id 1:1] [--path FILE]`

`terminal-observe` returns visible terminal rows plus cursor, selection, and
visible-cell metadata. `terminal-read` returns plain-text viewport rows,
scrollback buffer lines, or the selected text depending on `scope`.

## Stateful Tab Automation With `tabor agent`

`tabor agent` is the primary automation surface. It attaches to the live Tabor
instance you already opened, lists the existing tabs, selects one of them, and
then drives that tab with compact observations and batched actions.

There is no isolated browser-session flag in this workflow. The agent operates
on live tabs in the attached Tabor instance.

### Typical Workflow

```bash
export TABOR_SOCKET=/tmp/tabor.sock
tabor agent attach
tabor agent app
tabor agent use --active
tabor agent observe
tabor agent read --scope buffer --max-lines 200
tabor agent act '[{"type":"click","id":"a"},{"type":"wait","load":"networkidle"}]'
tabor agent inspect a
tabor agent screenshot
tabor agent events --kind console --kind network
tabor agent pdf
tabor agent downloads
tabor agent close
```

### `app`

Lists the live tab inventory plus the currently selected agent tab:

```json
{"type":"app","groups":[{"id":0,"name":null,"tabs":[...]}],"selected_tab_id":{"index":2,"generation":1}}
```

### `observe`

Returns compact state for the selected tab.

For web tabs:

```json
{
  "type":"observation",
  "observation":{
    "revision":3,
    "url":"https://example.com",
    "title":"Example",
    "ready_state":"complete",
    "pending_requests":0,
    "elements":[
      {"id":"a","role":"button","name":"Submit"},
      {"id":"b","role":"input","name":"Email","editable":true}
    ]
  }
}
```

Only visible interactive elements are returned by default.

For terminal tabs, the reply is a terminal observation with:
- terminal session state
- layout strips and display offset
- visible viewport rows
- cursor and selection metadata
- visible rendered cells with colors and flags

### `inspect`

Expands a single observed element when the compact observation is not enough.
This is only supported for web tabs:

```bash
tabor agent inspect a
```

### Artifact and event commands

- `tabor agent screenshot [--path FILE] [--full-page] [--element-id ID]`
- `tabor agent read [--scope viewport|buffer|selection] [--max-lines N]`
- `tabor agent events [--since N] [--max N] [--kind console] [--kind network]`
- `tabor agent pdf [--path FILE]`
- `tabor agent upload <element-id> <file>...`
- `tabor agent downloads`
- `tabor agent clipboard get`
- `tabor agent clipboard set --text 'value'`

Console, network, page, and log event capture starts when agent automation or an inspector uses
the tab. Agent capture expires after 60 seconds of inactivity. Each tab retains at most 2,048
events or 8 MiB, and event parameters larger than 256 KiB are replaced with truncation metadata.

### `act`

`tabor agent act` accepts a JSON array of actions and executes them as a batch.
The default reply includes a post-action observation.

Supported actions:
- `{"type":"goto","url":"https://example.com"}`
- `{"type":"click","id":"a"}`
- `{"type":"hover","id":"a"}`
- `{"type":"hover_at","x":320,"y":180}`
- `{"type":"click_at","x":320,"y":180}`
- `{"type":"mouse_down","x":320,"y":180}`
- `{"type":"mouse_up","x":320,"y":180}`
- `{"type":"drag","from_x":320,"from_y":180,"to_x":640,"to_y":240}`
- `{"type":"fill","id":"b","text":"user@example.com"}`
- `{"type":"press","key":"Tab","modifiers":{"shift":false,"control":false,"alt":false,"super_key":false}}`
- `{"type":"key_down","key":"Shift"}`
- `{"type":"key_up","key":"Shift"}`
- `{"type":"type","text":"hello"}`
- `{"type":"paste","text":"hello"}`
- `{"type":"scroll","dy":600}`
- `{"type":"wheel","dx":0,"dy":400}`
- `{"type":"dialog_accept","text":"optional prompt text"}`
- `{"type":"dialog_dismiss"}`
- `{"type":"wait","id":"a","timeout_ms":5000}`
- `{"type":"wait","text":"Success","timeout_ms":5000}`
- `{"type":"wait","url_contains":"dashboard","timeout_ms":5000}`
- `{"type":"wait","load":"networkidle","timeout_ms":5000}`
- `{"type":"wait","ms":250}`

On terminal tabs, `act` supports the terminal-safe subset only:
- `type`
- `paste`
- `press`
- `key_down`
- `key_up`
- `wait` with explicit `ms`

Example:

```bash
tabor agent act '[
  {"type":"fill","id":"b","text":"user@example.com"},
  {"type":"click","id":"c"},
  {"type":"wait","load":"networkidle","timeout_ms":5000}
]'
```

Reply shape:

```json
{
  "type":"act",
  "result":{
    "results":[{"index":0,"ok":true},{"index":1,"ok":true},{"index":2,"ok":true}],
    "observation":{...}
  }
}
```

### Raw IPC Requests

The raw request types exposed through `tabor msg send` are:
- `agent_observe`
- `agent_inspect`
- `agent_screenshot`
- `agent_events`
- `agent_pdf`
- `agent_upload`
- `agent_downloads`
- `agent_act`
- `terminal_observe`
- `terminal_read`
- `terminal_screenshot`

Examples:

```json
{"type":"agent_observe","tab_id":{"index":2,"generation":1}}
{"type":"agent_inspect","tab_id":{"index":2,"generation":1},"element_id":"a"}
{"type":"agent_screenshot","tab_id":{"index":2,"generation":1},"full_page":false}
{"type":"agent_events","tab_id":{"index":2,"generation":1},"since":41,"max":50,"kinds":["console","network"]}
{"type":"agent_pdf","tab_id":{"index":2,"generation":1}}
{"type":"agent_upload","tab_id":{"index":2,"generation":1},"element_id":"a","paths":["/tmp/file.txt"]}
{"type":"agent_downloads","tab_id":{"index":2,"generation":1}}
{"type":"agent_act","tab_id":{"index":2,"generation":1},"actions":[{"type":"click","id":"a"}],"observe":true}
{"type":"terminal_observe","tab_id":{"index":1,"generation":1}}
{"type":"terminal_read","tab_id":{"index":1,"generation":1},"scope":"buffer","max_lines":200}
{"type":"terminal_screenshot","tab_id":{"index":1,"generation":1}}
```

## Remote Inspector (macOS)

These commands require macOS and a web tab. On the CEF backend they speak the
Chromium DevTools Protocol (CDP).

Common commands:
- `tabor msg inspector list-targets`
- `tabor msg inspector attach --tab-id 1:1`
- `tabor msg inspector send --session-id cef:1:1 --message '{"id":1,"method":"Network.enable"}'`
- `tabor msg inspector poll --session-id cef:1:1 --max 100`
- `tabor msg inspector detach --session-id cef:1:1`
