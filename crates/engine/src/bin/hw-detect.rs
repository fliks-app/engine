//! Runs the host detection once and prints `{ capabilities, hostCaps }` (live parity checks).
//! Usage: `hw-detect [/dev/dri/renderD129]` with `FFMPEG_PATH` optionally set.
use fliks_engine::hw::{ffmpeg_detector, ffmpeg_path};
use fliks_engine_core::hw::selected_render_node;

#[tokio::main]
async fn main() {
    let node = std::env::args()
        .nth(1)
        .and_then(|n| selected_render_node(&n));
    let snap = ffmpeg_detector(ffmpeg_path())(node).await;
    let out =
        serde_json::json!({ "capabilities": snap.capabilities(), "hostCaps": snap.host_caps() });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
