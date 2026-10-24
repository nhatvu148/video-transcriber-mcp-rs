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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_dir_is_under_the_home_downloads_folder() {
        let home = home::home_dir().unwrap();
        assert_eq!(
            get_default_output_dir(),
            home.join("Downloads").join("video-transcripts")
        );
    }

    #[test]
    fn models_dir_is_under_the_home_cache_folder() {
        let home = home::home_dir().unwrap();
        assert_eq!(
            get_models_dir(),
            home.join(".cache")
                .join("video-transcriber-mcp")
                .join("models")
        );
    }
}
