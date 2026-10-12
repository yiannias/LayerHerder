//! Pure logic behind the Settings panel. No egui in this file.

use acad_layer_core::ImportReport;
use std::path::Path;

pub const PROJECT_WEBSITE_URL: &str = "https://layerherd.com/";
pub const PROJECT_REPO_URL: &str = "https://github.com/yiannias/LayerHerder";

pub const BETA_NOTICE: &str = "Beta: please try this on a copy of your drawing first, and keep a backup before you apply changes.";

#[derive(Default)]
pub struct SettingsPanel {
    pub open: bool,
    pub status: String,
}

pub enum SettingsAction {
    ChangeStandard,
    ChangeMemoryFile,
    ImportMemory,
    ExportMemory,
}

pub struct StandardsFileLabel {
    pub text: String,
    pub missing: bool,
}

/// Last path component, accepting both `\` and `/` so Windows paths work anywhere.
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

pub fn standards_file_label(path: &str, exists: bool) -> StandardsFileLabel {
    if path.trim().is_empty() {
        return StandardsFileLabel {
            text: "No Standards File chosen".to_string(),
            missing: false,
        };
    }
    let name = file_name(path);
    if exists {
        StandardsFileLabel {
            text: name.to_string(),
            missing: false,
        }
    } else {
        StandardsFileLabel {
            text: format!("Unavailable: {name}"),
            missing: true,
        }
    }
}

/// The label while the off-thread existence check may still be running (`None`):
/// "Checking..." rather than a missing-file warning.
pub fn standards_file_display(path: &str, exists: Option<bool>) -> StandardsFileLabel {
    match exists {
        Some(exists) => standards_file_label(path, exists),
        None if path.trim().is_empty() => standards_file_label(path, false),
        None => StandardsFileLabel {
            text: "Checking...".to_string(),
            missing: false,
        },
    }
}

/// Shortens a path to its root and file name, e.g. `C:\Standards\Office\STANDARD TEMPLATE.dws`
/// becomes `C:\…\STANDARD TEMPLATE.dws`. Paths with no folders are returned unchanged.
pub fn shorten_path_for_display(path: &str) -> String {
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let parts: Vec<&str> = path.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return path.to_string();
    }
    let lead = if path.starts_with(['\\', '/']) { sep.to_string() } else { String::new() };
    format!("{lead}{}{sep}…{sep}{}", parts[0], parts[parts.len() - 1])
}

pub const NO_MEMORY_LOCATION: &str = "No memory file location is available.";

pub fn memory_changed_status(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!("Memory file changed. Now using {name}.")
}

pub fn import_status(report: &ImportReport) -> String {
    let mappings = if report.imported == 1 {
        "1 mapping".to_string()
    } else {
        format!("{} mappings", report.imported)
    };
    let new = if report.added == 1 {
        "1 was new".to_string()
    } else {
        format!("{} were new", report.added)
    };
    format!("Imported {mappings}. {new}.")
}

pub fn export_status(path: &Path) -> String {
    format!("Exported your memory to {}.", path.display())
}

pub fn about_line(version: &str) -> String {
    format!("Layer Herder Beta {version}")
}

/// False while an Apply/Purge/Load Standard is in flight.
pub fn settings_actions_enabled(busy: bool) -> bool {
    !busy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standards_file_label_covers_empty_existing_and_missing() {
        let l = standards_file_label("", false);
        assert_eq!(l.text, "No Standards File chosen");
        assert!(!l.missing);
        let l = standards_file_label("", true);
        assert_eq!(l.text, "No Standards File chosen");
        assert!(!l.missing);

        let l = standards_file_label(r"C:\Standards\office.json", true);
        assert_eq!(l.text, "office.json");
        assert!(!l.missing);
        let l = standards_file_label("/srv/standards/office.json", true);
        assert_eq!(l.text, "office.json");

        let l = standards_file_label(r"C:\Standards\office.json", false);
        assert_eq!(l.text, "Unavailable: office.json");
        assert!(l.missing);
    }

    #[test]
    fn standards_file_display_says_checking_until_the_answer_arrives() {
        let l = standards_file_display(r"C:\Standards\office.json", None);
        assert_eq!(l.text, "Checking...");
        assert!(!l.missing);
        let l = standards_file_display("", None);
        assert_eq!(l.text, "No Standards File chosen");
        assert!(!l.missing);
        let l = standards_file_display(r"C:\Standards\office.json", Some(false));
        assert_eq!(l.text, "Unavailable: office.json");
        assert!(l.missing);
        let l = standards_file_display(r"C:\Standards\office.json", Some(true));
        assert_eq!(l.text, "office.json");
        assert!(!l.missing);
    }

    #[test]
    fn shorten_path_keeps_the_root_and_file_name() {
        assert_eq!(
            shorten_path_for_display(r"C:\Standards\Office\STANDARD TEMPLATE.dws"),
            "C:\\…\\STANDARD TEMPLATE.dws"
        );
        assert_eq!(
            shorten_path_for_display("/srv/standards/office.dws"),
            "/srv/…/office.dws"
        );
        assert_eq!(shorten_path_for_display(r"C:\office.dws"), r"C:\office.dws");
    }

    #[test]
    fn memory_changed_status_names_the_new_file() {
        assert_eq!(
            memory_changed_status(Path::new("C:/shared/team_memory.json")),
            "Memory file changed. Now using team_memory.json."
        );
    }

    #[test]
    fn import_status_uses_singular_and_plural_wording() {
        let r = |imported, added| ImportReport { imported, added };
        assert_eq!(import_status(&r(5, 3)), "Imported 5 mappings. 3 were new.");
        assert_eq!(import_status(&r(1, 1)), "Imported 1 mapping. 1 was new.");
        assert_eq!(import_status(&r(0, 0)), "Imported 0 mappings. 0 were new.");
    }

    #[test]
    fn export_status_names_the_file() {
        assert_eq!(
            export_status(Path::new("C:/tmp/memory.json")),
            "Exported your memory to C:/tmp/memory.json."
        );
    }

    #[test]
    fn about_line_names_the_app_and_version() {
        assert_eq!(about_line("1.4.1"), "Layer Herder Beta 1.4.1");
    }

    #[test]
    fn project_links_point_at_the_project() {
        assert_eq!(
            PROJECT_WEBSITE_URL,
            "https://layerherd.com/"
        );
        assert_eq!(PROJECT_REPO_URL, "https://github.com/yiannias/LayerHerder");
    }

    #[test]
    fn beta_notice_is_the_agreed_wording() {
        assert_eq!(
            BETA_NOTICE,
            "Beta: please try this on a copy of your drawing first, and keep a backup before you apply changes."
        );
    }

    #[test]
    fn settings_actions_are_disabled_while_busy() {
        assert!(settings_actions_enabled(false));
        assert!(!settings_actions_enabled(true));
    }
}
