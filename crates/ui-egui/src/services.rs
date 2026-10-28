use std::path::{Path, PathBuf};

/// Host services (file dialogs) injected by the app; absent in tests and on the web. They return
/// paths as picked: the caller converts one to text where a command takes it ([`crate::dialogs::unicode`]).
/// The About text of a school edition: what it is, who adapted it and where it comes from.
pub struct SchoolAbout {
    pub name: String,
    pub lines: Vec<String>,
    /// Label and address of a link to the original project.
    pub link: Option<(String, String)>,
    /// The signed-in account, asked each time the window is drawn.
    pub account: Option<Box<dyn Fn() -> Option<String>>>,
}

#[derive(Default)]
pub struct Services {
    /// The About text of a school edition (see [`SchoolAbout`]).
    pub school_about: Option<SchoolAbout>,
    pub pick_open: Option<Box<dyn Fn(&str, &[&str]) -> Option<PathBuf>>>,
    /// The purpose and the file to start from.
    pub pick_save: Option<Box<dyn Fn(&str, &Path) -> Option<PathBuf>>>,
    /// The folder to start in.
    pub pick_folder: Option<Box<dyn Fn(&Path) -> Option<PathBuf>>>,
}
