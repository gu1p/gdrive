//! `gdrive completions <SHELL>`: print a shell completion script to stdout.

use std::{
    error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
};

use clap_complete::Shell;

pub(crate) fn completions(shell: Shell, mut command: clap::Command) -> Result<(), Error> {
    let script = generate(shell, &mut command);

    let mut stdout = io::stdout().lock();
    match stdout.write_all(&script).and_then(|()| stdout.flush()) {
        // The reader closed the pipe (`gdrive completions bash | head`): it has what it wanted.
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result.map_err(Error::Write),
    }
}

/// Generates into memory first: `clap_complete` panics when its writer fails, for example when
/// stdout is a closed pipe, while writing the buffer out can report the error.
fn generate(shell: Shell, command: &mut clap::Command) -> Vec<u8> {
    let bin_name = command.get_name().to_owned();
    let mut script = Vec::new();
    clap_complete::generate(shell, command, bin_name, &mut script);
    script
}

#[derive(Debug)]
pub(crate) enum Error {
    Write(io::Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Write(_) => f.write_str("unable to write completion script to stdout"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Write(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, ValueEnum};
    use clap_complete::Shell;

    use super::generate;
    use crate::Cli;

    #[test]
    fn every_shell_gets_a_script_with_all_commands() {
        for &shell in Shell::value_variants() {
            let script = generate(shell, &mut Cli::command());
            let script = String::from_utf8(script).unwrap();

            assert!(script.contains("gdrive"), "{shell}");
            for word in ["files", "untrash", "completions", "acknowledge-abuse"] {
                assert!(script.contains(word), "{shell} script lacks {word}");
            }
        }
    }

    #[test]
    fn supported_shells() {
        let names: Vec<_> = Shell::value_variants()
            .iter()
            .map(ToString::to_string)
            .collect();
        for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
            assert!(names.iter().any(|name| name == shell), "{shell}");
        }
    }
}
