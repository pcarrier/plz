# Install with: plz init fish | source
status is-interactive; or return

function __plz_execute --description 'Check the entire buffer before executing it'
    # Let fish handle incomplete syntax and completion selection itself.
    if commandline --paging-mode
        commandline -f cancel
        return
    end
    if not commandline --is-valid
        commandline -f execute
        return
    end
    # A pipe preserves newlines and never expands or evaluates the buffer.
    if commandline --current-buffer | command plz check --shell fish
        commandline -f execute
    else
        # Like fish's __fish_echo, leave room for repaint to move back up over
        # the prompt and buffer without erasing the check's output.
        set -l lines (math (commandline --line) + (count (fish_prompt)) - 2)
        string repeat -N --count=$lines \n >&2
        commandline -f repaint
    end
end

for mode in default insert
    bind --mode $mode enter __plz_execute
    bind --mode $mode ctrl-j __plz_execute
    bind --mode $mode ctrl-enter __plz_execute
end
