#[path = "../src/windows_ocr.rs"]
mod windows_ocr;
#[path = "../src/winrt_runtime.rs"]
mod winrt_runtime;

fn main() {
    let languages = windows_ocr::available_languages();
    println!("Available Windows OCR languages: {}", languages.join(", "));
    let Some(path) = std::env::args().nth(1) else {
        return;
    };
    let language = std::env::args().nth(2).unwrap_or_else(|| "auto".into());
    match windows_ocr::recognize_lines(std::path::Path::new(&path), &language) {
        Ok(lines) => {
            println!("Recognized lines:");
            for line in lines {
                let words: Vec<String> = line
                    .iter()
                    .map(|word| format!("{}@{:.0},{:.0}", word.text, word.rect[0], word.rect[1]))
                    .collect();
                println!("{}", words.join(" "));
            }
        }
        Err(error) => {
            eprintln!("OCR failed: {error}");
            std::process::exit(1);
        }
    }
}
