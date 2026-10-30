#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileType {
    pub extension: String,
    pub name: String,
    pub prog_id: String,
    pub current_app: String,
    pub icon: String,
    pub index: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct App {
    pub name: String,
    pub executable: String,
    pub prog_id: String,
    pub publisher: String,
    pub icon: String,
    pub index: String,
    pub recommended: bool,
}

impl FileType {
    pub fn reindex(&mut self) {
        self.index = format!(
            "{} {} {} {}",
            self.extension.trim_start_matches('.'),
            self.name,
            self.prog_id,
            self.current_app
        )
        .to_lowercase();
    }
}
impl App {
    pub fn identity(&self) -> &str {
        if !self.executable.is_empty() {
            &self.executable
        } else if !self.prog_id.is_empty() {
            &self.prog_id
        } else {
            &self.icon
        }
    }
    pub fn reindex(&mut self) {
        self.index = format!(
            "{} {} {}",
            self.name,
            std::path::Path::new(&self.executable)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            if self.publisher == "Installed app" {
                ""
            } else {
                &self.publisher
            }
        )
        .to_lowercase();
    }
}
