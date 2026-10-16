use engine_files::VirtualPath;
use std::fs;
use std::path::Path;

/// Startup settings read from a `bootstrap.rves` file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapConfig {
    pub window_size_x: Option<u32>,
    pub window_size_y: Option<u32>,
    pub window_title: Option<String>,
    pub allow_multiple_instances: bool,
    pub app_icon: Option<String>,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            window_size_x: None,
            window_size_y: None,
            window_title: None,
            allow_multiple_instances: true,
            app_icon: None,
        }
    }
}

impl BootstrapConfig {
    /// Parses bootstrap contents, reporting errors against `bootstrap.rves`.
    pub fn parse(contents: &str) -> Result<Self, String> {
        Self::parse_with_path(contents, "bootstrap.rves")
    }

    /// Loads bootstrap contents; a missing file leaves every setting at its default.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };

        Self::parse_with_path(&contents, &path.display().to_string())
    }

    fn parse_with_path(contents: &str, path: &str) -> Result<Self, String> {
        let mut config = Self::default();
        let mut seen = [false; 5];

        for (line_index, raw_line) in contents.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let error = |message: &str| format!("{path}: строка {line_number}: {message}");
            let Some((raw_name, raw_value)) = line.split_once('=') else {
                return Err(error("ожидается Имя=Значение"));
            };
            let name = raw_name.trim();
            let seen_index = match name {
                "WindowSizeX" => 0,
                "WindowSizeY" => 1,
                "WindowTitle" => 2,
                "AllowMultipleInstances" => 3,
                "AppIcon" => 4,
                _ => return Err(error(&format!("неизвестный параметр {name}"))),
            };
            if seen[seen_index] {
                return Err(error(&format!("параметр {name} указан повторно")));
            }

            match name {
                "WindowSizeX" => {
                    let value = raw_value
                        .trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| error("WindowSizeX должен быть положительным целым u32"))?;
                    config.window_size_x = Some(value);
                }
                "WindowSizeY" => {
                    let value = raw_value
                        .trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| error("WindowSizeY должен быть положительным целым u32"))?;
                    config.window_size_y = Some(value);
                }
                "WindowTitle" => {
                    let value = raw_value.trim();
                    if value.is_empty() {
                        return Err(error("WindowTitle не должен быть пустым"));
                    }
                    config.window_title = Some(value.to_owned());
                }
                "AllowMultipleInstances" => {
                    config.allow_multiple_instances = match raw_value.trim() {
                        "true" => true,
                        "false" => false,
                        _ => {
                            return Err(error("AllowMultipleInstances должен быть true или false"));
                        }
                    };
                }
                "AppIcon" => {
                    let value = raw_value.trim();
                    if VirtualPath::parse(value).is_err() {
                        return Err(error(
                            "AppIcon должен быть корректным virtual path внутри Assets",
                        ));
                    }
                    config.app_icon = Some(value.to_owned());
                }
                _ => unreachable!("known bootstrap field"),
            }

            seen[seen_index] = true;
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::BootstrapConfig;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_file() -> PathBuf {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rve-bootstrap-{}-{id}.rves", std::process::id()))
    }

    #[test]
    fn parses_all_settings_and_preserves_text_after_the_first_equals() {
        let config = BootstrapConfig::parse(
            " WindowSizeY = 900\r\nWindowTitle = My Game = Preview  \r\n\r\nWindowSizeX=1600\r\nAllowMultipleInstances=false\r\nAppIcon=/ui/game.png\r\n",
        )
        .unwrap();

        assert_eq!(config.window_size_x, Some(1600));
        assert_eq!(config.window_size_y, Some(900));
        assert_eq!(config.window_title.as_deref(), Some("My Game = Preview"));
        assert!(!config.allow_multiple_instances);
        assert_eq!(config.app_icon.as_deref(), Some("/ui/game.png"));
    }

    #[test]
    fn old_size_only_files_remain_compatible_and_missing_values_stay_optional() {
        let config = BootstrapConfig::parse("WindowSizeX=800\nWindowSizeY=600").unwrap();
        assert_eq!(config.window_size_x, Some(800));
        assert_eq!(config.window_size_y, Some(600));
        assert_eq!(config.window_title, None);
        assert!(config.allow_multiple_instances);
        assert_eq!(config.app_icon, None);

        let partial = BootstrapConfig::parse("WindowSizeY=600").unwrap();
        assert_eq!(partial.window_size_x, None);
        assert_eq!(partial.window_size_y, Some(600));
        assert_eq!(
            BootstrapConfig::parse("\n \r\n").unwrap(),
            BootstrapConfig::default()
        );
    }

    #[test]
    fn reports_unknown_duplicate_and_malformed_parameters_with_file_and_line() {
        for (contents, line, expected) in [
            ("WindowSizeX=800\nUnknown=1", 2, "неизвестный параметр"),
            ("WindowSizeX=800\nWindowSizeX=900", 2, "указан повторно"),
            ("WindowSizeY=600\nWindowSizeY=700", 2, "указан повторно"),
            ("WindowTitle=One\nWindowTitle=Two", 2, "указан повторно"),
            ("WindowSizeX 800", 1, "Имя=Значение"),
            ("WindowWidth=800", 1, "неизвестный параметр"),
            ("windowsizex=800", 1, "неизвестный параметр"),
            ("=800", 1, "неизвестный параметр"),
        ] {
            let error = BootstrapConfig::parse(contents).unwrap_err();
            assert!(
                error.contains(&format!("bootstrap.rves: строка {line}:")),
                "{error}"
            );
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn rejects_invalid_values_with_source_location() {
        for value in ["", "0", "-1", "1.5", "abc", "4294967296", "720=800"] {
            for name in ["WindowSizeX", "WindowSizeY"] {
                let error = BootstrapConfig::parse(&format!("\n{name}={value}")).unwrap_err();
                assert!(error.contains("bootstrap.rves: строка 2:"), "{error}");
                assert!(error.contains(name), "{error}");
            }
        }

        for (name, value) in [
            ("WindowTitle", "  "),
            ("AllowMultipleInstances", "TRUE"),
            ("AppIcon", ""),
            ("AppIcon", "//icon.png"),
            ("AppIcon", "icon//game.png"),
            ("AppIcon", "icon/./game.png"),
            ("AppIcon", "icon/../game.png"),
            ("AppIcon", "icon\\game.png"),
            ("AppIcon", "C:/icon.png"),
            ("AppIcon", "icon\0.png"),
        ] {
            let error = BootstrapConfig::parse(&format!("\n{name}={value}")).unwrap_err();
            assert!(error.contains("bootstrap.rves: строка 2:"), "{error}");
            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn missing_file_defaults_and_read_errors_name_the_path() {
        let missing = temporary_file();
        assert_eq!(
            BootstrapConfig::load(&missing).unwrap(),
            BootstrapConfig::default()
        );

        let valid = temporary_file();
        fs::write(&valid, "WindowSizeX = 1024\nWindowSizeY = 768\n").unwrap();
        let config = BootstrapConfig::load(&valid).unwrap();
        assert_eq!(config.window_size_x, Some(1024));
        assert_eq!(config.window_size_y, Some(768));
        assert_eq!(config.window_title, None);
        fs::remove_file(valid).unwrap();

        let malformed = temporary_file();
        fs::write(&malformed, "WindowSizeX=0\n").unwrap();
        let error = BootstrapConfig::load(&malformed).unwrap_err();
        assert!(
            error.contains(&format!("{}: строка 1:", malformed.display())),
            "{error}"
        );
        fs::remove_file(malformed).unwrap();

        let invalid_utf8 = temporary_file();
        fs::write(&invalid_utf8, [0xff]).unwrap();
        let error = BootstrapConfig::load(&invalid_utf8).unwrap_err();
        assert!(
            error.contains(&invalid_utf8.display().to_string()),
            "{error}"
        );
        fs::remove_file(invalid_utf8).unwrap();
    }
}
