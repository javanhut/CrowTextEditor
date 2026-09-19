//! crow.toml — the only file you edit.
//!
//! The NvCrow idea, native: a declarative spec — theme, options, keys,
//! language servers — with no programming language in the config. Names in,
//! wiring out. Everything is optional; a missing file means defaults, and the
//! first run writes a commented template to grow from.
//!
//! Parsed with a tiny TOML subset — `[sections]`, `key = value` with quoted
//! strings and integers, `#` comments. ponytail: swap in the `toml` crate if
//! the config ever needs arrays or nesting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct Config {
    pub theme: String,
    pub tab_width: usize,
    pub scrolloff: usize,
    pub autoclose: bool,
    pub icons: bool,
    pub format_on_save: bool,
    pub show_hidden: bool,
    pub soft_wrap: bool,
    pub trailing_whitespace: bool,
    pub strip_trailing_whitespace: bool,
    /// Keep undo history across sessions, in the state dir.
    pub persistent_undo: bool,
    /// Write unsaved buffers to swap files so a crash loses seconds, not work.
    pub swap_files: bool,
    /// Take the mouse: click to place the cursor, drag to select, wheel to scroll.
    pub mouse: bool,
    /// Reload buffers whose file changed on disk (unmodified ones only).
    pub auto_reload: bool,
    /// Change markers in the gutter against the last ivaldi seal.
    pub vcs_gutter: bool,
    /// What `space t` runs; empty means `$SHELL`, falling back to `/bin/sh`.
    pub shell: String,
    /// Extra bindings per mode: (key sequence, command name).
    pub keys_normal: Vec<(String, String)>,
    pub keys_insert: Vec<(String, String)>,
    /// Language servers: (file extension, server command line).
    pub lsp: Vec<(String, String)>,
    /// Formatters: (file extension, command reading stdin, writing stdout).
    /// Entries here override the built-in table.
    pub fmt: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "tokyonight".into(),
            tab_width: 4,
            scrolloff: 3,
            autoclose: true,
            icons: true,
            format_on_save: true,
            show_hidden: false,
            soft_wrap: true,
            trailing_whitespace: true,
            strip_trailing_whitespace: true,
            persistent_undo: true,
            swap_files: true,
            mouse: true,
            auto_reload: true,
            vcs_gutter: true,
            shell: String::new(),
            keys_normal: Vec::new(),
            keys_insert: Vec::new(),
            lsp: vec![("rs".into(), "rust-analyzer".into())],
            fmt: Vec::new(),
        }
    }
}

// Options read from hot paths live in statics, set by `apply`. Nothing
// caches them, so `apply` running a second time (`:config!`) is all a
// reload needs for this half of the config.
static TAB_WIDTH: AtomicUsize = AtomicUsize::new(4);
static SCROLLOFF: AtomicUsize = AtomicUsize::new(3);
static AUTOCLOSE: AtomicBool = AtomicBool::new(true);
static ICONS: AtomicBool = AtomicBool::new(true);
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false);
static FORMAT_ON_SAVE: AtomicBool = AtomicBool::new(true);
static SOFT_WRAP: AtomicBool = AtomicBool::new(true);
static TRAILING_WHITESPACE: AtomicBool = AtomicBool::new(true);
static STRIP_TRAILING_WHITESPACE: AtomicBool = AtomicBool::new(true);
static PERSISTENT_UNDO: AtomicBool = AtomicBool::new(true);
static SWAP_FILES: AtomicBool = AtomicBool::new(true);
static MOUSE: AtomicBool = AtomicBool::new(true);
static AUTO_RELOAD: AtomicBool = AtomicBool::new(true);
static VCS_GUTTER: AtomicBool = AtomicBool::new(true);

pub fn tab_width() -> usize {
    TAB_WIDTH.load(Ordering::Relaxed)
}

pub fn scrolloff() -> usize {
    SCROLLOFF.load(Ordering::Relaxed)
}

pub fn autoclose() -> bool {
    AUTOCLOSE.load(Ordering::Relaxed)
}

pub fn icons() -> bool {
    ICONS.load(Ordering::Relaxed)
}

pub fn show_hidden() -> bool {
    SHOW_HIDDEN.load(Ordering::Relaxed)
}

/// Flip dotfile visibility at runtime. Returns the new value.
pub fn toggle_hidden() -> bool {
    !SHOW_HIDDEN.fetch_xor(true, Ordering::Relaxed)
}

pub fn format_on_save() -> bool {
    FORMAT_ON_SAVE.load(Ordering::Relaxed)
}

/// Wrap long lines onto the next row instead of scrolling sideways.
pub fn soft_wrap() -> bool {
    SOFT_WRAP.load(Ordering::Relaxed)
}

/// Flip soft wrap at runtime (`:wrap`). Returns the new value.
pub fn toggle_soft_wrap() -> bool {
    !SOFT_WRAP.fetch_xor(true, Ordering::Relaxed)
}

/// Tint spaces and tabs left at the end of a line.
pub fn trailing_whitespace() -> bool {
    TRAILING_WHITESPACE.load(Ordering::Relaxed)
}

/// Cut that whitespace on write.
pub fn strip_trailing_whitespace() -> bool {
    STRIP_TRAILING_WHITESPACE.load(Ordering::Relaxed)
}

pub fn persistent_undo() -> bool {
    PERSISTENT_UNDO.load(Ordering::Relaxed)
}

pub fn swap_files() -> bool {
    SWAP_FILES.load(Ordering::Relaxed)
}

pub fn mouse() -> bool {
    MOUSE.load(Ordering::Relaxed)
}

pub fn auto_reload() -> bool {
    AUTO_RELOAD.load(Ordering::Relaxed)
}

pub fn vcs_gutter() -> bool {
    VCS_GUTTER.load(Ordering::Relaxed)
}

/// Install the config's options and theme as the live values. False when the
/// theme name was not recognised — startup ignores that, `:config!` reports it
/// rather than looking like the reload did nothing.
pub fn apply(config: &Config) -> bool {
    TAB_WIDTH.store(config.tab_width.clamp(1, 16), Ordering::Relaxed);
    SCROLLOFF.store(config.scrolloff.min(50), Ordering::Relaxed);
    AUTOCLOSE.store(config.autoclose, Ordering::Relaxed);
    ICONS.store(config.icons, Ordering::Relaxed);
    SHOW_HIDDEN.store(config.show_hidden, Ordering::Relaxed);
    FORMAT_ON_SAVE.store(config.format_on_save, Ordering::Relaxed);
    SOFT_WRAP.store(config.soft_wrap, Ordering::Relaxed);
    TRAILING_WHITESPACE.store(config.trailing_whitespace, Ordering::Relaxed);
    STRIP_TRAILING_WHITESPACE.store(config.strip_trailing_whitespace, Ordering::Relaxed);
    PERSISTENT_UNDO.store(config.persistent_undo, Ordering::Relaxed);
    SWAP_FILES.store(config.swap_files, Ordering::Relaxed);
    MOUSE.store(config.mouse, Ordering::Relaxed);
    AUTO_RELOAD.store(config.auto_reload, Ordering::Relaxed);
    VCS_GUTTER.store(config.vcs_gutter, Ordering::Relaxed);
    *FMT.lock().unwrap() = config.fmt.clone();
    *SHELL.lock().unwrap() = config.shell.clone();
    crate::theme::set(&config.theme)
}

/// The `[fmt]` overrides. A `Mutex` rather than a `OnceLock` so a reload can
/// replace them.
/// ponytail: one lock taken on `:w` and `:fmt`, not per keystroke; an
/// `RwLock` if a formatter ever ends up on a hot path.
static FMT: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// The `shell` option, read when a terminal is opened.
static SHELL: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The program and arguments the terminal split runs: the config's `shell`
/// (split on whitespace, so `bash --login` works), else `$SHELL`, else
/// `/bin/sh`.
pub fn shell() -> (String, Vec<String>) {
    let configured = SHELL.lock().unwrap().clone();
    let line = if configured.trim().is_empty() {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    } else {
        configured
    };
    let mut parts = line.split_whitespace().map(str::to_string);
    let program = parts.next().unwrap_or_else(|| "/bin/sh".to_string());
    (program, parts.collect())
}

/// Built-in formatters, all reading the buffer on stdin and writing the
/// result to stdout. `{file}` becomes the buffer's path (for tools that
/// pick style/parser from the filename); `{tmp}` becomes a throwaway copy of
/// the buffer, for tools that only rewrite files in place.
const BUILTIN_FMT: &[(&str, &str)] = &[
    ("rs", "rustfmt --edition 2021"),
    ("oxi", "oxigen fmt {tmp}"),
    ("go", "gofmt"),
    ("py", "black -q -"),
    ("sh", "shfmt"),
    ("bash", "shfmt"),
    ("zig", "zig fmt --stdin"),
    ("lua", "stylua -"),
    ("toml", "taplo fmt -"),
    ("odin", "odinfmt -stdin"),
    ("c", "clang-format --assume-filename={file}"),
    ("h", "clang-format --assume-filename={file}"),
    ("cpp", "clang-format --assume-filename={file}"),
    ("hpp", "clang-format --assume-filename={file}"),
    ("cc", "clang-format --assume-filename={file}"),
    ("cxx", "clang-format --assume-filename={file}"),
    ("hh", "clang-format --assume-filename={file}"),
    ("java", "clang-format --assume-filename={file}"),
    ("js", "prettier --stdin-filepath {file}"),
    ("jsx", "prettier --stdin-filepath {file}"),
    ("mjs", "prettier --stdin-filepath {file}"),
    ("cjs", "prettier --stdin-filepath {file}"),
    ("ts", "prettier --stdin-filepath {file}"),
    ("mts", "prettier --stdin-filepath {file}"),
    ("tsx", "prettier --stdin-filepath {file}"),
    ("json", "prettier --stdin-filepath {file}"),
    ("css", "prettier --stdin-filepath {file}"),
    ("html", "prettier --stdin-filepath {file}"),
    ("htm", "prettier --stdin-filepath {file}"),
    ("md", "prettier --stdin-filepath {file}"),
    ("markdown", "prettier --stdin-filepath {file}"),
    ("yml", "prettier --stdin-filepath {file}"),
    ("yaml", "prettier --stdin-filepath {file}"),
];

/// Built-in language servers by file extension, used when crow.toml's [lsp]
/// section has no entry for the extension. Every one has an `:install` entry.
const BUILTIN_LSP: &[(&str, &str)] = &[
    ("rs", "rust-analyzer"),
    ("oxi", "oxigen-lsp"),
    ("py", "pyright-langserver --stdio"),
    ("go", "gopls"),
    ("js", "typescript-language-server --stdio"),
    ("jsx", "typescript-language-server --stdio"),
    ("mjs", "typescript-language-server --stdio"),
    ("cjs", "typescript-language-server --stdio"),
    ("ts", "typescript-language-server --stdio"),
    ("mts", "typescript-language-server --stdio"),
    ("tsx", "typescript-language-server --stdio"),
    ("c", "clangd"),
    ("h", "clangd"),
    ("cpp", "clangd"),
    ("hpp", "clangd"),
    ("cc", "clangd"),
    ("cxx", "clangd"),
    ("hh", "clangd"),
    ("sh", "bash-language-server start"),
    ("bash", "bash-language-server start"),
    ("lua", "lua-language-server"),
    ("zig", "zls"),
    ("odin", "ols"),
    ("java", "jdtls"),
    ("rb", "ruby-lsp"),
    ("php", "intelephense --stdio"),
    ("md", "marksman server"),
    ("markdown", "marksman server"),
    ("toml", "taplo lsp stdio"),
    ("yml", "yaml-language-server --stdio"),
    ("yaml", "yaml-language-server --stdio"),
    ("json", "vscode-json-language-server --stdio"),
    ("css", "vscode-css-language-server --stdio"),
    ("html", "vscode-html-language-server --stdio"),
    ("htm", "vscode-html-language-server --stdio"),
];

/// The built-in language server command for a file extension.
pub fn builtin_lsp(ext: &str) -> Option<&'static str> {
    BUILTIN_LSP.iter().find(|(e, _)| *e == ext).map(|(_, c)| *c)
}

// ---- installing missing tools ----------------------------------------------

/// A package manager crow can install through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pm {
    Brew,
    /// Raven's manager: a pacman front-end, so it carries pacman's packages.
    Rvn,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
}

impl Pm {
    /// The shell command that installs `pkg`, asking nothing. Homebrew owns
    /// its own prefix; every system manager writes into system directories,
    /// so every system manager needs root — except rvn when its daemon is up,
    /// which then does the privileged work on our behalf.
    fn install(self, pkg: &str) -> String {
        let (root, cmd) = match self {
            Pm::Brew => (false, format!("brew install {pkg}")),
            Pm::Rvn => (!rvnd_running(), format!("rvn install -y {pkg}")),
            Pm::Apt => (true, format!("apt-get install -y {pkg}")),
            Pm::Dnf => (true, format!("dnf install -y {pkg}")),
            Pm::Pacman => (true, format!("pacman -S --needed --noconfirm {pkg}")),
            Pm::Zypper => (true, format!("zypper --non-interactive install {pkg}")),
            Pm::Apk => (true, format!("apk add {pkg}")),
        };
        if root && !is_root() {
            format!("sudo {cmd}")
        } else {
            cmd
        }
    }
}

/// The manager to install through, worked out once from what is on PATH.
///
/// macOS means Homebrew. On Linux the distro's own manager wins even when
/// Homebrew is installed beside it: it is what owns /usr/bin, it is where
/// every other package on the machine came from, and every Linux box has one.
/// Homebrew is the answer there only when nothing else answered. rvn comes
/// before the pacman it wraps: a Raven box has both, and rvn is the one its
/// user reaches for.
pub fn package_manager() -> Option<Pm> {
    static PM: std::sync::OnceLock<Option<Pm>> = std::sync::OnceLock::new();
    *PM.get_or_init(|| {
        if cfg!(target_os = "macos") {
            return Some(Pm::Brew);
        }
        [
            ("rvn", Pm::Rvn),
            ("pacman", Pm::Pacman),
            ("apt-get", Pm::Apt),
            ("dnf", Pm::Dnf),
            ("zypper", Pm::Zypper),
            ("apk", Pm::Apk),
            ("brew", Pm::Brew),
        ]
        .into_iter()
        .find(|(program, _)| on_path(program))
        .map(|(_, pm)| pm)
    })
}

/// Is `program` runnable? A walk of PATH rather than spawning `which`.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Is rvn's daemon up? Then `rvn install` needs no sudo: the client hands
/// the job to rvnd over its control socket, and rvnd is the one running as
/// root. The socket path is rvn's own default, or `$RVN_SOCKET` when set.
fn rvnd_running() -> bool {
    let socket = std::env::var_os("RVN_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/rvn/ctl"));
    std::fs::metadata(socket).is_ok()
}

/// Already root: the package manager needs no `sudo`, and a container that
/// never installed `sudo` still gets its tools.
#[cfg(unix)]
fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// How to install a missing tool, keyed by the program :fmt or the LSP
/// tries to spawn. Powers `:install` and the "install? (y/N)" offer.
///
/// Anything its own ecosystem ships — rustup, npm, gem, a source build — is
/// installed that way on every platform, so it is one command here. Tools
/// that come from the operating system live in `PACKAGES` instead, where
/// each manager gets to name them itself.
const INSTALLERS: &[(&str, &str)] = &[
    ("prettier", "npm install -g prettier"),
    ("rustfmt", "rustup component add rustfmt"),
    ("rust-analyzer", "rustup component add rust-analyzer"),
    ("pyright-langserver", "npm install -g pyright"),
    (
        "typescript-language-server",
        "npm install -g typescript typescript-language-server",
    ),
    (
        "bash-language-server",
        "npm install -g bash-language-server",
    ),
    (
        "yaml-language-server",
        "npm install -g yaml-language-server",
    ),
    (
        "vscode-json-language-server",
        "npm install -g vscode-langservers-extracted",
    ),
    (
        "vscode-css-language-server",
        "npm install -g vscode-langservers-extracted",
    ),
    (
        "vscode-html-language-server",
        "npm install -g vscode-langservers-extracted",
    ),
    ("ruby-lsp", "gem install ruby-lsp"),
    ("intelephense", "npm install -g intelephense"),
    // Oxigen ships no package: build it from its own repo, cached under
    // ~/.cache/crow. `make install` picks /usr/local or ~/.local by itself,
    // so neither needs sudo.
    ("oxigen", OXIGEN_BUILD),
    ("oxigen-lsp", OXIGEN_LSP_BUILD),
];

/// Tools that come from the operating system: `(program, package name under
/// each manager known to carry it, what to do where none does)`.
///
/// A manager is listed only where the package is really there, so a machine
/// whose manager doesn't have the tool falls through to the third field —
/// building it from its own ecosystem — rather than being told to install a
/// package that doesn't exist. `None` there means we have nothing to suggest
/// beyond Homebrew, which is consulted last and only if this machine has it.
/// One `PACKAGES` row: program, (manager, package) pairs, fallback build.
type PackageRow = (
    &'static str,
    &'static [(Pm, &'static str)],
    Option<&'static str>,
);

const PACKAGES: &[PackageRow] = &[
    (
        "gofmt", // ships with the Go toolchain
        &[
            (Pm::Brew, "go"),
            (Pm::Apt, "golang-go"),
            (Pm::Dnf, "golang"),
            (Pm::Pacman, "go"),
            (Pm::Zypper, "go"),
            (Pm::Apk, "go"),
        ],
        None,
    ),
    (
        "gopls",
        &[(Pm::Brew, "gopls"), (Pm::Pacman, "gopls")],
        Some("go install golang.org/x/tools/gopls@latest"),
    ),
    (
        "black",
        &[
            (Pm::Brew, "black"),
            (Pm::Apt, "black"),
            (Pm::Dnf, "python3-black"),
            (Pm::Pacman, "python-black"),
            (Pm::Zypper, "python3-black"),
            (Pm::Apk, "py3-black"),
        ],
        Some("pipx install black"),
    ),
    (
        "ruff",
        &[
            (Pm::Brew, "ruff"),
            (Pm::Apt, "ruff"),
            (Pm::Dnf, "ruff"),
            (Pm::Pacman, "ruff"),
            (Pm::Apk, "ruff"),
        ],
        Some("pipx install ruff"),
    ),
    (
        "shfmt",
        &[
            (Pm::Brew, "shfmt"),
            (Pm::Apt, "shfmt"),
            (Pm::Dnf, "shfmt"),
            (Pm::Pacman, "shfmt"),
            (Pm::Apk, "shfmt"),
        ],
        Some("go install mvdan.cc/sh/v3/cmd/shfmt@latest"),
    ),
    (
        "stylua",
        &[(Pm::Brew, "stylua"), (Pm::Pacman, "stylua")],
        Some("cargo install stylua"),
    ),
    (
        "taplo",
        &[(Pm::Brew, "taplo")],
        Some("cargo install taplo-cli --locked"),
    ),
    (
        "clang-format",
        &[
            (Pm::Brew, "clang-format"),
            (Pm::Apt, "clang-format"),
            (Pm::Dnf, "clang-tools-extra"),
            (Pm::Pacman, "clang"),
            (Pm::Zypper, "clang-tools"),
            (Pm::Apk, "clang-extra-tools"),
        ],
        None,
    ),
    (
        "clangd",
        &[
            (Pm::Brew, "llvm"),
            (Pm::Apt, "clangd"),
            (Pm::Dnf, "clang-tools-extra"),
            (Pm::Pacman, "clang"),
            (Pm::Zypper, "clang-tools"),
            (Pm::Apk, "clang-extra-tools"),
        ],
        None,
    ),
    (
        "zig",
        &[
            (Pm::Brew, "zig"),
            (Pm::Dnf, "zig"),
            (Pm::Pacman, "zig"),
            (Pm::Zypper, "zig"),
            (Pm::Apk, "zig"),
        ],
        None,
    ),
    ("zls", &[(Pm::Brew, "zls"), (Pm::Pacman, "zls")], None),
    // Odin's language server carries odinfmt with it.
    ("ols", &[(Pm::Brew, "ols")], None),
    ("odinfmt", &[(Pm::Brew, "ols")], None),
    ("jdtls", &[(Pm::Brew, "jdtls")], None),
    (
        "lua-language-server",
        &[
            (Pm::Brew, "lua-language-server"),
            (Pm::Apt, "lua-language-server"),
            (Pm::Dnf, "lua-language-server"),
            (Pm::Pacman, "lua-language-server"),
        ],
        None,
    ),
    ("marksman", &[(Pm::Brew, "marksman")], None),
];

const OXIGEN_BUILD: &str = concat!(
    "git clone --depth 1 https://github.com/javanhut/OxigenLang.git ~/.cache/crow/OxigenLang",
    " || git -C ~/.cache/crow/OxigenLang pull --ff-only;",
    " make -C ~/.cache/crow/OxigenLang install"
);
const OXIGEN_LSP_BUILD: &str = concat!(
    "git clone --depth 1 https://github.com/javanhut/OxigenLang.git ~/.cache/crow/OxigenLang",
    " || git -C ~/.cache/crow/OxigenLang pull --ff-only;",
    " make -C ~/.cache/crow/OxigenLang install-lsp"
);

/// The shell command that installs `program` on this machine, if we know one.
pub fn installer(program: &str) -> Option<String> {
    install_command(program, package_manager())
}

/// `installer`, with the manager passed in — so a test can ask what a Debian
/// box would be told without being one.
fn install_command(program: &str, pm: Option<Pm>) -> Option<String> {
    if let Some((_, cmd)) = INSTALLERS.iter().find(|(p, _)| *p == program) {
        return Some((*cmd).to_string());
    }
    let (_, names, fallback) = PACKAGES.iter().find(|(p, _, _)| *p == program)?;
    // rvn installs from pacman's repositories, so a package is called
    // whatever pacman calls it — one column in the table covers both.
    let named = |pm: Pm| {
        let table_pm = match pm {
            Pm::Rvn => Pm::Pacman,
            pm => pm,
        };
        names
            .iter()
            .find(|(m, _)| *m == table_pm)
            .map(|(_, name)| *name)
    };

    // This machine's own manager first; then a build from the tool's own
    // ecosystem; then Homebrew, which runs on Linux too — if it is installed
    // there, someone put it there on purpose.
    if let Some((pm, name)) = pm.and_then(|pm| Some((pm, named(pm)?))) {
        return Some(pm.install(name));
    }
    if let Some(cmd) = fallback {
        return Some((*cmd).to_string());
    }
    named(Pm::Brew)
        .filter(|_| on_path("brew"))
        .map(|name| Pm::Brew.install(name))
}

/// The formatter command line for a file extension: config entries first,
/// then the built-in table.
pub fn formatter(ext: &str) -> Option<String> {
    if let Some((_, command)) = FMT.lock().unwrap().iter().find(|(e, _)| e == ext) {
        return Some(command.clone());
    }
    BUILTIN_FMT
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, command)| command.to_string())
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("crow/crow.toml")
}

/// Where crow keeps what it learns between runs: recent files, undo
/// history, swap files. XDG state, not config — none of it is hand-edited.
pub fn state_dir() -> PathBuf {
    // Tests save and open files; their undo and swap files must not land in
    // the real state dir of whoever runs `cargo test`.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("crow-test-state-{}", std::process::id()));
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_default()
        .join("crow")
}

/// The recent-files list, most recent first.
fn recent_path() -> PathBuf {
    state_dir().join("recent")
}

/// Recently opened files that still exist, most recent first.
pub fn recent_files() -> Vec<PathBuf> {
    std::fs::read_to_string(recent_path())
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

/// Move `path` to the front of the recent-files list.
pub fn record_recent(path: &Path) {
    let Ok(abs) = path.canonicalize() else {
        return;
    };
    let mut lines = vec![abs.to_string_lossy().into_owned()];
    lines.extend(
        recent_files()
            .into_iter()
            .filter(|p| *p != abs)
            .map(|p| p.to_string_lossy().into_owned()),
    );
    lines.truncate(50);
    let file = recent_path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, lines.join("\n"));
}

const TEMPLATE: &str = r#"# crow.toml — crow's config. Everything here is optional;
# delete a line and the default comes back.

theme = "tokyonight"     # tokyonight | gruvbox | mono | default (terminal colors)

[options]
tab_width = 4
scrolloff = 3
autoclose = true         # type ( [ { " ' and the closer appears
icons = true             # Nerd Font file icons in the tree (needs a Nerd Font)
format_on_save = true    # pipe the buffer through its [fmt] formatter on :w
show_hidden = false      # dotfiles, .git, and build dirs everywhere (toggle: . in the tree, :toggle_hidden)
soft_wrap = true         # wrap long lines instead of scrolling sideways (toggle: :wrap)
trailing_whitespace = true          # tint spaces left at the end of a line
strip_trailing_whitespace = true    # and cut them on :w (never in markdown, where they mean a line break)
persistent_undo = true   # undo history survives closing the file
swap_files = true        # unsaved edits are written aside; :recover brings them back after a crash
mouse = true             # click, drag to select, wheel to scroll (shift+drag for the terminal's own selection)
auto_reload = true       # a file changed on disk reloads if you haven't edited it
vcs_gutter = true        # added/changed/deleted markers against the last ivaldi seal
# shell = "zsh"          # what space t / :term runs (default: $SHELL, then /bin/sh)

# Language servers: file extension = server command. crow starts the first
# server whose extension matches an open file.
[lsp]
rs = "rust-analyzer"
# py = "pyright-langserver --stdio"
# go = "gopls"
# oxi = "oxigen-lsp"

# Extra keybindings: "sequence" = "command". Any command callable with
# `:name` can be bound. Later bindings win over defaults.
# Modifiers: "Ctrl-" and "Alt-" (the short "C-"/"A-" forms also work).
[keys.normal]
# "gq" = "quit"
# "Ctrl-p" = "search"

[keys.insert]

# Formatters for :fmt — file extension = command reading stdin, writing
# stdout ({file} becomes the buffer's path). Common tools (rustfmt, gofmt,
# black, prettier, clang-format...) are built in; entries here override.
[fmt]
# py = "ruff format -"
"#;

/// Read the config, creating a commented template on first run.
pub fn load() -> Config {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, TEMPLATE);
            Config::default()
        }
    }
}

fn parse(text: &str) -> Config {
    let mut config = Config {
        lsp: Vec::new(), // replaced wholesale if the file has an [lsp] section
        ..Config::default()
    };
    let mut has_lsp_section = false;
    let mut section = String::new();

    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            if section == "lsp" {
                has_lsp_section = true;
            }
            continue;
        }
        let Some((key, value)) = split_kv(&line) else {
            continue;
        };
        match section.as_str() {
            "" => {
                if key == "theme" {
                    config.theme = value;
                }
            }
            "options" => match key.as_str() {
                "tab_width" => config.tab_width = value.parse().unwrap_or(config.tab_width),
                "scrolloff" => config.scrolloff = value.parse().unwrap_or(config.scrolloff),
                "autoclose" => config.autoclose = value.parse().unwrap_or(config.autoclose),
                "icons" => config.icons = value.parse().unwrap_or(config.icons),
                "show_hidden" => config.show_hidden = value.parse().unwrap_or(config.show_hidden),
                "format_on_save" => {
                    config.format_on_save = value.parse().unwrap_or(config.format_on_save)
                }
                "soft_wrap" => config.soft_wrap = value.parse().unwrap_or(config.soft_wrap),
                "trailing_whitespace" => {
                    config.trailing_whitespace = value.parse().unwrap_or(config.trailing_whitespace)
                }
                "strip_trailing_whitespace" => {
                    config.strip_trailing_whitespace =
                        value.parse().unwrap_or(config.strip_trailing_whitespace)
                }
                "persistent_undo" => {
                    config.persistent_undo = value.parse().unwrap_or(config.persistent_undo)
                }
                "swap_files" => config.swap_files = value.parse().unwrap_or(config.swap_files),
                "mouse" => config.mouse = value.parse().unwrap_or(config.mouse),
                "auto_reload" => config.auto_reload = value.parse().unwrap_or(config.auto_reload),
                "vcs_gutter" => config.vcs_gutter = value.parse().unwrap_or(config.vcs_gutter),
                "shell" => config.shell = value,
                _ => {}
            },
            "keys.normal" => config.keys_normal.push((key, value)),
            "keys.insert" => config.keys_insert.push((key, value)),
            "lsp" => config.lsp.push((key, value)),
            "fmt" => config.fmt.push((key, value)),
            _ => {}
        }
    }

    if !has_lsp_section {
        config.lsp = Config::default().lsp;
    }
    config
}

/// Drop a `#` comment, ignoring `#` inside quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

/// `key = value` with optionally quoted key and value.
fn split_kv(line: &str) -> Option<(String, String)> {
    let eq = find_unquoted_eq(line)?;
    let key = unquote(line[..eq].trim());
    let value = unquote(line[eq + 1..].trim());
    if key.is_empty() {
        None
    } else {
        Some((key, value))
    }
}

fn find_unquoted_eq(line: &str) -> Option<usize> {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '=' if !in_string => return Some(i),
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let config = parse(
            r##"
theme = "gruvbox"  # comment after value

[options]
tab_width = 8
scrolloff = 5

[keys.normal]
"C-p" = "search"
gq = "quit"

[keys.insert]
"C-s" = "save"

[lsp]
py = "pyright-langserver --stdio"
"##,
        );
        assert_eq!(config.theme, "gruvbox");
        assert_eq!(config.tab_width, 8);
        assert_eq!(config.scrolloff, 5);
        assert_eq!(
            config.keys_normal,
            vec![
                ("C-p".to_string(), "search".to_string()),
                ("gq".to_string(), "quit".to_string()),
            ]
        );
        assert_eq!(
            config.keys_insert,
            vec![("C-s".to_string(), "save".to_string())]
        );
        assert_eq!(
            config.lsp,
            vec![("py".to_string(), "pyright-langserver --stdio".to_string())]
        );
    }

    #[test]
    fn missing_sections_mean_defaults() {
        let config = parse("theme = \"mono\"\n");
        assert_eq!(config.theme, "mono");
        assert_eq!(config.tab_width, 4);
        // No [lsp] section: the built-in rust-analyzer entry stays.
        assert_eq!(config.lsp, Config::default().lsp);
    }

    #[test]
    fn an_lsp_section_replaces_the_default_table() {
        let config = parse("[lsp]\ngo = \"gopls\"\n");
        assert_eq!(config.lsp, vec![("go".to_string(), "gopls".to_string())]);
    }

    #[test]
    fn comments_and_junk_are_ignored() {
        let config = parse("# hello\ntheme = \"x # not a comment\"\nnoise without equals\n");
        assert_eq!(config.theme, "x # not a comment");
    }

    #[test]
    fn formatter_lookup_falls_back_to_builtins() {
        assert_eq!(formatter("go").as_deref(), Some("gofmt"));
        assert!(formatter("xyz").is_none());
    }

    #[test]
    fn ecosystem_tools_install_the_same_way_everywhere() {
        for pm in [None, Some(Pm::Brew), Some(Pm::Apt), Some(Pm::Pacman)] {
            assert_eq!(
                install_command("prettier", pm).as_deref(),
                Some("npm install -g prettier")
            );
        }
        assert!(install_command("no-such-tool", Some(Pm::Apt)).is_none());
    }

    /// The thing this table exists to get right: a Linux box is told to use
    /// the package manager it actually has, not Homebrew.
    #[test]
    fn a_system_package_uses_this_machines_manager() {
        let arch = install_command("clangd", Some(Pm::Pacman)).unwrap();
        assert!(
            arch.ends_with("pacman -S --needed --noconfirm clang"),
            "{arch}"
        );
        assert!(!arch.contains("brew"));
        let debian = install_command("clangd", Some(Pm::Apt)).unwrap();
        assert!(debian.ends_with("apt-get install -y clangd"), "{debian}");
        // The package is whatever that manager calls it, not one name for all.
        let fedora = install_command("clangd", Some(Pm::Dnf)).unwrap();
        assert!(fedora.ends_with("clang-tools-extra"), "{fedora}");
        assert_eq!(
            install_command("clangd", Some(Pm::Brew)).as_deref(),
            Some("brew install llvm")
        );
    }

    /// Everything but Homebrew writes into system directories, so everything
    /// but Homebrew is prefixed with sudo — unless we are already root.
    #[test]
    fn system_managers_need_root_and_homebrew_does_not() {
        let sudo = |program, pm| {
            install_command(program, Some(pm))
                .unwrap()
                .starts_with("sudo ")
        };
        assert_eq!(sudo("clangd", Pm::Dnf), !is_root());
        assert_eq!(sudo("clangd", Pm::Apk), !is_root());
        assert!(!sudo("clangd", Pm::Brew));
    }

    /// Raven's rvn wraps pacman: same package names, its own command line,
    /// and no sudo once its daemon is the one doing the writing.
    #[test]
    fn rvn_installs_pacmans_packages_its_own_way() {
        let raven = install_command("clang-format", Some(Pm::Rvn)).unwrap();
        assert!(raven.ends_with("rvn install -y clang"), "{raven}");
        assert!(!raven.contains("pacman"));
        assert_eq!(
            raven.starts_with("sudo "),
            !is_root() && !rvnd_running(),
            "{raven}"
        );
        // A package pacman lacks is one rvn lacks too: build it instead.
        assert_eq!(
            install_command("taplo", Some(Pm::Rvn)).as_deref(),
            Some("cargo install taplo-cli --locked")
        );
    }

    /// A distro that has no package for a tool gets a build from the tool's
    /// own ecosystem rather than an install command that would just fail.
    #[test]
    fn a_distro_without_the_package_builds_it_instead() {
        assert!(install_command("stylua", Some(Pm::Pacman))
            .unwrap()
            .ends_with("pacman -S --needed --noconfirm stylua"));
        assert_eq!(
            install_command("stylua", Some(Pm::Apt)).as_deref(),
            Some("cargo install stylua")
        );
        assert_eq!(
            install_command("gopls", Some(Pm::Dnf)).as_deref(),
            Some("go install golang.org/x/tools/gopls@latest")
        );
    }

    #[test]
    fn a_fmt_section_is_parsed() {
        let config = parse("[fmt]\npy = \"ruff format -\"\n");
        assert_eq!(
            config.fmt,
            vec![("py".to_string(), "ruff format -".to_string())]
        );
    }

    /// The whole reason `FMT` is a `Mutex`: with a `OnceLock` the second
    /// `apply` was silently discarded and `:config!` would keep formatting
    /// with the config you just edited away.
    #[test]
    fn apply_replaces_the_fmt_overrides() {
        let _guard = crate::theme::TEST_LOCK.lock().unwrap();
        apply(&Config {
            fmt: vec![("py".into(), "ruff format -".into())],
            ..Config::default()
        });
        assert_eq!(formatter("py").as_deref(), Some("ruff format -"));
        apply(&Config {
            fmt: vec![("py".into(), "blue -".into())],
            ..Config::default()
        });
        assert_eq!(formatter("py").as_deref(), Some("blue -"));
        // Dropping the entry falls back to the built-in table.
        apply(&Config::default());
        assert_eq!(formatter("py").as_deref(), Some("black -q -"));
    }

    #[test]
    fn the_template_parses_to_defaults() {
        let config = parse(TEMPLATE);
        assert_eq!(config.theme, "tokyonight");
        assert_eq!(config.tab_width, 4);
        assert_eq!(config.scrolloff, 3);
        assert!(config.keys_normal.is_empty());
        assert_eq!(config.lsp, Config::default().lsp);
    }

    #[test]
    fn recent_files_dedupe_most_recent_first() {
        let state = std::env::temp_dir().join("crow-recent-test");
        std::fs::create_dir_all(&state).unwrap();
        std::env::set_var("XDG_STATE_HOME", &state);
        let _ = std::fs::remove_file(state.join("crow/recent"));
        let a = state.join("a.txt");
        let b = state.join("b.txt");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        record_recent(&a);
        record_recent(&b);
        record_recent(&a); // back to the front, not duplicated
        let names: Vec<String> = recent_files()
            .iter()
            .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        std::env::remove_var("XDG_STATE_HOME");
    }
}
