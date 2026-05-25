# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_devme_global_optspecs
	string join \n json no-input q/quiet no-color h/help V/version
end

function __fish_devme_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_devme_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_devme_using_subcommand
	set -l cmd (__fish_devme_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c devme -n "__fish_devme_needs_command" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_needs_command" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_needs_command" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_needs_command" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_needs_command" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_needs_command" -s V -l version -d 'Print version'
complete -c devme -n "__fish_devme_needs_command" -f -a "up" -d 'Start the supervisor (or attach to a running one) and bring services up'
complete -c devme -n "__fish_devme_needs_command" -f -a "down" -d 'Shut down this instance\'s supervisor'
complete -c devme -n "__fish_devme_needs_command" -f -a "status" -d 'Print a snapshot of current service status'
complete -c devme -n "__fish_devme_needs_command" -f -a "restart" -d 'Restart a service'
complete -c devme -n "__fish_devme_needs_command" -f -a "stop" -d 'Stop a single service (keep the daemon running)'
complete -c devme -n "__fish_devme_needs_command" -f -a "start" -d 'Start a single service'
complete -c devme -n "__fish_devme_needs_command" -f -a "logs" -d 'Tail logs for a service'
complete -c devme -n "__fish_devme_needs_command" -f -a "completions" -d 'Print a shell completion script. Pipe into your shell\'s completion directory: `devme completions fish > ~/.config/fish/completions/devme.fish`'
complete -c devme -n "__fish_devme_needs_command" -f -a "doctor" -d 'Diagnostic snapshot: service states + recent error logs. Designed for agents — outputs structured JSON with everything needed to diagnose failures without multiple round-trips'
complete -c devme -n "__fish_devme_needs_command" -f -a "config" -d 'View or change devme global settings'
complete -c devme -n "__fish_devme_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c devme -n "__fish_devme_using_subcommand up" -l timeout -d 'Seconds to wait for `--wait`. 0 means "no timeout" (docker convention). Default 30s, only consulted with `--wait`' -r
complete -c devme -n "__fish_devme_using_subcommand up" -s d -l detach -d 'Start services then exit without tailing logs. The daemon keeps running in the background; use `devme down` to stop it'
complete -c devme -n "__fish_devme_using_subcommand up" -l wait -d 'With `-d`, block until every service is healthy (or has Started) before exiting. Pairs with `--timeout` to cap the wait'
complete -c devme -n "__fish_devme_using_subcommand up" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand up" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand up" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand up" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand up" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand down" -s t -l timeout -d 'Seconds to wait for graceful service stops before SIGKILL. Matches `docker compose down -t`' -r
complete -c devme -n "__fish_devme_using_subcommand down" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand down" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand down" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand down" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand down" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand status" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand status" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand status" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand status" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand status" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand restart" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand restart" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand restart" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand restart" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand restart" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand stop" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand stop" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand stop" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand stop" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand stop" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand start" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand start" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand start" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand start" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand start" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand logs" -l tail -d 'Show only the last N lines of buffered output before following. 0 means "all" (the daemon\'s full ring). Default 200 — a `docker compose logs` of a long-running service is a wall of text' -r
complete -c devme -n "__fish_devme_using_subcommand logs" -s f -l follow
complete -c devme -n "__fish_devme_using_subcommand logs" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand logs" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand logs" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand logs" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand logs" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand completions" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand completions" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand completions" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand completions" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand completions" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand doctor" -l tail -d 'Maximum log lines per service (default 50)' -r
complete -c devme -n "__fish_devme_using_subcommand doctor" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand doctor" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand doctor" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand doctor" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand doctor" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -f -a "get" -d 'Print the value of a setting'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -f -a "set" -d 'Set a value'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -f -a "unset" -d 'Remove a value (reset to default)'
complete -c devme -n "__fish_devme_using_subcommand config; and not __fish_seen_subcommand_from get set unset help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from get" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from get" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from get" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from get" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from get" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from set" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from set" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from set" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from set" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from set" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from unset" -l json -d 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from unset" -l no-input -d 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from unset" -s q -l quiet -d 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from unset" -l no-color -d 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from unset" -s h -l help -d 'Print help'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "get" -d 'Print the value of a setting'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "set" -d 'Set a value'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "unset" -d 'Remove a value (reset to default)'
complete -c devme -n "__fish_devme_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "up" -d 'Start the supervisor (or attach to a running one) and bring services up'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "down" -d 'Shut down this instance\'s supervisor'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "status" -d 'Print a snapshot of current service status'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "restart" -d 'Restart a service'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "stop" -d 'Stop a single service (keep the daemon running)'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "start" -d 'Start a single service'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "logs" -d 'Tail logs for a service'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "completions" -d 'Print a shell completion script. Pipe into your shell\'s completion directory: `devme completions fish > ~/.config/fish/completions/devme.fish`'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "doctor" -d 'Diagnostic snapshot: service states + recent error logs. Designed for agents — outputs structured JSON with everything needed to diagnose failures without multiple round-trips'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "config" -d 'View or change devme global settings'
complete -c devme -n "__fish_devme_using_subcommand help; and not __fish_seen_subcommand_from up down status restart stop start logs completions doctor config help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c devme -n "__fish_devme_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "get" -d 'Print the value of a setting'
complete -c devme -n "__fish_devme_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "set" -d 'Set a value'
complete -c devme -n "__fish_devme_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "unset" -d 'Remove a value (reset to default)'
