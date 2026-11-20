//! Icons Flint draws that gpui-kit's default set lacks, and the asset source that serves them
//! beside gpui-kit's. They are Lucide icons, as gpui-kit's are (`assets/icons/LICENSE-LUCIDE`);
//! only these few are embedded, not the whole catalog.

use std::borrow::Cow;

use gpui_kit::assets::IconNamed;
use gpui_kit::*;

/// An icon of Flint's own set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlintIcon {
    /// A protected cluster (ADR-0015).
    Lock,
    /// A cluster unlocked for this session.
    LockOpen,
    /// A trusted cluster.
    ShieldCheck,
}

impl FlintIcon {
    const ALL: [FlintIcon; 3] = [FlintIcon::Lock, FlintIcon::LockOpen, FlintIcon::ShieldCheck];

    fn asset_path(self) -> &'static str {
        match self {
            FlintIcon::Lock => "icons/flint/lock.svg",
            FlintIcon::LockOpen => "icons/flint/lock-open.svg",
            FlintIcon::ShieldCheck => "icons/flint/shield-check.svg",
        }
    }

    fn svg(self) -> &'static [u8] {
        match self {
            FlintIcon::Lock => include_bytes!("../../assets/icons/lock.svg"),
            FlintIcon::LockOpen => include_bytes!("../../assets/icons/lock-open.svg"),
            FlintIcon::ShieldCheck => include_bytes!("../../assets/icons/shield-check.svg"),
        }
    }
}

impl IconNamed for FlintIcon {
    fn path(self) -> SharedString {
        self.asset_path().into()
    }
}

/// gpui-kit's assets, and Flint's icons under `icons/flint/`.
pub struct FlintAssets;

impl AssetSource for FlintAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match FlintIcon::ALL.iter().find(|i| i.asset_path() == path) {
            Some(icon) => Ok(Some(Cow::Borrowed(icon.svg()))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed = gpui_kit::assets::Assets.list(path)?;
        listed.extend(
            FlintIcon::ALL
                .iter()
                .map(|i| i.asset_path())
                .filter(|p| p.starts_with(path))
                .map(SharedString::from),
        );
        Ok(listed)
    }
}

#[cfg(test)]
mod tests {
    use super::{FlintAssets, FlintIcon};
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconNamed as _;

    #[test]
    fn flint_icons_load_beside_gpui_kits() {
        for icon in FlintIcon::ALL {
            let svg = FlintAssets.load(&icon.path()).unwrap().expect("embedded");
            assert!(svg.starts_with(b"<svg"), "{icon:?}");
        }
        assert!(FlintAssets.load("icons/bot.svg").unwrap().is_some());
        let listed = FlintAssets.list("icons/flint/").unwrap();
        assert_eq!(listed.len(), 3, "{listed:?}");
    }
}
