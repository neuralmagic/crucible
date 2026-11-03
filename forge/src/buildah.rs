//! What a rootless buildah needs from the pod it runs in, shared by the cluster build Job and the
//! loop pod that builds in place.

use k8s_openapi::api::core::v1 as core;

/// `BUILDAH_ISOLATION=chroot`: no privilege, so no runc.
pub const ISOLATION: (&str, &str) = ("BUILDAH_ISOLATION", "chroot");

/// `STORAGE_DRIVER=vfs`: no /dev/fuse for fuse-overlayfs and no privilege for kernel overlay.
pub const STORAGE_DRIVER: (&str, &str) = ("STORAGE_DRIVER", "vfs");

fn env(pair: (&str, &str)) -> core::EnvVar {
    core::EnvVar {
        name: pair.0.to_string(),
        value: Some(pair.1.to_string()),
        value_from: None,
    }
}

pub fn isolation_env() -> core::EnvVar {
    env(ISOLATION)
}

pub fn storage_driver_env() -> core::EnvVar {
    env(STORAGE_DRIVER)
}

/// The pod annotation that lifts the node's default AppArmor profile off `container`, which
/// otherwise denies the mount syscalls a rootless build needs even inside a user namespace.
pub fn apparmor_unconfined(container: &str) -> (String, String) {
    (
        format!("container.apparmor.security.beta.kubernetes.io/{container}"),
        "unconfined".to_string(),
    )
}
