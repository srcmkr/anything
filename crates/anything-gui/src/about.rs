//! What the About page shows, and the short system report it copies for bug
//! reports. The report names no files or folders.

use crate::service::Info;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

pub fn issues_url() -> String {
    format!("{REPOSITORY}/issues")
}

/// Plain English on purpose: it ends up in bug reports.
pub fn report(service: Option<&Info>, using_service: bool) -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| name.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "unknown".into());
    let service = match service {
        None => "unknown".to_owned(),
        Some(info) if info.manual() => format!("started by hand, {}", answer(info)),
        Some(info) if !info.installed => "not installed".to_owned(),
        Some(info) => format!(
            "{}, {}{}",
            if info.enabled { "enabled" } else { "disabled" },
            info.active_state,
            if info.answer.is_some() { format!(", {}", answer(info)) } else { String::new() },
        ),
    };
    format!(
        "Anything {VERSION}\n\
         System: {os}, Linux {}\n\
         Desktop: {}, {}\n\
         Service: {service}\n\
         Index: {}\n",
        kernel.trim(),
        env("XDG_CURRENT_DESKTOP"),
        env("XDG_SESSION_TYPE"),
        if using_service { "from the service" } else { "own scan" },
    )
}

fn answer(info: &Info) -> String {
    match info.answer {
        Some((state, entries)) => format!("{state:?}, {entries} entries"),
        None => "no answer".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_names_version_and_service() {
        let info = Info {
            installed: true,
            enabled: true,
            active_state: "active".into(),
            answer: Some((anything_proto::State::Ready, 42)),
            ..Default::default()
        };
        let text = report(Some(&info), true);
        assert!(text.starts_with(&format!("Anything {VERSION}\n")));
        assert!(text.contains("Service: enabled, active, Ready, 42 entries\n"));
        assert!(text.contains("Index: from the service\n"));
        assert!(report(Some(&Info::default()), false).contains("Service: not installed\n"));
        assert_eq!(issues_url(), "https://github.com/srcmkr/anything/issues");
    }
}
