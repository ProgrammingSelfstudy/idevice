//! DTX——Instruments 用的方法调用协议,跑在直连 `com.apple.instruments.dtservicehub`
//! (RSD 发现出来的端口)那条 TCP 连接上,不再套 XPC/HTTP2(那两层只是拿来发现
//! 端口用的,见 `rsd` crate)。
//!
//! 顶层是"频道(channel)"概念:0 号频道是控制频道,用来请求打开新频道(每个
//! Instruments 服务——`sysmontap`/`graphics` 之类——各自占一个频道);频道内
//! 是"调用方法"(selector 字符串 + 参数列表,参数用 `AuxValue` 类型化编码,
//! 复杂参数额外套一层 NSKeyedArchiver)。

mod message;
mod nka;

use std::collections::{HashMap, VecDeque};

use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::{SocketHandle, TunnelStack};

pub use message::{Aux, AuxValue, DtxError, Message};

const CONTROL_CHANNEL: i32 = 0;

pub type Result<T> = std::result::Result<T, DtxError>;

pub struct DtxClient<'a, S> {
    stack: &'a mut TunnelStack<S>,
    handle: SocketHandle,
    recv_buf: Vec<u8>,
    queues: HashMap<i32, VecDeque<Message>>,
    next_identifier: u32,
    next_channel_code: i32,
}

/// 打开的一个频道——包一层薄的句柄,省得调用方到处传 `i32` 频道号。
pub struct Channel(i32);

impl<'a, S> DtxClient<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub fn new(stack: &'a mut TunnelStack<S>, handle: SocketHandle) -> Self {
        Self {
            stack,
            handle,
            recv_buf: Vec::new(),
            queues: HashMap::new(),
            next_identifier: 0,
            next_channel_code: 1,
        }
    }

    /// 请求打开一个具名服务频道(比如
    /// `"com.apple.instruments.server.services.sysmontap"`),返回后续调用要
    /// 用的频道句柄。
    pub async fn make_channel(&mut self, identifier: &str) -> Result<Channel> {
        let code = self.next_channel_code;
        self.next_channel_code += 1;

        let args = vec![
            AuxValue::U32(code as u32),
            AuxValue::archived(plist::Value::String(identifier.to_string())),
        ];
        self.call_method(
            CONTROL_CHANNEL,
            Some("_requestChannelWithCode:identifier:"),
            args,
            true,
        )
        .await?;
        // 打开频道的确认回复没有 body,直接读一条、不细究内容——跟真机联调
        // 时观察到的行为一致:没有 body 就是"知道了"。
        self.read_message(CONTROL_CHANNEL).await?;

        Ok(Channel(code))
    }

    /// 在指定频道上调用一个方法(`selector`),`expects_reply` 决定是否要
    /// 设置"期待回复"标志——真正等回复要另外调 `read_message`。
    pub async fn call_method(
        &mut self,
        channel: i32,
        selector: Option<&str>,
        args: Vec<AuxValue>,
        expects_reply: bool,
    ) -> Result<u32> {
        self.next_identifier += 1;
        let identifier = self.next_identifier;
        let msg = message::OutgoingMessage {
            identifier,
            conversation_index: 0,
            channel,
            expects_reply,
            aux: Aux::new(args),
            data: selector.map(|s| plist::Value::String(s.to_string())),
        };
        self.stack.send_all(self.handle, &msg.serialize()).await?;
        Ok(identifier)
    }

    pub async fn call_method_on(
        &mut self,
        channel: &Channel,
        selector: Option<&str>,
        args: Vec<AuxValue>,
        expects_reply: bool,
    ) -> Result<u32> {
        self.call_method(channel.0, selector, args, expects_reply).await
    }

    pub async fn read_message_on(&mut self, channel: &Channel) -> Result<Message> {
        self.read_message(channel.0).await
    }

    /// 对设备主动推过来、`expects_reply=true` 的消息回一个空确认——真机联调
    /// 发现 sysmontap 的 tap 协议在 `start` 之后会先推一条"k/tv"控制消息,
    /// 不回复的话连接很快被设备关掉,不会继续推真正的采样数据。回复用同一个
    /// `identifier`(让设备认出这是对哪条消息的回复)、`conversation_index`
    /// 在原值基础上加一(DTX 的请求/回复配对惯例:发起方 0,回复方 1)、
    /// `channel` 用收到时已经解出来的值(线上字节怎么编码由 `serialize`
    /// 自己处理,调用方不用管符号翻转那套规则)。
    pub async fn reply_to(&mut self, msg: &Message) -> Result<()> {
        let reply = message::OutgoingMessage {
            identifier: msg.identifier,
            conversation_index: msg.conversation_index + 1,
            channel: msg.channel,
            expects_reply: false,
            aux: Aux::default(),
            data: None,
        };
        self.stack.send_all(self.handle, &reply.serialize()).await?;
        Ok(())
    }

    /// 读指定频道的下一条消息——先看有没有已经收到、排队等着的(读别的频道时
    /// 顺手攒下的),没有就继续从连接上读,读到别的频道的消息就存进对应队列,
    /// 直到读到目标频道的为止。
    pub async fn read_message(&mut self, channel: i32) -> Result<Message> {
        loop {
            if let Some(msg) = self.queues.get_mut(&channel).and_then(|q| q.pop_front()) {
                return Ok(msg);
            }

            let msg = self.next_message().await?;
            if msg.channel == channel {
                return Ok(msg);
            }
            self.queues.entry(msg.channel).or_default().push_back(msg);
        }
    }

    async fn next_message(&mut self) -> Result<Message> {
        loop {
            if let Some((msg, consumed)) = message::parse(&self.recv_buf)? {
                self.recv_buf.drain(..consumed);
                return Ok(msg);
            }
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(DtxError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
    }
}
