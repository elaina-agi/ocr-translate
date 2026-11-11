//! Reads translations aloud: installed Windows voices offline, or Google / Azure speech online.
//! Every engine produces an audio stream that a Windows media player plays on a worker thread.
use crate::language::{self, Script};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    core::{IInspectable, Interface, Ref, HSTRING},
    Foundation::TypedEventHandler,
    Media::{
        Core::MediaSource,
        Playback::{MediaPlayer, MediaPlayerFailedEventArgs},
        SpeechSynthesis::{SpeechSynthesizer, VoiceInformation},
    },
    Storage::Streams::{DataWriter, IRandomAccessStream, InMemoryRandomAccessStream},
};

/// Window id of the main window's text cards; popups use their own ids from 1 up.
pub const MAIN: u64 = 0;
/// Window id of the settings page's sample button.
pub const SAMPLE: u64 = u64::MAX;

/// Which text of a window is being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Source,
    Translation,
}

/// Identifies one read-aloud button: the window it sits in and the text it reads.
pub type Key = (u64, Part);
pub const RATE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=2.0;
/// For one online speech request.
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    #[default]
    Windows,
    Google,
    Azure,
}

impl Engine {
    pub const ALL: [Self; 3] = [Self::Windows, Self::Google, Self::Azure];

    pub fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows 本地语音（离线免费）",
            Self::Google => "Google 在线语音（免 Key，可能限流）",
            Self::Azure => "Azure 神经语音（每月 50 万字符免费）",
        }
    }
}

/// One spoken language: its Windows tag, Google code, and Azure neural voice.
pub struct Voice {
    pub name: &'static str,
    pub tag: &'static str,
    google: &'static str,
    azure: &'static str,
}

const VOICES: [Voice; 9] = [
    Voice {
        name: "简体中文",
        tag: "zh-CN",
        google: "zh-CN",
        azure: "zh-CN-XiaoxiaoNeural",
    },
    Voice {
        name: "繁体中文",
        tag: "zh-TW",
        google: "zh-TW",
        azure: "zh-TW-HsiaoChenNeural",
    },
    Voice {
        name: "英语",
        tag: "en-US",
        google: "en",
        azure: "en-US-AriaNeural",
    },
    Voice {
        name: "日语",
        tag: "ja-JP",
        google: "ja",
        azure: "ja-JP-NanamiNeural",
    },
    Voice {
        name: "韩语",
        tag: "ko-KR",
        google: "ko",
        azure: "ko-KR-SunHiNeural",
    },
    Voice {
        name: "法语",
        tag: "fr-FR",
        google: "fr",
        azure: "fr-FR-DeniseNeural",
    },
    Voice {
        name: "西班牙语",
        tag: "es-ES",
        google: "es",
        azure: "es-ES-ElviraNeural",
    },
    Voice {
        name: "俄语",
        tag: "ru-RU",
        google: "ru",
        azure: "ru-RU-SvetlanaNeural",
    },
    Voice {
        name: "德语",
        tag: "de-DE",
        google: "de",
        azure: "de-DE-KatjaNeural",
    },
];

/// The voice for a translation: the target language's when it is known (as a Google
/// translation code), otherwise guessed from the script of the text itself.
pub fn voice_for(target_code: Option<&str>, text: &str) -> &'static Voice {
    target_code
        .and_then(|code| VOICES.iter().find(|voice| voice.google == code))
        .unwrap_or_else(|| detect(text))
}

/// The voice for recognized or typed source text. A non-Latin script settles the language;
/// otherwise the OCR language, when it names one of ours of the same script, is more specific.
pub fn source_voice(ocr_language: &str, text: &str) -> &'static Voice {
    let detected = detect(text);
    let script = |voice: &Voice| Script::of(voice.google);
    match ocr_voice(ocr_language) {
        Some(configured) if script(configured) == script(detected) => configured,
        _ => detected,
    }
}

/// Maps a Windows OCR language tag such as `en-US`, `zh-Hans-CN` or `ja` to a voice.
fn ocr_voice(language: &str) -> Option<&'static Voice> {
    let language = language.to_ascii_lowercase();
    let base = language.split('-').next()?;
    if base == "zh" {
        let traditional = ["hant", "tw", "hk", "mo"]
            .iter()
            .any(|subtag| language.split('-').any(|part| part == *subtag));
        let tag = if traditional { "zh-TW" } else { "zh-CN" };
        return VOICES.iter().find(|voice| voice.tag == tag);
    }
    VOICES.iter().find(|voice| {
        voice
            .tag
            .split('-')
            .next()
            .is_some_and(|voice_base| voice_base.eq_ignore_ascii_case(base))
    })
}

fn detect(text: &str) -> &'static Voice {
    let tag = match language::dominant(text) {
        Some(Script::Kana) => "ja-JP",
        Some(Script::Hangul) => "ko-KR",
        Some(Script::Han) => "zh-CN",
        Some(Script::Cyrillic) => "ru-RU",
        Some(Script::Latin) | None => "en-US",
    };
    VOICES
        .iter()
        .find(|voice| voice.tag == tag)
        .unwrap_or(&VOICES[2])
}

/// A short sentence for trying out an engine in the voice's language.
pub fn sample_text(voice: &Voice) -> &'static str {
    match voice.tag {
        "zh-CN" => "你好，这是朗读功能的试听。",
        "zh-TW" => "你好，這是朗讀功能的試聽。",
        "ja-JP" => "こんにちは、読み上げのテストです。",
        "ko-KR" => "안녕하세요, 읽기 기능 테스트입니다.",
        "fr-FR" => "Bonjour, ceci est un essai de lecture à voix haute.",
        "es-ES" => "Hola, esta es una prueba de lectura en voz alta.",
        "ru-RU" => "Здравствуйте, это проверка чтения вслух.",
        "de-DE" => "Hallo, dies ist ein Test der Vorlesefunktion.",
        _ => "Hello, this is a read-aloud test.",
    }
}

/// Everything one utterance needs, copied out of the settings so the worker owns it.
pub struct Request {
    pub engine: Engine,
    pub text: String,
    pub voice: &'static Voice,
    /// Speaking speed; 1.0 is normal.
    pub rate: f32,
    pub azure_key: String,
    pub azure_region: String,
}

/// Language tags of the installed Windows voices, for the settings page.
pub fn installed_languages() -> Vec<String> {
    // Keep voice interfaces alive until enumeration has converted and dropped them all.
    let Ok(apartment) = crate::winrt_runtime::Apartment::current() else {
        return Vec::new();
    };
    let mut languages: Vec<String> = windows_voices(&apartment)
        .into_iter()
        .map(|(language, _)| language)
        .collect();
    languages.sort();
    languages.dedup();
    languages
}

fn windows_voices(_apartment: &crate::winrt_runtime::Apartment) -> Vec<(String, VoiceInformation)> {
    let Ok(voices) = SpeechSynthesizer::AllVoices() else {
        return Vec::new();
    };
    let Ok(count) = voices.Size() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| voices.GetAt(index).ok())
        .filter_map(|voice| Some((voice.Language().ok()?.to_string(), voice)))
        .collect()
}

pub enum Status {
    Idle,
    Active,
    Failed(String),
}

struct Utterance {
    key: Key,
    cancel: Arc<AtomicBool>,
    outcome: Receiver<Result<(), String>>,
}

/// Plays at most one utterance at a time, remembering which window asked for it.
#[derive(Default)]
pub struct Speaker {
    current: Option<Utterance>,
    /// The last failure and the window it belongs to, kept until another utterance starts.
    failure: Option<(Key, String)>,
}

impl Speaker {
    /// Starts reading `request` for `key`, or stops when `key` is the one already speaking.
    /// `wake` runs on the worker once the utterance has finished.
    pub fn toggle(&mut self, key: Key, request: Request, wake: impl FnOnce() + Send + 'static) {
        let stopping = self
            .current
            .as_ref()
            .is_some_and(|current| current.key == key);
        self.stop();
        self.failure = None;
        if stopping {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, outcome) = mpsc::channel();
        thread::spawn(move || {
            let result = speak(&request, &worker_cancel, 1.0);
            // A stopped utterance may fail on its way out; that is not worth reporting.
            let result = if worker_cancel.load(Ordering::Acquire) {
                Ok(())
            } else {
                result
            };
            let _ = sender.send(result);
            wake();
        });
        self.current = Some(Utterance {
            key,
            cancel,
            outcome,
        });
    }

    pub fn stop(&mut self) {
        if let Some(current) = self.current.take() {
            current.cancel.store(true, Ordering::Release);
        }
    }

    /// Collects a finished utterance and returns its failure, once.
    pub fn poll(&mut self) -> Option<(Key, String)> {
        let result = match self.current.as_ref()?.outcome.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("朗读意外结束。".into()),
        };
        let key = self.current.take()?.key;
        let error = result.err()?;
        self.failure = Some((key, error.clone()));
        Some((key, error))
    }

    pub fn active(&self) -> bool {
        self.current.is_some()
    }

    pub fn status(&self, key: Key) -> Status {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.key == key)
        {
            return Status::Active;
        }
        match &self.failure {
            Some((failed, error)) if *failed == key => Status::Failed(error.clone()),
            _ => Status::Idle,
        }
    }

    /// Stops and forgets utterances whose window is gone; `keep` receives window ids.
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| !keep(current.key.0))
        {
            self.stop();
        }
        if self.failure.as_ref().is_some_and(|(key, _)| !keep(key.0)) {
            self.failure = None;
        }
    }
}

fn speak(request: &Request, cancel: &AtomicBool, volume: f64) -> Result<(), String> {
    // Every engine eventually uses WinRT streams and MediaPlayer on this worker.
    // All streams/player callbacks are released before the apartment guard leaves scope.
    let apartment = crate::winrt_runtime::Apartment::mta()?;
    if request.text.trim().is_empty() {
        return Err("没有可朗读的文字。".into());
    }
    let (stream, content_type) = match request.engine {
        Engine::Windows => windows_stream(request, &apartment)?,
        Engine::Google => (memory_stream(&google_audio(request, cancel)?)?, mp3()),
        Engine::Azure => (memory_stream(&azure_audio(request, cancel)?)?, mp3()),
    };
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    play(&stream, &content_type, cancel, volume)
}

fn mp3() -> HSTRING {
    HSTRING::from("audio/mpeg")
}

fn windows_stream(
    request: &Request,
    apartment: &crate::winrt_runtime::Apartment,
) -> Result<(IRandomAccessStream, HSTRING), String> {
    let failed = |error: windows::core::Error| format!("Windows 语音合成失败：{error}");
    let voice = windows_voice(request.voice, apartment)?;
    let synthesizer =
        SpeechSynthesizer::new().map_err(|error| format!("无法创建 Windows 语音合成：{error}"))?;
    synthesizer.SetVoice(&voice).map_err(failed)?;
    if let Ok(options) = synthesizer.Options() {
        let _ = options.SetSpeakingRate(f64::from(request.rate));
    }
    let stream = synthesizer
        .SynthesizeTextToStreamAsync(&HSTRING::from(request.text.as_str()))
        .and_then(|operation| operation.get())
        .map_err(failed)?;
    let content_type = stream.ContentType().map_err(failed)?;
    Ok((stream.cast().map_err(failed)?, content_type))
}

/// The installed voice for the exact language, else one for the same base language.
fn windows_voice(
    voice: &Voice,
    apartment: &crate::winrt_runtime::Apartment,
) -> Result<VoiceInformation, String> {
    let voices = windows_voices(apartment);
    let base = |tag: &str| {
        tag.split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    voices
        .iter()
        .find(|(language, _)| language.eq_ignore_ascii_case(voice.tag))
        .or_else(|| {
            voices
                .iter()
                .find(|(language, _)| base(language) == base(voice.tag))
        })
        .map(|(_, info)| info.clone())
        .ok_or_else(|| {
            format!(
                "系统没有安装{}语音。请在 Windows 设置 → 时间和语言 → 语音 中添加，或改用在线朗读引擎。",
                voice.name
            )
        })
}

fn memory_stream(bytes: &[u8]) -> Result<IRandomAccessStream, String> {
    let write = || -> windows::core::Result<IRandomAccessStream> {
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.get()?;
        writer.FlushAsync()?.get()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        stream.cast()
    };
    write().map_err(|error| format!("准备朗读音频失败：{error}"))
}

/// Google reads at most about 200 characters per request; the MP3 pieces simply concatenate.
fn google_audio(request: &Request, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    let client = crate::net::client()?;
    let mut audio = Vec::new();
    for piece in chunks(&request.text, 180) {
        if cancel.load(Ordering::Acquire) {
            return Ok(audio);
        }
        let response = client
            .get("https://translate.googleapis.com/translate_tts")
            .timeout(TIMEOUT)
            .header(reqwest::header::USER_AGENT, crate::net::BROWSER_AGENT)
            .query(&[
                ("ie", "UTF-8"),
                ("client", "gtx"),
                ("tl", request.voice.google),
                ("q", piece.as_str()),
            ])
            .send()
            .map_err(|error| format!("Google 朗读请求失败：{error}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .map_err(|error| format!("读取 Google 朗读音频失败：{error}"))?;
        if !status.is_success() {
            return Err(format!(
                "Google 朗读返回 {status}：{}",
                crate::compact_error(&String::from_utf8_lossy(&bytes))
            ));
        }
        audio.extend_from_slice(&bytes);
    }
    Ok(audio)
}

fn azure_audio(request: &Request, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    let key = request.azure_key.trim();
    let region = request.azure_region.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err("请先在设置中填写 Azure 语音密钥。".into());
    }
    if region.is_empty() || !region.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Azure 区域无效，请填写语音资源所在区域，例如 eastasia。".into());
    }
    let url = format!("https://{region}.tts.speech.microsoft.com/cognitiveservices/v1");
    let client = crate::net::client()?;
    let mut audio = Vec::new();
    for piece in chunks(&request.text, 1000) {
        if cancel.load(Ordering::Acquire) {
            return Ok(audio);
        }
        let response = client
            .post(&url)
            .timeout(TIMEOUT)
            // Azure rejects speech requests without a user agent.
            .header(reqwest::header::USER_AGENT, "ocr-translate")
            .header("Ocp-Apim-Subscription-Key", key)
            .header("Content-Type", "application/ssml+xml")
            .header(
                "X-Microsoft-OutputFormat",
                "audio-24khz-48kbitrate-mono-mp3",
            )
            .body(ssml(request.voice, request.rate, &piece))
            .send()
            .map_err(|error| format!("Azure 语音请求失败：{error}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .map_err(|error| format!("读取 Azure 语音音频失败：{error}"))?;
        match status.as_u16() {
            200..=299 => audio.extend_from_slice(&bytes),
            401 | 403 => {
                return Err("Azure 拒绝了请求：密钥无效，或与填写的区域不一致。".into());
            }
            429 => {
                return Err("Azure 请求过于频繁，或本月免费额度已用完，请稍后再试。".into());
            }
            _ => {
                return Err(format!(
                    "Azure 语音返回 {status}：{}",
                    crate::compact_error(&String::from_utf8_lossy(&bytes))
                ));
            }
        }
    }
    Ok(audio)
}

/// Azure bills SSML markup other than `speak` and `voice`, so normal speed sends no prosody.
fn ssml(voice: &Voice, rate: f32, text: &str) -> String {
    let percent = ((rate - 1.0) * 100.0).round() as i32;
    let text = escape_xml(text);
    let body = if percent == 0 {
        text
    } else {
        format!("<prosody rate='{percent:+}%'>{text}</prosody>")
    };
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='{}'>\
         <voice name='{}'>{body}</voice></speak>",
        voice.tag, voice.azure
    )
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Break points tried in order: sentence ends, then clause marks, then spaces.
const BREAKS: [&[char]; 3] = [
    &['。', '！', '？', '；', '.', '!', '?', ';', '\n'],
    &['，', '、', '：', ',', ':'],
    &[' ', '\t'],
];

/// Splits text into pieces of at most `max` characters, cutting at the most natural break.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    pack(text, max, 0, &mut pieces);
    pieces
        .into_iter()
        .map(|piece| piece.trim().to_owned())
        .filter(|piece| !piece.is_empty())
        .collect()
}

fn pack(text: &str, max: usize, level: usize, pieces: &mut Vec<String>) {
    let Some(breaks) = BREAKS.get(level) else {
        let chars: Vec<char> = text.chars().collect();
        pieces.extend(
            chars
                .chunks(max)
                .map(|chunk| chunk.iter().collect::<String>()),
        );
        return;
    };
    let mut current = String::new();
    let mut length = 0;
    for segment in text.split_inclusive(*breaks) {
        let segment_length = segment.chars().count();
        if length + segment_length > max {
            pieces.push(std::mem::take(&mut current));
            length = 0;
        }
        if segment_length > max {
            pack(segment, max, level + 1, pieces);
        } else {
            current.push_str(segment);
            length += segment_length;
        }
    }
    pieces.push(current);
}

/// Plays the stream to its end, or until `cancel` is set.
fn play(
    stream: &IRandomAccessStream,
    content_type: &HSTRING,
    cancel: &AtomicBool,
    volume: f64,
) -> Result<(), String> {
    let failed = |error: windows::core::Error| format!("播放朗读音频失败：{error}");
    let player = MediaPlayer::new().map_err(failed)?;
    // Keep the read-aloud out of the system media overlay and its media keys.
    if let Ok(manager) = player.CommandManager() {
        let _ = manager.SetIsEnabled(false);
    }
    let _ = player.SetVolume(volume);
    let opened = Arc::new(AtomicBool::new(false));
    let ended = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(Mutex::new(None::<String>));
    player
        .MediaOpened(&TypedEventHandler::<MediaPlayer, IInspectable>::new({
            let opened = opened.clone();
            move |_, _| {
                opened.store(true, Ordering::Release);
                Ok(())
            }
        }))
        .map_err(failed)?;
    player
        .MediaEnded(&TypedEventHandler::<MediaPlayer, IInspectable>::new({
            let ended = ended.clone();
            move |_, _| {
                ended.store(true, Ordering::Release);
                Ok(())
            }
        }))
        .map_err(failed)?;
    player
        .MediaFailed(
            &TypedEventHandler::<MediaPlayer, MediaPlayerFailedEventArgs>::new({
                let failure = failure.clone();
                move |_, args: Ref<MediaPlayerFailedEventArgs>| {
                    let message = args
                        .as_ref()
                        .and_then(|args| args.ErrorMessage().ok())
                        .map(|message| message.to_string())
                        .filter(|message| !message.trim().is_empty())
                        .unwrap_or_else(|| "音频格式无法播放".into());
                    if let Ok(mut failure) = failure.lock() {
                        *failure = Some(message);
                    }
                    Ok(())
                }
            }),
        )
        .map_err(failed)?;
    let source = MediaSource::CreateFromStream(stream, content_type).map_err(failed)?;
    player.SetSource(&source).map_err(failed)?;
    player.Play().map_err(failed)?;

    let started = Instant::now();
    let result = loop {
        if cancel.load(Ordering::Acquire) {
            let _ = player.Pause();
            break Ok(());
        }
        if let Some(message) = failure.lock().ok().and_then(|mut failure| failure.take()) {
            break Err(format!("播放朗读音频失败：{message}"));
        }
        if ended.load(Ordering::Acquire) {
            break Ok(());
        }
        if !opened.load(Ordering::Acquire) && started.elapsed() > Duration::from_secs(20) {
            break Err("朗读音频加载超时。".into());
        }
        thread::sleep(Duration::from_millis(30));
    };
    let _ = player.Close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(engine: Engine, text: &str) -> Request {
        Request {
            engine,
            text: text.into(),
            voice: voice_for(None, text),
            rate: 1.0,
            azure_key: String::new(),
            azure_region: "eastasia".into(),
        }
    }

    #[test]
    fn chunks_respect_the_limit_and_prefer_sentence_ends() {
        assert_eq!(
            chunks("第一句。第二句。第三句。", 8),
            ["第一句。第二句。", "第三句。"]
        );
        let long = "word ".repeat(100);
        let pieces = chunks(&long, 30);
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 30));
        assert_eq!(pieces.join(" "), long.trim());
        let unbroken = "字".repeat(25);
        assert_eq!(chunks(&unbroken, 10).concat(), unbroken);
        assert!(chunks("  \n ", 10).is_empty());
    }

    #[test]
    fn picks_the_target_voice_and_falls_back_to_the_script() {
        assert_eq!(voice_for(Some("zh-TW"), "hello").tag, "zh-TW");
        assert_eq!(voice_for(Some("fr"), "bonjour").tag, "fr-FR");
        assert_eq!(voice_for(None, "这是中文").tag, "zh-CN");
        assert_eq!(voice_for(None, "これは日本語です").tag, "ja-JP");
        assert_eq!(voice_for(None, "한국어").tag, "ko-KR");
        assert_eq!(voice_for(None, "Привет").tag, "ru-RU");
        assert_eq!(voice_for(Some("unknown"), "plain text").tag, "en-US");
    }

    #[test]
    fn source_voice_combines_the_ocr_language_with_the_script() {
        assert_eq!(source_voice("fr-FR", "Bonjour tout le monde").tag, "fr-FR");
        assert_eq!(source_voice("zh-Hant-TW", "繁體中文").tag, "zh-TW");
        assert_eq!(source_voice("zh-Hans-CN", "简体中文").tag, "zh-CN");
        assert_eq!(source_voice("ja", "日本語のテキスト").tag, "ja-JP");
        // Typed text in another script than the OCR language reads in its own language.
        assert_eq!(source_voice("en-US", "これは日本語です").tag, "ja-JP");
        assert_eq!(source_voice("ja", "Hello").tag, "en-US");
        assert_eq!(source_voice("auto", "Hallo Welt").tag, "en-US");
        assert_eq!(source_voice("de-DE", "Hallo Welt").tag, "de-DE");
    }

    #[test]
    fn ssml_escapes_text_and_sets_the_rate() {
        let voice = voice_for(Some("en"), "");
        let ssml = ssml(voice, 1.25, "a < b & 'c'");
        assert!(ssml.contains("<voice name='en-US-AriaNeural'>"));
        assert!(ssml.contains("rate='+25%'"));
        assert!(ssml.contains("a &lt; b &amp; &apos;c&apos;"));
        assert!(super::ssml(voice, 0.5, "x").contains("rate='-50%'"));
        assert!(!super::ssml(voice, 1.0, "x").contains("prosody"));
    }

    #[test]
    fn azure_requires_a_key_and_a_plain_region() {
        let cancel = AtomicBool::new(false);
        let mut missing_key = request(Engine::Azure, "hello");
        assert!(azure_audio(&missing_key, &cancel)
            .unwrap_err()
            .contains("密钥"));
        missing_key.azure_key = "key".into();
        missing_key.azure_region = "evil.example/".into();
        assert!(azure_audio(&missing_key, &cancel)
            .unwrap_err()
            .contains("区域"));
    }

    #[test]
    fn speaker_reports_status_per_window() {
        let mut speaker = Speaker {
            current: None,
            failure: Some(((3, Part::Source), "失败".into())),
        };
        assert!(matches!(
            speaker.status((3, Part::Source)),
            Status::Failed(_)
        ));
        assert!(matches!(
            speaker.status((3, Part::Translation)),
            Status::Idle
        ));
        assert!(matches!(speaker.status((MAIN, Part::Source)), Status::Idle));
        speaker.retain(|window| window != 3);
        assert!(matches!(speaker.status((3, Part::Source)), Status::Idle));
    }

    #[test]
    #[ignore = "plays (muted) through an installed Windows voice"]
    fn plays_windows_speech_muted() {
        let Some(language) = installed_languages().into_iter().next() else {
            return;
        };
        let voice = VOICES
            .iter()
            .find(|voice| voice.tag.eq_ignore_ascii_case(&language))
            .unwrap_or(&VOICES[2]);
        let mut request = request(Engine::Windows, sample_text(voice));
        request.voice = voice;
        speak(&request, &AtomicBool::new(false), 0.0).unwrap();
    }

    #[test]
    #[ignore = "uses Google's live read-aloud endpoint and plays it muted"]
    fn plays_live_google_speech_muted() {
        let request = request(Engine::Google, "你好，世界。这是一段测试。");
        let audio = google_audio(&request, &AtomicBool::new(false)).unwrap();
        assert!(audio.len() > 1000);
        speak(&request, &AtomicBool::new(false), 0.0).unwrap();
    }

    #[test]
    #[ignore = "uses Azure Speech with AZURE_SPEECH_KEY / AZURE_SPEECH_REGION and plays it muted"]
    fn plays_live_azure_speech_muted() {
        let mut request = request(Engine::Azure, "你好，世界。这是一段测试。");
        request.azure_key = std::env::var("AZURE_SPEECH_KEY").unwrap();
        request.azure_region = std::env::var("AZURE_SPEECH_REGION").unwrap();
        request.rate = 1.2;
        let audio = azure_audio(&request, &AtomicBool::new(false)).unwrap();
        assert!(audio.len() > 1000);
        speak(&request, &AtomicBool::new(false), 0.0).unwrap();
    }
}
