use std::path::Path;
use windows::{
    core::HSTRING,
    Globalization::Language,
    Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
        ExifOrientationMode,
    },
    Media::Ocr::{OcrEngine, OcrResult},
    Storage::StorageFile,
};

pub fn available_languages() -> Vec<String> {
    let Ok(_apartment) = crate::winrt_runtime::Apartment::current() else {
        return Vec::new();
    };
    let Ok(languages) = OcrEngine::AvailableRecognizerLanguages() else {
        return Vec::new();
    };
    let Ok(count) = languages.Size() else {
        return Vec::new();
    };
    (0..count)
        .filter_map(|index| languages.GetAt(index).ok())
        .filter_map(|language| language.LanguageTag().ok())
        .map(|tag| tag.to_string())
        .collect()
}

const NO_TEXT: &str = "没有识别到文字。请换一张清晰图片，或切换原文识别语言。";

/// One recognized word and its box `[x, y, width, height]` in original image pixels.
#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub rect: [f32; 4],
}

/// Recognized lines, each a list of words with their positions.
pub fn recognize_lines(path: &Path, language_tag: &str) -> Result<Vec<Vec<Word>>, String> {
    // Keep this guard outside `run`: OcrResult and its line/word interfaces are read here too.
    // Only owned Strings/rectangles leave the apartment, never WinRT interface pointers.
    let _apartment = crate::winrt_runtime::Apartment::current()?;
    let (result, scale) = run(path, language_tag)?;
    let read = |error: windows::core::Error| format!("读取 OCR 结果失败：{error}");
    let mut lines = Vec::new();
    for line in result.Lines().map_err(read)? {
        let mut words = Vec::new();
        for word in line.Words().map_err(read)? {
            let rect = word.BoundingRect().map_err(read)?;
            words.push(Word {
                text: word.Text().map_err(read)?.to_string(),
                // OCR ran on a downscaled bitmap when the image was too large.
                rect: [rect.X, rect.Y, rect.Width, rect.Height].map(|value| value / scale),
            });
        }
        if !words.is_empty() {
            lines.push(words);
        }
    }
    if lines.is_empty() {
        Err(NO_TEXT.into())
    } else {
        Ok(lines)
    }
}

/// Runs OCR and returns the result with the factor the image was scaled by before recognition.
fn run(path: &Path, language_tag: &str) -> Result<(OcrResult, f32), String> {
    let engine = if language_tag == "auto" {
        OcrEngine::TryCreateFromUserProfileLanguages()
    } else {
        let language = Language::CreateLanguage(&HSTRING::from(language_tag))
            .map_err(|error| format!("OCR 语言无效：{error}"))?;
        OcrEngine::TryCreateFromLanguage(&language)
    }
    .map_err(|error| format!("无法创建 Windows OCR：{error}"))?;

    let file_path = path.to_str().ok_or("图片路径无法转换为 Unicode。")?;
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(file_path))
        .and_then(|operation| operation.get())
        .map_err(|error| format!("Windows 无法打开图片：{error}"))?;
    let stream = file
        .OpenReadAsync()
        .and_then(|operation| operation.get())
        .map_err(|error| format!("读取图片失败：{error}"))?;
    let decoder = BitmapDecoder::CreateAsync(&stream)
        .and_then(|operation| operation.get())
        .map_err(|error| format!("Windows 无法解码图片：{error}"))?;

    let width = decoder.PixelWidth().map_err(|error| error.to_string())?;
    let height = decoder.PixelHeight().map_err(|error| error.to_string())?;
    let max = OcrEngine::MaxImageDimension().map_err(|error| error.to_string())?;
    let scale = (max as f64 / width.max(height) as f64).min(1.0);
    let bitmap = if scale < 1.0 {
        let transform = BitmapTransform::new().map_err(|error| error.to_string())?;
        transform
            .SetScaledWidth(((width as f64 * scale).round() as u32).max(1))
            .map_err(|error| error.to_string())?;
        transform
            .SetScaledHeight(((height as f64 * scale).round() as u32).max(1))
            .map_err(|error| error.to_string())?;
        decoder.GetSoftwareBitmapTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
            &transform,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
    } else {
        decoder.GetSoftwareBitmapConvertedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Premultiplied,
        )
    }
    .and_then(|operation| operation.get())
    .map_err(|error| format!("准备 OCR 图片失败：{error}"))?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .map_err(|error| format!("Windows OCR 识别失败：{error}"))?;
    Ok((result, scale as f32))
}
