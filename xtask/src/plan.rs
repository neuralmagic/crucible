use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use crate::model::Feedstock;

/// Paths whose change invalidates every image. Cargo.toml/Cargo.lock stay out:
/// their churn is controller-driven, and generator changes already go global
/// through the xtask/ prefix.
const GLOBAL_FILES: &[&str] = &[".github/workflows/images.yml", "images/matrix.toml"];
const GLOBAL_PREFIXES: &[&str] = &["xtask/", ".cargo/"];

/// The CI build plan: one build entry per (image, arch), one manifest entry per image.
#[derive(Debug, Serialize)]
pub struct CiMatrix {
    pub count: usize,
    pub build: Vec<BuildEntry>,
    pub manifest: Vec<ManifestEntry>,
}

#[derive(Debug, Serialize)]
pub struct BuildEntry {
    pub image: String,
    pub arch: String,
}

#[derive(Debug, Serialize)]
pub struct ManifestEntry {
    pub image: String,
    /// Space-separated for a bash loop in the workflow.
    pub arches: String,
}

/// Image names affected by a set of changed paths, in matrix order.
/// `None` means "everything" (no diff base available).
pub fn affected(feedstock: &Feedstock, changed: Option<&[String]>) -> Result<Vec<String>> {
    let Some(changed) = changed else {
        return Ok(feedstock
            .matrix
            .image
            .iter()
            .map(|i| i.name.clone())
            .collect());
    };
    let mut features = BTreeSet::new();
    for path in changed {
        if GLOBAL_FILES.contains(&path.as_str())
            || GLOBAL_PREFIXES.iter().any(|p| path.starts_with(p))
        {
            return affected(feedstock, None);
        }
        if let Some(rest) = path.strip_prefix("images/features/")
            && let Some((feature, _)) = rest.split_once('/')
        {
            features.insert(feature.to_string());
        }
    }
    let mut out = Vec::new();
    for spec in &feedstock.matrix.image {
        let resolved = feedstock.resolve(spec)?;
        if resolved.features.iter().any(|f| features.contains(&f.name)) {
            out.push(spec.name.clone());
        }
    }
    Ok(out)
}

pub fn ci_matrix(
    feedstock: &Feedstock,
    changed: Option<&[String]>,
    skip_heavy: bool,
) -> Result<CiMatrix> {
    let names = affected(feedstock, changed)?;
    let mut build = Vec::new();
    let mut manifest = Vec::new();
    for spec in &feedstock.matrix.image {
        if !names.contains(&spec.name) || (skip_heavy && spec.heavy) {
            continue;
        }
        for arch in &spec.arches {
            build.push(BuildEntry {
                image: spec.name.clone(),
                arch: arch.clone(),
            });
        }
        manifest.push(ManifestEntry {
            image: spec.name.clone(),
            arches: spec.arches.join(" "),
        });
    }
    Ok(CiMatrix {
        count: manifest.len(),
        build,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use crate::model::testutil::feedstock;

    const MATRIX: &str = concat!(
        "schema = 1\n\n",
        "[[image]]\nname = \"sandbox-go-cc\"\nfeatures = [\"go\", \"claude-code\"]\n\n",
        "[[image]]\nname = \"sandbox-cc\"\nfeatures = [\"claude-code\"]\nheavy = true\narches = [\"amd64\"]\n\n",
        "[[image]]\nname = \"loop-base\"\nfeatures = [\"go\"]\n",
    );

    fn changed(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn feature_change_affects_exactly_its_images() {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = feedstock(dir.path(), MATRIX);
        let got = crate::plan::affected(&fs_, Some(&changed(&["images/features/go/install.sh"])))
            .unwrap();
        // go is in the sandbox and the loop base: lockstep rebuild of both.
        assert_eq!(got, ["sandbox-go-cc", "loop-base"]);
        let got =
            crate::plan::affected(&fs_, Some(&changed(&["images/features/base/feature.toml"])))
                .unwrap();
        assert_eq!(got, ["sandbox-go-cc", "sandbox-cc", "loop-base"]);
        let got = crate::plan::affected(&fs_, Some(&changed(&["docs/readme.md"]))).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn global_paths_widen_the_set() {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = feedstock(dir.path(), MATRIX);
        let got = crate::plan::affected(&fs_, Some(&changed(&["xtask/src/main.rs"]))).unwrap();
        assert_eq!(got.len(), 3);
        let got = crate::plan::affected(&fs_, None).unwrap();
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn ci_matrix_expands_arches_and_skips_heavy() {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = feedstock(dir.path(), MATRIX);
        let m = crate::plan::ci_matrix(&fs_, None, true).unwrap();
        assert_eq!(m.count, 2);
        let pairs: Vec<(&str, &str)> = m
            .build
            .iter()
            .map(|b| (b.image.as_str(), b.arch.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("sandbox-go-cc", "amd64"),
                ("sandbox-go-cc", "arm64"),
                ("loop-base", "amd64"),
                ("loop-base", "arm64"),
            ]
        );
        let m = crate::plan::ci_matrix(&fs_, None, false).unwrap();
        assert_eq!(m.count, 3);
        let cc = m.manifest.iter().find(|e| e.image == "sandbox-cc").unwrap();
        assert_eq!(cc.arches, "amd64");
    }
}
