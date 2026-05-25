
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'devme' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'devme'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'devme' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('up', 'up', [CompletionResultType]::ParameterValue, 'Start the supervisor (or attach to a running one) and bring services up')
            [CompletionResult]::new('down', 'down', [CompletionResultType]::ParameterValue, 'Shut down this instance''s supervisor')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Print a snapshot of current service status')
            [CompletionResult]::new('restart', 'restart', [CompletionResultType]::ParameterValue, 'Restart a service')
            [CompletionResult]::new('stop', 'stop', [CompletionResultType]::ParameterValue, 'Stop a single service (keep the daemon running)')
            [CompletionResult]::new('start', 'start', [CompletionResultType]::ParameterValue, 'Start a single service')
            [CompletionResult]::new('logs', 'logs', [CompletionResultType]::ParameterValue, 'Tail logs for a service')
            [CompletionResult]::new('completions', 'completions', [CompletionResultType]::ParameterValue, 'Print a shell completion script. Pipe into your shell''s completion directory: `devme completions fish > ~/.config/fish/completions/devme.fish`')
            [CompletionResult]::new('doctor', 'doctor', [CompletionResultType]::ParameterValue, 'Diagnostic snapshot: service states + recent error logs. Designed for agents — outputs structured JSON with everything needed to diagnose failures without multiple round-trips')
            [CompletionResult]::new('config', 'config', [CompletionResultType]::ParameterValue, 'View or change devme global settings')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'devme;up' {
            [CompletionResult]::new('--timeout', '--timeout', [CompletionResultType]::ParameterName, 'Seconds to wait for `--wait`. 0 means "no timeout" (docker convention). Default 30s, only consulted with `--wait`')
            [CompletionResult]::new('-d', '-d', [CompletionResultType]::ParameterName, 'Start services then exit without tailing logs. The daemon keeps running in the background; use `devme down` to stop it')
            [CompletionResult]::new('--detach', '--detach', [CompletionResultType]::ParameterName, 'Start services then exit without tailing logs. The daemon keeps running in the background; use `devme down` to stop it')
            [CompletionResult]::new('--wait', '--wait', [CompletionResultType]::ParameterName, 'With `-d`, block until every service is healthy (or has Started) before exiting. Pairs with `--timeout` to cap the wait')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;down' {
            [CompletionResult]::new('-t', '-t', [CompletionResultType]::ParameterName, 'Seconds to wait for graceful service stops before SIGKILL. Matches `docker compose down -t`')
            [CompletionResult]::new('--timeout', '--timeout', [CompletionResultType]::ParameterName, 'Seconds to wait for graceful service stops before SIGKILL. Matches `docker compose down -t`')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;status' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;restart' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;stop' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;start' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;logs' {
            [CompletionResult]::new('--tail', '--tail', [CompletionResultType]::ParameterName, 'Show only the last N lines of buffered output before following. 0 means "all" (the daemon''s full ring). Default 200 — a `docker compose logs` of a long-running service is a wall of text')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'f')
            [CompletionResult]::new('--follow', '--follow', [CompletionResultType]::ParameterName, 'follow')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;completions' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;doctor' {
            [CompletionResult]::new('--tail', '--tail', [CompletionResultType]::ParameterName, 'Maximum log lines per service (default 50)')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;config' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('get', 'get', [CompletionResultType]::ParameterValue, 'Print the value of a setting')
            [CompletionResult]::new('set', 'set', [CompletionResultType]::ParameterValue, 'Set a value')
            [CompletionResult]::new('unset', 'unset', [CompletionResultType]::ParameterValue, 'Remove a value (reset to default)')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'devme;config;get' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;config;set' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;config;unset' {
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Emit machine-readable JSON instead of human-friendly output. Honored by every subcommand that prints data')
            [CompletionResult]::new('--no-input', '--no-input', [CompletionResultType]::ParameterName, 'Disable interactive prompts. Required for non-tty contexts (CI, agents). Fails closed: any prompt aborts with exit code 2')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational/progress output on stderr. Errors still print. Combines with `NO_COLOR=1` and `--no-color` for fully-quiet pipes')
            [CompletionResult]::new('--no-color', '--no-color', [CompletionResultType]::ParameterName, 'Strip ANSI color codes from all output. Also honored: the `NO_COLOR` environment variable (see https://no-color.org) and a non-TTY stdout')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'devme;config;help' {
            [CompletionResult]::new('get', 'get', [CompletionResultType]::ParameterValue, 'Print the value of a setting')
            [CompletionResult]::new('set', 'set', [CompletionResultType]::ParameterValue, 'Set a value')
            [CompletionResult]::new('unset', 'unset', [CompletionResultType]::ParameterValue, 'Remove a value (reset to default)')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'devme;config;help;get' {
            break
        }
        'devme;config;help;set' {
            break
        }
        'devme;config;help;unset' {
            break
        }
        'devme;config;help;help' {
            break
        }
        'devme;help' {
            [CompletionResult]::new('up', 'up', [CompletionResultType]::ParameterValue, 'Start the supervisor (or attach to a running one) and bring services up')
            [CompletionResult]::new('down', 'down', [CompletionResultType]::ParameterValue, 'Shut down this instance''s supervisor')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Print a snapshot of current service status')
            [CompletionResult]::new('restart', 'restart', [CompletionResultType]::ParameterValue, 'Restart a service')
            [CompletionResult]::new('stop', 'stop', [CompletionResultType]::ParameterValue, 'Stop a single service (keep the daemon running)')
            [CompletionResult]::new('start', 'start', [CompletionResultType]::ParameterValue, 'Start a single service')
            [CompletionResult]::new('logs', 'logs', [CompletionResultType]::ParameterValue, 'Tail logs for a service')
            [CompletionResult]::new('completions', 'completions', [CompletionResultType]::ParameterValue, 'Print a shell completion script. Pipe into your shell''s completion directory: `devme completions fish > ~/.config/fish/completions/devme.fish`')
            [CompletionResult]::new('doctor', 'doctor', [CompletionResultType]::ParameterValue, 'Diagnostic snapshot: service states + recent error logs. Designed for agents — outputs structured JSON with everything needed to diagnose failures without multiple round-trips')
            [CompletionResult]::new('config', 'config', [CompletionResultType]::ParameterValue, 'View or change devme global settings')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'devme;help;up' {
            break
        }
        'devme;help;down' {
            break
        }
        'devme;help;status' {
            break
        }
        'devme;help;restart' {
            break
        }
        'devme;help;stop' {
            break
        }
        'devme;help;start' {
            break
        }
        'devme;help;logs' {
            break
        }
        'devme;help;completions' {
            break
        }
        'devme;help;doctor' {
            break
        }
        'devme;help;config' {
            [CompletionResult]::new('get', 'get', [CompletionResultType]::ParameterValue, 'Print the value of a setting')
            [CompletionResult]::new('set', 'set', [CompletionResultType]::ParameterValue, 'Set a value')
            [CompletionResult]::new('unset', 'unset', [CompletionResultType]::ParameterValue, 'Remove a value (reset to default)')
            break
        }
        'devme;help;config;get' {
            break
        }
        'devme;help;config;set' {
            break
        }
        'devme;help;config;unset' {
            break
        }
        'devme;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
