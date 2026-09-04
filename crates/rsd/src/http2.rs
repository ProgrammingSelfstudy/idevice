//! 极简 HTTP/2 客户端,跑在 `tunnel::TunnelStack` 的一条 TCP 连接上——只实现
//! RSD 实际用到的那一小撮东西(连接前言、SETTINGS、WINDOW_UPDATE、开流、按流
//! 收发 DATA 帧的流控),不是通用 HTTP/2 实现。

use std::collections::{HashMap, VecDeque};

use tokio::io::{AsyncRead, AsyncWrite};
use tunnel::{SocketHandle, TunnelStack};

use crate::http2_frame::{self, Frame, Setting};
use crate::RsdError;

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const DEFAULT_WINDOW: i64 = 65535;

pub struct Http2Client<'a, S> {
    stack: &'a mut TunnelStack<S>,
    handle: SocketHandle,
    cache: HashMap<u32, VecDeque<Vec<u8>>>,
    conn_send_window: i64,
    stream_send_windows: HashMap<u32, i64>,
    peer_initial_window: i64,
    recv_buf: Vec<u8>,
}

impl<'a, S> Http2Client<'a, S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    pub async fn new(stack: &'a mut TunnelStack<S>, handle: SocketHandle) -> Result<Self, RsdError> {
        stack.send_all(handle, CLIENT_PREFACE).await?;
        Ok(Self {
            stack,
            handle,
            cache: HashMap::new(),
            conn_send_window: DEFAULT_WINDOW,
            stream_send_windows: HashMap::new(),
            peer_initial_window: DEFAULT_WINDOW,
            recv_buf: Vec::new(),
        })
    }

    async fn next_frame(&mut self) -> Result<Frame, RsdError> {
        loop {
            if let Some((frame, consumed)) = Frame::parse(&self.recv_buf)? {
                self.recv_buf.drain(..consumed);
                return Ok(frame);
            }
            let chunk = self.stack.recv(self.handle).await?;
            if chunk.is_empty() {
                return Err(RsdError::ConnectionClosed);
            }
            self.recv_buf.extend(chunk);
        }
    }

    pub async fn set_settings(&mut self, settings: Vec<Setting>, stream_id: u32) -> Result<(), RsdError> {
        let frame = http2_frame::settings_frame(&settings, stream_id, 0);
        self.stack.send_all(self.handle, &frame).await?;
        Ok(())
    }

    pub async fn window_update(&mut self, increment: u32, stream_id: u32) -> Result<(), RsdError> {
        let frame = http2_frame::window_update_frame(stream_id, increment);
        self.stack.send_all(self.handle, &frame).await?;
        Ok(())
    }

    pub async fn open_stream(&mut self, stream_id: u32) -> Result<(), RsdError> {
        self.cache.entry(stream_id).or_default();
        let frame = http2_frame::headers_frame(stream_id);
        self.stack.send_all(self.handle, &frame).await?;
        Ok(())
    }

    pub async fn send(&mut self, payload: &[u8], stream_id: u32) -> Result<(), RsdError> {
        const MAX_FRAME: usize = 16384;
        if payload.is_empty() {
            let frame = http2_frame::data_frame(stream_id, &[]);
            self.stack.send_all(self.handle, &frame).await?;
            return Ok(());
        }
        for chunk in payload.chunks(MAX_FRAME) {
            let need = chunk.len() as i64;
            while self.conn_send_window < need || self.stream_send_window(stream_id) < need {
                self.pump().await?;
            }
            let frame = http2_frame::data_frame(stream_id, chunk);
            self.stack.send_all(self.handle, &frame).await?;
            self.conn_send_window -= need;
            *self.stream_send_windows.get_mut(&stream_id).unwrap() -= need;
        }
        Ok(())
    }

    fn stream_send_window(&mut self, stream_id: u32) -> i64 {
        *self
            .stream_send_windows
            .entry(stream_id)
            .or_insert(self.peer_initial_window)
    }

    pub async fn read(&mut self, stream_id: u32) -> Result<Vec<u8>, RsdError> {
        self.cache.entry(stream_id).or_default();
        loop {
            if let Some(d) = self.cache.get_mut(&stream_id).and_then(|c| c.pop_front()) {
                return Ok(d);
            }
            self.pump().await?;
        }
    }

    async fn pump(&mut self) -> Result<(), RsdError> {
        let frame = self.next_frame().await?;
        match frame {
            Frame::Settings { settings, flags } if flags != 1 => {
                for setting in &settings {
                    if let Setting::InitialWindowSize(new) = setting {
                        let delta = *new as i64 - self.peer_initial_window;
                        self.peer_initial_window = *new as i64;
                        for w in self.stream_send_windows.values_mut() {
                            *w += delta;
                        }
                    }
                }
                let ack = http2_frame::settings_frame(&[], 0, 1);
                self.stack.send_all(self.handle, &ack).await?;
            }
            Frame::WindowUpdate { stream_id, increment } => {
                if stream_id == 0 {
                    self.conn_send_window += increment as i64;
                } else {
                    let initial = self.peer_initial_window;
                    *self.stream_send_windows.entry(stream_id).or_insert(initial) += increment as i64;
                }
            }
            Frame::Data { stream_id, payload } => {
                let len = payload.len() as u32;
                self.cache.entry(stream_id).or_default().push_back(payload);
                if len > 0 {
                    let conn_update = http2_frame::window_update_frame(0, len);
                    let stream_update = http2_frame::window_update_frame(stream_id, len);
                    self.stack.send_all(self.handle, &conn_update).await?;
                    self.stack.send_all(self.handle, &stream_update).await?;
                }
            }
            Frame::Headers | Frame::Settings { .. } => {
                // SETTINGS ack、HEADERS——不需要做什么。
            }
        }
        Ok(())
    }
}
