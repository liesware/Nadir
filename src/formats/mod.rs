//! Payload formats used by Nadir protocols.

pub(crate) mod json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFormat {
    Json,
}

impl PayloadFormat {
    pub(crate) fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
        }
    }

    pub(crate) fn encode(self, value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
        match self {
            Self::Json => json::encode(value),
        }
    }
}
