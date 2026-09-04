//! smoltcp 的 `Device` 实现——smoltcp 是同步/轮询式的 API(`receive`/`transmit`
//! 只是"现在手头有没有一个包"),真正的字节收发是靠后台任务从隧道读包塞进
//! `rx_queue`、`tx_queue` 里攒的包由 `TunnelStack::pump` 每次轮询之后取出来
//! 写回隧道——这个类型本身不接触 tokio/async,只是两个队列 + smoltcp 要求的
//! `Device`/`RxToken`/`TxToken` 三件套。

use std::collections::VecDeque;

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

pub struct SmolDevice {
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_queue: VecDeque<Vec<u8>>,
    mtu: usize,
}

impl SmolDevice {
    pub fn new(mtu: usize) -> Self {
        Self {
            rx_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            mtu,
        }
    }
}

pub struct SmolRxToken(Vec<u8>);
pub struct SmolTxToken<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for SmolRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

impl<'a> TxToken for SmolTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        self.0.push_back(buf);
        result
    }
}

impl Device for SmolDevice {
    type RxToken<'a> = SmolRxToken;
    type TxToken<'a> = SmolTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.rx_queue.pop_front()?;
        Some((SmolRxToken(packet), SmolTxToken(&mut self.tx_queue)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(SmolTxToken(&mut self.tx_queue))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps
    }
}
