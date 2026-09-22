//! Build identity is independent of Cargo's optimization profile.
use std::path::{Path, PathBuf};

pub const fn channel() -> &'static str {
    match option_env!("NOCHES_CHANNEL") {
        Some(value) => value,
        None => "local",
    }
}

pub fn distributed() -> bool {
    matches!(channel(), "stable" | "dev")
}
pub fn app_name() -> &'static str {
    if channel() == "dev" {
        "Noches Dev"
    } else {
        "Noches"
    }
}
pub fn slug() -> &'static str {
    if channel() == "dev" {
        "noches-dev"
    } else {
        "noches"
    }
}
pub fn bundle_name() -> String {
    format!("{}.app", app_name())
}
pub fn bundle_id() -> &'static str {
    if channel() == "dev" {
        "io.github.kldsseeghosts.noches.dev"
    } else {
        "io.github.kldsseeghosts.noches"
    }
}
pub fn service_name() -> &'static str {
    match channel() {
        "dev" => "noches-dev.service",
        "stable" => "noches.service",
        _ => "zeron.service",
    }
}
pub fn launchd_label() -> &'static str {
    if distributed() {
        bundle_id()
    } else {
        "sh.zeron.app"
    }
}
pub fn data_folder() -> &'static str {
    match channel() {
        "dev" => ".noches-dev",
        "stable" => ".noches",
        _ => ".zeron",
    }
}
pub fn ipc_port() -> u16 {
    match channel() {
        "dev" => 27656,
        "stable" => 27655,
        _ => 27654,
    }
}
pub fn app_root(home: &Path) -> PathBuf {
    if distributed() {
        home.join(".local/share").join(slug()).join("app")
    } else {
        home.join(".zeron/app")
    }
}
pub const fn repository() -> &'static str {
    match option_env!("NOCHES_REPOSITORY") {
        Some(value) => value,
        None => "KldsSeeGhosts/noches",
    }
}
pub const fn commit() -> &'static str {
    match option_env!("NOCHES_COMMIT") {
        Some(value) => value,
        None => "local",
    }
}

pub const SERVICE_NAME: &str = env!("NOCHES_SERVICE_NAME");
pub const LAUNCHD_LABEL: &str = env!("NOCHES_LAUNCHD_LABEL");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_identity_keeps_channels_separate() {
        let home = Path::new("/home/test");
        match channel() {
            "dev" => {
                assert_eq!(app_name(), "Noches Dev");
                assert_eq!(ipc_port(), 27656);
                assert_eq!(data_folder(), ".noches-dev");
                assert_eq!(app_root(home), home.join(".local/share/noches-dev/app"));
                assert_eq!(SERVICE_NAME, "noches-dev.service");
            }
            "stable" => {
                assert_eq!(app_name(), "Noches");
                assert_eq!(ipc_port(), 27655);
                assert_eq!(data_folder(), ".noches");
                assert_eq!(app_root(home), home.join(".local/share/noches/app"));
                assert_eq!(SERVICE_NAME, "noches.service");
            }
            "local" => {
                assert!(!distributed());
                assert_eq!(ipc_port(), 27654);
                assert_eq!(data_folder(), ".zeron");
            }
            other => panic!("unexpected compiled channel: {other}"),
        }
        assert_eq!(service_name(), SERVICE_NAME);
        assert_eq!(launchd_label(), LAUNCHD_LABEL);
    }
}
