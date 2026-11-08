//! Makes template output valid on every supported filesystem.

const MAX_COMPONENT_BYTES: usize = 200;
const MAX_KEPT_EXTENSION_BYTES: usize = 16;

/// Turns arbitrary text into one safe path component. The result is never empty, `.` or `..`,
/// and is valid on Windows, macOS and Linux. Applying it twice changes nothing.
pub fn sanitize_component(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let mut name = trim(&replaced).to_owned();
    if is_reserved(&name) {
        name.insert(0, '_');
    }
    let clamped = clamp(&name);
    let clamped = trim(&clamped);
    if clamped.is_empty() {
        "_".to_owned()
    } else {
        clamped.to_owned()
    }
}

/// Leading spaces and trailing spaces or dots are stripped; Windows drops the trailing ones
/// silently, which would make the stored path wrong.
fn trim(name: &str) -> &str {
    name.trim_start_matches(' ').trim_end_matches([' ', '.'])
}

fn is_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let mut chars = stem.chars();
            let prefix: String = chars.by_ref().take(3).collect();
            let rest = chars.as_str();
            (prefix == "COM" || prefix == "LPT")
                && rest.chars().count() == 1
                && rest
                    .chars()
                    .all(|c| c.is_ascii_digit() || "¹²³".contains(c))
        }
    }
}

/// Shortens to the byte limit on a character boundary, keeping a short extension.
fn clamp(name: &str) -> std::borrow::Cow<'_, str> {
    if name.len() <= MAX_COMPONENT_BYTES {
        return name.into();
    }
    let ext = name
        .rfind('.')
        .map(|dot| &name[dot..])
        .filter(|ext| ext.len() <= MAX_KEPT_EXTENSION_BYTES)
        .unwrap_or_default();
    let mut end = MAX_COMPONENT_BYTES - ext.len();
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ext}", trim(&name[..end])).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn assert_valid(name: &str) {
        assert!(!name.is_empty());
        assert!(name != "." && name != "..");
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert!(
            !name
                .chars()
                .any(|c| "<>:\"/\\|?*".contains(c) || c.is_control()),
            "{name:?}"
        );
        assert!(!name.ends_with([' ', '.']), "{name:?}");
        assert!(!name.starts_with(' '), "{name:?}");
        assert!(!is_reserved(name), "{name:?}");
    }

    proptest! {
        #[test]
        fn output_is_valid_and_stable(raw in any::<String>()) {
            let once = sanitize_component(&raw);
            assert_valid(&once);
            prop_assert_eq!(sanitize_component(&once), once);
        }

        #[test]
        fn awkward_names_are_handled(
            raw in "(CON|nul|Com1|LPT9|\\.\\.|\\.| |a|é|/|\\\\|\\.flac){0,300}"
        ) {
            let once = sanitize_component(&raw);
            assert_valid(&once);
            prop_assert_eq!(sanitize_component(&once), once);
        }
    }

    #[test]
    fn examples() {
        assert_eq!(sanitize_component("AC/DC: Back?"), "AC_DC_ Back_");
        assert_eq!(sanitize_component("con"), "_con");
        assert_eq!(sanitize_component("CON.mp3"), "_CON.mp3");
        assert_eq!(sanitize_component("Console"), "Console");
        assert_eq!(sanitize_component("COM¹"), "_COM¹");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("name. . "), "name");
        assert_eq!(sanitize_component(".hidden"), ".hidden");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let name = sanitize_component(&format!("{}.flac", "é".repeat(300)));
        assert!(name.len() <= MAX_COMPONENT_BYTES);
        assert!(name.ends_with("é.flac"));
    }
}
