use engine_window::WindowConfig;
use std::fs;
use std::io::ErrorKind;

const FILE_NAME: &str = "bootstrap.rves";

pub fn load() -> Result<WindowConfig, String> {
    match fs::read_to_string(FILE_NAME) {
        Ok(contents) => parse(&contents),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(WindowConfig::default()),
        Err(error) => Err(format!("{FILE_NAME}: {error}")),
    }
}

fn parse(contents: &str) -> Result<WindowConfig, String> {
    let mut config = WindowConfig::default();
    let mut seen = [false; 2];
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let error = |message: String| format!("{FILE_NAME}: строка {}: {message}", index + 1);
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| error("ожидается Имя=Значение".to_owned()))?;
        let name = name.trim();
        let (dimension, seen_index) = match name {
            "WindowSizeX" => (&mut config.width, 0),
            "WindowSizeY" => (&mut config.height, 1),
            _ => return Err(error(format!("неизвестный параметр {name}"))),
        };
        if seen[seen_index] {
            return Err(error(format!("параметр {name} указан повторно")));
        }
        let value = value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| error(format!("{name} должен быть положительным целым u32")))?;
        *dimension = value;
        seen[seen_index] = true;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_dimensions_with_whitespace_and_windows_line_endings() {
        let config = parse(" WindowSizeY = 900\r\n\r\nWindowSizeX=1600\r\n").unwrap();
        assert_eq!((config.width, config.height), (1600, 900));
    }

    #[test]
    fn defaults_only_missing_dimensions() {
        for (contents, expected) in [
            ("", (1280, 720)),
            ("\n  \n", (1280, 720)),
            ("WindowSizeX=800", (800, 720)),
            ("WindowSizeY=600", (1280, 600)),
        ] {
            let config = parse(contents).unwrap();
            assert_eq!((config.width, config.height), expected);
        }
    }

    #[test]
    fn rejects_invalid_dimensions_and_reports_the_line() {
        for value in ["", "0", "-1", "1.5", "abc", "4294967296", "720=800"] {
            for name in ["WindowSizeX", "WindowSizeY"] {
                let error = parse(&format!("\n{name}={value}")).err().unwrap();
                assert!(error.contains("bootstrap.rves: строка 2:"), "{error}");
                assert!(error.contains(name), "{error}");
            }
        }
    }

    #[test]
    fn rejects_malformed_unknown_and_duplicate_parameters() {
        for contents in [
            "WindowSizeX 800",
            "WindowWidth=800",
            "windowsizex=800",
            "=800",
            "WindowSizeX=800\nWindowSizeX=900",
            "WindowSizeY=600\nWindowSizeY=700",
        ] {
            assert!(parse(contents).is_err(), "{contents}");
        }
    }
}
