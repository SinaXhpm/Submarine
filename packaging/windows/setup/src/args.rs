//! The command line, in NSIS's form, so scripts and package managers that ran
//! the NSIS installer keep working:
//!
//!   /S          silent: no window at all
//!   /P          passive: only the progress, no questions
//!   /NS         no shortcuts
//!   /R          start the app when a silent or passive install is done
//!   /UPDATE     an update: keep the shortcuts as they are
//!   /D=folder   install folder; last, unquoted, may contain spaces
//!   _?=folder   (uninstaller) the install folder; last, like /D=
//!
//! Anything else is ignored.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    pub silent: bool,
    pub passive: bool,
    pub no_shortcuts: bool,
    pub relaunch: bool,
    pub update: bool,
    pub dir: Option<String>,
    pub in_place_dir: Option<String>,
}

pub fn from_process() -> Args {
    let line = unsafe { windows::Win32::System::Environment::GetCommandLineW().to_string() }.unwrap_or_default();
    parse(&line)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..])
}

fn value(v: &str) -> Option<String> {
    let v = v.trim().trim_matches('"').trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// The program's own name: quoted, or up to the first space.
fn skip_program(line: &str) -> &str {
    let line = line.trim_start();
    if let Some(rest) = line.strip_prefix('"') {
        rest.find('"').map_or("", |i| &rest[i + 1..])
    } else {
        line.find(char::is_whitespace).map_or("", |i| &line[i..])
    }
}

/// One argument (quotes removed) and what follows it.
fn next_token(s: &str) -> (String, &str) {
    let mut token = String::new();
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return (token, &s[i..]),
            c => token.push(c),
        }
    }
    (token, "")
}

pub fn parse(line: &str) -> Args {
    let mut args = Args::default();
    let mut rest = skip_program(line);
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // /D= and _?= take the rest of the line, spaces and all.
        if let Some(v) = strip_prefix_ci(rest, "/D=").or_else(|| strip_prefix_ci(rest, "\"/D=")) {
            args.dir = value(v);
            break;
        }
        if let Some(v) = strip_prefix_ci(rest, "_?=").or_else(|| strip_prefix_ci(rest, "\"_?=")) {
            args.in_place_dir = value(v);
            break;
        }
        let (token, after) = next_token(rest);
        match token.to_ascii_uppercase().as_str() {
            "/S" => args.silent = true,
            "/P" => args.passive = true,
            "/NS" => args.no_shortcuts = true,
            "/R" => args.relaunch = true,
            "/UPDATE" => args.update = true,
            _ => {}
        }
        rest = after;
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_folder() {
        let a = parse(r#""C:\Downloads\Submarine setup.exe" /s /NS /D=C:\Program Files\Sub marine"#);
        assert!(a.silent && a.no_shortcuts && !a.passive);
        assert_eq!(a.dir.as_deref(), Some(r"C:\Program Files\Sub marine"));
    }

    #[test]
    fn unquoted_program_and_in_place() {
        let a = parse(r"C:\x\uninstall.exe /P /UPDATE _?=C:\Users\a b\AppData\Local\Submarine");
        assert!(a.passive && a.update);
        assert_eq!(a.in_place_dir.as_deref(), Some(r"C:\Users\a b\AppData\Local\Submarine"));
    }

    #[test]
    fn quoted_folder_and_unknown_flags() {
        let a = parse(r#"setup.exe /NCRC /R "/D=D:\Apps\Submarine""#);
        assert!(a.relaunch);
        assert_eq!(a.dir.as_deref(), Some(r"D:\Apps\Submarine"));
        assert_eq!(parse("setup.exe"), Args::default());
        assert_eq!(parse(r#""setup.exe""#), Args::default());
        assert_eq!(parse("setup.exe /D="), Args::default());
    }
}
