//! AFC(Apple File Conduit)——设备文件系统访问协议,装包前把 .ipa 传到设备
//! `/PublicStaging` 目录用的就是这个。跟 `installation_proxy`/
//! `mobile_image_mounter` 是同一批经典 lockdown 服务原样代理到隧道上的
//! `*.shim.remote`,连接握手复用 `rsd::ShimServiceConnection`——但
//! `RSDCheckin` 握手一结束,后续走的不是 plist 帧,是 AFC 自己的二进制协议
//! (参考 pymobiledevice3 的 `services/afc.py`):
//!
//! ```text
//! Header(40B): magic(8B,"CFA6LPAA") entire_length(u64) this_length(u64)
//!               packet_num(u64) operation(u64)
//! Body(entire_length - 40 字节): 按 operation 而定的定长字段 + 变长数据
//! ```
//!
//! `this_length` 一般等于 `entire_length`,只有 `WRITE` 例外——它标记"头 +
//! 8 字节 handle"这段固定长度(48),后面跟的才是真正的文件内容字节。

use std::collections::HashMap;
use std::net::Ipv6Addr;

use rsd::ShimServiceConnection;
use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::{SocketHandle, TunnelStack};

pub const RSD_SERVICE_NAME: &str = "com.apple.afc.shim.remote";

const AFC_MAGIC: [u8; 8] = *b"CFA6LPAA";
const HEADER_LEN: usize = 40;

mod opcode {
    pub const STATUS: u64 = 0x01;
    pub const READ_DIR: u64 = 0x03;
    pub const REMOVE_PATH: u64 = 0x08;
    pub const MAKE_DIR: u64 = 0x09;
    pub const GET_FILE_INFO: u64 = 0x0A;
    pub const GET_DEVINFO: u64 = 0x0B;
    pub const FILE_OPEN: u64 = 0x0D;
    pub const READ: u64 = 0x0F;
    pub const WRITE: u64 = 0x10;
    pub const FILE_CLOSE: u64 = 0x14;
}

/// `FILE_OPEN` 请求里的打开模式——数值跟 AFC 协议本身对齐,不是我们编的。
#[derive(Debug, Clone, Copy)]
pub enum FopenMode {
    RdOnly = 1,
    Rw = 2,
    WrOnly = 3,
    Wr = 4,
    Append = 5,
    RdAppend = 6,
}

#[derive(Debug, thiserror::Error)]
pub enum AfcError {
    #[error(transparent)]
    Rsd(#[from] rsd::RsdError),
    #[error(transparent)]
    Tunnel(#[from] tunnel::TunnelError),
    #[error("device reported AFC error code {0}")]
    Device(u64),
    #[error("malformed AFC response: {0}")]
    Malformed(String),
    #[error("connection closed before a full AFC packet arrived")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, AfcError>;

pub struct AfcClient<'a, S> {
    stack: &'a mut TunnelStack<S>,
    handle: SocketHandle,
    recv_buf: Vec<u8>,
    next_packet_num: u64,
}

impl<'a, S> AfcClient<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub async fn connect(stack: &'a mut TunnelStack<S>, server_addr: Ipv6Addr, port: u16) -> Result<Self> {
        let conn = ShimServiceConnection::connect(stack, server_addr, port, "idevice-rs-native").await?;
        let (stack, handle, leftover) = conn.into_raw();
        Ok(Self {
            stack,
            handle,
            recv_buf: leftover,
            next_packet_num: 0,
        })
    }

    async fn send_packet(&mut self, operation: u64, data: &[u8], this_length: Option<u64>) -> Result<u64> {
        let packet_num = self.next_packet_num;
        self.next_packet_num += 1;
        let entire_length = (HEADER_LEN + data.len()) as u64;
        let this_length = this_length.unwrap_or(entire_length);

        let mut out = Vec::with_capacity(HEADER_LEN + data.len());
        out.extend(AFC_MAGIC);
        out.extend(entire_length.to_le_bytes());
        out.extend(this_length.to_le_bytes());
        out.extend(packet_num.to_le_bytes());
        out.extend(operation.to_le_bytes());
        out.extend(data);
        self.stack.send_all(self.handle, &out).await?;
        Ok(packet_num)
    }

    /// 读一整条 AFC 响应包,返回 `(operation, body)`——`body` 不含 40 字节头。
    async fn recv_packet(&mut self) -> Result<(u64, Vec<u8>)> {
        while self.recv_buf.len() < HEADER_LEN {
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(AfcError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
        if self.recv_buf[0..8] != AFC_MAGIC {
            return Err(AfcError::Malformed(format!("bad AFC magic {:x?}", &self.recv_buf[0..8])));
        }
        let entire_length = u64::from_le_bytes(self.recv_buf[8..16].try_into().unwrap()) as usize;
        let operation = u64::from_le_bytes(self.recv_buf[32..40].try_into().unwrap());

        while self.recv_buf.len() < entire_length {
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(AfcError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
        let body = self.recv_buf[HEADER_LEN..entire_length].to_vec();
        self.recv_buf.drain(..entire_length);
        Ok((operation, body))
    }

    /// 发一条请求、等对应响应——AFC 是同步的一问一答(每条连接同一时间只有
    /// 一个未完成的请求),不需要像 pymobiledevice3 那样按 `packet_num` 做
    /// 后台分发。`STATUS` 类型的响应按错误码处理;其余类型(不管设备标的是
    /// `DATA` 还是别的操作码)一律当成"成功,body 就是结果"——这跟
    /// pymobiledevice3 的行为一致,设备侧对不同请求回的 opcode 不完全统一。
    async fn do_operation(&mut self, operation: u64, data: &[u8]) -> Result<Vec<u8>> {
        self.send_packet(operation, data, None).await?;
        let (op, body) = self.recv_packet().await?;
        if op == opcode::STATUS {
            let code = read_u64(&body)?;
            if code != 0 {
                return Err(AfcError::Device(code));
            }
        }
        Ok(body)
    }

    pub async fn get_device_info(&mut self) -> Result<HashMap<String, String>> {
        let data = self.do_operation(opcode::GET_DEVINFO, &[]).await?;
        parse_kv_list(&data)
    }

    /// 列目录,已经去掉 `.`/`..` 这两条。
    pub async fn listdir(&mut self, path: &str) -> Result<Vec<String>> {
        let data = self.do_operation(opcode::READ_DIR, &cstring(path)).await?;
        let names = parse_cstring_list(&data)?;
        Ok(names.into_iter().skip(2).collect())
    }

    pub async fn stat(&mut self, path: &str) -> Result<HashMap<String, String>> {
        let data = self.do_operation(opcode::GET_FILE_INFO, &cstring(path)).await?;
        parse_kv_list(&data)
    }

    pub async fn makedir(&mut self, path: &str) -> Result<()> {
        self.do_operation(opcode::MAKE_DIR, &cstring(path)).await?;
        Ok(())
    }

    pub async fn remove(&mut self, path: &str) -> Result<()> {
        self.do_operation(opcode::REMOVE_PATH, &cstring(path)).await?;
        Ok(())
    }

    pub async fn fopen(&mut self, path: &str, mode: FopenMode) -> Result<u64> {
        let mut req = (mode as u64).to_le_bytes().to_vec();
        req.extend(cstring(path));
        let data = self.do_operation(opcode::FILE_OPEN, &req).await?;
        read_u64(&data)
    }

    pub async fn fclose(&mut self, handle: u64) -> Result<()> {
        self.do_operation(opcode::FILE_CLOSE, &handle.to_le_bytes()).await?;
        Ok(())
    }

    pub async fn fread(&mut self, handle: u64, size: u64) -> Result<Vec<u8>> {
        let mut req = handle.to_le_bytes().to_vec();
        req.extend(size.to_le_bytes());
        self.do_operation(opcode::READ, &req).await
    }

    /// `this_length=48` 是 40 字节头 + 8 字节 handle——`WRITE` 的固定字段
    /// 到这里为止,后面才是真正的文件内容。
    pub async fn fwrite(&mut self, handle: u64, data: &[u8]) -> Result<()> {
        let mut body = handle.to_le_bytes().to_vec();
        body.extend(data);
        self.send_packet(opcode::WRITE, &body, Some(48)).await?;
        let (op, resp) = self.recv_packet().await?;
        if op == opcode::STATUS {
            let code = read_u64(&resp)?;
            if code != 0 {
                return Err(AfcError::Device(code));
            }
        }
        Ok(())
    }

    /// 打开(创建/截断)+ 写 + 关闭的组合,`set_file_contents` 的等价物。
    pub async fn write_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let handle = self.fopen(path, FopenMode::WrOnly).await?;
        let result = self.fwrite(handle, data).await;
        self.fclose(handle).await?;
        result
    }

    pub async fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let size = self
            .stat(path)
            .await?
            .get("st_size")
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| AfcError::Malformed("stat response missing st_size".into()))?;
        let handle = self.fopen(path, FopenMode::RdOnly).await?;
        let result = self.fread(handle, size).await;
        self.fclose(handle).await?;
        result
    }

    pub async fn close(self) -> Result<()> {
        self.stack.close(self.handle).await?;
        Ok(())
    }
}

fn cstring(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn read_u64(bytes: &[u8]) -> Result<u64> {
    bytes
        .get(0..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| AfcError::Malformed("expected 8 bytes for a u64 field".into()))
}

/// 空字节分隔、交替出现的 key/value 字符串列表(`GET_DEVINFO`/`GET_FILE_INFO`
/// 的响应形状),最后一个字符串后面也有一个空字节结尾。
fn parse_kv_list(data: &[u8]) -> Result<HashMap<String, String>> {
    let parts = parse_cstring_list(data)?;
    if parts.len() % 2 != 0 {
        return Err(AfcError::Malformed("AFC key/value list is not evenly paired".into()));
    }
    let mut map = HashMap::new();
    let mut it = parts.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        map.insert(k, v);
    }
    Ok(map)
}

fn parse_cstring_list(data: &[u8]) -> Result<Vec<String>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::str::from_utf8(data)
        .map_err(|_| AfcError::Malformed("AFC string list is not valid UTF-8".into()))?
        .trim_end_matches('\0');
    if text.is_empty() {
        return Ok(Vec::new());
    }
    Ok(text.split('\0').map(str::to_string).collect())
}
