use std::fmt;

#[derive(Debug)]
pub struct PlatformError {
    message: String,
}

impl PlatformError {
    pub fn new(err: impl ToString) -> Self {
        Self {
            message: err.to_string(),
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlatformError {}
