//! Exec-wrapper tables and option helpers (`approval_wrappers.py` and the wrapper tables of
//! `approval_detection.py`). Only the pieces the command-position walkers need so far.

pub const WRAPPER_WORDS: [&str; 14] = [
    "sudo", "env", "exec", "nohup", "setsid", "time", "command", "builtin", "nice", "timeout",
    "stdbuf", "ionice", "chrt", "taskset",
];

/// `_COMMAND_WRAPPER_WORDS` also holds `chroot`.
pub fn is_command_wrapper_word(name: &str) -> bool {
    WRAPPER_WORDS.contains(&name) || name == "chroot"
}

pub const SHELL_COMMAND_TRANSITIONS: [&str; 8] =
    ["if", "then", "else", "elif", "do", "while", "until", "!"];

const SUDO_OPTIONS_WITH_ARG: [&str; 10] = [
    "-c",
    "--close-from",
    "-g",
    "--group",
    "-h",
    "--host",
    "-p",
    "--prompt",
    "-u",
    "--user",
];

/// `_COMMAND_WRAPPER_OPTIONS_WITH_ARG`.
pub fn command_wrapper_options_with_arg(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "chroot" => &["--groups", "--userspec"],
        "sudo" => &SUDO_OPTIONS_WITH_ARG,
        "env" => &[
            "-a",
            "--argv0",
            "-C",
            "--chdir",
            "-S",
            "--split-string",
            "-u",
            "--unset",
        ],
        "exec" => &["-a"],
        "nice" => &["-n", "--adjustment"],
        "time" => &["-f", "--format", "-o", "--output"],
        "timeout" => &["-k", "--kill-after", "-s", "--signal"],
        "stdbuf" => &["-e", "--error", "-i", "--input", "-o", "--output"],
        "ionice" => &["-c", "--class", "-n", "--classdata"],
        _ => &[],
    }
}

/// `_COMMAND_WRAPPER_NON_EXECUTING_OPTIONS`.
pub fn non_executing_options(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "command" => &["-v", "-V"],
        "chrt" => &["-p", "--pid"],
        "ionice" => &["-p", "--pid", "--pgid", "--uid"],
        "taskset" => &["-p", "--pid"],
        "sudo" => &[
            "-l",
            "--list",
            "-v",
            "--validate",
            "-K",
            "--remove-timestamp",
        ],
        _ => &[],
    }
}

/// `_COMMAND_WRAPPER_POSITIONAL_ARGS`.
pub fn positional_args(wrapper: &str) -> usize {
    match wrapper {
        "chroot" | "chrt" | "taskset" | "timeout" => 1,
        _ => 0,
    }
}

/// `_WRAPPER_STR_OPERAND`.
pub fn str_operand(base: &str) -> &'static [&'static str] {
    match base {
        "sudo" => &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-U",
            "-D",
            "-R",
            "-T",
            "-a",
            "-c",
            "-r",
            "-t",
            "--user",
            "--group",
            "--host",
            "--prompt",
            "--chdir",
            "--auth-type",
            "--login-class",
            "--role",
            "--type",
            "--chroot",
            "--command-timeout",
            "--other-user",
        ],
        "doas" => &["-u", "-C"],
        "env" => &[
            "-u",
            "-C",
            "-a",
            "-S",
            "--unset",
            "--chdir",
            "--argv0",
            "--split-string",
        ],
        "exec" => &["-a"],
        "ionice" => &["-c", "-p", "--class", "--pid"],
        "stdbuf" => &["-i", "-o", "-e", "--input", "--output", "--error"],
        "time" => &["-f", "-o", "--format", "--output"],
        "timeout" => &["-s", "-k", "--signal", "--kill-after"],
        "xargs" => &[
            "-a",
            "-d",
            "-E",
            "-I",
            "-L",
            "-n",
            "-P",
            "-s",
            "--arg-file",
            "--delimiter",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--process-slot-var",
        ],
        _ => &[],
    }
}

/// `_WRAPPER_NUM_OPERAND`.
pub fn num_operand(base: &str) -> &'static [&'static str] {
    match base {
        "sudo" => &["-C", "--close-from"],
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-n", "--classdata"],
        _ => &[],
    }
}

/// `_WRAPPER_QUERY_OPTIONS`: lookup modes that name a program without running it.
pub fn query_options(base: &str) -> &'static [&'static str] {
    match base {
        "command" => &["-v", "-V"],
        "ionice" => &["-p", "--pid", "--pgid", "--uid"],
        "sudo" => &[
            "-l",
            "--list",
            "-v",
            "--validate",
            "-K",
            "--remove-timestamp",
        ],
        _ => &[],
    }
}

/// getopt walk of a short-option bundle: (leading flags, first option owning a value, the rest of
/// the word, which is that option's attached value).
pub fn short_bundle(base: &str, word: &str) -> (String, Option<String>, String) {
    let chars: Vec<char> = word.chars().collect();
    for k in 1..chars.len() {
        let option = format!("-{}", chars[k]);
        if str_operand(base).contains(&option.as_str())
            || num_operand(base).contains(&option.as_str())
        {
            return (
                chars[1..k].iter().collect(),
                Some(option),
                chars[k + 1..].iter().collect(),
            );
        }
    }
    (chars.iter().skip(1).collect(), None, String::new())
}

pub fn is_query_option(base: &str, word: &str) -> bool {
    let queries = query_options(base);
    if word.starts_with("--") {
        return queries.contains(&word.split('=').next().unwrap_or(""));
    }
    let (flags, option, _) = short_bundle(base, word);
    option.is_some_and(|o| queries.contains(&o.as_str()))
        || flags
            .chars()
            .any(|c| queries.contains(&format!("-{c}").as_str()))
}
