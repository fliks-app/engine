//! ffprobe JSON, read as leniently as the host does: a field of an unexpected type is absent.
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;

fn string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(String::from))
}

fn float<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(Value::deserialize(d)?.as_f64())
}

fn list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Array(a) => a,
        _ => vec![],
    })
}

/// Stream tags in file order: the first key matching case-insensitively wins, like `Object.keys`.
#[derive(Debug, Default, Clone)]
pub struct Tags(Vec<(String, String)>);

impl Tags {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.to_lowercase() == key.to_lowercase())
            .map(|(_, v)| v.as_str())
    }
}

impl<'de> Deserialize<'de> for Tags {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Tags;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Tags, A::Error> {
                let mut out = vec![];
                while let Some((k, v)) = m.next_entry::<String, Value>()? {
                    if let Value::String(v) = v {
                        out.push((k, v));
                    }
                }
                Ok(Tags(out))
            }
            fn visit_unit<E>(self) -> Result<Tags, E> {
                Ok(Tags::default())
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Disposition {
    #[serde(default, deserialize_with = "float")]
    pub forced: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub hearing_impaired: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub default: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub attached_pic: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub comment: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub visual_impaired: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub descriptions: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Stream {
    #[serde(default, deserialize_with = "float")]
    pub index: Option<f64>,
    #[serde(default, deserialize_with = "string")]
    pub codec_name: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub codec_type: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub profile: Option<String>,
    #[serde(default, deserialize_with = "float")]
    pub level: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub width: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub height: Option<f64>,
    #[serde(default, deserialize_with = "string")]
    pub display_aspect_ratio: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub pix_fmt: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub r_frame_rate: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub avg_frame_rate: Option<String>,
    #[serde(default, deserialize_with = "float")]
    pub has_b_frames: Option<f64>,
    #[serde(default, deserialize_with = "string")]
    pub time_base: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub start_time: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub duration: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub bit_rate: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub bits_per_raw_sample: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub color_space: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub color_transfer: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub color_primaries: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub color_range: Option<String>,
    #[serde(default, deserialize_with = "list")]
    pub side_data_list: Vec<Value>,
    #[serde(default, deserialize_with = "float")]
    pub channels: Option<f64>,
    #[serde(default, deserialize_with = "string")]
    pub channel_layout: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub sample_rate: Option<String>,
    #[serde(default)]
    pub tags: Tags,
    #[serde(default)]
    pub disposition: Disposition,
}

#[derive(Debug, Default, Deserialize)]
pub struct Format {
    #[serde(default, deserialize_with = "string")]
    pub duration: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub bit_rate: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub start_time: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub format_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Chapter {
    #[serde(default, deserialize_with = "string")]
    pub start_time: Option<String>,
    #[serde(default, deserialize_with = "string")]
    pub end_time: Option<String>,
    #[serde(default)]
    pub tags: Tags,
}

/// An element that is not the expected object reads as the default one.
#[derive(Deserialize)]
#[serde(untagged)]
enum Maybe<T> {
    Item(T),
    Other(IgnoredAny),
}

/// Buffers through serde's own `Content`, which keeps the key order of the tags (a `Value` would not).
#[derive(Deserialize)]
#[serde(untagged)]
enum Many<T> {
    List(Vec<Maybe<T>>),
    Other(IgnoredAny),
}

fn objects<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(match Many::<T>::deserialize(d)? {
        Many::List(l) => l
            .into_iter()
            .map(|m| match m {
                Maybe::Item(t) => t,
                Maybe::Other(_) => T::default(),
            })
            .collect(),
        Many::Other(_) => vec![],
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct Main {
    #[serde(default, deserialize_with = "objects")]
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Format,
    #[serde(default, deserialize_with = "objects")]
    pub chapters: Vec<Chapter>,
}

/// First frame of an `-of json` frame dump: `pts`, `best_effort_timestamp`, `side_data_list`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Frame {
    #[serde(default, deserialize_with = "float")]
    pub pts: Option<f64>,
    #[serde(default, deserialize_with = "float")]
    pub best_effort_timestamp: Option<f64>,
    #[serde(default, deserialize_with = "list")]
    pub side_data_list: Vec<Value>,
}

/// Streams of the late-audio probe.
#[derive(Debug, Default, Deserialize)]
pub struct AudioParams {
    #[serde(default, deserialize_with = "objects")]
    pub streams: Vec<Stream>,
}
