use bytes::BytesMut;
use serde_json::Value;
use tokio_util::codec::{Decoder, Encoder};

#[derive(Default)]
pub struct LspCodec {
    content_length: Option<usize>,
    buffer: String,
}

impl Decoder for LspCodec {
    type Item = Value;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Try to find Content-Length header
        if self.content_length.is_none() {
            if let Some(idx) = src.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&src[..idx]);
                if let Some(len_str) = header
                    .lines()
                    .find(|line| line.starts_with("Content-Length: "))
                {
                    if let Ok(len) = len_str["Content-Length: ".len()..].trim().parse() {
                        self.content_length = Some(len);
                        src.advance(idx + 4); // Skip headers
                    }
                }
            } else {
                return Ok(None); // Need more data
            }
        }

        // Try to read message body
        if let Some(len) = self.content_length {
            if src.len() >= len {
                let json_data = String::from_utf8_lossy(&src[..len]).to_string();
                src.advance(len);
                self.content_length = None;
                
                match serde_json::from_str(&json_data) {
                    Ok(value) => Ok(Some(value)),
                    Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                }
            } else {
                Ok(None) // Need more data
            }
        } else {
            Ok(None)
        }
    }
}

impl Encoder<Value> for LspCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Value, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let msg = serde_json::to_string(&item)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        
        // Write headers
        let headers = format!("Content-Length: {}\r\n\r\n", msg.len());
        dst.extend_from_slice(headers.as_bytes());
        
        // Write message
        dst.extend_from_slice(msg.as_bytes());
        Ok(())
    }
}