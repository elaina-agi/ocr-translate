# OCR Translate

[English](README.en.md) | **简体中文**

一个轻量的 Windows 截图 OCR 与翻译工具。使用 `Ctrl+Shift+T` 框选屏幕，识别原文后在悬浮窗口中查看译文；也可以在主窗口翻译图片或直接输入的文字。

[下载 Windows 版本](https://github.com/elaina-agi/ocr-translate/releases/latest) · [查看所有版本](https://github.com/elaina-agi/ocr-translate/releases)

## 功能

- **屏幕框选翻译**：默认快捷键为 `Ctrl+Shift+T`，也可点击“框选屏幕”。OCR 识别原文后会尽早显示结果窗和翻译进度。
- **最新截图优先**：翻译过程中可以再次框选；新截图成功后会取代旧任务，旧任务的迟到结果不会覆盖新结果。
- **多个悬浮窗口**：固定的结果窗会在后续截图中保留，新结果窗会避开已有窗口。窗口可拖动、缩放，也可复制译文或打开主窗口。
- **本地 Windows OCR**：使用 Windows OCR 识别文字，支持已安装的 OCR 语言包。常规翻译模式只把识别出的文字发送给翻译服务。
- **多种翻译渠道**：腾讯和 Google 免密钥文本翻译；火山引擎机器翻译（AK/SK）；OpenAI Chat Completions 兼容的视觉模型翻译。
- **智能翻译方向**：原文自动识别语言，可在中、英、日、韩、法、西、俄、德等语言之间互译。开启后，原文已是目标语言时改译成第二语言（默认简体中文 ⇄ 英语：中文译成英语，其他语言译成中文），无需来回切换设置。方向按原文的文字判断，英、法、西、德等拉丁字母语言之间无法区分。
- **图片与文字翻译**：支持 PNG、JPEG、WebP、BMP 图片；可在主窗口输入文字翻译，也可把图片粘贴到原文区域。
- **译文图片视图**：把译文绘制在截图上，可与原图切换对比；也可以切换到纯文字视图。
- **系统托盘和轻量模式**：可隐藏到托盘，保留全局快捷键。可选轻量模式会在空闲时关闭界面子进程以释放图形资源。
- **词典卡片**：目标语言为中文时，单个英文单词可显示音标、词性、释义、词形和例句。
- **朗读原文和译文**：悬浮窗和主窗口可朗读原文和译文；译文按目标语言、原文按文字和 OCR 识别语言选择语音。支持 Windows 本地语音（离线）、Google 免 Key 在线语音和 Azure 神经语音（F0 免费层每月 50 万字符），语速可调。
- **外观与网络设置**：日间/夜间主题、字号以及系统代理、直连和自定义 HTTP/SOCKS5 代理。

## 下载与运行

1. 前往[最新版本页面](https://github.com/elaina-agi/ocr-translate/releases/latest)，下载 Windows `.exe` 附件。
2. 运行程序。无需单独安装 Visual C++ Redistributable。
3. 按 `Ctrl+Shift+T` 框选屏幕，或在主窗口选择图片、输入文字。
4. 如使用火山引擎或视觉模型，在设置中配置对应密钥、服务地址和模型。

应用面向 Windows 10/11。屏幕框选需要 Windows 自带的截图工具；OCR 需要 Windows 中安装对应源语言的 OCR 语言包。视觉模型还需要可用的网络连接和支持图像输入的 OpenAI Chat Completions 兼容服务。

## 翻译服务与隐私

- **腾讯 / Google**：通过其公开网页翻译接口进行文本翻译，可能受限流、服务变更或网络环境影响。只发送 OCR 识别出的文本，OCR 本身在本机执行。
- **火山引擎**：使用账户 AK/SK 和 `TranslateText` 服务。费用、免费额度和账单查询能力由火山引擎账户决定。
- **视觉模型**：所选图片会发送到设置中配置的模型服务。请确认该服务符合你对图片内容和隐私的要求。
- **朗读**：Windows 本地语音在本机合成，不上传文字，缺少的语言需在 Windows 设置 → 时间和语言 → 语音 中添加。Google 朗读把要朗读的文字发送到其公开网页接口；Azure 朗读把要朗读的文字发送到你的 Azure 语音资源，额度和费用由 Azure 账户决定。
- 配置保存在当前 Windows 用户的 `%APPDATA%\ocr-translate\config.json`。视觉模型、火山引擎和 Azure 语音密钥写入文件前使用 Windows DPAPI 加密，只能由同一台电脑上的同一 Windows 用户解密。
- 图片大小限制为 20 MB；网络请求有超时限制。临时截图保存在系统临时目录，并由程序清理。

## 从源码构建

需要 Windows 10/11、Rust 工具链和 Visual Studio C++ Build Tools：

```powershell
cargo run
cargo test --locked
cargo build --release --locked
```

发布构建产物位于 `target/release/ocr-translate.exe`。该程序为 Windows 桌面应用，不支持在非 Windows 系统上运行。

## 图标资源

- `assets/icon.svg`：可编辑的矢量源文件。
- `assets/icon.ico`：嵌入 Windows 程序的多尺寸图标。
- `assets/icon_256.png`、`assets/icon_64.png`：窗口和托盘图标源文件。

## 反馈

欢迎通过 [GitHub Issues](https://github.com/elaina-agi/ocr-translate/issues) 报告问题或提出建议。
