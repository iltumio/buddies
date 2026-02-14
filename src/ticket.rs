use std::fmt;
use std::str::FromStr;

use anyhow::Result;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::protocol::TopicId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomTicket {
    pub room: String,
    pub topic: TopicId,
    pub endpoints: Vec<EndpointAddr>,
}

impl RoomTicket {
    pub fn new(room: String, topic: TopicId, endpoints: Vec<EndpointAddr>) -> Self {
        Self {
            room,
            topic,
            endpoints,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ticket serialization is infallible")
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

impl fmt::Display for RoomTicket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut text = data_encoding::BASE32_NOPAD.encode(&self.to_bytes());
        text.make_ascii_lowercase();
        write!(f, "{text}")
    }
}

impl FromStr for RoomTicket {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = data_encoding::BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes())?;
        Self::from_bytes(&bytes)
    }
}
