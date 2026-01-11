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
complete -c tabor -n "__fish_tabor_needs_command" -l config-file -d 'Specify alternative configuration file [default: $XDG_CONFIG_HOME/tabor/tabor.toml]' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -l socket -d 'Path for IPC socket creation' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c tabor -n "__fish_tabor_needs_command" -s e -l command -d 'Command and args to execute (must be last argument)' -r
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
complete -c tabor -n "__fish_tabor_needs_command" -f -a "migrate" -d 'Migrate the configuration file'
complete -c tabor -n "__fish_tabor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -s s -l socket -d 'IPC socket connection path override' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -f -a "get-config" -d 'Read runtime Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -f -a "send" -d 'Send a raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and not __fish_seen_subcommand_from config get-config send help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s w -l window-id -d 'Window ID for the new config' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s r -l reset -d 'Clear all runtime configuration changes'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s w -l window-id -d 'Window ID for the config request' -r
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "send" -d 'Send a raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-config" -d 'Read runtime Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s c -l config-file -d 'Path to the configuration file' -r -F
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s d -l dry-run -d 'Only output TOML config to STDOUT'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s i -l skip-imports -d 'Do not recurse over imports'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -l skip-renames -d 'Do not move renamed fields to their new location'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s s -l silent -d 'Do not output to STDOUT'
complete -c tabor -n "__fish_tabor_using_subcommand migrate" -s h -l help -d 'Print help'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg migrate help" -f -a "msg" -d 'Send a message to the Tabor socket'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg migrate help" -f -a "migrate" -d 'Migrate the configuration file'
complete -c tabor -n "__fish_tabor_using_subcommand help; and not __fish_seen_subcommand_from msg migrate help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "send" -d 'Send a raw JSON IPC message'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "config" -d 'Update the Tabor configuration'
complete -c tabor -n "__fish_tabor_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-config" -d 'Read runtime Tabor configuration'
