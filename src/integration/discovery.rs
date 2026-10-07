use std::fs;

use crate::TerminalChannel;

use super::{IntegrationConfig, TerminalSettingsTarget};

/// Discovers every initialized Windows Terminal channel settings file.
///
/// Portable installs store `settings.json` next to the executable and cannot be
/// located from `LOCALAPPDATA`; install them by running Windows Terminal from a
/// packaged channel or configure the bridge manually.
///
/// Symlinked `settings.json` files are excluded: `symlink_metadata` inspects
/// the link itself, so a link never counts as a real settings file and cannot
/// abort installation for every channel when the transaction layer later
/// rejects it.
#[must_use]
pub fn discover_targets(config: &IntegrationConfig) -> Vec<TerminalSettingsTarget> {
    auto_candidates(&config.local_app_data)
        .into_iter()
        .filter(|target| {
            fs::symlink_metadata(&target.settings_path)
                .is_ok_and(|metadata| metadata.file_type().is_file())
        })
        .collect()
}

fn auto_candidates(local_app_data: &std::path::Path) -> [TerminalSettingsTarget; 4] {
    let packages = local_app_data.join("Packages");
    [
        TerminalSettingsTarget {
            channel: TerminalChannel::Stable,
            settings_path: packages
                .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
        },
        TerminalSettingsTarget {
            channel: TerminalChannel::Preview,
            settings_path: packages
                .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
        },
        TerminalSettingsTarget {
            channel: TerminalChannel::Canary,
            settings_path: packages
                .join("Microsoft.WindowsTerminalCanary_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
        },
        TerminalSettingsTarget {
            channel: TerminalChannel::Unpackaged,
            settings_path: local_app_data
                .join("Microsoft")
                .join("Windows Terminal")
                .join("settings.json"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn auto_discovers_only_initialized_channels() {
        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let stable = temp
            .path()
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(stable.parent().expect("fixture should have a parent"))
            .expect("fixture directory should be created");
        fs::write(&stable, b"{}\n").expect("fixture should be written");

        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        let targets = discover_targets(&config);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].channel, TerminalChannel::Stable);
        assert_eq!(targets[0].settings_path, stable);
    }

    #[cfg(windows)]
    #[test]
    fn symlinked_settings_files_are_not_discovered() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().expect("temporary directory should be created");
        let stable = temp
            .path()
            .join("Packages")
            .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(stable.parent().expect("fixture should have a parent"))
            .expect("fixture directory should be created");
        fs::write(&stable, b"{}\n").expect("fixture should be written");

        let preview = temp
            .path()
            .join("Packages")
            .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
            .join("LocalState")
            .join("settings.json");
        fs::create_dir_all(preview.parent().expect("fixture should have a parent"))
            .expect("fixture directory should be created");
        let link_target = temp.path().join("real-settings.json");
        fs::write(&link_target, b"{}\n").expect("link target should be written");
        if let Err(error) = symlink_file(&link_target, &preview) {
            eprintln!("skipping symlinked settings discovery test: {error}");
            return;
        }

        let config = IntegrationConfig::new(
            temp.path(),
            temp.path().join("state"),
            temp.path().join("documents"),
        );
        let targets = discover_targets(&config);

        assert_eq!(targets.len(), 1, "symlinked settings must be excluded");
        assert_eq!(targets[0].channel, TerminalChannel::Stable);
        assert_eq!(targets[0].settings_path, stable);
    }
}
