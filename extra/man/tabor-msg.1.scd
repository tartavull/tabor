TABOR-MSG(1)

# NAME

tabor-msg - Send messages to Tabor.

# SYNOPSIS

This command communicates with running Tabor instances through a socket,
making it possible to control Tabor without directly accessing it.

# OPTIONS

*-s, --socket* _<SOCKET>_

	Path for IPC socket communication.

# MESSAGES

*config*

	Update the Tabor configuration.

	*ARGS*
		*<CONFIG_OPTIONS>...*

			Configuration file options.

			Example: _tabor msg config 'cursor.style="Beam"'_

	*FLAGS*
		*-r, --reset*

			Clear all runtime configuration changes.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the new config.

			Use _-1_ to apply this change to all windows.

			Default: _$TABOR_WINDOW_ID_

*get-config*

	Read runtime Tabor configuration.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the config request.

			Use _-1_ to get the global config.

			Default: _$TABOR_WINDOW_ID_

*send*

	Send a raw JSON IPC message.

	*ARGS*
		*<JSON>*

			JSON payload to send to the IPC socket.

			Example: _tabor msg send '{"type":"list_tabs"}'_

			Inspector example:
			_tabor msg send '{"type":"send_inspector_message","session_id":"PID:12345-1","message":"{\"id\":1,\"method\":\"Network.enable\"}"}'_

	See _docs/ipc.md_ for the full IPC protocol and request list.

# SEE ALSO

*tabor*(1), *tabor*(5), *tabor-bindings*(5)

# BUGS

Found a bug? Please report it at _https://github.com/tartavull/tabor/issues_.

# MAINTAINERS

- Christian Duerr <contact@christianduerr.com>
- Kirill Chibisov <contact@kchibisov.com>
