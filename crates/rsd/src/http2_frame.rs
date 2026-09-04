//! 极简 HTTP/2 帧格式(RFC 7540 §4.1)——RSD 借用 HTTP/2 只是为了拿它的连接
//! 前言/流复用/流控这几件事,不是真的在传 HTTP 语义,所以这里没有 HPACK 头部
//! 压缩,`HEADERS` 帧发出去内容压根不重要,只是用来"正式开一个流"。
//!
//! ```text
//! [3B 大端 payload 长度][1B 帧类型][1B flags][4B 大端 stream id][payload]
//! ```

use crate::RsdError;

const FRAME_HEADER_LEN: usize = 9;

#[derive(Debug, Clone)]
pub enum Setting {
    MaxConcurrentStreams(u32),
    InitialWindowSize(u32),
}

impl Setting {
    fn serialize(&self) -> Vec<u8> {
        let (id, value) = match self {
            Setting::MaxConcurrentStreams(v) => (0x03u16, *v),
            Setting::InitialWindowSize(v) => (0x04u16, *v),
        };
        let mut out = id.to_be_bytes().to_vec();
        out.extend(value.to_be_bytes());
        out
    }
}

#[derive(Debug)]
pub enum Frame {
    Settings { settings: Vec<Setting>, flags: u8 },
    WindowUpdate { stream_id: u32, increment: u32 },
    /// 收到的 stream id 不需要记——发起方总是知道自己刚开了哪个流。
    Headers,
    Data { stream_id: u32, payload: Vec<u8> },
}

impl Frame {
    /// 从 `buf` 头上解析一帧。不够一整帧时返回 `Ok(None)`,调用方读更多字节
    /// 再重试——保留 `buf` 里已有的字节,不吃掉不完整的帧,这样上层可以安全地
    /// 在读了一半的时候被 cancel。
    pub fn parse(buf: &[u8]) -> Result<Option<(Frame, usize)>, RsdError> {
        if buf.len() < FRAME_HEADER_LEN {
            return Ok(None);
        }
        let payload_len = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]) as usize;
        let frame_type = buf[3];
        let flags = buf[4];
        let stream_id = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);

        let total = FRAME_HEADER_LEN + payload_len;
        if buf.len() < total {
            return Ok(None);
        }
        let payload = &buf[FRAME_HEADER_LEN..total];

        let frame = match frame_type {
            0x00 => Frame::Data {
                stream_id,
                payload: payload.to_vec(),
            },
            0x01 => Frame::Headers,
            0x03 => return Err(RsdError::StreamReset),
            0x04 => {
                let mut settings = Vec::new();
                let mut i = 0;
                while i + 6 <= payload.len() {
                    let id = u16::from_be_bytes([payload[i], payload[i + 1]]);
                    let value = u32::from_be_bytes([
                        payload[i + 2],
                        payload[i + 3],
                        payload[i + 4],
                        payload[i + 5],
                    ]);
                    match id {
                        0x03 => settings.push(Setting::MaxConcurrentStreams(value)),
                        0x04 => settings.push(Setting::InitialWindowSize(value)),
                        _ => {} // 不认识的 setting 直接忽略,不是致命错误
                    }
                    i += 6;
                }
                Frame::Settings { settings, flags }
            }
            0x07 => {
                let msg = if payload.len() < 8 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&payload[8..]).into_owned()
                };
                return Err(RsdError::GoAway(msg));
            }
            0x08 => {
                if payload.len() != 4 {
                    return Err(RsdError::MalformedFrame(
                        "WINDOW_UPDATE payload must be 4 bytes".into(),
                    ));
                }
                Frame::WindowUpdate {
                    stream_id,
                    increment: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                }
            }
            other => return Err(RsdError::MalformedFrame(format!("unknown frame type {other:#x}"))),
        };

        Ok(Some((frame, total)))
    }
}

pub fn settings_frame(settings: &[Setting], stream_id: u32, flags: u8) -> Vec<u8> {
    let body: Vec<u8> = settings.iter().flat_map(|s| s.serialize()).collect();
    frame_header(body.len(), 0x04, flags, stream_id, &body)
}

pub fn window_update_frame(stream_id: u32, increment: u32) -> Vec<u8> {
    frame_header(4, 0x08, 0, stream_id, &increment.to_be_bytes())
}

pub fn headers_frame(stream_id: u32) -> Vec<u8> {
    // END_HEADERS(0x04) 标志——没有 CONTINUATION 帧跟着,body 本身是空的
    // (不需要真的编码任何头部字段,见模块文档)。
    frame_header(0, 0x01, 0x04, stream_id, &[])
}

pub fn data_frame(stream_id: u32, payload: &[u8]) -> Vec<u8> {
    frame_header(payload.len(), 0x00, 0x00, stream_id, payload)
}

fn frame_header(len: usize, frame_type: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len_bytes = (len as u32).to_be_bytes();
    let mut out = vec![len_bytes[1], len_bytes[2], len_bytes[3], frame_type, flags];
    out.extend(stream_id.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_frame_round_trips() {
        let bytes = settings_frame(
            &[Setting::MaxConcurrentStreams(100), Setting::InitialWindowSize(1048576)],
            0,
            0,
        );
        let (frame, consumed) = Frame::parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        match frame {
            Frame::Settings { settings, flags } => {
                assert_eq!(flags, 0);
                assert_eq!(settings.len(), 2);
            }
            other => panic!("wrong frame variant: {other:?}"),
        }
    }

    #[test]
    fn data_frame_round_trips() {
        let bytes = data_frame(3, b"hello");
        let (frame, consumed) = Frame::parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        match frame {
            Frame::Data { stream_id, payload } => {
                assert_eq!(stream_id, 3);
                assert_eq!(payload, b"hello");
            }
            other => panic!("wrong frame variant: {other:?}"),
        }
    }

    #[test]
    fn incomplete_frame_returns_none_not_an_error() {
        let bytes = data_frame(3, b"hello");
        assert!(Frame::parse(&bytes[..5]).unwrap().is_none());
    }
}
