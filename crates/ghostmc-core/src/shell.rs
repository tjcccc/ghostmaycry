//! POSIX shell quoting. Only used where a shell string is unavoidable (the
//! remote side of `ssh`, Ghostty's `command` setting). Everywhere else we pass
//! structured argv.

/// Quote a single argument so a POSIX shell reads it back verbatim.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Join argv into one shell-safe command line.
pub fn join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|a| quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn plain_words_stay_unquoted() {
        assert_eq!(quote("tmux"), "tmux");
        assert_eq!(quote("/tmp/tmux-501/default"), "/tmp/tmux-501/default");
    }

    #[test]
    fn dangerous_input_is_quoted() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("$3"), "'$3'");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("x; rm -rf ~"), "'x; rm -rf ~'");
    }

    #[test]
    fn round_trips_through_sh() {
        let nasty = [
            "$3",
            "it's \"quoted\"",
            "a\nb",
            "`whoami`",
            "$(id)",
            "~",
            "*",
            "back\\slash",
            "ünïcode 🎮",
        ];
        let line = format!("printf '%s\\0' {}", join(&nasty));
        let out = Command::new("sh").arg("-c").arg(&line).output().unwrap();
        let got: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        assert_eq!(got, nasty);
    }
}
