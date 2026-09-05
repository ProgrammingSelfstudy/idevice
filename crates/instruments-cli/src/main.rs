//! `idevice-instruments`——sysmon(CPU/内存)、fps(FPS/GPU)两个采集子命令的
//! 生产可用 CLI 封装：在 `crates/dtx/examples/{sysmon,fps}.rs`（真机验证过的
//! 协议实现）基础上，加上 `--json` 逐行输出和按进程名过滤，供上层(比如
//! automation 平台 client 里的 Go 子进程调用方)取代 pymobiledevice3。
//!
//! 目前只覆盖 macOS/Linux 路径——`usbmuxd` crate 连接写死 `/var/run/usbmuxd`，
//! 还没做 Windows 传输层，Windows 上仍需回退到 pymobiledevice3。

mod connect;
mod fps;
mod sysmon;

use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        "usage:\n  \
         idevice-instruments sysmon [-s <udid>] [-f <process-name>] [--json] [--interval-ms <ms>]\n  \
         idevice-instruments fps [-s <udid>] [--json]\n\n\
         -s, --udid <udid>     只有一台设备连接时可省略\n\
         -f, --process <name>  sysmon: 按进程名精确匹配，只输出这一个进程\n\
         --json                每个采样周期输出一行 JSON，供程序化调用方解析\n\
         --interval-ms <ms>    sysmon 采样间隔，默认 1000（对应平台原来 pymobiledevice3 -i 1000）"
    );
}

struct CommonArgs {
    udid: Option<String>,
    json: bool,
}

/// 从子命令自己的参数列表里摘出通用的 `-s`/`--json`，剩下的原样返回给子命令
/// 自己解析——避免每个子命令都重复写一遍这两个开关。
fn parse_common(args: Vec<String>) -> anyhow::Result<(CommonArgs, Vec<String>)> {
    let mut udid = None;
    let mut json = false;
    let mut rest = Vec::with_capacity(args.len());

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-s" | "--udid" => udid = Some(it.next().ok_or_else(|| anyhow::anyhow!("-s/--udid 后面缺参数"))?),
            "--json" => json = true,
            _ => rest.push(arg),
        }
    }
    Ok((CommonArgs { udid, json }, rest))
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::FAILURE;
    }
    let command = args.remove(0);

    let result = match command.as_str() {
        "sysmon" => run_sysmon(args).await,
        "fps" => run_fps(args).await,
        "-h" | "--help" => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        other => Err(anyhow::anyhow!("unknown subcommand: {other} (want \"sysmon\" or \"fps\")")),
    };

    if let Err(e) = result {
        eprintln!("error: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run_sysmon(args: Vec<String>) -> anyhow::Result<()> {
    let (common, rest) = parse_common(args)?;
    let mut process_filter = None;
    let mut interval_ms = 1000i64;

    let mut it = rest.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-f" | "--process" => {
                process_filter = Some(it.next().ok_or_else(|| anyhow::anyhow!("-f/--process 后面缺参数"))?)
            }
            "--interval-ms" => {
                let raw = it.next().ok_or_else(|| anyhow::anyhow!("--interval-ms 后面缺参数"))?;
                interval_ms = raw.parse().map_err(|_| anyhow::anyhow!("--interval-ms 不是合法整数: {raw}"))?;
            }
            other => anyhow::bail!("sysmon: unrecognized argument: {other}"),
        }
    }

    let (mut usbmuxd, device) = connect::select_device(common.udid.as_deref()).await?;
    if !common.json {
        eprintln!("device: {}", device.udid);
    }
    let (mut stack, handle) = connect::open_dtx_socket(&mut usbmuxd, &device).await?;
    let mut dtx = connect::handshake(&mut stack, handle).await?;

    sysmon::run(&mut dtx, &sysmon::Options { process_filter, json: common.json, interval_ms }).await
}

async fn run_fps(args: Vec<String>) -> anyhow::Result<()> {
    let (common, rest) = parse_common(args)?;
    if let Some(extra) = rest.first() {
        anyhow::bail!("fps: unrecognized argument: {extra}");
    }

    let (mut usbmuxd, device) = connect::select_device(common.udid.as_deref()).await?;
    if !common.json {
        eprintln!("device: {}", device.udid);
    }
    let (mut stack, handle) = connect::open_dtx_socket(&mut usbmuxd, &device).await?;
    let mut dtx = connect::handshake(&mut stack, handle).await?;

    fps::run(&mut dtx, &fps::Options { json: common.json }).await
}
