use std::fmt;

pub(crate) struct CliCommand<'a> {
    program: &'a str,
    args: &'a [String],
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CliCommandError {
    #[error("protocol 'cli' requires a non-empty 'command' list")]
    EmptyList,
    #[error("protocol 'cli' requires a non-empty executable as the first element of 'command'")]
    EmptyExecutable,
}

impl<'a> CliCommand<'a> {
    pub(crate) fn parse(command: Option<&'a [String]>) -> Result<Self, CliCommandError> {
        let (program, args) = command
            .and_then(<[String]>::split_first)
            .ok_or(CliCommandError::EmptyList)?;
        if program.is_empty() {
            return Err(CliCommandError::EmptyExecutable);
        }
        Ok(Self { program, args })
    }

    pub(crate) fn parts(&self) -> (&'a str, &'a [String]) {
        (self.program, self.args)
    }
}

impl fmt::Display for CliCommand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program)?;
        for arg in self.args {
            write!(f, " {arg}")?;
        }
        Ok(())
    }
}
