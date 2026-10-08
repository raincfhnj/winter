use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::prefix::{
    KeyChord, LogicalKey, PrefixConfig, default_prefix_chord, is_reserved_system_chord,
    shortcut_specs,
};
use crate::{AppError, AppResult};

pub const CONFIG_SCHEMA_VERSION: u32 = 2;
const LEGACY_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_PREFIX_TIMEOUT_MS: u64 = 1_500;
pub const DISABLED_SHORTCUT: &str = "disabled";
const MIN_PREFIX_TIMEOUT_MS: u64 = 250;
const MAX_PREFIX_TIMEOUT_MS: u64 = 5_000;
const MAX_DIVIDER_HIT_SLOP_PX: u8 = 32;
const MIN_PANE_GEOMETRY_POLL_INTERVAL_MS: u64 = 50;
const MAX_PANE_GEOMETRY_POLL_INTERVAL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MouseResizeConfig {
    /// Enables native Windows Terminal divider dragging through UI Automation.
    pub enabled: bool,
    /// Extra clickable pixels on either side of the visible native divider.
    ///
    /// 8px matches the grab tolerance users expect from native window borders
    /// while staying below the smallest pane's half-width.
    #[serde(deserialize_with = "deserialize_divider_hit_slop_px")]
    pub divider_hit_slop_px: u8,
    /// Refresh cadence for the disposable native pane geometry snapshot.
    #[serde(deserialize_with = "deserialize_geometry_poll_interval_ms")]
    pub geometry_poll_interval_ms: u64,
}

impl Default for MouseResizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            divider_hit_slop_px: 8,
            geometry_poll_interval_ms: 100,
        }
    }
}

impl MouseResizeConfig {
    fn validate(self) -> AppResult<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.divider_hit_slop_px > MAX_DIVIDER_HIT_SLOP_PX {
            return Err(AppError::InvalidConfiguration(format!(
                "mouse_resize.divider_hit_slop_px must be between 0 and {MAX_DIVIDER_HIT_SLOP_PX}"
            )));
        }
        if !(MIN_PANE_GEOMETRY_POLL_INTERVAL_MS..=MAX_PANE_GEOMETRY_POLL_INTERVAL_MS)
            .contains(&self.geometry_poll_interval_ms)
        {
            return Err(AppError::InvalidConfiguration(format!(
                "mouse_resize.geometry_poll_interval_ms must be between {MIN_PANE_GEOMETRY_POLL_INTERVAL_MS} and {MAX_PANE_GEOMETRY_POLL_INTERVAL_MS}"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub const fn geometry_poll_interval(self) -> Duration {
        Duration::from_millis(self.geometry_poll_interval_ms)
    }
}

fn deserialize_divider_hit_slop_px<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    u8::try_from(value).map_err(|_| {
        serde::de::Error::custom(format!(
            "mouse_resize.divider_hit_slop_px must be between 0 and {MAX_DIVIDER_HIT_SLOP_PX}"
        ))
    })
}

fn deserialize_geometry_poll_interval_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    u64::try_from(value).map_err(|_| {
        serde::de::Error::custom(format!(
            "mouse_resize.geometry_poll_interval_ms must be between {MIN_PANE_GEOMETRY_POLL_INTERVAL_MS} and {MAX_PANE_GEOMETRY_POLL_INTERVAL_MS}"
        ))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControllerConfig {
    pub schema_version: u32,
    pub prefix_timeout_ms: u64,
    pub launch_terminal_on_start: bool,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_shortcuts")]
    pub shortcuts: BTreeMap<String, String>,
    #[serde(default)]
    pub mouse_resize: MouseResizeConfig,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            prefix_timeout_ms: DEFAULT_PREFIX_TIMEOUT_MS,
            launch_terminal_on_start: true,
            prefix: default_prefix(),
            shortcuts: default_shortcuts(),
            mouse_resize: MouseResizeConfig::default(),
        }
    }
}

impl ControllerConfig {
    pub fn load(path: &Path) -> AppResult<Self> {
        let source = fs::read_to_string(path)
            .map_err(|error| AppError::io("read controller config", path, error))?;
        let mut config = parse_config_source(&source, path)?;
        config.validate()?;
        config.migrate_schema(path);
        Ok(config)
    }

    pub fn load_or_create(path: &Path) -> AppResult<Self> {
        if path.exists() {
            return Self::load(path);
        }

        let config = Self::default();
        config.validate()?;
        let parent = path.parent().ok_or_else(|| {
            AppError::InvalidConfiguration(format!(
                "configuration path has no parent: {}",
                path.display()
            ))
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| AppError::io("create controller config directory", parent, error))?;

        let serialized = toml::to_string_pretty(&config).map_err(|error| {
            AppError::InvalidConfiguration(format!("serialize default configuration: {error}"))
        })?;
        if install_file_if_absent(path, &serialized)? {
            return Self::load(path);
        }
        Ok(config)
    }

    /// Validates scalar settings and shortcut names without compiling chords.
    ///
    /// Chord compilation stays in [`Self::prefix_config`], which the controller
    /// and `winter doctor` call where a usable key map is actually required.
    pub fn validate(&self) -> AppResult<()> {
        self.validate_scalar_fields()?;
        self.validate_shortcut_names()
    }

    fn validate_shortcut_names(&self) -> AppResult<()> {
        let known_names = shortcut_specs()
            .iter()
            .map(|spec| spec.name)
            .collect::<HashSet<_>>();
        let unknown_names = self
            .shortcuts
            .keys()
            .filter(|name| !known_names.contains(name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown_names.is_empty() {
            return Err(AppError::InvalidConfiguration(format!(
                "unknown shortcut name(s): {}",
                unknown_names.join(", ")
            )));
        }
        Ok(())
    }

    fn validate_scalar_fields(&self) -> AppResult<()> {
        if !matches!(
            self.schema_version,
            LEGACY_CONFIG_SCHEMA_VERSION | CONFIG_SCHEMA_VERSION
        ) {
            return Err(AppError::InvalidConfiguration(format!(
                "unsupported schema_version {}; expected {} or legacy {}",
                self.schema_version, CONFIG_SCHEMA_VERSION, LEGACY_CONFIG_SCHEMA_VERSION
            )));
        }
        if !(MIN_PREFIX_TIMEOUT_MS..=MAX_PREFIX_TIMEOUT_MS).contains(&self.prefix_timeout_ms) {
            return Err(AppError::InvalidConfiguration(format!(
                "prefix_timeout_ms must be between {MIN_PREFIX_TIMEOUT_MS} and {MAX_PREFIX_TIMEOUT_MS}"
            )));
        }
        self.mouse_resize.validate()?;
        Ok(())
    }

    pub fn prefix_config(&self) -> AppResult<PrefixConfig> {
        self.validate()?;
        let prefix_chord = parse_configured_chord("prefix", &self.prefix)?;
        if !prefix_chord.modifiers.has_ctrl_or_alt() {
            return Err(AppError::InvalidConfiguration(
                "prefix must include ctrl or alt so ordinary typing is never captured".to_owned(),
            ));
        }
        if prefix_chord.key == LogicalKey::Escape || is_reserved_system_chord(prefix_chord) {
            return Err(AppError::InvalidConfiguration(format!(
                "prefix {:?} is reserved by Windows or by Prefix cancellation",
                self.prefix
            )));
        }

        let mut bindings = HashMap::new();
        let mut owners = HashMap::<KeyChord, &'static str>::new();
        for spec in shortcut_specs() {
            let configured = self.shortcuts.get(spec.name);
            let chord = match configured.map(String::as_str) {
                Some(value) if value.trim().eq_ignore_ascii_case(DISABLED_SHORTCUT) => {
                    if !spec.allow_disabled {
                        return Err(AppError::InvalidConfiguration(format!(
                            "shortcut {:?} cannot be disabled; assign another chord instead",
                            spec.name
                        )));
                    }
                    continue;
                }
                Some(value) => parse_configured_chord(spec.name, value)?,
                None => spec.default_chord,
            };

            if chord.key == LogicalKey::Escape {
                return Err(AppError::InvalidConfiguration(format!(
                    "shortcut {:?} cannot use Escape because Escape cancels Prefix mode",
                    spec.name
                )));
            }
            if is_reserved_system_chord(chord) {
                return Err(AppError::InvalidConfiguration(format!(
                    "shortcut {:?} uses reserved system chord {chord}",
                    spec.name
                )));
            }
            if let Some(existing) = owners.insert(chord, spec.name) {
                return Err(AppError::InvalidConfiguration(format!(
                    "shortcuts {existing:?} and {:?} both use {chord}",
                    spec.name
                )));
            }
            bindings.insert(chord, spec.command);
        }

        Ok(PrefixConfig::with_bindings(
            self.prefix_timeout(),
            prefix_chord,
            bindings,
        ))
    }

    pub fn to_pretty_toml(&self) -> AppResult<String> {
        self.validate()?;
        let defaults = default_shortcuts();
        let mut shortcuts = defaults
            .iter()
            .map(|(name, chord)| (name.as_str(), chord.as_str()))
            .collect::<BTreeMap<&str, &str>>();
        for (name, chord) in &self.shortcuts {
            shortcuts.insert(name.as_str(), chord.as_str());
        }
        let effective = EffectiveControllerConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            prefix_timeout_ms: self.prefix_timeout_ms,
            launch_terminal_on_start: self.launch_terminal_on_start,
            prefix: &self.prefix,
            shortcuts,
            mouse_resize: &self.mouse_resize,
        };
        toml::to_string_pretty(&effective).map_err(|error| {
            AppError::InvalidConfiguration(format!("serialize controller configuration: {error}"))
        })
    }

    #[must_use]
    pub const fn prefix_timeout(&self) -> Duration {
        Duration::from_millis(self.prefix_timeout_ms)
    }

    fn migrate_schema(&mut self, path: &Path) {
        if self.schema_version != LEGACY_CONFIG_SCHEMA_VERSION {
            return;
        }
        self.schema_version = CONFIG_SCHEMA_VERSION;
        let write_back = toml::to_string_pretty(self)
            .map_err(|error| {
                AppError::InvalidConfiguration(format!("serialize migrated configuration: {error}"))
            })
            .and_then(|contents| replace_file_contents(path, &contents));
        if let Err(error) = write_back {
            eprintln!(
                "warning: could not write back migrated configuration {}: {error}; continuing with the in-memory schema {CONFIG_SCHEMA_VERSION}",
                path.display()
            );
        }
    }
}

/// Borrowed serialization view of the effective configuration.
///
/// Lets `to_pretty_toml` expand defaults without cloning the source config.
#[derive(Serialize)]
struct EffectiveControllerConfig<'a> {
    schema_version: u32,
    prefix_timeout_ms: u64,
    launch_terminal_on_start: bool,
    prefix: &'a str,
    shortcuts: BTreeMap<&'a str, &'a str>,
    mouse_resize: &'a MouseResizeConfig,
}

/// Minimal probe that reads only `schema_version` from a configuration file.
#[derive(Debug, Deserialize)]
struct SchemaProbe {
    #[serde(default)]
    schema_version: Option<u32>,
}

/// Deserializes `source`, rejecting unsupported schema versions before the
/// strict `deny_unknown_fields` parse can misreport them as unknown fields.
fn parse_config_source(source: &str, path: &Path) -> AppResult<ControllerConfig> {
    let unsupported_version = toml::from_str::<SchemaProbe>(source)
        .ok()
        .and_then(|probe| probe.schema_version)
        .filter(|version| {
            !(LEGACY_CONFIG_SCHEMA_VERSION..=CONFIG_SCHEMA_VERSION).contains(version)
        });
    if let Some(version) = unsupported_version {
        return Err(AppError::InvalidConfiguration(format!(
            "{}: unsupported config schema_version {version} (this winter supports up to {CONFIG_SCHEMA_VERSION})",
            path.display()
        )));
    }
    toml::from_str(source)
        .map_err(|error| AppError::InvalidConfiguration(format!("{}: {error}", path.display())))
}

/// Sibling temp path used for atomic configuration writes.
fn config_temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "config.toml".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

/// Writes `contents` to `temp` and syncs it to disk, removing `temp` on failure.
fn write_temp_file(temp: &Path, target: &Path, contents: &str) -> AppResult<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)
        .map_err(|error| AppError::io("create temporary controller config", temp, error))?;
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(temp);
        return Err(AppError::io("write controller config", target, error));
    }
    Ok(())
}

/// Atomically replaces `path` with `contents` via a sibling temp file.
fn replace_file_contents(path: &Path, contents: &str) -> AppResult<()> {
    let temp = config_temp_path(path);
    write_temp_file(&temp, path, contents)?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(AppError::io("replace controller config", path, error))
        }
    }
}

/// Atomically installs `contents` at `path` only while `path` is absent.
///
/// The commit is a hard link because `fs::rename` replaces an existing target
/// on Windows, which would break the "never overwrite an existing config"
/// contract under a create race. Returns `true` when another process created
/// `path` first, so the caller must load that file instead.
fn install_file_if_absent(path: &Path, contents: &str) -> AppResult<bool> {
    let temp = config_temp_path(path);
    write_temp_file(&temp, path, contents)?;
    match fs::hard_link(&temp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp);
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp);
            Ok(true)
        }
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(AppError::io("create controller config", path, error))
        }
    }
}

fn default_prefix() -> String {
    default_prefix_chord().to_string()
}

fn default_shortcuts() -> BTreeMap<String, String> {
    shortcut_specs()
        .iter()
        .map(|spec| (spec.name.to_owned(), spec.default_chord.to_string()))
        .collect()
}

fn parse_configured_chord(field: &str, value: &str) -> AppResult<KeyChord> {
    value.parse::<KeyChord>().map_err(|error| {
        AppError::InvalidConfiguration(format!("invalid chord for {field:?}: {value:?}: {error}"))
    })
}

pub fn default_app_data_dir() -> AppResult<PathBuf> {
    let local_app_data = env::var_os("LOCALAPPDATA").ok_or_else(|| {
        AppError::InvalidConfiguration("LOCALAPPDATA is not available".to_owned())
    })?;
    Ok(PathBuf::from(local_app_data).join(crate::integration::APP_DATA_DIR_NAME))
}

pub fn default_config_path() -> AppResult<PathBuf> {
    Ok(default_app_data_dir()?.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use crate::TerminalAction;
    use crate::prefix::ShortcutCommand;

    use super::*;

    fn temporary_leftovers(directory: &Path) -> Vec<String> {
        let mut leftovers = Vec::new();
        for entry in fs::read_dir(directory).expect("directory should be readable") {
            let name = entry
                .expect("directory entry should be readable")
                .file_name()
                .to_string_lossy()
                .into_owned();
            if name.contains(".tmp-") {
                leftovers.push(name);
            }
        }
        leftovers
    }

    #[test]
    fn creates_and_reloads_default_configuration() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("nested").join("config.toml");

        let created = ControllerConfig::load_or_create(&path)
            .expect("default configuration should be created");
        let loaded = ControllerConfig::load(&path).expect("configuration should reload");

        assert_eq!(created, ControllerConfig::default());
        assert_eq!(loaded, created);
        assert!(
            temporary_leftovers(path.parent().expect("config should have a parent")).is_empty(),
            "atomic creation must not leave a temporary file behind"
        );
    }

    #[test]
    fn rejects_out_of_range_timeout() {
        let config = ControllerConfig {
            prefix_timeout_ms: MIN_PREFIX_TIMEOUT_MS - 1,
            ..ControllerConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(AppError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn rejects_out_of_range_mouse_resize_settings() {
        let too_wide = ControllerConfig {
            mouse_resize: MouseResizeConfig {
                divider_hit_slop_px: MAX_DIVIDER_HIT_SLOP_PX + 1,
                ..MouseResizeConfig::default()
            },
            ..ControllerConfig::default()
        };
        assert!(
            too_wide
                .validate()
                .expect_err("oversized divider hit target must fail")
                .to_string()
                .contains("divider_hit_slop_px")
        );

        let too_frequent = ControllerConfig {
            mouse_resize: MouseResizeConfig {
                geometry_poll_interval_ms: MIN_PANE_GEOMETRY_POLL_INTERVAL_MS - 1,
                ..MouseResizeConfig::default()
            },
            ..ControllerConfig::default()
        };
        assert!(
            too_frequent
                .validate()
                .expect_err("overly frequent geometry polling must fail")
                .to_string()
                .contains("geometry_poll_interval_ms")
        );
    }

    #[test]
    fn disabled_mouse_resize_ignores_out_of_range_settings() {
        let config = ControllerConfig {
            mouse_resize: MouseResizeConfig {
                enabled: false,
                divider_hit_slop_px: u8::MAX,
                geometry_poll_interval_ms: MAX_PANE_GEOMETRY_POLL_INTERVAL_MS + 1,
            },
            ..ControllerConfig::default()
        };

        config
            .validate()
            .expect("a disabled feature must not range-check its settings");
        config
            .prefix_config()
            .expect("chord compilation must agree with scalar validation");

        let parsed: ControllerConfig = toml::from_str(
            "schema_version = 2\n[mouse_resize]\nenabled = false\ndivider_hit_slop_px = 200\ngeometry_poll_interval_ms = 5\n",
        )
        .expect("representable dummy values must deserialize");
        parsed
            .validate()
            .expect("a disabled feature must accept dummy values from TOML");
    }

    #[test]
    fn enabled_mouse_resize_still_range_checks_representable_values() {
        let config: ControllerConfig =
            toml::from_str("schema_version = 2\n[mouse_resize]\ndivider_hit_slop_px = 100\n")
                .expect("values within u8 must deserialize");

        assert!(
            config
                .validate()
                .expect_err("slop above the documented maximum must fail while enabled")
                .to_string()
                .contains("divider_hit_slop_px must be between 0 and 32")
        );
    }

    #[test]
    fn out_of_range_mouse_resize_values_report_documented_bounds() {
        let slop_error = toml::from_str::<ControllerConfig>(
            "schema_version = 2\n[mouse_resize]\ndivider_hit_slop_px = 400\n",
        )
        .expect_err("values beyond u8 must be rejected");
        let slop_message = slop_error.to_string();
        assert!(
            slop_message.contains("divider_hit_slop_px must be between 0 and 32"),
            "unexpected message: {slop_message}"
        );

        let poll_error = toml::from_str::<ControllerConfig>(
            "schema_version = 2\n[mouse_resize]\ngeometry_poll_interval_ms = -1\n",
        )
        .expect_err("negative poll intervals must be rejected");
        let poll_message = poll_error.to_string();
        assert!(
            poll_message.contains("geometry_poll_interval_ms must be between 50 and 1000"),
            "unexpected message: {poll_message}"
        );
    }

    #[test]
    fn does_not_overwrite_an_existing_invalid_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = 99\n").expect("invalid fixture should be written");

        assert!(ControllerConfig::load_or_create(&path).is_err());
        assert_eq!(
            fs::read_to_string(path).expect("fixture should remain readable"),
            "schema_version = 99\n"
        );
    }

    #[test]
    fn legacy_config_loads_with_effective_default_shortcuts() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "schema_version = 1\nprefix_timeout_ms = 1500\nlaunch_terminal_on_start = true\n",
        )
        .expect("legacy fixture should be written");

        let config = ControllerConfig::load(&path).expect("legacy configuration should load");
        let runtime = config
            .prefix_config()
            .expect("default shortcut configuration should compile");

        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        let migrated = fs::read_to_string(&path).expect("legacy fixture should remain readable");
        assert!(
            migrated.contains("schema_version = 2"),
            "migration must write the current schema back to disk: {migrated}"
        );
        assert!(migrated.contains("prefix_timeout_ms = 1500"));
        assert!(migrated.contains("launch_terminal_on_start = true"));
        assert!(
            temporary_leftovers(directory.path()).is_empty(),
            "migration must not leave a temporary file behind"
        );
        assert_eq!(config.prefix, "ctrl+b");
        assert_eq!(config.mouse_resize, MouseResizeConfig::default());
        assert_eq!(config.shortcuts.len(), shortcut_specs().len());
        assert_eq!(runtime.prefix_chord, default_prefix_chord());
        assert_eq!(runtime.bindings.len(), shortcut_specs().len());
    }

    #[test]
    fn custom_prefix_and_shortcut_override_compile() {
        let mut config = ControllerConfig {
            prefix: "alt+a".to_owned(),
            ..ControllerConfig::default()
        };
        config
            .shortcuts
            .insert("new_tab".to_owned(), "t".to_owned());
        config
            .shortcuts
            .insert("rename_tab".to_owned(), DISABLED_SHORTCUT.to_owned());

        let runtime = config
            .prefix_config()
            .expect("custom shortcut configuration should compile");

        assert_eq!(runtime.prefix_chord, "alt+a".parse().expect("valid chord"));
        assert_eq!(
            runtime.bindings.get(&"t".parse().expect("valid chord")),
            Some(&ShortcutCommand::Terminal(TerminalAction::NewTab))
        );
        assert!(
            !runtime
                .bindings
                .contains_key(&"comma".parse().expect("valid chord"))
        );
    }

    #[test]
    fn partial_shortcut_table_inherits_unspecified_defaults() {
        let config: ControllerConfig = toml::from_str(
            r#"
schema_version = 1
prefix_timeout_ms = 800
launch_terminal_on_start = true
prefix = "ctrl+a"

[shortcuts]
new_tab = "t"
"#,
        )
        .expect("partial shortcut table should deserialize");

        let runtime = config
            .prefix_config()
            .expect("partial shortcut table should compile");

        assert_eq!(config.shortcuts.len(), 1);
        assert_eq!(runtime.bindings.len(), shortcut_specs().len());
        assert_eq!(
            runtime.bindings.get(&"left".parse().expect("valid chord")),
            Some(&ShortcutCommand::Terminal(TerminalAction::FocusPane {
                direction: crate::Direction::Left,
            }))
        );
    }

    #[test]
    fn duplicate_and_unknown_shortcuts_are_rejected() {
        let mut duplicate = ControllerConfig::default();
        duplicate
            .shortcuts
            .insert("new_tab".to_owned(), "n".to_owned());
        let duplicate_error = duplicate
            .prefix_config()
            .expect_err("duplicate chord must fail");
        assert!(duplicate_error.to_string().contains("both use n"));

        let mut unknown = ControllerConfig::default();
        unknown
            .shortcuts
            .insert("launch_spaceship".to_owned(), "s".to_owned());
        let unknown_error = unknown.validate().expect_err("unknown shortcut must fail");
        assert!(unknown_error.to_string().contains("launch_spaceship"));
        let unknown_error = unknown
            .prefix_config()
            .expect_err("unknown shortcut must fail chord compilation too");
        assert!(unknown_error.to_string().contains("launch_spaceship"));
    }

    #[test]
    fn unsafe_prefix_and_disabled_shutdown_are_rejected() {
        let plain_prefix = ControllerConfig {
            prefix: "b".to_owned(),
            ..ControllerConfig::default()
        };
        assert!(
            plain_prefix
                .prefix_config()
                .expect_err("plain prefix must fail")
                .to_string()
                .contains("must include ctrl or alt")
        );

        let mut no_shutdown = ControllerConfig::default();
        no_shutdown
            .shortcuts
            .insert("shutdown".to_owned(), DISABLED_SHORTCUT.to_owned());
        assert!(
            no_shutdown
                .prefix_config()
                .expect_err("shutdown cannot be disabled")
                .to_string()
                .contains("cannot be disabled")
        );

        let mut system_shortcut = ControllerConfig::default();
        system_shortcut
            .shortcuts
            .insert("new_tab".to_owned(), "alt+f4".to_owned());
        assert!(
            system_shortcut
                .prefix_config()
                .expect_err("system shortcut must fail")
                .to_string()
                .contains("reserved system chord")
        );
    }

    #[test]
    fn scalar_validation_defers_chord_compilation_to_prefix_config() {
        let plain_prefix = ControllerConfig {
            prefix: "b".to_owned(),
            ..ControllerConfig::default()
        };
        plain_prefix
            .validate()
            .expect("scalar validation must not compile chords");

        let mut duplicate = ControllerConfig::default();
        duplicate
            .shortcuts
            .insert("new_tab".to_owned(), "n".to_owned());
        duplicate
            .shortcuts
            .insert("rename_tab".to_owned(), "n".to_owned());
        duplicate
            .validate()
            .expect("duplicate chords must not fail scalar validation");
        assert!(
            duplicate
                .prefix_config()
                .expect_err("duplicate chords must fail compilation")
                .to_string()
                .contains("both use n")
        );
    }

    #[test]
    fn schema_version_above_current_is_rejected_before_strict_parse() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        let fixture = "schema_version = 99\nfuture_field = true\n";
        fs::write(&path, fixture).expect("future fixture should be written");

        let error = ControllerConfig::load(&path).expect_err("future schema must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("unsupported config schema_version 99 (this winter supports up to 2)"),
            "unexpected message: {message}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("fixture should remain readable"),
            fixture
        );
    }

    #[test]
    fn unknown_field_on_current_schema_keeps_serde_message() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        fs::write(&path, "schema_version = 2\nfuture_field = true\n")
            .expect("fixture should be written");

        let error = ControllerConfig::load(&path).expect_err("unknown field must fail");
        let message = error.to_string();
        assert!(message.contains("unknown field"), "unexpected: {message}");
        assert!(message.contains("future_field"), "unexpected: {message}");
    }

    #[test]
    fn atomic_replace_removes_temp_file_when_rename_fails() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        fs::create_dir(&path).expect("blocking destination should be created");

        assert!(
            replace_file_contents(&path, "schema_version = 2\n").is_err(),
            "renaming a file over a directory must fail"
        );
        assert!(
            temporary_leftovers(directory.path()).is_empty(),
            "the temporary file must be removed after a failed rename"
        );
    }

    #[cfg(windows)]
    #[test]
    fn migration_write_back_failure_is_non_fatal() {
        use std::os::windows::fs::OpenOptionsExt;

        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "schema_version = 1\nprefix_timeout_ms = 1500\nlaunch_terminal_on_start = true\n",
        )
        .expect("legacy fixture should be written");

        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(&path)
            .expect("fixture should stay readable");

        let config =
            ControllerConfig::load(&path).expect("a failed write-back must not fail the load");
        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        drop(locked);

        assert!(
            fs::read_to_string(&path)
                .expect("fixture should remain readable")
                .contains("schema_version = 1"),
            "the on-disk file must keep its schema when the write-back fails"
        );
        assert!(
            temporary_leftovers(directory.path()).is_empty(),
            "a failed write-back must not leave a temporary file behind"
        );
    }

    #[test]
    fn pretty_toml_materializes_the_complete_shortcut_table() {
        let config: ControllerConfig = toml::from_str(
            "schema_version = 1\nprefix_timeout_ms = 900\nlaunch_terminal_on_start = false\n",
        )
        .expect("legacy shape should deserialize");

        let rendered = config
            .to_pretty_toml()
            .expect("effective configuration should serialize");

        assert!(rendered.contains("schema_version = 2"));
        assert!(rendered.contains("prefix = \"ctrl+b\""));
        assert!(rendered.contains("[shortcuts]"));
        assert!(rendered.contains("new_tab = \"c\""));
        assert!(rendered.contains("shutdown = \"q\""));
        assert!(rendered.contains("[mouse_resize]"));
        assert!(rendered.contains("divider_hit_slop_px = 8"));

        let reparsed: ControllerConfig =
            toml::from_str(&rendered).expect("rendered configuration should parse again");
        assert_eq!(
            reparsed
                .prefix_config()
                .expect("rendered semantics should compile"),
            config
                .prefix_config()
                .expect("source semantics should compile")
        );
    }
}
