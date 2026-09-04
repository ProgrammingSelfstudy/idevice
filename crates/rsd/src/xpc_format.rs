//! XPC 的两层二进制格式:
//!
//! - **外层包裹**(`XpcMessage`):`[4B LE magic 0x29b00b92][4B LE flags]
//!   [8B LE body_len][8B LE message_id][body]`——固定 24 字节头 + body。
//! - **内层对象**(`XpcObject`,就是 body 的内容):`[4B LE magic 0x42133742]
//!   [4B LE version(固定 5)]` 后面跟一棵类型标记的值树,每个值前面是个 4 字节
//!   LE 类型码,字符串/data/字典 key 都对齐到 4 字节边界(不足补 0)。

use indexmap::IndexMap;

use crate::RsdError;

pub type Dictionary = IndexMap<String, XpcObject>;

#[derive(Debug, Clone, PartialEq)]
pub enum XpcObject {
    Null,
    Bool(bool),
    Dictionary(Dictionary),
    Array(Vec<XpcObject>),
    Int64(i64),
    UInt64(u64),
    Double(f64),
    String(String),
    Data(Vec<u8>),
    Uuid(uuid::Uuid),
}

const OBJECT_MAGIC: u32 = 0x4213_3742;
const OBJECT_VERSION: u32 = 5;

const TYPE_NULL: u32 = 0x0000_1000;
const TYPE_BOOL: u32 = 0x0000_2000;
const TYPE_INT64: u32 = 0x0000_3000;
const TYPE_UINT64: u32 = 0x0000_4000;
const TYPE_DOUBLE: u32 = 0x0000_5000;
const TYPE_STRING: u32 = 0x0000_9000;
const TYPE_DATA: u32 = 0x0000_8000;
const TYPE_UUID: u32 = 0x0000_a000;
const TYPE_ARRAY: u32 = 0x0000_e000;
const TYPE_DICTIONARY: u32 = 0x0000_f000;

impl XpcObject {
    pub fn dict(entries: impl IntoIterator<Item = (&'static str, XpcObject)>) -> XpcObject {
        XpcObject::Dictionary(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn as_dictionary(&self) -> Option<&Dictionary> {
        match self {
            XpcObject::Dictionary(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            XpcObject::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            XpcObject::Int64(v) => Some(*v),
            XpcObject::UInt64(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    /// 阶段5还用不上——DTX/instruments 那层(阶段6+)读设备返回的布尔字段会
    /// 用到,先留着不删,不是死代码。
    #[allow(dead_code)]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            XpcObject::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(OBJECT_MAGIC.to_le_bytes());
        buf.extend(OBJECT_VERSION.to_le_bytes());
        self.encode_value(&mut buf);
        buf
    }

    fn encode_value(&self, buf: &mut Vec<u8>) {
        match self {
            XpcObject::Null => buf.extend(TYPE_NULL.to_le_bytes()),
            XpcObject::Bool(v) => {
                buf.extend(TYPE_BOOL.to_le_bytes());
                buf.extend([if *v { 1u8 } else { 0 }, 0, 0, 0]);
            }
            XpcObject::Int64(v) => {
                buf.extend(TYPE_INT64.to_le_bytes());
                buf.extend(v.to_le_bytes());
            }
            XpcObject::UInt64(v) => {
                buf.extend(TYPE_UINT64.to_le_bytes());
                buf.extend(v.to_le_bytes());
            }
            XpcObject::Double(v) => {
                buf.extend(TYPE_DOUBLE.to_le_bytes());
                buf.extend(v.to_le_bytes());
            }
            XpcObject::String(s) => {
                let len = s.len() + 1; // 含结尾 NUL
                buf.extend(TYPE_STRING.to_le_bytes());
                buf.extend((len as u32).to_le_bytes());
                buf.extend(s.as_bytes());
                buf.push(0);
                buf.extend(std::iter::repeat_n(0u8, padding(len)));
            }
            XpcObject::Data(d) => {
                buf.extend(TYPE_DATA.to_le_bytes());
                buf.extend((d.len() as u32).to_le_bytes());
                buf.extend(d);
                buf.extend(std::iter::repeat_n(0u8, padding(d.len())));
            }
            XpcObject::Uuid(u) => {
                buf.extend(TYPE_UUID.to_le_bytes());
                buf.extend(u.as_bytes());
            }
            XpcObject::Array(items) => {
                buf.extend(TYPE_ARRAY.to_le_bytes());
                let mut content = Vec::new();
                content.extend((items.len() as u32).to_le_bytes());
                for item in items {
                    item.encode_value(&mut content);
                }
                buf.extend((content.len() as u32).to_le_bytes());
                buf.extend(content);
            }
            XpcObject::Dictionary(dict) => {
                buf.extend(TYPE_DICTIONARY.to_le_bytes());
                let mut content = Vec::new();
                content.extend((dict.len() as u32).to_le_bytes());
                for (k, v) in dict {
                    let key_len = k.len() + 1;
                    content.extend(k.as_bytes());
                    content.push(0);
                    content.extend(std::iter::repeat_n(0u8, padding(key_len)));
                    v.encode_value(&mut content);
                }
                buf.extend((content.len() as u32).to_le_bytes());
                buf.extend(content);
            }
        }
    }

    pub fn decode(buf: &[u8]) -> Result<XpcObject, RsdError> {
        if buf.len() < 8 {
            return Err(RsdError::MalformedXpcObject("too short for header".into()));
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != OBJECT_MAGIC {
            return Err(RsdError::MalformedXpcObject(format!("bad magic {magic:#x}")));
        }
        let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        if version != OBJECT_VERSION {
            return Err(RsdError::MalformedXpcObject(format!(
                "unexpected version {version}"
            )));
        }
        let mut cursor = Cursor { buf: &buf[8..], pos: 0 };
        decode_value(&mut cursor)
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RsdError> {
        if self.pos + n > self.buf.len() {
            return Err(RsdError::MalformedXpcObject("unexpected end of buffer".into()));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn take_u32(&mut self) -> Result<u32, RsdError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

fn decode_value(cursor: &mut Cursor) -> Result<XpcObject, RsdError> {
    let ty = cursor.take_u32()?;
    match ty {
        TYPE_NULL => Ok(XpcObject::Null),
        TYPE_BOOL => Ok(XpcObject::Bool(cursor.take(4)?[0] != 0)),
        TYPE_INT64 => Ok(XpcObject::Int64(i64::from_le_bytes(
            cursor.take(8)?.try_into().unwrap(),
        ))),
        TYPE_UINT64 => Ok(XpcObject::UInt64(u64::from_le_bytes(
            cursor.take(8)?.try_into().unwrap(),
        ))),
        TYPE_DOUBLE => Ok(XpcObject::Double(f64::from_le_bytes(
            cursor.take(8)?.try_into().unwrap(),
        ))),
        TYPE_STRING => {
            let len = cursor.take_u32()? as usize;
            let bytes = cursor.take(len)?;
            cursor.take(padding(len))?;
            let s = std::ffi::CStr::from_bytes_with_nul(bytes)
                .map_err(|_| RsdError::MalformedXpcObject("string missing NUL terminator".into()))?
                .to_str()
                .map_err(|_| RsdError::MalformedXpcObject("string is not valid UTF-8".into()))?
                .to_string();
            Ok(XpcObject::String(s))
        }
        TYPE_DATA => {
            let len = cursor.take_u32()? as usize;
            let data = cursor.take(len)?.to_vec();
            cursor.take(padding(len))?;
            Ok(XpcObject::Data(data))
        }
        TYPE_UUID => {
            let bytes: [u8; 16] = cursor.take(16)?.try_into().unwrap();
            Ok(XpcObject::Uuid(uuid::Uuid::from_bytes(bytes)))
        }
        TYPE_ARRAY => {
            let _content_len = cursor.take_u32()?;
            let count = cursor.take_u32()?;
            let mut items = Vec::with_capacity(count as usize);
            for _ in 0..count {
                items.push(decode_value(cursor)?);
            }
            Ok(XpcObject::Array(items))
        }
        TYPE_DICTIONARY => {
            let _content_len = cursor.take_u32()?;
            let count = cursor.take_u32()?;
            let mut dict = IndexMap::new();
            for _ in 0..count {
                let key_start = cursor.pos;
                let nul_at = cursor.buf[key_start..]
                    .iter()
                    .position(|&b| b == 0)
                    .ok_or_else(|| RsdError::MalformedXpcObject("dict key missing NUL".into()))?;
                let key_len = nul_at + 1;
                let key_bytes = cursor.take(key_len)?;
                let key = std::str::from_utf8(&key_bytes[..key_bytes.len() - 1])
                    .map_err(|_| RsdError::MalformedXpcObject("dict key is not valid UTF-8".into()))?
                    .to_string();
                cursor.take(padding(key_len))?;
                dict.insert(key, decode_value(cursor)?);
            }
            Ok(XpcObject::Dictionary(dict))
        }
        other => Err(RsdError::MalformedXpcObject(format!("unknown type tag {other:#x}"))),
    }
}

fn padding(len: usize) -> usize {
    (4 - (len % 4)) % 4
}

/// XPC 消息外层包裹(24 字节固定头 + body)。
pub struct XpcMessage {
    pub flags: u32,
    pub object: Option<XpcObject>,
    pub message_id: u64,
}

const MESSAGE_MAGIC: u32 = 0x29b0_0b92;
const MESSAGE_HEADER_LEN: usize = 24;

pub mod flags {
    pub const ALWAYS_SET: u32 = 0x0000_0001;
    pub const DATA_FLAG: u32 = 0x0000_0100;
    /// 阶段5的握手用不上(消息都是单向通知,不等回复)——阶段6+ 真的发
    /// DTX/instruments 请求、等设备回包时才会用到,先留着不删。
    #[allow(dead_code)]
    pub const WANTING_REPLY: u32 = 0x0001_0000;
    pub const INIT_HANDSHAKE: u32 = 0x0040_0000;
}

impl XpcMessage {
    pub fn new(flags: u32, object: Option<XpcObject>, message_id: u64) -> Self {
        Self { flags, object, message_id }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MESSAGE_MAGIC.to_le_bytes().to_vec();
        out.extend(self.flags.to_le_bytes());
        match &self.object {
            Some(obj) => {
                let body = obj.encode();
                out.extend((body.len() as u64).to_le_bytes());
                out.extend(self.message_id.to_le_bytes());
                out.extend(body);
            }
            None => {
                out.extend(0u64.to_le_bytes());
                out.extend(self.message_id.to_le_bytes());
            }
        }
        out
    }

    /// 从 `buf` 头上解析一条完整消息,返回消息本身和吃掉的字节数——跟
    /// `http2_frame::Frame::parse` 一个道理,不够一条完整消息时返回
    /// `Ok(None)` 而不是报错,调用方读更多字节再重试。
    pub fn parse(buf: &[u8]) -> Result<Option<(XpcMessage, usize)>, RsdError> {
        if buf.len() < MESSAGE_HEADER_LEN {
            return Ok(None);
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MESSAGE_MAGIC {
            return Err(RsdError::MalformedXpcObject(format!(
                "bad XPC message magic {magic:#x}"
            )));
        }
        let flags = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let body_len = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
        let message_id = u64::from_le_bytes(buf[16..24].try_into().unwrap());

        let total = MESSAGE_HEADER_LEN + body_len;
        if buf.len() < total {
            return Ok(None);
        }

        let object = if body_len > 0 {
            Some(XpcObject::decode(&buf[MESSAGE_HEADER_LEN..total])?)
        } else {
            None
        };

        Ok(Some((XpcMessage { flags, object, message_id }, total)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_object_round_trips() {
        for obj in [
            XpcObject::Null,
            XpcObject::Bool(true),
            XpcObject::Int64(-42),
            XpcObject::UInt64(42),
            XpcObject::Double(3.5),
            XpcObject::String("hello".into()),
            XpcObject::Data(vec![1, 2, 3, 4, 5]),
            XpcObject::Uuid(uuid::Uuid::nil()),
        ] {
            let encoded = obj.encode();
            let decoded = XpcObject::decode(&encoded).unwrap();
            assert_eq!(obj, decoded);
        }
    }

    #[test]
    fn dictionary_round_trips_and_preserves_key_order() {
        let dict = XpcObject::dict([
            ("zeta", XpcObject::Int64(1)),
            ("alpha", XpcObject::String("s".into())),
            ("nested", XpcObject::dict([("inner", XpcObject::Bool(false))])),
        ]);
        let encoded = dict.encode();
        let decoded = XpcObject::decode(&encoded).unwrap();
        assert_eq!(dict, decoded);
    }

    #[test]
    fn array_round_trips() {
        let arr = XpcObject::Array(vec![XpcObject::Int64(1), XpcObject::Int64(2), XpcObject::Int64(3)]);
        assert_eq!(arr.clone(), XpcObject::decode(&arr.encode()).unwrap());
    }

    #[test]
    fn message_wrapper_round_trips() {
        let msg = XpcMessage::new(
            flags::ALWAYS_SET | flags::DATA_FLAG,
            Some(XpcObject::dict([("k", XpcObject::String("v".into()))])),
            7,
        );
        let bytes = msg.encode();
        let (decoded, consumed) = XpcMessage::parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded.flags, flags::ALWAYS_SET | flags::DATA_FLAG);
        assert_eq!(decoded.message_id, 7);
        assert_eq!(decoded.object.unwrap().as_dictionary().unwrap().get("k").unwrap().as_string(), Some("v"));
    }

    #[test]
    fn empty_message_wrapper_round_trips() {
        let msg = XpcMessage::new(flags::INIT_HANDSHAKE | flags::ALWAYS_SET, None, 0);
        let bytes = msg.encode();
        let (decoded, consumed) = XpcMessage::parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert!(decoded.object.is_none());
    }
}
