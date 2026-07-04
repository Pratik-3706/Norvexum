# Norvexum 🦀

Norvexum is a highly responsive, multi-threaded developer agent CLI and Terminal User Interface (TUI) built in Rust. It utilizes modern AI providers (including Gemini, Claude, OpenAI, and AICredits.in) to autonomously solve coding tasks, read and edit files, run shell commands, perform secure package checks, and search/fetch web resources—all sandboxed within the project workspace.

## 🚀 Key Features

*   **⚡ Parallel Tool Execution:** Runs multiple operations (files, web, shell commands) concurrently using Tokio wakers.
*   **🛡️ Work Space Sandbox:** Enforces strict boundary checks on all filesystem commands to keep the agent safely locked within the repository.
*   **📸 Vision & OCR Integration:** 
    *   Automatically parses user messages for image paths, encodes them, and sends them to multimodal models.
    *   Autonomous image viewing via `view_image` tool.
    *   OCR fallback via OCR.space API for text-only models.
*   **🛡️ Dependency Safety Shield:** Scans PyPI and npm package names against the Open Source Vulnerabilities (OSV) database and runs typosquatting heuristics before installing software.
*   **💾 Config Persistence:** Automatically persists active providers and models between sessions inside `.norvexum/config.toml`.

---

## 🛠️ Installation & Setup

### Prerequisites
Make sure you have Rust and Cargo installed:
*   [Rust Installation Guide](https://www.rust-lang.org/tools/install)

### Build and Run
1. Clone the repository and navigate to it:
   ```bash
   git clone https://github.com/Pratik-3706/Norvexum.git
   cd Norvexum
   ```
2. Build the release binary:
   ```bash
   cargo build --release
   ```
3. Run the interactive TUI:
   ```bash
   cargo run --release
   ```

### Configuration
1. Initialize the environment:
   ```bash
   norvexum init
   ```
2. Open the generated `.env` file and insert your API keys:
   ```env
   AICREDITS_API_KEY=your_key_here
   GOOGLE_AI_API_KEY=your_key_here
   TAVILY_API_KEY=your_key_here
   OCR_SPACE_API_KEY=your_key_here
   ```

---

## ⚖️ License & Copyright

### Software / Codebase
The code of this project is subject to standard copyright laws. See [LICENSE](LICENSE) for more details.

### Assets Copyright (CRITICAL)
> [!IMPORTANT]
> All files, graphics, and animations located in the `assets/` directory (including `assets/cheap_logo.png` and contents of `assets/loading_animation/`) are **strictly copyrighted by the project author**.
>
> **You are NOT permitted to:**
> *   Sell or distribute these assets.
> *   Modify, alter, or adapt these assets in any way.
> *   Use these assets for any commercial purposes.
>
> Violation of these terms is subject to legal action under copyright infringement laws.
