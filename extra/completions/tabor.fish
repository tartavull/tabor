# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_tabor_global_optspecs
	string join \n print-events ref-test embed= config-file= socket= q v daemon working-directory= hold e/command= T/title= class= o/option= h/help V/version
end

function __fish_tabor_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_tabor_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_tabor_using_subcommand
	set -l cmd (__fish_tabor_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c tabor -n "__fish_tabor_needs_command" -l embed -d 'X11 window ID to embed Tabor within (decimal or hexadecimal with "0x" prefix)' -r
complete -c tabor -n "__fish_tabor_needs_command" -l config-file -d 'Specify alternative configuration file [default: $HOME/.config/tabor/tabor.toml]' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -l socket -d 'Path for IPC socket creation' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -s e -l command -d 'Command and args to execute in the default shell (must be last argument)' -r
complete -c tabor -n "__fish_tabor_needs_command" -s T -l title -d 'Defines the window title [default: Tabor]' -r
complete -c tabor -n "__fish_tabor_needs_command" -l class -d 'Defines window class/app_id on X11/Wayland [default: Tabor]' -r
complete -c tabor -n "__fish_tabor_needs_command" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c tabor -n "__fish_tabor_needs_command" -l print-events -d 'Print all events to STDOUT'
complete -c tabor -n "__fish_tabor_needs_command" -l ref-test -d 'Generates ref test'
complete -c tabor -n "__fish_tabor_needs_command" -s q -d 'Reduces the level of verbosity (the min level is -qq)'
complete -c tabor -n "__fish_tabor_needs_command" -s v -d 'Increases the level of verbosity (the max level is -vvv)'
complete -c tabor -n "__fish_tabor_needs_command" -l daemon -d 'Do not spawn an initial window'
complete -c tabor -n "__fish_tabor_needs_command" -l hold -d 'Remain open after child process exit'
complete -c tabor -n "__fish_tabor_needs_command" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_needs_command" -s V -l version -d 'Print version'
complete -c tabor -n "__fish_tabor_needs_command" -f -a "msg" -d 'Send a message to the Tabor socket'
complete -c tabor -n "__fish_tabor_needs_command" -f -a "agent" -d 'Stateful agent control for an attached Tabor instance'
complete -c tabor -n "__fish_tabor_needs_command" -f -a "workspace"
complete -c tabor -n "__fish_tabor_needs_command" -f -a "migrate" -d 'Migrate the configuration file'
complete -c tabor -n "__fish_tabor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -s s -l socket -d 'IPC socket connection path override' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "get-config" -d 'Read runtime Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "ping" -d 'Ping the IPC socket'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "get-capabilities" -d 'List IPC capabilities'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "list-tabs" -d 'List all tabs'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "get-tab-state" -d 'Get a single tab state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "create-tab" -d 'Create a new tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "create-group" -d 'Create a new tab group'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "close-tab" -d 'Close a tab (defaults to active)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "select-tab" -d 'Select a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "move-tab" -d 'Move a tab within or across groups'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "set-tab-title" -d 'Set or clear a tab title'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "set-group-name" -d 'Set or clear a tab group name'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "restore-closed-tab" -d 'Restore the most recently closed tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "open-url" -d 'Open a URL in a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "set-web-url" -d 'Set the URL for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "reload-web" -d 'Reload a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "open-inspector" -d 'Open the Web Inspector for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "get-tab-panel" -d 'Get tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "set-tab-panel" -d 'Set tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "dispatch-action" -d 'Dispatch a configured action'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "send-input" -d 'Send literal input to a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "run-command-bar" -d 'Run a command in the command bar'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "inspector" -d 'Web Inspector commands'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "send" -d 'Send raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "list-requests" -d 'List available IPC request types'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config ping get-capabilities list-tabs get-tab-state create-tab create-group close-tab select-tab move-tab set-tab-title set-group-name restore-closed-tab open-url set-web-url reload-web open-inspector get-tab-panel set-tab-panel dispatch-action send-input run-command-bar inspector send list-requests help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s w -l window-id -d 'Window ID for the new config' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s r -l reset -d 'Clear all runtime configuration changes'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s w -l window-id -d 'Window ID for the config request' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from ping" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-capabilities" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from list-tabs" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-tab-state" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-tab-state" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l web -d 'Create a web tab with the provided URL' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l group-id -d 'Target group id for the new tab' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l group-name -d 'Target group name for the new tab' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -s e -l command -d 'Command and args to execute in the default shell (must be last argument)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -s T -l title -d 'Defines the window title [default: Tabor]' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l class -d 'Defines window class/app_id on X11/Wayland [default: Tabor]' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -l hold -d 'Remain open after child process exit'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-tab" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-group" -l name -d 'Optional name for the new group' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from create-group" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from close-tab" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from close-tab" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l index -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l active
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l next
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l previous
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -l last
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from select-tab" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from move-tab" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from move-tab" -l target-group-id -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from move-tab" -l target-index -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from move-tab" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-title" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-title" -l title -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-title" -l clear
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-title" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-group-name" -l group-id -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-group-name" -l name -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-group-name" -l clear
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-group-name" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from restore-closed-tab" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from open-url" -l tab-id -d 'Target tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from open-url" -l new-tab
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from open-url" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-web-url" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-web-url" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from reload-web" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from reload-web" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from open-inspector" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from open-inspector" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-tab-panel" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-panel" -l width -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-panel" -l enable
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-panel" -l disable
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from set-tab-panel" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l action -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l vi-motion -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l vi-action -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l search-action -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l mouse-action -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l esc -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -l command -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from dispatch-action" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from send-input" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from send-input" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from run-command-bar" -l tab-id -d 'Tab id formatted as <index>:<generation> (defaults to active tab)' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from run-command-bar" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "list-targets" -d 'List Web Inspector targets'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "attach" -d 'Attach to a Web Inspector target'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "detach" -d 'Detach a Web Inspector session'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "send" -d 'Send a Web Inspector message'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "poll" -d 'Poll for Web Inspector messages'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from inspector" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from list-requests" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-config" -d 'Read runtime Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "ping" -d 'Ping the IPC socket'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-capabilities" -d 'List IPC capabilities'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "list-tabs" -d 'List all tabs'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-tab-state" -d 'Get a single tab state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "create-tab" -d 'Create a new tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "create-group" -d 'Create a new tab group'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "close-tab" -d 'Close a tab (defaults to active)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "select-tab" -d 'Select a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "move-tab" -d 'Move a tab within or across groups'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-tab-title" -d 'Set or clear a tab title'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-group-name" -d 'Set or clear a tab group name'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "restore-closed-tab" -d 'Restore the most recently closed tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "open-url" -d 'Open a URL in a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-web-url" -d 'Set the URL for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "reload-web" -d 'Reload a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "open-inspector" -d 'Open the Web Inspector for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-tab-panel" -d 'Get tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-tab-panel" -d 'Set tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "dispatch-action" -d 'Dispatch a configured action'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "send-input" -d 'Send literal input to a tab'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "run-command-bar" -d 'Run a command in the command bar'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "inspector" -d 'Web Inspector commands'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "send" -d 'Send raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "list-requests" -d 'List available IPC request types'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -s s -l socket -d 'IPC socket connection path override' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "attach" -d 'Attach to a running Tabor instance and start the local controller'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "app" -d 'List tabs and current agent state'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "use" -d 'Select the active tab or a specific tab for automation'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "observe" -d 'Observe the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "inspect" -d 'Inspect one observed element by id'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "screenshot" -d 'Capture a screenshot from the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "events" -d 'Read buffered console, network, and page events'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "pdf" -d 'Render the selected web tab as a PDF'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "upload" -d 'Upload local files into a selected file input'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "downloads" -d 'List tracked downloads for the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "act" -d 'Execute batched actions encoded as JSON'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "clipboard" -d 'Read or update the system clipboard'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "close" -d 'Stop the local controller for this Tabor socket'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and not __fish_seen_subcommand_from attach app use observe inspect screenshot events pdf upload downloads act clipboard close help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from attach" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from app" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from use" -l tab-id -d 'Tab id formatted as <index>:<generation>' -r
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from use" -l active -d 'Select the active tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from use" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from observe" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from inspect" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from screenshot" -l path -d 'Write the PNG to this path. A temp file is used by default' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from screenshot" -l element-id -d 'Crop the viewport screenshot to the observed element' -r
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from screenshot" -l full-page -d 'Capture the full page instead of the viewport'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from screenshot" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from events" -l since -d 'Return events after this event id' -r
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from events" -l max -d 'Maximum number of events to return' -r
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from events" -l kind -d 'Restrict events to one or more kinds' -r
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from events" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from pdf" -l path -d 'Write the PDF to this path. A temp file is used by default' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from pdf" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from upload" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from downloads" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from act" -l observe -d 'Include a post-action observation in the reply' -r -f -a "true\t''
false\t''"
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from act" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from clipboard" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from clipboard" -f -a "get" -d 'Read the system clipboard'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from clipboard" -f -a "set" -d 'Replace the system clipboard contents'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from clipboard" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from close" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "attach" -d 'Attach to a running Tabor instance and start the local controller'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "app" -d 'List tabs and current agent state'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "use" -d 'Select the active tab or a specific tab for automation'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "observe" -d 'Observe the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "inspect" -d 'Inspect one observed element by id'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "screenshot" -d 'Capture a screenshot from the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "events" -d 'Read buffered console, network, and page events'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "pdf" -d 'Render the selected web tab as a PDF'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "upload" -d 'Upload local files into a selected file input'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "downloads" -d 'List tracked downloads for the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "act" -d 'Execute batched actions encoded as JSON'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "clipboard" -d 'Read or update the system clipboard'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "close" -d 'Stop the local controller for this Tabor socket'
complete -c tabor -n "__fish_tabor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and not __fish_seen_subcommand_from status stop restart-terminal help" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and not __fish_seen_subcommand_from status stop restart-terminal help" -f -a "status"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and not __fish_seen_subcommand_from status stop restart-terminal help" -f -a "stop"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and not __fish_seen_subcommand_from status stop restart-terminal help" -f -a "restart-terminal"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and not __fish_seen_subcommand_from status stop restart-terminal help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from stop" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from restart-terminal" -l id -r
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from restart-terminal" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from help" -f -a "status"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from help" -f -a "stop"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from help" -f -a "restart-terminal"
complete -c tabor -n "__fish_tabor_using_subcommand workspace; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s c -l config-file -d 'Path to the configuration file' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s d -l dry-run -d 'Only output TOML config to STDOUT'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s i -l skip-imports -d 'Do not recurse over imports'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -l skip-renames -d 'Do not move renamed fields to their new location'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s s -l silent -d 'Do not output to STDOUT'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg agent workspace migrate help" -f -a "msg" -d 'Send a message to the Tabor socket'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg agent workspace migrate help" -f -a "agent" -d 'Stateful agent control for an attached Tabor instance'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg agent workspace migrate help" -f -a "workspace"
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg agent workspace migrate help" -f -a "migrate" -d 'Migrate the configuration file'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg agent workspace migrate help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-config" -d 'Read runtime Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "ping" -d 'Ping the IPC socket'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-capabilities" -d 'List IPC capabilities'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "list-tabs" -d 'List all tabs'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-tab-state" -d 'Get a single tab state'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-tab" -d 'Create a new tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-group" -d 'Create a new tab group'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "close-tab" -d 'Close a tab (defaults to active)'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "select-tab" -d 'Select a tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "move-tab" -d 'Move a tab within or across groups'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-tab-title" -d 'Set or clear a tab title'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-group-name" -d 'Set or clear a tab group name'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "restore-closed-tab" -d 'Restore the most recently closed tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "open-url" -d 'Open a URL in a tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-web-url" -d 'Set the URL for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "reload-web" -d 'Reload a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "open-inspector" -d 'Open the Web Inspector for a web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-tab-panel" -d 'Get tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-tab-panel" -d 'Set tab panel state'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "dispatch-action" -d 'Dispatch a configured action'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "send-input" -d 'Send literal input to a tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "run-command-bar" -d 'Run a command in the command bar'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "inspector" -d 'Web Inspector commands'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "send" -d 'Send raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "list-requests" -d 'List available IPC request types'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "attach" -d 'Attach to a running Tabor instance and start the local controller'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "app" -d 'List tabs and current agent state'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "use" -d 'Select the active tab or a specific tab for automation'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "observe" -d 'Observe the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "inspect" -d 'Inspect one observed element by id'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "screenshot" -d 'Capture a screenshot from the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "events" -d 'Read buffered console, network, and page events'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "pdf" -d 'Render the selected web tab as a PDF'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "upload" -d 'Upload local files into a selected file input'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "downloads" -d 'List tracked downloads for the selected web tab'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "act" -d 'Execute batched actions encoded as JSON'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "clipboard" -d 'Read or update the system clipboard'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "close" -d 'Stop the local controller for this Tabor socket'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from workspace" -f -a "status"
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from workspace" -f -a "stop"
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from workspace" -f -a "restart-terminal"
