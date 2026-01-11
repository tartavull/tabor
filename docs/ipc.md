# IPC Protocol

Tabor exposes a local Unix socket for automation. Messages are single-line JSON
objects with a required `type` field. Use `tabor msg send` to send raw JSON.

## Transport

- Socket discovery:
  - `TABOR_SOCKET` environment variable (preferred).
  - `tabor --socket <PATH>` when launching Tabor.
  - Fallback: the newest socket in the platform temp dir.
- One request per connection. `tabor msg send` opens a socket, sends one JSON
  object, then prints the reply (if any).

## Common types

`tab_id` is an object:

```json
{"index":1,"generation":1}
```

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
- `tab_id` with `tab_id`

Tab kind (`IpcTabKind`) in tab state responses:
- `"terminal"`
- `{"web":{"url":"https://example.com"}}`

## Requests and replies

All request types are snake_case.

### ping
Request:
```json
{"type":"ping"}
```
Reply: `{"type":"pong"}`

### get_capabilities
Request:
```json
{"type":"get_capabilities"}
```
Reply:
```json
{"type":"capabilities","capabilities":{"protocol_version":1,"platform":"macos","version":"0.x","web_tabs":true}}
```

### list_tabs
Request:
```json
{"type":"list_tabs"}
```
Reply:
```json
{"type":"tab_list","groups":[{"id":0,"name":null,"tabs":[{"tab_id":{"index":1,"generation":1},"group_id":0,"index":0,"is_active":true,"title":"...","custom_title":null,"program_name":"...","kind":"terminal","activity":null}]}]}
```

### get_tab_state
Request:
```json
{"type":"get_tab_state","tab_id":{"index":1,"generation":1}}
```
Reply: `{"type":"tab_state","tab":{...}}`

### create_tab
Request:
```json
{"type":"create_tab","options":{"terminal_options":{},"window_identity":{},"window_kind":{"kind":"terminal"}}}
```
Reply: `{"type":"tab_created","tab_id":{"index":2,"generation":1}}`
`window_kind` values are `{"kind":"terminal"}` or `{"kind":"web","url":"https://example.com"}`.

### close_tab
Request:
```json
{"type":"close_tab","tab_id":{"index":1,"generation":1}}
```
If `tab_id` is omitted, the active tab is closed. Reply: `{"type":"ok"}`

### select_tab
Request:
```json
{"type":"select_tab","selection":{"type":"next"}}
```
Reply: `{"type":"ok"}`

### move_tab
Request:
```json
{"type":"move_tab","tab_id":{"index":1,"generation":1},"target_group_id":0,"target_index":2}
```
Reply: `{"type":"ok"}`

### set_tab_title
Request:
```json
{"type":"set_tab_title","tab_id":{"index":1,"generation":1},"title":"Build"}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### set_group_name
Request:
```json
{"type":"set_group_name","group_id":0,"name":"Work"}
```
Reply: `{"type":"ok"}`

### restore_closed_tab
Request:
```json
{"type":"restore_closed_tab"}
```
Reply: `{"type":"ok"}`

### open_url
Request:
```json
{"type":"open_url","url":"https://example.com","target":{"type":"new_tab"}}
```
Reply: `{"type":"ok"}` or `{"type":"tab_created",...}` (when a new tab is created).

### set_web_url
Request:
```json
{"type":"set_web_url","tab_id":{"index":1,"generation":1},"url":"https://example.com"}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### reload_web
Request:
```json
{"type":"reload_web","tab_id":{"index":1,"generation":1}}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### open_inspector
Opens the UI Web Inspector for a web tab.
Request:
```json
{"type":"open_inspector","tab_id":{"index":1,"generation":1}}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### get_tab_panel
Request:
```json
{"type":"get_tab_panel"}
```
Reply:
```json
{"type":"tab_panel","panel":{"enabled":true,"width":260}}
```

### set_tab_panel
Request:
```json
{"type":"set_tab_panel","enabled":true,"width":260}
```
Reply: `{"type":"ok"}`

### dispatch_action
Dispatches a configured action by name.
Request:
```json
{"type":"dispatch_action","tab_id":{"index":1,"generation":1},"action":{"type":"action","name":"copy"}}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### send_input
Request:
```json
{"type":"send_input","tab_id":{"index":1,"generation":1},"text":"ls -la\n"}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

### run_command_bar
Request:
```json
{"type":"run_command_bar","tab_id":{"index":1,"generation":1},"input":":toggle_tab_panel"}
```
`tab_id` is optional (defaults to active tab). Reply: `{"type":"ok"}`

## Remote Inspector (macOS)

These commands require macOS and a web tab. They return `unsupported` on other
platforms.

### list_inspector_targets
Request:
```json
{"type":"list_inspector_targets"}
```
Reply:
```json
{"type":"inspector_targets","targets":[{"target_id":42,"target_type":"WIRTypeWebPage","url":"https://example.com","title":"Example","override_name":null,"host_app_identifier":"PID:12345","tab_id":{"index":1,"generation":1}}]}
```

### attach_inspector
Request (by tab id):
```json
{"type":"attach_inspector","tab_id":{"index":1,"generation":1}}
```
Request (by target id):
```json
{"type":"attach_inspector","target_id":42}
```
Reply:
```json
{"type":"inspector_attached","session":{"session_id":"PID:12345-1","target_id":42,"tab_id":{"index":1,"generation":1}}}
```

### send_inspector_message
Sends a raw WebKit Inspector Protocol JSON string.
Request:
```json
{"type":"send_inspector_message","session_id":"PID:12345-1","message":"{\"id\":1,\"method\":\"Network.enable\"}"}
```
Reply: `{"type":"ok"}`

### poll_inspector_messages
Request:
```json
{"type":"poll_inspector_messages","session_id":"PID:12345-1","max":50}
```
Reply:
```json
{"type":"inspector_messages","messages":[{"session_id":"PID:12345-1","payload":"{\"method\":\"Network.requestWillBeSent\",...}"}]}
```

### detach_inspector
Request:
```json
{"type":"detach_inspector","session_id":"PID:12345-1"}
```
Reply: `{"type":"ok"}`

## Example: watch network traffic

```sh
tabor msg send '{"type":"open_url","url":"https://example.com","target":{"type":"new_tab"}}'
tabor msg send '{"type":"attach_inspector","tab_id":{"index":1,"generation":1}}'
tabor msg send '{"type":"send_inspector_message","session_id":"PID:12345-1","message":"{\"id\":1,\"method\":\"Network.enable\"}"}'
tabor msg send '{"type":"reload_web","tab_id":{"index":1,"generation":1}}'
tabor msg send '{"type":"poll_inspector_messages","session_id":"PID:12345-1","max":100}'
```

## Errors

Errors are returned as:

```json
{"type":"error","error":{"code":"not_found","message":"Tab not found"}}
```

Possible `code` values:
`not_found`, `invalid_request`, `unsupported`, `ambiguous`,
`permission_denied`, `timeout`, `internal`.
