//! Runs one argv vector (`fixtures/argv`, or a fuzz line) through `core::args`.
use fliks_engine_core::api::HostCaps;
use fliks_engine_core::args::{self, EncoderInput, IframeArgs, LiveArgs, RemuxArgs, VodArgs};
use fliks_engine_core::hw::Device;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn de<T: DeserializeOwned>(v: &Value, what: &str) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("<deserialize {what}>: {e}"))
}

/// `{argv}` or `{error}`, as the host stores `expected`.
pub fn run(v: &Value) -> Result<Value, String> {
    let caps: HostCaps = de(&v["hostCaps"], "hostCaps")?;
    let dev = Device {
        platform: caps.platform,
        render_node: v["env"]["renderNode"].as_str().map(String::from),
        opencl_device: v["env"]["openclDevice"].as_str().map(String::from),
    };
    let input = &v["input"];
    let out = match v["kind"].as_str() {
        Some("vod") => args::vod(&de::<VodArgs>(input, "vod")?, &caps, &dev),
        Some("remux") => args::remux(&de::<RemuxArgs>(input, "remux")?, &caps),
        Some("live") => Ok(args::live(&de::<LiveArgs>(input, "live")?, &caps, &dev)),
        Some("iframe") => Ok(args::iframe(&de::<IframeArgs>(input, "iframe")?)),
        Some("encoder") => args::encoder_args(
            input["id"].as_str().unwrap_or_default(),
            &de::<EncoderInput>(&input["input"], "encoder")?,
        ),
        k => return Err(format!("<unknown kind {k:?}>")),
    };
    Ok(match out {
        Ok(argv) => json!({ "argv": argv }),
        Err(error) => json!({ "error": error }),
    })
}

/// First differing argv element, or the two values.
pub fn first_diff(got: &Value, want: &Value) -> String {
    if let (Some(g), Some(w)) = (got["argv"].as_array(), want["argv"].as_array())
        && let Some(i) = (0..g.len().max(w.len())).find(|i| g.get(*i) != w.get(*i))
    {
        let at = |a: &Vec<Value>| a.get(i).map_or("<end>".into(), |x| x.to_string());
        let prev = i.checked_sub(1).map_or(String::new(), |p| w[p].to_string());
        return format!("argv[{i}] after {prev}: got {}, expected {}", at(g), at(w));
    }
    format!("got {got}, expected {want}")
}
