use std::path::PathBuf;

pub fn get_default_output_dir() -> PathBuf {
    let home = home::home_dir().expect("Could not find home directory");
    home.join("Downloads").join("video-transcripts")
}

pub fn get_models_dir() -> PathBuf {
    let home = home::home_dir().expect("Could not find home directory");
    home.join(".cache")
        .join("video-transcriber-mcp")
        .join("models")
}
