//! Application configuration (TOML file in the platform config directory).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::{Error, Result};
use super::paths::ConflictStrategy;
use crate::i18n::Language;
use crate::tr;

/// Write `data` to `path`, readable only by the user (the config holds the
/// Jellyfin API key).
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(0o600);
        // `mode` only applies to new files.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// Platform specific application directories.
#[derive(Debug, Clone)]
pub struct AppDirs {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
}

impl AppDirs {
    /// Resolve the standard directories (XDG / AppData / Library).
    /// Falls back to `./.framewise` if no home directory is known.
    pub fn resolve() -> Self {
        match directories::ProjectDirs::from("de", "framewise", "Framewise") {
            Some(dirs) => Self {
                config_file: dirs.config_dir().join("config.toml"),
                data_dir: dirs.data_dir().to_path_buf(),
            },
            None => {
                let base = PathBuf::from(".framewise");
                Self {
                    config_file: base.join("config.toml"),
                    data_dir: base,
                }
            }
        }
    }

    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join("framewise.sqlite")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn job_log_dir(&self) -> PathBuf {
        self.log_dir().join("jobs")
    }

    pub fn default_work_dir(&self) -> PathBuf {
        self.data_dir.join("work")
    }
}

/// Which hardware backend to use for encoding or decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HwBackend {
    /// Pick the best verified backend, otherwise CPU.
    #[default]
    Auto,
    Cpu,
    Nvidia,
    IntelQsv,
    Vaapi,
    AmdAmf,
    VideoToolbox,
}

impl HwBackend {
    pub const ALL: [HwBackend; 7] = [
        HwBackend::Auto,
        HwBackend::Cpu,
        HwBackend::Nvidia,
        HwBackend::IntelQsv,
        HwBackend::Vaapi,
        HwBackend::AmdAmf,
        HwBackend::VideoToolbox,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HwBackend::Auto => tr!("Automatic"),
            HwBackend::Cpu => "CPU",
            HwBackend::Nvidia => "NVIDIA (NVENC/NVDEC)",
            HwBackend::IntelQsv => "Intel Quick Sync",
            HwBackend::Vaapi => "VAAPI",
            HwBackend::AmdAmf => "AMD AMF",
            HwBackend::VideoToolbox => "Apple VideoToolbox",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanSettings {
    /// Lower-case file extensions without dot.
    pub extensions: Vec<String>,
    pub follow_symlinks: bool,
    pub skip_hidden: bool,
    /// Number of concurrent ffprobe processes, 0 = automatic.
    pub probe_parallelism: usize,
    pub detect_duplicates: bool,
    /// Highlight the N largest files per folder.
    pub top_n_per_folder: usize,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            extensions: vec!["mkv".into(), "mp4".into()],
            follow_symlinks: true,
            skip_hidden: true,
            probe_parallelism: 0,
            detect_duplicates: true,
            top_n_per_folder: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncodingSettings {
    /// Id of the active encoding profile.
    pub active_profile: String,
    pub conflict_strategy: ConflictStrategy,
    /// Run a full decode of the result after encoding.
    pub full_decode_test: bool,
    /// Move outputs that failed validation into `<work>/quarantine`.
    pub quarantine_failed: bool,
}

impl Default for EncodingSettings {
    fn default() -> Self {
        Self {
            active_profile: "universal".into(),
            conflict_strategy: ConflictStrategy::Skip,
            full_decode_test: false,
            quarantine_failed: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceSettings {
    pub max_parallel_jobs: usize,
    /// Encoder threads, 0 = let the encoder decide.
    pub cpu_threads: usize,
    /// Do not start new jobs while total CPU usage is above this value (percent).
    pub cpu_target_percent: f32,
    /// Do not start new jobs while available RAM is below this value.
    pub min_free_ram_mb: u64,
    /// Do not start new jobs while free space in work/target dir is below this value.
    pub min_free_disk_gb: u64,
    /// Unix nice level for ffmpeg processes (0..=19).
    pub nice_level: i32,
    pub hw_encode: HwBackend,
    pub hw_decode: HwBackend,
    pub hw_encode_enabled: bool,
    pub hw_decode_enabled: bool,
    /// Device used for VAAPI.
    pub vaapi_device: String,
    /// GPU index for NVIDIA (`-gpu`).
    pub gpu_index: u32,
    /// Retry with CPU encoding if a hardware encode fails.
    pub fallback_to_cpu: bool,
}

impl Default for ResourceSettings {
    fn default() -> Self {
        Self {
            max_parallel_jobs: 1,
            cpu_threads: 0,
            cpu_target_percent: 85.0,
            min_free_ram_mb: 1024,
            min_free_disk_gb: 5,
            nice_level: 10,
            hw_encode: HwBackend::Auto,
            hw_decode: HwBackend::Auto,
            hw_encode_enabled: false,
            hw_decode_enabled: false,
            vaapi_device: "/dev/dri/renderD128".into(),
            gpu_index: 0,
            fallback_to_cpu: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub scale: f32,
    /// `None` = derive from the system locale.
    pub language: Option<Language>,
    /// Desktop notification when a job or the whole queue finishes.
    pub desktop_notifications: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            scale: 1.0,
            language: None,
            desktop_notifications: true,
        }
    }
}

/// Where the library comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SourceKind {
    /// Files in the local source directory (scanned with ffprobe).
    #[default]
    Local,
    /// Items of a Jellyfin server, read through its API (report only).
    Jellyfin,
}

impl SourceKind {
    pub const ALL: [SourceKind; 2] = [SourceKind::Local, SourceKind::Jellyfin];

    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Local => tr!("Local directory"),
            SourceKind::Jellyfin => "Jellyfin",
        }
    }
}

/// Connection to a Jellyfin server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JellyfinSettings {
    /// Base URL, e.g. `http://jellyfin.local:8096`.
    pub url: String,
    /// API key (Dashboard → API keys). Stored in the config file, which is
    /// only readable by the user.
    pub api_key: String,
    /// Library ids to read; empty = all movie and series libraries.
    pub libraries: Vec<String>,
}

impl JellyfinSettings {
    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty() && !self.api_key.trim().is_empty()
    }
}

/// Complete persisted configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub ffmpeg_path: PathBuf,
    pub ffprobe_path: PathBuf,
    pub source: SourceKind,
    pub source_dir: Option<PathBuf>,
    pub target_dir: Option<PathBuf>,
    /// Working directory for temporary files; `None` = data dir.
    pub work_dir: Option<PathBuf>,
    pub jellyfin: JellyfinSettings,
    pub scan: ScanSettings,
    pub encoding: EncodingSettings,
    pub resources: ResourceSettings,
    pub ui: UiSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ffmpeg_path: "ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
            source: SourceKind::Local,
            source_dir: None,
            target_dir: None,
            work_dir: None,
            jellyfin: JellyfinSettings::default(),
            scan: ScanSettings::default(),
            encoding: EncodingSettings::default(),
            resources: ResourceSettings::default(),
            ui: UiSettings::default(),
        }
    }
}

impl AppConfig {
    /// Load the configuration. A missing file yields defaults; a broken file
    /// is reported as error (and never silently overwritten).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::Config(format!("'{}': {e}", path.display()))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(Error::io(path, err)),
        }
    }

    /// Save atomically (write temp file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        write_private(&tmp, text.as_bytes()).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
    }

    /// Key of the stored scan for the active source.
    pub fn library_key(&self) -> Option<PathBuf> {
        match self.source {
            SourceKind::Local => self.source_dir.clone(),
            SourceKind::Jellyfin => self.jellyfin.is_configured().then(|| {
                PathBuf::from(format!(
                    "jellyfin:{}",
                    self.jellyfin.url.trim().trim_end_matches('/')
                ))
            }),
        }
    }

    /// Work directory for temporary encodes. Default: a hidden directory in
    /// the target directory, so large intermediate files never land on the
    /// system disk and finished results are moved by a rename on the same
    /// file system instead of a copy.
    pub fn work_dir(&self, dirs: &AppDirs) -> PathBuf {
        match (&self.work_dir, &self.target_dir) {
            (Some(w), _) => w.clone(),
            (None, Some(t)) => t.join(".framewise-work"),
            (None, None) => dirs.default_work_dir(),
        }
    }

    /// Effective ffprobe parallelism.
    pub fn probe_parallelism(&self) -> usize {
        match self.scan.probe_parallelism {
            0 => std::thread::available_parallelism()
                .map(|n| (n.get() / 2).max(1))
                .unwrap_or(2),
            n => n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/config.toml");
        assert_eq!(AppConfig::load(&path).unwrap(), AppConfig::default());

        let cfg = AppConfig {
            source_dir: Some("/media/movies".into()),
            resources: ResourceSettings {
                max_parallel_jobs: 3,
                ..ResourceSettings::default()
            },
            ..AppConfig::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap(), cfg);
    }

    #[test]
    fn jellyfin_settings_roundtrip_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig {
            source: SourceKind::Jellyfin,
            jellyfin: JellyfinSettings {
                url: "http://jf.local:8096/".into(),
                api_key: "secret".into(),
                libraries: vec!["abc".into()],
            },
            ..AppConfig::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap(), cfg);
        assert_eq!(
            cfg.library_key(),
            Some(PathBuf::from("jellyfin:http://jf.local:8096"))
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Old configs without the new fields keep the local source.
        let old: AppConfig = toml::from_str("source_dir = \"/m\"").unwrap();
        assert_eq!(old.source, SourceKind::Local);
        assert_eq!(old.library_key(), Some(PathBuf::from("/m")));
    }

    #[test]
    fn work_dir_defaults_to_target_drive() {
        let dirs = AppDirs {
            config_file: "/home/u/.config/framewise/config.toml".into(),
            data_dir: "/home/u/.local/share/framewise".into(),
        };
        let mut cfg = AppConfig::default();
        assert_eq!(
            cfg.work_dir(&dirs),
            PathBuf::from("/home/u/.local/share/framewise/work")
        );
        cfg.target_dir = Some("/media/usb/Filme".into());
        assert_eq!(
            cfg.work_dir(&dirs),
            PathBuf::from("/media/usb/Filme/.framewise-work")
        );
        cfg.work_dir = Some("/scratch/fw".into());
        assert_eq!(cfg.work_dir(&dirs), PathBuf::from("/scratch/fw"));
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[resources]\nmax_parallel_jobs = 2\n").unwrap();
        let cfg = AppConfig::load(&path).unwrap();
        assert_eq!(cfg.resources.max_parallel_jobs, 2);
        assert_eq!(cfg.scan, ScanSettings::default());
    }

    #[test]
    fn broken_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is = = not toml").unwrap();
        assert!(AppConfig::load(&path).is_err());
    }
}
