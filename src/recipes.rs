//! Curated GitHub release recipes for interactive onboarding.
//!
//! Recipes are suggestions, not artifact confirmation: callers should preview the
//! live release and let the user confirm its asset before installing. Asset names
//! and archive layouts are upstream contracts and must be maintained as they change.

use crate::config::AppFile;

/// One explicitly supported Linux architecture and its upstream asset filter.
#[derive(Debug)]
pub struct RecipeTarget {
    pub architecture: &'static str,
    pub asset_regex: &'static str,
}

/// A recipe publishes a single executable from an upstream release archive.
#[derive(Debug)]
pub struct Recipe {
    pub id: &'static str,
    pub name: &'static str,
    pub repo: &'static str,
    pub targets: &'static [RecipeTarget],
    pub executable: &'static str,
}

static RIPGREP_TARGETS: &[RecipeTarget] = &[
    RecipeTarget { architecture: "x86_64", asset_regex: r"^ripgrep-[0-9]+\.[0-9]+\.[0-9]+-x86_64-unknown-linux-musl\.tar\.gz$" },
    RecipeTarget { architecture: "aarch64", asset_regex: r"^ripgrep-[0-9]+\.[0-9]+\.[0-9]+-aarch64-unknown-linux-gnu\.tar\.gz$" },
];
static FD_TARGETS: &[RecipeTarget] = &[
    RecipeTarget { architecture: "x86_64", asset_regex: r"^fd-v[0-9]+\.[0-9]+\.[0-9]+-x86_64-unknown-linux-musl\.tar\.gz$" },
    RecipeTarget { architecture: "aarch64", asset_regex: r"^fd-v[0-9]+\.[0-9]+\.[0-9]+-aarch64-unknown-linux-gnu\.tar\.gz$" },
];

/// The complete catalog. Architectures refer to Linux targets, not the host OS.
pub static RECIPES: &[Recipe] = &[
    Recipe { id: "ripgrep", name: "ripgrep", repo: "BurntSushi/ripgrep", targets: RIPGREP_TARGETS, executable: "rg" },
    Recipe { id: "fd", name: "fd", repo: "sharkdp/fd", targets: FD_TARGETS, executable: "fd" },
];

/// Look up a recipe by its stable ID.
pub fn find(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|recipe| recipe.id == id)
}

impl Recipe {
    /// Find a supported Linux architecture by its canonical Rust architecture name.
    pub fn target(&self, architecture: &str) -> Option<&RecipeTarget> {
        self.targets.iter().find(|target| target.architecture == architecture)
    }

    /// Construct a normal app definition for a target from this recipe.
    ///
    /// Returns `None` for unsupported targets. The resulting archive installer
    /// publishes the executable under the managed install root and links it into
    /// the configured bin directory. No installed-version probe is configured:
    /// adopting an existing installation requires a separately verified probe.
    pub fn app(&self, architecture: &str) -> anyhow::Result<Option<AppFile>> {
        let Some(target) = self.target(architecture) else { return Ok(None) };
        let value = serde_json::json!({
            "id": self.id,
            "provider": "github-releases",
            "repo": self.repo,
            "tag_regex": r"^v?[0-9]+\.[0-9]+\.[0-9]+$",
            "asset_regex": target.asset_regex,
            "install": {
                "type": "archive",
                "bin_glob": [self.executable],
                "link_bins": [self.executable]
            }
        });
        AppFile::parse_value(value, self.id).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InstallType, SourceKind};
    use crate::installer::{publish_from_staging, strip_single_root_dir};

    #[test]
    fn catalog_configs_and_asset_filters() {
        let examples = [
            ("ripgrep", "x86_64", "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz"),
            ("ripgrep", "aarch64", "ripgrep-14.1.1-aarch64-unknown-linux-gnu.tar.gz"),
            ("fd", "x86_64", "fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz"),
            ("fd", "aarch64", "fd-v10.2.0-aarch64-unknown-linux-gnu.tar.gz"),
        ];
        for (id, arch, asset) in examples {
            let recipe = find(id).unwrap();
            let app = recipe.app(arch).unwrap().unwrap();
            assert_eq!(app.id, id);
            assert_eq!(app.install.kind, InstallType::Archive);
            assert_eq!(app.install.bin_glob, [recipe.executable]);
            assert_eq!(app.install.link_bins, [recipe.executable]);
            assert!(app.installed_version_cmd.is_none());
            let SourceKind::GithubReleases(source) = app.source_kind else { panic!("wrong provider") };
            assert_eq!(source.repo, recipe.repo);
            let regex = regex::Regex::new(source.asset_regex.as_ref().unwrap()).unwrap();
            assert!(regex.is_match(asset));
            assert!(!regex.is_match(&format!("{asset}.sha256")));
            assert!(!regex.is_match(&asset.replace(arch, "other")));
            assert!(!regex.is_match(&asset.replace("linux", "windows")));
            let tags = regex::Regex::new(source.tag_regex.as_ref().unwrap()).unwrap();
            assert!(tags.is_match("v1.2.3"));
            assert!(!tags.is_match("v1.2.3-rc1"));
        }
        assert!(find("missing").is_none());
        for recipe in RECIPES {
            assert!(recipe.app("unsupported").unwrap().is_none());
        }
    }

    #[test]
    fn archive_layouts_publish_recipe_executable() {
        for recipe in RECIPES {
            let root = tempfile::tempdir().unwrap();
            let staging = root.path().join("staging");
            let archive_root = staging.join("upstream-release");
            std::fs::create_dir_all(&archive_root).unwrap();
            std::fs::write(archive_root.join(recipe.executable), b"binary").unwrap();
            std::fs::write(archive_root.join("README.md"), b"readme").unwrap();
            strip_single_root_dir(&staging).unwrap();
            let app = recipe.app("x86_64").unwrap().unwrap();
            let current = root.path().join("current");
            let publication = publish_from_staging(&staging, &app.install, &current).unwrap();
            assert_eq!(publication.published, [recipe.executable]);
            assert_eq!(std::fs::read(current.join(recipe.executable)).unwrap(), b"binary");
            assert!(!current.join("README.md").exists());
        }
    }
}
