//! `graphics.opengl`——FPS/GPU 利用率。跟 sysmontap 完全不同的协议形状：不用
//! `setConfig:`，直接 `startSamplingAtTimeInterval:`，设备之后周期性推数据。

use dtx::{AuxValue, DtxClient};
use serde::Serialize;
use tunnel::Transport;

#[derive(Serialize)]
struct GraphicsSample {
    #[serde(rename = "coreAnimationFramesPerSecond")]
    fps: f64,
    #[serde(rename = "deviceUtilizationPercent")]
    device_utilization_percent: f64,
    #[serde(rename = "rendererUtilizationPercent")]
    renderer_utilization_percent: f64,
    #[serde(rename = "tilerUtilizationPercent")]
    tiler_utilization_percent: f64,
    #[serde(rename = "inUseSystemMemoryBytes")]
    in_use_system_memory_bytes: f64,
}

pub struct Options {
    pub json: bool,
}

fn as_f64(v: &plist::Value) -> Option<f64> {
    v.as_real().or_else(|| v.as_signed_integer().map(|i| i as f64))
}

pub async fn run(dtx: &mut DtxClient<'_, Box<dyn Transport>>, opts: &Options) -> anyhow::Result<()> {
    let channel = dtx.make_channel("com.apple.instruments.server.services.graphics.opengl").await?;

    dtx.call_method_on(&channel, Some("startSamplingAtTimeInterval:"), vec![AuxValue::Double(0.0)], true).await?;
    let ack = dtx.read_message_on(&channel).await?;
    if ack.expects_reply {
        dtx.reply_to(&ack).await?;
    }
    if !opts.json {
        eprintln!("sampling started, waiting for frames (ctrl-c to quit)...\n");
        println!("{:>6} {:>6} {:>6} {:>10} FPS", "DEV%", "REND%", "TILE%", "MEM");
    }

    loop {
        let msg = dtx.read_message_on(&channel).await?;
        if msg.expects_reply {
            dtx.reply_to(&msg).await?;
        }
        let Some(dict) = msg.data.as_ref().and_then(|v| v.as_dictionary()) else {
            continue;
        };
        let get = |k: &str| dict.get(k).and_then(as_f64).unwrap_or(0.0);

        let sample = GraphicsSample {
            fps: get("CoreAnimationFramesPerSecond"),
            device_utilization_percent: get("Device Utilization %"),
            renderer_utilization_percent: get("Renderer Utilization %"),
            tiler_utilization_percent: get("Tiler Utilization %"),
            in_use_system_memory_bytes: get("In use system memory"),
        };

        if opts.json {
            println!("{}", serde_json::to_string(&sample).expect("GraphicsSample serializes"));
        } else {
            println!(
                "{:>6.0} {:>6.0} {:>6.0} {:>9.1}M {}",
                sample.device_utilization_percent,
                sample.renderer_utilization_percent,
                sample.tiler_utilization_percent,
                sample.in_use_system_memory_bytes / 1_048_576.0,
                sample.fps,
            );
        }
    }
}
