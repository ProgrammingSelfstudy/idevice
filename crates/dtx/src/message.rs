//! DTX 消息帧格式——Instruments 用的这套二进制协议(不是这次要破解的谜团,
//! 字段布局是从真机抓包 + 调试日志核对过的):
//!
//! ```text
//! MessageHeader(32B): magic(u32,0x1F3D5B79) header_len(u32,32) fragment_id(u16)
//!                      fragment_count(u16) length(u32) identifier(u32)
//!                      conversation_index(u32) channel(i32) expects_reply(u32 bool)
//! PayloadHeader(16B): msg_type(u8) flags_a(u8) flags_b(u8) reserved(u8)
//!                      aux_length(u32) total_length(u32) flags(u32)
//! Aux(可选,aux_length 字节): 见 `AuxValue`
//! Payload data(剩余字节): NSKeyedArchiver 归档的 plist(见 `nka` 模块)
//! ```
//!
//! `channel` 字段有个不直观的符号翻转:线上字节按 `conversation_index` 的奇偶
//! 决定要不要取反。具体原理苹果没有公开文档,这里照着已验证过能在真机上跑通
//! 的行为原样实现(不是瞎猜的),重点是"收发双方按同一套规则解释这个字段"。

const HEADER_MAGIC: u32 = 0x1F3D_5B79;
const MESSAGE_HEADER_LEN: usize = 32;
const PAYLOAD_HEADER_LEN: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct MessageHeader {
    pub fragment_id: u16,
    pub fragment_count: u16,
    pub identifier: u32,
    pub conversation_index: u32,
    pub channel: i32,
    pub expects_reply: bool,
}

impl MessageHeader {
    pub fn new(identifier: u32, conversation_index: u32, channel: i32, expects_reply: bool) -> Self {
        Self {
            fragment_id: 0,
            fragment_count: 1,
            identifier,
            conversation_index,
            channel,
            expects_reply,
        }
    }

    fn serialize(&self, body_len: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(MESSAGE_HEADER_LEN);
        out.extend(HEADER_MAGIC.to_le_bytes());
        out.extend((MESSAGE_HEADER_LEN as u32).to_le_bytes());
        out.extend(self.fragment_id.to_le_bytes());
        out.extend(self.fragment_count.to_le_bytes());
        out.extend(body_len.to_le_bytes());
        out.extend(self.identifier.to_le_bytes());
        out.extend(self.conversation_index.to_le_bytes());
        out.extend(self.channel.to_le_bytes());
        out.extend(if self.expects_reply { 1u32 } else { 0 }.to_le_bytes());
        out
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PayloadHeader {
    msg_type: u8,
    aux_length: u32,
    total_length: u32,
}

impl PayloadHeader {
    fn method_invocation() -> Self {
        Self { msg_type: 2, ..Default::default() }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = vec![self.msg_type, 0, 0, 0];
        out.extend(self.aux_length.to_le_bytes());
        out.extend(self.total_length.to_le_bytes());
        out.extend(0u32.to_le_bytes()); // flags——目前没有用到,固定 0
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuxValue {
    Null,
    String(String),
    /// 原始字节数组——用来塞 NSKeyedArchiver 归档过的参数(比如
    /// `setConfig:` 的配置字典)。
    Array(Vec<u8>),
    U32(u32),
    I64(i64),
    Double(f64),
}

impl AuxValue {
    pub fn archived(value: plist::Value) -> Self {
        AuxValue::Array(crate::nka::encode(value))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Aux {
    pub values: Vec<AuxValue>,
}

impl Aux {
    pub fn new(values: Vec<AuxValue>) -> Self {
        Self { values }
    }

    /// **真机联调踩过好几次坑,记录一下走过的弯路**:
    ///
    /// 1. 最初照抄 `idevice` 那份参考实现,用的是"每个参数各自平铺、前面拿
    ///    `0x0a` 占位"的老式(iOS 16 及更早)编码——`setConfig:`/`start` 发过去
    ///    能收到一条 `{k:0, tv:65536}` 的确认,但紧接着连接就被设备挂断,
    ///    收不到真正的采样数据。
    /// 2. 查了 `pymobiledevice3`(另一个独立、成熟的参考实现)的
    ///    `dtx/message_aux.py`,发现出站方法参数按惯例应该包一层
    ///    `PrimitiveDictionary`——一开始理解成"一个 key(占位 Null)对应一个
    ///    装了所有参数的列表",改完之后设备反而完全没反应了(连那条 ack 都
    ///    没有)。
    /// 3. 又去核对了 `dtx/primitives.py` 里 `PDict` 真正的构建代码
    ///    (`for key, values in self.items(): for value in values: build(key);
    ///    build(value)`)才搞清楚:**key 是每个值前面都重复写一遍的**,不是
    ///    只写一次——也就是说线上字节其实是
    ///    `[key][value1] [key][value2] [key][value3] ...`,跟老式编码"每个
    ///    值前面一个 `0x0a`"这个身位其实是一样的,`PrimitiveDictionary` 跟
    ///    老式编码真正的区别只在最外层的 16 字节头(magic 是 `0x1F0` 不是
    ///    `0xF0`——这俩小端第一个字节碰巧相同,是另一个绕进去的弯路;
    ///    `body_len` 是 8 字节不是 4 字节),不在参数本身怎么排。
    fn serialize(&self) -> Vec<u8> {
        // 没有参数的调用(比如 `start`)完全不带 aux 段——不是带一个"空的
        // PrimitiveDictionary"。
        if self.values.is_empty() {
            return Vec::new();
        }
        let mut body = Vec::new();
        for v in &self.values {
            body.extend(0x0au32.to_le_bytes()); // key:每个值前面都重复写一次 PNULL 占位
            write_primitive(v, &mut body);
        }
        let mut out = Vec::with_capacity(16 + body.len());
        out.extend(0x1F0u32.to_le_bytes()); // type_and_flags:PrimitiveDictionary 的 magic
        out.extend(0u32.to_le_bytes()); // unknown flags
        out.extend((body.len() as u64).to_le_bytes()); // body_len——8 字节,跟老式头的 4 字节不一样
        out.extend(body);
        out
    }

    fn parse(bytes: &[u8]) -> Result<Self, DtxError> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        if bytes[0] == 0xF0 {
            return Self::parse_modern(bytes);
        }
        Self::parse_legacy(bytes)
    }

    /// 老式(iOS 16 及更早)编码:16 字节头 + 一串"类型标记 [+内容]"平铺,
    /// `0x0a`(PNULL)本身既当分隔符又能作为一个真正的 Null 值出现,靠下一个
    /// 4 字节是不是另一个类型标记来区分。
    fn parse_legacy(bytes: &[u8]) -> Result<Self, DtxError> {
        if bytes.len() < 16 {
            return Err(DtxError::MalformedAux("legacy aux shorter than 16-byte header".into()));
        }
        let body_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let body_end = (16 + body_len).min(bytes.len());
        let mut values = Vec::new();
        let mut pos = 16;
        while pos + 4 <= body_end {
            let ty = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if ty == 0x0a {
                continue; // PNULL 分隔符,跳过,下一轮读真正的值
            }
            let (value, consumed) = parse_primitive(ty, &bytes[pos..])?;
            values.push(value);
            pos += consumed;
        }
        Ok(Self { values })
    }

    /// 新式(iOS 17+/RSD 路径)编码:整个 aux 就是一个 `PrimitiveDictionary`
    /// 块,`[type+flags(4B)][unknown(4B)][body_len(8B)]` 后面跟一串
    /// `[key][value]` 对,读到 `body_len` 结束为止——key 我们这边只关心值,
    /// 直接丢掉(真机上收到的 key 基本都是占位用的 PNULL;哪怕不是,目前
    /// 用不上按 key 区分)。
    fn parse_modern(bytes: &[u8]) -> Result<Self, DtxError> {
        if bytes.len() < 16 {
            return Err(DtxError::MalformedAux("modern aux shorter than 16-byte header".into()));
        }
        let body_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let body_end = (16 + body_len).min(bytes.len());
        let mut pos = 16;
        let mut values = Vec::new();
        while pos + 4 <= body_end {
            let (_key, key_consumed) = parse_primitive_tagged(&bytes[pos..])?;
            pos += key_consumed;
            if pos + 4 > body_end {
                break;
            }
            let (value, value_consumed) = parse_primitive_tagged(&bytes[pos..])?;
            pos += value_consumed;
            values.push(value);
        }
        Ok(Self { values })
    }
}

/// 读一个"4 字节类型标记 + 内容"整体,返回值和总共吃掉的字节数(含类型标记
/// 那 4 字节)。
fn parse_primitive_tagged(bytes: &[u8]) -> Result<(AuxValue, usize), DtxError> {
    if bytes.len() < 4 {
        return Err(DtxError::MalformedAux("truncated primitive type tag".into()));
    }
    let ty = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let (value, consumed) = parse_primitive(ty, &bytes[4..])?;
    Ok((value, 4 + consumed))
}

/// 写一个值的"类型标记(4B) + 内容"——`parse_primitive`的反操作。
fn write_primitive(v: &AuxValue, out: &mut Vec<u8>) {
    match v {
        AuxValue::Null => out.extend(0x0au32.to_le_bytes()),
        AuxValue::String(s) => {
            out.extend(0x01u32.to_le_bytes());
            out.extend((s.len() as u32).to_le_bytes());
            out.extend(s.as_bytes());
        }
        AuxValue::Array(bytes) => {
            out.extend(0x02u32.to_le_bytes());
            out.extend((bytes.len() as u32).to_le_bytes());
            out.extend(bytes);
        }
        AuxValue::U32(v) => {
            out.extend(0x03u32.to_le_bytes());
            out.extend(v.to_le_bytes());
        }
        AuxValue::I64(v) => {
            out.extend(0x06u32.to_le_bytes());
            out.extend(v.to_le_bytes());
        }
        AuxValue::Double(v) => {
            out.extend(0x09u32.to_le_bytes());
            out.extend(v.to_le_bytes());
        }
    }
}

/// 读一个已知类型标记后面的内容,返回值和内容部分吃掉的字节数(不含类型
/// 标记那 4 字节,调用方已经读过了)。
fn parse_primitive(ty: u32, bytes: &[u8]) -> Result<(AuxValue, usize), DtxError> {
    match ty {
        0x0a => Ok((AuxValue::Null, 0)),
        0x01 => {
            let len = read_u32(bytes)? as usize;
            let s = std::str::from_utf8(get(bytes, 4, len)?)
                .map_err(|_| DtxError::MalformedAux("string value is not valid UTF-8".into()))?
                .to_string();
            Ok((AuxValue::String(s), 4 + len))
        }
        0x02 => {
            let len = read_u32(bytes)? as usize;
            Ok((AuxValue::Array(get(bytes, 4, len)?.to_vec()), 4 + len))
        }
        0x03 => Ok((AuxValue::U32(read_u32(bytes)?), 4)),
        0x06 => Ok((AuxValue::I64(read_u64(bytes)? as i64), 8)),
        0x09 => Ok((AuxValue::Double(f64::from_le_bytes(get(bytes, 0, 8)?.try_into().unwrap())), 8)),
        other => Err(DtxError::MalformedAux(format!("unknown aux primitive type {other:#x}"))),
    }
}

fn read_u32(bytes: &[u8]) -> Result<u32, DtxError> {
    Ok(u32::from_le_bytes(get(bytes, 0, 4)?.try_into().unwrap()))
}
fn read_u64(bytes: &[u8]) -> Result<u64, DtxError> {
    Ok(u64::from_le_bytes(get(bytes, 0, 8)?.try_into().unwrap()))
}
fn get(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], DtxError> {
    bytes
        .get(offset..offset + len)
        .ok_or_else(|| DtxError::MalformedAux("primitive value runs past end of aux buffer".into()))
}

#[derive(Debug, thiserror::Error)]
pub enum DtxError {
    #[error("malformed aux data: {0}")]
    MalformedAux(String),
    #[error("malformed message data: {0}")]
    MalformedData(#[from] crate::nka::NkaError),
    #[error(transparent)]
    Tunnel(#[from] tunnel::TunnelError),
    #[error("connection closed before a full message arrived")]
    ConnectionClosed,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub identifier: u32,
    pub conversation_index: u32,
    pub channel: i32,
    pub expects_reply: bool,
    pub aux: Aux,
    pub data: Option<plist::Value>,
}

pub struct OutgoingMessage {
    pub identifier: u32,
    pub conversation_index: u32,
    pub channel: i32,
    pub expects_reply: bool,
    pub aux: Aux,
    pub data: Option<plist::Value>,
}

impl OutgoingMessage {
    pub fn serialize(&self) -> Vec<u8> {
        let aux_bytes = self.aux.serialize();
        let data_bytes = match &self.data {
            Some(v) => crate::nka::encode(v.clone()),
            None => Vec::new(),
        };

        let mheader = MessageHeader::new(self.identifier, self.conversation_index, self.channel, self.expects_reply);
        let pheader = PayloadHeader {
            msg_type: PayloadHeader::method_invocation().msg_type,
            aux_length: aux_bytes.len() as u32,
            total_length: (aux_bytes.len() + data_bytes.len()) as u32,
        };

        let body_len = (PAYLOAD_HEADER_LEN + aux_bytes.len() + data_bytes.len()) as u32;
        let mut out = mheader.serialize(body_len);
        out.extend(pheader.serialize());
        out.extend(aux_bytes);
        out.extend(data_bytes);
        out
    }
}

/// 从累积的字节缓冲区里解析一整条(可能跨多个物理分片)DTX 消息。跟
/// `rsd` crate 里 HTTP/2 帧、XPC 消息的解析函数是同一个套路:不够一条完整
/// 消息时返回 `Ok(None)`,不吃掉任何字节,调用方读更多字节再retry。
///
/// 分片消息(`fragment_count > 1`)在真机联调里没实际遇到过——sysmontap/
/// installation_proxy 走的这几条路径观察到的都是单分片——但协议格式支持,
/// 这里按官方(?)行为原样实现:第一个分片(`fragment_id == 0`)只有消息头
/// 没有 body,是"接下来有 N 个分片"的预告,真正的内容从 `fragment_id == 1`
/// 开始按顺序拼起来。
pub fn parse(buf: &[u8]) -> Result<Option<(Message, usize)>, DtxError> {
    let mut pos = 0;
    let mut body = Vec::new();
    #[allow(unused_assignments)]
    let mut mheader = (0u32, 0u32, 0i32, false);

    loop {
        if buf.len() < pos + MESSAGE_HEADER_LEN {
            return Ok(None);
        }
        let h = &buf[pos..pos + MESSAGE_HEADER_LEN];
        let magic = u32::from_le_bytes(h[0..4].try_into().unwrap());
        if magic != HEADER_MAGIC {
            return Err(DtxError::MalformedAux(format!("bad DTX message magic {magic:#x}")));
        }
        let fragment_id = u16::from_le_bytes(h[8..10].try_into().unwrap());
        let fragment_count = u16::from_le_bytes(h[10..12].try_into().unwrap());
        let length = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
        let identifier = u32::from_le_bytes(h[16..20].try_into().unwrap());
        let conversation_index = u32::from_le_bytes(h[20..24].try_into().unwrap());
        let wire_channel = i32::from_le_bytes(h[24..28].try_into().unwrap());
        let expects_reply = u32::from_le_bytes(h[28..32].try_into().unwrap()) == 1;
        pos += MESSAGE_HEADER_LEN;

        if fragment_count > 1 && fragment_id == 0 {
            // 预告分片,没有 body,继续读下一个物理分片的头。
            continue;
        }

        if buf.len() < pos + length {
            return Ok(None);
        }
        body.extend_from_slice(&buf[pos..pos + length]);
        pos += length;

        let channel = if conversation_index.is_multiple_of(2) { -wire_channel } else { wire_channel };
        mheader = (identifier, conversation_index, channel, expects_reply);

        if fragment_id + 1 >= fragment_count.max(1) {
            break;
        }
    }

    if body.len() < PAYLOAD_HEADER_LEN {
        return Err(DtxError::MalformedAux("message body shorter than payload header".into()));
    }
    let aux_length = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    let total_length = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
    if PAYLOAD_HEADER_LEN + total_length > body.len() {
        return Err(DtxError::MalformedAux("payload total_length runs past message body".into()));
    }

    let aux_bytes = &body[PAYLOAD_HEADER_LEN..PAYLOAD_HEADER_LEN + aux_length];
    let aux = Aux::parse(aux_bytes)?;

    let data_start = PAYLOAD_HEADER_LEN + aux_length;
    let data_end = PAYLOAD_HEADER_LEN + total_length;
    let data = if data_end > data_start {
        Some(crate::nka::decode(&body[data_start..data_end])?)
    } else {
        None
    };

    let (identifier, conversation_index, channel, expects_reply) = mheader;
    Ok(Some((
        Message { identifier, conversation_index, channel, expects_reply, aux, data },
        pos,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_selector_call_with_no_args() {
        let msg = OutgoingMessage {
            identifier: 1,
            conversation_index: 0,
            channel: 0,
            expects_reply: true,
            aux: Aux::default(),
            data: Some(plist::Value::String("someMethod".into())),
        };
        let bytes = msg.serialize();
        let (decoded, consumed) = parse(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded.identifier, 1);
        assert_eq!(decoded.data, Some(plist::Value::String("someMethod".into())));
    }

    #[test]
    fn round_trips_aux_values() {
        let aux = Aux::new(vec![
            AuxValue::U32(42),
            AuxValue::String("hi".into()),
            AuxValue::archived(plist::Value::Boolean(true)),
        ]);
        let msg = OutgoingMessage {
            identifier: 2,
            conversation_index: 0,
            channel: 1,
            expects_reply: false,
            aux,
            data: None,
        };
        let bytes = msg.serialize();
        let (decoded, _) = parse(&bytes).unwrap().unwrap();
        assert_eq!(decoded.aux.values.len(), 3);
        assert_eq!(decoded.aux.values[0], AuxValue::U32(42));
        assert_eq!(decoded.aux.values[1], AuxValue::String("hi".into()));
    }

    #[test]
    fn incomplete_message_returns_none() {
        let msg = OutgoingMessage {
            identifier: 1,
            conversation_index: 0,
            channel: 0,
            expects_reply: false,
            aux: Aux::default(),
            data: Some(plist::Value::String("x".into())),
        };
        let bytes = msg.serialize();
        assert!(parse(&bytes[..bytes.len() - 1]).unwrap().is_none());
    }
}
