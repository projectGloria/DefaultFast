//! Plain data shared between discovery, search and the UI. Search strings are lowercased once
//! at build time so per-keystroke matching never allocates.

/// A file type (".mp4") or protocol ("https"): anything that can have a default handler.
pub fn is_protocol(target: &str) -> bool {
    !target.starts_with('.')
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Support {
    /// ".mp4" or "https", lowercase.
    pub target: String,
    /// The ProgID the program registered for this target ("VLC.mp4", "ChromeHTML",
    /// "Applications\\vlc.exe", "AppX…").
    pub prog_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub name: String,
    /// Absolute executable path; empty for packaged apps.
    pub exe: String,
    /// AppUserModelID for packaged apps.
    pub aumid: String,
    /// Name under RegisteredApplications (enables the ms-settings deep link).
    pub registered_name: String,
    pub registered_machine: bool,
    pub supports: Vec<Support>,
    /// Lowercase: name, exe file name.
    pub search: String,
    pub search_name: String,
    /// How trustworthy `name` is (higher wins on merge): 0 file stem, 1 version resource,
    /// 2 Start Menu shortcut / FriendlyAppName, 3 RegisteredApplications / packaged display name.
    pub name_rank: u8,
}

impl Program {
    /// Dedupe identity: executable path or AUMID, lowercase.
    pub fn key(&self) -> String {
        if !self.exe.is_empty() {
            self.exe.to_lowercase()
        } else {
            self.aumid.to_lowercase()
        }
    }
    pub fn exe_name(&self) -> &str {
        self.exe.rsplit(['\\', '/']).next().unwrap_or("")
    }
    pub fn reindex(&mut self) {
        self.search_name = self.name.to_lowercase();
        self.search = format!("{} {}", self.search_name, self.exe_name().to_lowercase());
    }
    pub fn add_support(&mut self, target: &str, prog_id: &str) {
        let target = target.to_lowercase();
        if let Some(s) = self.supports.iter_mut().find(|s| s.target == target) {
            // A Capabilities ProgID beats a generic Applications\x.exe registration.
            if s.prog_id.starts_with("Applications\\") && !prog_id.starts_with("Applications\\") {
                s.prog_id = prog_id.to_owned();
            }
        } else {
            self.supports.push(Support { target, prog_id: prog_id.to_owned() });
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileType {
    pub ext: String,
    pub friendly: String,
    pub search: String,
    pub search_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct Protocol {
    pub name: String,
    pub friendly: String,
    pub search: String,
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    pub programs: Vec<Program>,
    pub file_types: Vec<FileType>,
    pub protocols: Vec<Protocol>,
    /// False while the slower second pass (Start Menu, packaged apps) is still running.
    pub complete: bool,
}

impl Index {
    pub fn program_by_key(&self, key: &str) -> Option<&Program> {
        self.programs.iter().find(|p| p.key() == key)
    }
    /// Programs that declare support for `target`.
    pub fn programs_for<'a>(&'a self, target: &'a str) -> impl Iterator<Item = (&'a Program, &'a Support)> + 'a {
        self.programs
            .iter()
            .filter_map(move |p| p.supports.iter().find(|s| s.target == target).map(|s| (p, s)))
    }
}
