//! Windows-only WinRT/OCR lifecycle regressions. Native failures are contained in a
//! separate copy of this test binary; no GUI, user settings, clipboard, or network is used.
#![cfg(target_os = "windows")]

#[path = "../src/windows_ocr.rs"]
mod windows_ocr;
#[path = "../src/winrt_runtime.rs"]
mod winrt_runtime;

use std::{
    ffi::c_void,
    fs,
    mem::size_of,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{factory, w, Interface, HSTRING},
    Globalization::{ILanguageFactory, Language},
    Win32::System::{
        Com::CoFreeUnusedLibrariesEx,
        LibraryLoader::GetModuleHandleW,
        Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_NOACCESS},
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::GetCurrentProcess,
    },
};

const CHILD_ENV: &str = "OCR_TRANSLATE_WINRT_LIFETIME_CHILD";
const FACTORY_MODE: &str = "factory";
const FACTORY_STA_MODE: &str = "factory-sta";
const OCR_MODE: &str = "ocr";
const EXPECTED_TEXT: &str = "HELLOWORLD";

/// The child's small, bounded stdout/stderr can be collected after it exits. A native
/// access violation or timeout becomes a parent assertion rather than killing the suite.
fn run_isolated(child_test: &str, mode: &str, timeout: Duration, completion: &str) {
    let mut child = Command::new(std::env::current_exe().expect("test executable path"))
        .args(["--exact", child_test, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start isolated WinRT test process");
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("reap timed-out test child");
                panic!(
                    "{mode} child timed out after {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("wait for {mode} child: {error}");
            }
        }
    }
    let output = child
        .wait_with_output()
        .expect("collect isolated test output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{mode} child failed with {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status.code(),
    );
    assert!(
        stdout.contains(completion),
        "child helper did not execute its stress loop\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    println!("{stdout}");
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
}

#[test]
fn language_factory_survives_worker_exit_idle_and_concurrent_workers() {
    run_isolated(
        "language_factory_child",
        FACTORY_MODE,
        Duration::from_secs(90),
        "factory lifecycle completed",
    );
}

#[test]
fn language_factory_survives_first_sta_exit_and_later_mta_workers() {
    run_isolated(
        "language_factory_child",
        FACTORY_STA_MODE,
        Duration::from_secs(90),
        "factory lifecycle completed",
    );
}

#[test]
#[ignore = "requires an installed English Windows OCR language pack; run explicitly on Windows"]
fn real_ocr_survives_worker_exit_idle_and_concurrent_workers() {
    run_isolated(
        "real_ocr_child",
        OCR_MODE,
        Duration::from_secs(180),
        "OCR lifecycle completed",
    );
}

#[derive(Clone, Copy, Debug)]
struct FactoryAddress {
    // Integers, not COM objects, cross the worker boundary. They are only passed to
    // VirtualQuery/GetModuleHandleW outside the apartment, never dereferenced there.
    vtable: usize,
    module: usize,
}

fn language_task(tag: &'static str) -> (FactoryAddress, usize) {
    language_task_in_apartment(tag, false)
}

fn language_task_in_apartment(tag: &'static str, sta: bool) -> (FactoryAddress, usize) {
    let _apartment = if sta {
        winrt_runtime::Apartment::sta().expect("initialize first STA")
    } else {
        winrt_runtime::Apartment::mta().expect("initialize worker MTA")
    };
    // Exercise the generated static factory cache, not just an uncached factory call.
    let language = Language::CreateLanguage(&HSTRING::from(tag)).expect("create language");
    assert_eq!(
        language
            .LanguageTag()
            .expect("read language tag")
            .to_string(),
        tag
    );
    let languages = windows_ocr::available_languages();
    let factory = factory::<Language, ILanguageFactory>().expect("obtain language factory");
    // The factory is live and belongs to this initialized apartment while read here.
    let vtable = unsafe { *(factory.as_raw() as *const *const c_void) } as usize;
    let module = unsafe { GetModuleHandleW(w!("Windows.Globalization.dll")) }
        .expect("language DLL should be loaded")
        .0 as usize;
    let address = FactoryAddress { vtable, module };
    assert_factory_mapped(address);
    // Factory, language, and returned WinRT values drop before the apartment guard.
    (address, languages.len())
}

fn assert_factory_mapped(address: FactoryAddress) {
    let module = unsafe { GetModuleHandleW(w!("Windows.Globalization.dll")) }
        .expect("language DLL must remain loaded between OCR tasks")
        .0 as usize;
    assert_eq!(module, address.module, "language DLL was unloaded/reloaded");
    let mut memory = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        VirtualQuery(
            Some(address.vtable as *const c_void),
            &mut memory,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    assert_eq!(queried, size_of::<MEMORY_BASIC_INFORMATION>());
    assert_eq!(
        memory.State, MEM_COMMIT,
        "cached factory vtable became unmapped"
    );
    assert_ne!(
        memory.Protect, PAGE_NOACCESS,
        "cached vtable is not readable"
    );
    assert_eq!(
        memory.AllocationBase as usize, module,
        "vtable no longer belongs to the language DLL",
    );
}

fn idle_unload_pressure(address: FactoryAddress) {
    assert!(
        matches!(
            std::env::var(CHILD_ENV).as_deref(),
            Ok(FACTORY_MODE) | Ok(FACTORY_STA_MODE) | Ok(OCR_MODE)
        ),
        "COM unload pressure is permitted only in the isolated child",
    );
    // No task-specific COM objects or apartment guards remain on the calling thread.
    // The runtime module's process keeper must protect generated static caches.
    for _ in 0..2 {
        unsafe { CoFreeUnusedLibrariesEx(0, None) };
        thread::sleep(Duration::from_millis(20));
        assert_factory_mapped(address);
    }
}

fn report_memory(phase: &str) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    }
    .expect("read isolated child's memory counters");
    // Informational only: OS caches and runtime allocation make exact thresholds flaky.
    println!(
        "{phase}: working_set={} private_bytes={}",
        counters.WorkingSetSize, counters.PrivateUsage,
    );
}

#[test]
fn language_factory_child() {
    let first_sta = match std::env::var(CHILD_ENV).as_deref() {
        Ok(FACTORY_MODE) => false,
        Ok(FACTORY_STA_MODE) => true,
        _ => return,
    };
    // No WinRT use in this process precedes the short-lived first MTA/STA worker.
    let (address, language_count) =
        thread::spawn(move || language_task_in_apartment("en-US", first_sta))
            .join()
            .expect("initial language worker panicked");
    println!(
        "initial apartment: {}",
        if first_sta { "STA" } else { "MTA" }
    );
    println!(
        "synthetic language factory primed: vtable={:#x} installed_ocr_languages={language_count}",
        address.vtable,
    );
    report_memory("factory warmed");
    idle_unload_pressure(address);
    for iteration in 0..48 {
        let tag = ["en-US", "fr-FR", "ja"][iteration % 3];
        let (next, _) = thread::spawn(move || language_task(tag))
            .join()
            .expect("sequential language worker panicked");
        assert_eq!(next.vtable, address.vtable);
        idle_unload_pressure(address);
    }
    for _ in 0..16 {
        let first = thread::spawn(|| language_task("en-US"));
        let second = thread::spawn(|| language_task("ja"));
        for worker in [first, second] {
            let (next, _) = worker.join().expect("concurrent language worker panicked");
            assert_eq!(next.vtable, address.vtable);
        }
        idle_unload_pressure(address);
    }
    report_memory("factory stress complete");
    println!("factory lifecycle completed: 81 tasks, 48 sequential and 16 concurrent pairs");
}

struct SyntheticImage {
    directory: PathBuf,
    file: PathBuf,
}

impl SyntheticImage {
    fn new() -> Self {
        let temp = std::env::temp_dir()
            .canonicalize()
            .expect("resolve system temp directory");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = temp.join(format!(
            "ocr-translate-winrt-test-{}-{stamp}",
            std::process::id(),
        ));
        fs::create_dir(&directory).expect("create exclusive synthetic-image directory");
        let directory = directory
            .canonicalize()
            .expect("resolve synthetic directory");
        assert_eq!(directory.parent(), Some(temp.as_path()));
        let file = directory.join("hello-world.png");
        let image = Self { directory, file };
        image.write_text();
        image
    }

    fn write_text(&self) {
        // Embedded 5x7 uppercase glyphs: no font files or user-provided images needed.
        let text = "HELLO WORLD";
        let scale = 8;
        let margin = 48;
        let advance = 7 * scale;
        let mut image = image::RgbImage::from_pixel(
            2 * margin + text.len() as u32 * advance,
            2 * margin + 7 * scale,
            image::Rgb([255, 255, 255]),
        );
        for (index, letter) in text.chars().enumerate() {
            let rows: [u8; 7] = match letter {
                'H' => [17, 17, 17, 31, 17, 17, 17],
                'E' => [31, 16, 16, 30, 16, 16, 31],
                'L' => [16, 16, 16, 16, 16, 16, 31],
                'O' => [14, 17, 17, 17, 17, 17, 14],
                'W' => [17, 17, 17, 21, 21, 21, 10],
                'R' => [30, 17, 17, 30, 20, 18, 17],
                'D' => [30, 17, 17, 17, 17, 17, 30],
                ' ' => [0; 7],
                _ => unreachable!(),
            };
            for (row, bits) in rows.into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            image.put_pixel(
                                margin + index as u32 * advance + column * scale + dx,
                                margin + row as u32 * scale + dy,
                                image::Rgb([0, 0, 0]),
                            );
                        }
                    }
                }
            }
        }
        image
            .save(&self.file)
            .expect("save fixed HELLO WORLD test image");
    }
}

impl Drop for SyntheticImage {
    fn drop(&mut self) {
        // Only remove the exact image and empty directory created by this test. Never
        // recursively remove a computed temp path or touch unrelated captures.
        if self.directory.canonicalize().ok().as_ref() != Some(&self.directory) {
            return;
        }
        if let Ok(file) = self.file.canonicalize() {
            if file == self.file
                && file.parent() == Some(self.directory.as_path())
                && file
                    .file_name()
                    .is_some_and(|name| name == "hello-world.png")
            {
                let _ = fs::remove_file(&file);
            }
        }
        let _ = fs::remove_dir(&self.directory);
    }
}

fn recognize_synthetic(path: &Path, language: &str) -> Result<String, String> {
    let _apartment = winrt_runtime::Apartment::mta()?;
    // canonicalize() yields a verbatim prefix on Windows, while StorageFile expects
    // a regular absolute DOS/UNC path. Cleanup still checks the canonical path.
    let canonical = path.to_str().ok_or("synthetic image path is not Unicode")?;
    let regular_path = if let Some(unc) = canonical.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(dos) = canonical.strip_prefix(r"\\?\") {
        PathBuf::from(dos)
    } else {
        path.to_path_buf()
    };
    let lines = windows_ocr::recognize_lines(&regular_path, language)?;
    assert!(
        lines.iter().flatten().all(|word| {
            word.rect.iter().all(|coordinate| coordinate.is_finite())
                && word.rect[2] > 0.0
                && word.rect[3] > 0.0
        }),
        "synthetic OCR returned invalid word geometry",
    );
    Ok(lines
        .iter()
        .flatten()
        .flat_map(|word| word.text.chars())
        .filter(char::is_ascii_alphabetic)
        .flat_map(char::to_uppercase)
        .collect())
}

fn ocr_task(path: PathBuf, language: String) -> thread::JoinHandle<Result<String, String>> {
    thread::spawn(move || recognize_synthetic(&path, &language))
}

#[test]
fn real_ocr_child() {
    if std::env::var(CHILD_ENV).as_deref() != Ok(OCR_MODE) {
        return;
    }
    let (address, _) = thread::spawn(|| language_task("en-US"))
        .join()
        .expect("initial OCR language worker panicked");
    let languages = windows_ocr::available_languages();
    let language = languages
        .iter()
        .find(|language| language.eq_ignore_ascii_case("en-US"))
        .or_else(|| {
            languages
                .iter()
                .find(|language| language.to_ascii_lowercase().starts_with("en-"))
        })
        .expect(
            "explicit OCR lifecycle test requires an installed English Windows OCR language pack",
        )
        .clone();
    let image = SyntheticImage::new();
    let first = ocr_task(image.file.clone(), language.clone())
        .join()
        .expect("initial OCR worker panicked")
        .expect("initial synthetic English OCR failed");
    assert_eq!(
        first, EXPECTED_TEXT,
        "fixed English test image was not recognized"
    );
    let automatic = ocr_task(image.file.clone(), "auto".into())
        .join()
        .expect("automatic OCR worker panicked");
    let auto_supported = match automatic {
        Ok(text) => {
            assert!(
                !text.is_empty(),
                "automatic OCR returned empty synthetic text"
            );
            println!("automatic synthetic OCR: {text}");
            true
        }
        Err(error) => {
            eprintln!("automatic OCR unavailable for this profile: {error}");
            false
        }
    };
    report_memory("OCR warmed");
    idle_unload_pressure(address);
    for iteration in 0..24 {
        let text = ocr_task(image.file.clone(), language.clone())
            .join()
            .expect("sequential OCR worker panicked")
            .expect("sequential synthetic OCR failed");
        assert_eq!(text, EXPECTED_TEXT);
        if auto_supported && iteration % 4 == 0 {
            let text = ocr_task(image.file.clone(), "auto".into())
                .join()
                .expect("repeated automatic OCR worker panicked")
                .expect("previously available automatic OCR failed");
            assert!(!text.is_empty());
        }
        idle_unload_pressure(address);
    }
    for _ in 0..8 {
        let first = ocr_task(image.file.clone(), language.clone());
        let second = ocr_task(image.file.clone(), language.clone());
        for worker in [first, second] {
            let text = worker
                .join()
                .expect("concurrent OCR worker panicked")
                .expect("concurrent synthetic OCR failed");
            assert_eq!(text, EXPECTED_TEXT);
        }
        idle_unload_pressure(address);
    }
    report_memory("OCR stress complete");
    println!(
        "OCR lifecycle completed: synthetic HELLO WORLD, 24 sequential and 8 concurrent pairs"
    );
}
