use super::{
    Error, ReadOptions, ReadValue, Resolver, ResolverContext, ResolverDescriptor, Result,
    TextOptions, TextProjectionContext, detect_media_type, media::facts,
};
use crate::{
    Core, HydratedContent,
    ai::{ContentPart, Media},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Semaphore, time::timeout};

const MAX_INPUT: usize = 128 * 1024 * 1024;
const MAX_PROBE_OUTPUT: u64 = 1024 * 1024;
const MAX_SUBTITLE_OUTPUT: u64 = 2 * 1024 * 1024;
const FD_INPUT: [&str; 6] = [
    "-hide_banner",
    "-loglevel",
    "error",
    "-protocol_whitelist",
    "fd",
    "-i",
];

/// Local executables used as bounded format inspectors for storage-backed media.
/// The embedding application supplies the directory; the SDK reads no environment configuration.
pub struct MediaTools {
    probe: PathBuf,
    ffmpeg: PathBuf,
    permits: Semaphore,
}

impl MediaTools {
    pub fn from_directory(directory: &Path) -> Result<Self> {
        let executable = |name| {
            let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
            path.canonicalize()
                .ok()
                .filter(|path| path.is_file())
                .ok_or_else(|| {
                    Error::InvalidInput(
                        "media directory must contain ffprobe and ffmpeg executables",
                    )
                })
        };
        Ok(Self {
            probe: executable("ffprobe")?,
            ffmpeg: executable("ffmpeg")?,
            permits: Semaphore::new(2),
        })
    }

    /// Diagnose a missing input protocol before the Peer admits media requests.
    pub async fn verify(&self) -> Result<()> {
        for program in [&self.probe, &self.ffmpeg] {
            let mut command = Command::new(program);
            command
                .env_clear()
                .arg("-protocols")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let output = timeout(Duration::from_secs(3), command.output())
                .await
                .map_err(|_| Error::InvalidInput("media tool protocol discovery timed out"))?
                .map_err(|_| Error::InvalidInput("unable to run configured media tools"))?;
            let protocols = String::from_utf8_lossy(&output.stdout);
            if !output.status.success()
                || !protocols
                    .split("Output:")
                    .next()
                    .is_some_and(|inputs| inputs.lines().any(|line| line.trim() == "fd"))
            {
                return Err(Error::InvalidInput(
                    "configured media tools require the fd input protocol",
                ));
            }
        }
        Ok(())
    }

    /// Supply local inspection tools for the built-in audio and video Resolvers.
    pub fn register(self, core: &Core) -> Result<()> {
        core.media_tools
            .set(self)
            .map_err(|_| Error::Conflict("media tools are already configured".into()))
    }

    pub fn is_configured(core: &Core) -> bool {
        core.media_tools.get().is_some()
    }
}

pub(super) fn resolvers() -> [Arc<dyn Resolver>; 2] {
    [
        Arc::new(AvResolver { kind: Kind::Audio }),
        Arc::new(AvResolver { kind: Kind::Video }),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Audio,
    Video,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Audio => "core.audio.v1",
            Self::Video => "core.video.v1",
        }
    }
    fn invalid(self) -> Error {
        Error::InvalidContent {
            resolver: self.id().into(),
            reason: "invalid media or no matching stream",
        }
    }
}

struct AvResolver {
    kind: Kind,
}

impl AvResolver {
    async fn inspect(&self, context: &ResolverContext, refresh: bool) -> Result<(Vec<u8>, Value)> {
        let tools = context.knowledge.core().media_tools.get().ok_or_else(|| {
            Error::Unsupported("audio and video inspection requires local media tools".into())
        })?;
        let _permit = tools
            .permits
            .acquire()
            .await
            .map_err(|_| Error::InvalidInput("media inspector stopped"))?;
        let HydratedContent::Bytes(bytes) = context.actual_content(refresh).await? else {
            return Err(self.kind.invalid());
        };
        if bytes.len() > MAX_INPUT {
            return Err(Error::Limit("media inspection input exceeds 128 MiB"));
        }
        timeout(Duration::from_secs(25), async {
            let (bytes, mut input) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
                let mut input = tempfile::tempfile()?;
                input.write_all(&bytes)?;
                Ok((bytes, input))
            }).await.map_err(|_| Error::InvalidInput("media staging task failed"))?
                .map_err(|_| Error::InvalidInput("unable to stage media for inspection"))?;
            let probe = run_inspector(&tools.probe, &mut input, &[
                "-show_entries",
                "format=format_name,duration,bit_rate:stream=index,codec_type,codec_name,channels,sample_rate,bit_rate,width,height,avg_frame_rate,duration_ts,time_base:stream_disposition=default,attached_pic",
                "-of", "json",
            ], MAX_PROBE_OUTPUT).await?.ok_or_else(|| self.kind.invalid())?;
            let probe: Value = serde_json::from_slice(&probe).map_err(|_| self.kind.invalid())?;
            let streams = probe["streams"].as_array().ok_or_else(|| self.kind.invalid())?;
            let stream = primary_stream(streams, self.kind).ok_or_else(|| self.kind.invalid())?;
            let format = &probe["format"];
            let duration = stream["duration_ts"].as_f64().zip(ratio(&stream["time_base"]))
                .map(|(ticks, unit)| ticks * unit).or_else(|| number(&format["duration"]));
            let duration = duration.map(|value| (value * 1000.0).round_ties_even() as i64);
            let container = format["format_name"].as_str().and_then(|value| {
                let mut names = value.split(',');
                let first = names.next();
                if first == Some("mp4") || names.any(|name| name == "mp4") { Some("mp4") } else { first }
            });
            let mut metadata = json!({"byte_size":bytes.len(),"detected_media_type":detect_media_type(&bytes),
                "container":container,"duration_ms":duration});
            if self.kind == Kind::Audio {
                metadata["codec"] = stream["codec_name"].clone();
                metadata["channels"] = stream["channels"].clone();
                metadata["sample_rate_hz"] = json!(integer(&stream["sample_rate"]));
                metadata["bitrate_bps"] = json!(integer(&stream["bit_rate"]).filter(|value| *value > 0)
                    .or_else(|| integer(&format["bit_rate"])));
            } else {
                metadata["video_codec"] = stream["codec_name"].clone();
                metadata["width"] = stream["width"].clone();
                metadata["height"] = stream["height"].clone();
                metadata["frame_rate"] = json!(ratio(&stream["avg_frame_rate"]));
                metadata["subtitles"] = json!(subtitles(&tools.ffmpeg, &mut input, streams).await?);
            }
            Ok((bytes, metadata))
        }).await.map_err(|_| Error::Limit("media inspection timed out"))?
    }

    async fn materialize(
        &self,
        context: &ResolverContext,
        bytes: Vec<u8>,
        metadata: &Value,
    ) -> Result<()> {
        use super::materialization::{
            audio_config, materialize_text_child, try_materialize_model_text, video_config,
        };
        let core = context.knowledge.core();
        let block = context.block.id;
        let media_type = metadata["detected_media_type"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                metadata["container"]
                    .as_str()
                    .map(|container| format!("{}/{container}", self.kind.name()))
            });
        if self.kind == Kind::Audio {
            if let (Some(config), Some(mime_type)) = (audio_config(core).await?, media_type) {
                let media = ContentPart::Audio(Media {
                    data: bytes,
                    mime_type,
                    transfer_url: context.transfer_url().await?,
                });
                try_materialize_model_text(core, block, "transcript", config.transcript_model,
                    "Transcribe only speech and authored spoken language in the audio. Return plain text with no summary or explanation.", media).await;
            }
        } else {
            if let Some(cues) = metadata["subtitles"]
                .as_array()
                .filter(|cues| !cues.is_empty())
            {
                let text = cues
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                materialize_text_child(core, block, "subtitle", &text).await?;
            }
            if let (Some(config), Some(mime_type)) = (video_config(core).await?, media_type) {
                let media = ContentPart::Video(Media {
                    data: bytes,
                    mime_type,
                    transfer_url: context.transfer_url().await?,
                });
                try_materialize_model_text(core, block, "transcript", config.transcript_model,
                    "Transcribe only speech and authored spoken language in the video. Return plain text with no summary or visual description.", media.clone()).await;
                try_materialize_model_text(core, block, "text", config.text_model,
                    "Transcribe only written text visibly present in the video. Return plain text with no summary or scene description.", media).await;
            }
        }
        Ok(())
    }
}

fn primary_stream(streams: &[Value], kind: Kind) -> Option<&Value> {
    let selected = streams
        .iter()
        .filter(|stream| stream["codec_type"] == kind.name());
    selected
        .clone()
        .find(|stream| stream["disposition"]["default"] == 1)
        .or_else(|| {
            selected
                .clone()
                .find(|stream| stream["disposition"]["attached_pic"] != 1)
        })
        .or_else(|| selected.into_iter().next())
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|number| number.is_finite())
}

fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn ratio(value: &Value) -> Option<f64> {
    let (a, b) = value.as_str()?.split_once('/')?;
    let value = a.parse::<f64>().ok()? / b.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

async fn run_inspector(
    executable: &Path,
    input: &mut File,
    args: &[&str],
    cap: u64,
) -> Result<Option<Vec<u8>>> {
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| Error::InvalidInput("unable to rewind media input"))?;
    let stdin = input
        .try_clone()
        .map_err(|_| Error::InvalidInput("unable to pass media descriptor"))?;
    let mut child = Command::new(executable)
        .env_clear()
        .args(FD_INPUT)
        .arg("fd:")
        .args(args)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::Unsupported("configured media tools are unavailable".into()))?;
    let mut output = Vec::new();
    child
        .stdout
        .take()
        .expect("media stdout was piped")
        .take(cap + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| Error::InvalidInput("media inspector output failed"))?;
    if output.len() as u64 > cap {
        return Err(Error::Limit(
            "media inspector output exceeds the inspection limit",
        ));
    }
    let exit = child
        .wait()
        .await
        .map_err(|_| Error::InvalidInput("media inspector did not complete"))?;
    Ok(exit.success().then_some(output))
}

async fn subtitles(tool: &Path, input: &mut File, streams: &[Value]) -> Result<Vec<String>> {
    let indices: Vec<_> = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "subtitle")
        .filter_map(|stream| stream["index"].as_u64())
        .collect();
    if indices.len() > 32 {
        return Err(Error::Limit(
            "media inspection supports at most 32 subtitle tracks",
        ));
    }
    let mut cues = Vec::new();
    for index in indices {
        let map = format!("0:{index}");
        let Some(data) = run_inspector(
            tool,
            input,
            &["-map", &map, "-f", "srt", "pipe:1"],
            MAX_SUBTITLE_OUTPUT,
        )
        .await?
        else {
            continue;
        };
        let normalized = String::from_utf8_lossy(&data).replace("\r\n", "\n");
        for block in normalized.split("\n\n") {
            let mut lines = block.lines();
            lines.next();
            if let Some((time, _)) = lines.next().and_then(|stamp| stamp.split_once(" --> ")) {
                let cue = lines.collect::<Vec<_>>().join("\n");
                if !cue.trim().is_empty() {
                    cues.push((time.to_owned(), cue.trim().to_owned()));
                }
            }
        }
    }
    cues.sort_by(|a, b| a.0.cmp(&b.0));
    let mut texts: Vec<_> = cues.into_iter().map(|(_, cue)| cue).collect();
    texts.dedup();
    Ok(texts)
}

#[async_trait]
impl Resolver for AvResolver {
    fn descriptor(&self) -> ResolverDescriptor {
        ResolverDescriptor {
            id: self.kind.id().into(),
            description: format!(
                "Container and primary {} stream inspection",
                self.kind.name()
            ),
            read_methods: vec![],
            draft: None,
        }
    }
    fn validate_content(&self, content: &HydratedContent) -> Result<()> {
        if matches!(content, HydratedContent::Bytes(_)) {
            Ok(())
        } else {
            Err(self.kind.invalid())
        }
    }
    async fn solve(&self, context: &ResolverContext, options: ReadOptions) -> Result<Value> {
        Ok(self.solve_value(context, options).await?.into_json())
    }
    async fn solve_value(
        &self,
        context: &ResolverContext,
        options: ReadOptions,
    ) -> Result<ReadValue> {
        let (bytes, metadata) = self.inspect(context, options.refresh).await?;
        Ok(ReadValue::media_content(bytes, metadata))
    }
    async fn text(
        &self,
        context: &ResolverContext,
        options: TextOptions,
    ) -> Result<Option<String>> {
        if options.context == TextProjectionContext::Default {
            return Err(Error::Unsupported(format!(
                "{} text projection",
                self.kind.id()
            )));
        }
        let (bytes, mut metadata) = self.inspect(context, options.refresh).await?;
        if options.materialize_missing {
            self.materialize(context, bytes, &metadata).await?;
        }
        let fields: &[(&str, &str)] = if self.kind == Kind::Audio {
            &[
                ("media type", "detected_media_type"),
                ("container", "container"),
                ("codec", "codec"),
                ("duration ms", "duration_ms"),
                ("channels", "channels"),
                ("sample rate Hz", "sample_rate_hz"),
                ("bitrate bps", "bitrate_bps"),
            ]
        } else {
            if let Some(cues) = metadata["subtitles"]
                .as_array()
                .filter(|cues| !cues.is_empty())
            {
                metadata["subtitle_count"] = json!(cues.len());
            }
            &[
                ("media type", "detected_media_type"),
                ("container", "container"),
                ("codec", "video_codec"),
                ("duration ms", "duration_ms"),
                ("width", "width"),
                ("height", "height"),
                ("frame rate", "frame_rate"),
                ("subtitle cues", "subtitle_count"),
            ]
        };
        Ok(Some(facts(self.kind.name(), &metadata, fields)))
    }
    async fn label(&self, _context: &ResolverContext, _refresh: bool) -> Result<String> {
        Ok(self.kind.name().into())
    }
}
