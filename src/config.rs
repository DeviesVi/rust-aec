// Persistent device selection config stored next to the executable.
// Format: simple key=value lines (mic=<id>, speaker=<id>, output=<id>).

use std::collections::HashMap;
use std::path::PathBuf;

pub struct Config {
    pub mic: Option<String>,
    pub speaker: Option<String>,
    pub output: Option<String>,
}

fn config_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("rust-aec.cfg"))
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config {
            mic: None,
            speaker: None,
            output: None,
        };
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config {
            mic: None,
            speaker: None,
            output: None,
        };
    };
    let mut map: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim(), v.trim());
        }
    }
    Config {
        mic: map.get("mic").map(|s| s.to_string()),
        speaker: map.get("speaker").map(|s| s.to_string()),
        output: map.get("output").map(|s| s.to_string()),
    }
}

pub fn save(mic: Option<&str>, speaker: Option<&str>, output: Option<&str>) {
    let Some(path) = config_path() else { return };
    let mut lines = Vec::new();
    if let Some(id) = mic {
        lines.push(format!("mic={id}"));
    }
    if let Some(id) = speaker {
        lines.push(format!("speaker={id}"));
    }
    if let Some(id) = output {
        lines.push(format!("output={id}"));
    }
    let _ = std::fs::write(path, lines.join("\n"));
}
