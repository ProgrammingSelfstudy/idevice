# idevice-rs-native

从零手写的 iOS 设备协议栈（usbmuxd/lockdownd/配对/CDTunnel隧道/RSD/DTX），不依赖
`idevice`（也不依赖任何其他现成的苹果协议实现）。目标跟 `idevice-perf` 一样：装包、
查进程、CPU/内存、FPS/GPU，但这次协议层完全自己维护。

## 为什么另起一个仓库、不在 idevice-perf 里改

`idevice-perf`（`../idevice-perf`）接的第三方 crate `idevice` 在 `ps`（sysmontap
CPU/内存/进程）这步卡死了——真机调试确认是 `idevice` 自己的 DTX 消息解码器不处理
`DTXBlockCompression`（设备汇报支持、且看起来无法通过能力协商关掉），块压缩过的 tap
消息解不出内容。与其等上游修或者 fork 别人的库打补丁，直接自己重新实现整条协议栈，
自己踩坑自己修。

**例外**：隧道内的 TCP/IP 协议栈用 `smoltcp`（成熟的通用网络协议栈，不是苹果协议，
自己写一遍 TCP 状态机没有意义）。密码学原语（TLS/证书生成）、plist 编解码也用现成
库——这些都是通用基础设施，不是这次要重新实现的苹果私有协议部分。

## 进度

- ✅ **阶段 1 - usbmuxd**（`crates/usbmuxd`）：`ListDevices`/`ReadPairRecord`/
  `SavePairRecord`/`ReadBUID`/`Connect` 全部手写实现，真机验证过（udid、BUID、
  完整配对记录都读得出来，字段一个不少）。见 `crates/usbmuxd/examples/`。
- ⬜ 阶段 2：lockdownd 明文查询
- ⬜ 阶段 3：配对（优先读 usbmuxd 缓存记录这条路径）
- ⬜ 阶段 4：CDTunnel 握手 + smoltcp
- ⬜ 阶段 5：RSD/XPC 握手
- ⬜ 阶段 6：DTX 消息层
- ⬜ 阶段 7：DTX 块压缩（全计划最大风险，`idevice` 卡住的原因）
- ⬜ 阶段 8：instruments（`ps`/`sysmon`/`fps`）
- ⬜ 阶段 9：`install`

权威计划文件：`/Users/test0/.claude/plans/kind-floating-dragon.md`（协议细节、字段
布局、每阶段验收标准都在里面）。
