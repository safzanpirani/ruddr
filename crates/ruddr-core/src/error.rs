//! Errors carry the process exit code. Exit codes are an API that agents
//! branch on: keep them stable and document any new one in the usage text,
//! the README, and the skill.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Success = 0,
    /// A run failed or another error happened.
    Failed = 1,
    /// Bad usage: unknown flags, missing arguments, invalid values.
    Usage = 2,
    /// Still running: `wait` timed out, or `result` on an unfinished run.
    Running = 3,
    /// A controller died and left non-terminal state behind.
    Stale = 4,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub exit: Exit,
    pub message: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Error { exit, message: message.into() }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Error::new(Exit::Failed, message)
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Error::new(Exit::Usage, message)
    }
    pub fn running(message: impl Into<String>) -> Self {
        Error::new(Exit::Running, message)
    }
    pub fn stale(message: impl Into<String>) -> Self {
        Error::new(Exit::Stale, message)
    }
    pub fn code(&self) -> i32 {
        self.exit as i32
    }
    /// Prefixes the message with context, keeping the exit code.
    pub fn context(mut self, context: impl fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::failed(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::failed(error.to_string())
    }
}

/// Adds context to any error convertible into [`Error`].
pub trait Context<T> {
    fn context(self, context: impl fmt::Display) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.map_err(|e| e.into().context(context))
    }
}
