#![allow(clippy::disallowed_macros)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Every image gets this feature first; it is the substrate layer and must not
/// appear in a matrix entry's feature list.
pub const BASE_FEATURE: &str = "base";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSpec {
    pub summary: String,
    /// Canonical layer order is ascending (layer, name): heavy shared toolchains
    /// take low values so sibling images share their layer prefix; agent CLIs
    /// take high values so a cc/codex pair differs only in its last layers.
    pub layer: u32,
    /// Extra dnf repo files the feature's rpms come from, relative to images/;
    /// copied into /etc/yum.repos.d before the install.
    #[serde(default)]
    pub repofiles: Vec<String>,
    /// RPM packages, dnf-installed in the feature's layer.
    #[serde(default)]
    pub rpms: Vec<String>,
    /// Non-RPM version pins, exposed to install.sh as PIN_<NAME> build args.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    /// Capability predicates the feature grants: namespaced predicate -> version.
    /// A `{ pin = "name" }` value derives the predicate from that pin's effective
    /// version, so the doc cannot drift from what is installed.
    #[serde(default)]
    pub capabilities: BTreeMap<String, CapabilityValue>,
    /// Image ENV the feature needs at install and run time (e.g. RUSTUP_HOME, PATH).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Agent-facing orientation for this feature, assembled into the image's
    /// INTRO.md. `{pin.<name>}` resolves to the pin's effective version.
    #[serde(default)]
    pub intro: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum CapabilityValue {
    Literal(String),
    FromPin { pin: String },
}

#[derive(Debug)]
pub struct Feature {
    pub name: String,
    pub spec: FeatureSpec,
    /// install.sh next to feature.toml, if present.
    pub install: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matrix {
    pub schema: u32,
    pub image: Vec<ImageSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    pub name: String,
    /// Features beyond the implicit base, unordered; canonical order is derived.
    pub features: Vec<String>,
    /// Heavy images (large downloads, e.g. vllm) are skipped by PR CI.
    #[serde(default)]
    pub heavy: bool,
    /// Architectures to build; subset of KNOWN_ARCHES.
    #[serde(default = "default_arches")]
    pub arches: Vec<String>,
    /// Base image override (digest-pinned ref). A custom base skips the implicit
    /// dnf substrate feature and only rpm-free features may be composed on it.
    #[serde(default)]
    pub base: Option<String>,
    /// Capability predicates a custom base itself supplies (torch, python, ...),
    /// merged into the document as literals.
    #[serde(default)]
    pub provides: BTreeMap<String, String>,
    /// Version window for one pin: `{ pin = ["x.y.z", ...] }` expands this entry
    /// into one image per version, with `{ver}` in the name replaced by the minor.
    #[serde(default)]
    pub versions: Option<BTreeMap<String, Vec<String>>>,
    /// Set by matrix expansion: this instance builds with pin 0 at version 1.
    #[serde(skip)]
    pub pin_override: Option<(String, String)>,
}

/// "x.y.z" -> "x.y"; fewer components pass through unchanged.
pub fn minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

pub const KNOWN_ARCHES: &[&str] = &["amd64", "arm64"];

fn default_arches() -> Vec<String> {
    KNOWN_ARCHES.iter().map(|a| a.to_string()).collect()
}

#[derive(Debug)]
pub struct Feedstock {
    pub features: BTreeMap<String, Feature>,
    pub matrix: Matrix,
}

/// An image's feature list in canonical build order, with merged views.
pub struct ResolvedImage<'a> {
    pub spec: &'a ImageSpec,
    pub features: Vec<&'a Feature>,
}

impl Feedstock {
    pub fn load(root: &Path) -> Result<Self> {
        let features_dir = root.join("features");
        let mut features = BTreeMap::new();
        for entry in fs::read_dir(&features_dir)
            .with_context(|| format!("reading {}", features_dir.display()))?
        {
            let dir = entry?.path();
            if !dir.is_dir() {
                continue;
            }
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .with_context(|| format!("non-utf8 feature dir {}", dir.display()))?
                .to_string();
            let spec_path = dir.join("feature.toml");
            let spec: FeatureSpec = toml::from_str(
                &fs::read_to_string(&spec_path)
                    .with_context(|| format!("reading {}", spec_path.display()))?,
            )
            .with_context(|| format!("parsing {}", spec_path.display()))?;
            for repofile in &spec.repofiles {
                if !root.join(repofile).is_file() {
                    bail!(
                        "feature {name}: repofile {repofile} not found under {}",
                        root.display()
                    );
                }
            }
            let install = dir.join("install.sh").is_file();
            features.insert(
                name.clone(),
                Feature {
                    name,
                    spec,
                    install,
                },
            );
        }
        if !features.contains_key(BASE_FEATURE) {
            bail!("features/{BASE_FEATURE}/feature.toml is required");
        }

        let matrix_path = root.join("matrix.toml");
        let mut matrix: Matrix = toml::from_str(
            &fs::read_to_string(&matrix_path)
                .with_context(|| format!("reading {}", matrix_path.display()))?,
        )
        .with_context(|| format!("parsing {}", matrix_path.display()))?;
        if matrix.schema != 1 {
            bail!(
                "matrix.toml schema {} is not supported (want 1)",
                matrix.schema
            );
        }

        let mut expanded = Vec::new();
        for entry in matrix.image {
            let Some(map) = &entry.versions else {
                expanded.push(entry);
                continue;
            };
            if map.len() != 1 {
                bail!("image {}: versions takes exactly one pin", entry.name);
            }
            let (pin, list) = map
                .iter()
                .next()
                .with_context(|| format!("image {}: empty versions table", entry.name))?;
            if !entry.name.contains("{ver}") {
                bail!(
                    "image {}: a versioned entry's name needs {{ver}}",
                    entry.name
                );
            }
            if list.is_empty() {
                bail!("image {}: versions.{pin} must not be empty", entry.name);
            }
            for version in list {
                if version.split('.').count() < 2
                    || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
                {
                    bail!("image {}: version {version} is not x.y[.z]", entry.name);
                }
                let mut inst = entry.clone();
                inst.name = entry.name.replace("{ver}", &minor(version));
                inst.versions = None;
                inst.pin_override = Some((pin.clone(), version.clone()));
                expanded.push(inst);
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for image in &expanded {
            if !seen.insert(image.name.as_str()) {
                bail!("duplicate image name after expansion: {}", image.name);
            }
        }
        matrix.image = expanded;

        let feedstock = Feedstock { features, matrix };
        for image in &feedstock.matrix.image {
            feedstock.resolve(image)?;
            if image.arches.is_empty() {
                bail!("image {}: arches must not be empty", image.name);
            }
            for arch in &image.arches {
                if !KNOWN_ARCHES.contains(&arch.as_str()) {
                    bail!("image {}: unknown arch {arch}", image.name);
                }
            }
        }
        Ok(feedstock)
    }

    pub fn resolve<'a>(&'a self, spec: &'a ImageSpec) -> Result<ResolvedImage<'a>> {
        let mut names = if spec.base.is_none() {
            vec![BASE_FEATURE.to_string()]
        } else {
            Vec::new()
        };
        for f in &spec.features {
            if f == BASE_FEATURE && spec.base.is_none() {
                bail!(
                    "image {}: base is implicit, drop it from features",
                    spec.name
                );
            }
            if names.contains(f) {
                bail!("image {}: duplicate feature {f}", spec.name);
            }
            names.push(f.clone());
        }
        let mut features = Vec::new();
        for name in &names {
            let feature = self
                .features
                .get(name)
                .with_context(|| format!("image {}: unknown feature {name}", spec.name))?;
            features.push(feature);
        }
        features.sort_by_key(|f| (f.spec.layer, f.name.as_str()));

        let mut predicates: BTreeMap<&str, (&str, &CapabilityValue)> = BTreeMap::new();
        let mut pins: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        for feature in &features {
            for (predicate, value) in &feature.spec.capabilities {
                if let CapabilityValue::FromPin { pin } = value
                    && !feature.spec.pins.contains_key(pin)
                {
                    bail!(
                        "feature {}: capability {predicate} derives from pin {pin}, which the feature does not declare",
                        feature.name
                    );
                }
                if let Some((other, held)) = predicates.get(predicate.as_str())
                    && *held != value
                {
                    bail!(
                        "image {}: features {other} and {} both grant {predicate} with different versions",
                        spec.name,
                        feature.name
                    );
                }
                predicates.insert(predicate, (&feature.name, value));
            }
            for (pin, version) in &feature.spec.pins {
                if let Some((other, held)) = pins.get(pin.as_str())
                    && *held != version
                {
                    bail!(
                        "image {}: features {other} and {} both pin {pin} with different versions",
                        spec.name,
                        feature.name
                    );
                }
                pins.insert(pin, (&feature.name, version));
            }
        }
        if let Some((pin, _)) = &spec.pin_override
            && !pins.contains_key(pin.as_str())
        {
            bail!(
                "image {}: versions pin {pin} is not pinned by any of its features",
                spec.name
            );
        }
        if spec.base.is_some() {
            for feature in &features {
                if !feature.spec.rpms.is_empty() || !feature.spec.repofiles.is_empty() {
                    bail!(
                        "image {}: feature {} needs dnf, which a custom base does not promise",
                        spec.name,
                        feature.name
                    );
                }
            }
        }
        for predicate in spec.provides.keys() {
            if predicates.contains_key(predicate.as_str()) {
                bail!(
                    "image {}: provides.{predicate} collides with a feature-granted predicate",
                    spec.name
                );
            }
        }

        Ok(ResolvedImage { spec, features })
    }
}

impl<'a> ResolvedImage<'a> {
    /// The version a pin builds with: the matrix override when it names this pin,
    /// else the feature's declared default.
    pub fn effective_pin(&self, pin: &str, default: &'a str) -> &'a str {
        match &self.spec.pin_override {
            Some((p, v)) if p == pin => v,
            _ => default,
        }
    }

    /// The image's agent orientation document: a header plus each feature's
    /// intro in layer order, with `{pin.<name>}` filled from effective pins.
    pub fn intro(&self) -> Result<String> {
        let mut out = format!(
            "# Sandbox image `{}`\n\nFeatures: {}. Machine-readable capabilities live in the OCI label\n`{}`; this file orients you, the agent, to what is already here.\n",
            self.spec.name,
            self.features
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            crucible_capability::CAPABILITIES_LABEL,
        );
        for feature in &self.features {
            let Some(intro) = &feature.spec.intro else {
                continue;
            };
            let mut body = intro.trim_end().to_string();
            for (pin, default) in &feature.spec.pins {
                body = body.replace(&format!("{{pin.{pin}}}"), self.effective_pin(pin, default));
            }
            if let Some(pos) = body.find("{pin.") {
                bail!(
                    "feature {}: intro references an unknown pin near `{}`",
                    feature.name,
                    &body[pos..body.len().min(pos + 30)]
                );
            }
            out.push_str(&format!("\n## {}\n\n{body}\n", feature.name));
        }
        Ok(out)
    }

    pub fn predicates(&self) -> Result<BTreeMap<&'a str, String>> {
        let mut out = BTreeMap::new();
        for feature in &self.features {
            for (predicate, value) in &feature.spec.capabilities {
                let resolved = match value {
                    CapabilityValue::Literal(s) => s.clone(),
                    CapabilityValue::FromPin { pin } => {
                        let default = feature.spec.pins.get(pin).with_context(|| {
                            format!("feature {}: pin {pin} missing", feature.name)
                        })?;
                        // Full precision: requirements pick their own precision as
                        // semver ranges, so truncating here would only lose signal.
                        self.effective_pin(pin, default).to_string()
                    }
                };
                out.insert(predicate.as_str(), resolved);
            }
        }
        for (predicate, value) in &self.spec.provides {
            out.insert(predicate.as_str(), value.clone());
        }
        Ok(out)
    }
}

pub fn generated_dir(root: &Path) -> PathBuf {
    root.join("generated")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::model::{Feedstock, minor};

    fn write_feature(dir: &std::path::Path, name: &str, body: &str) {
        let d = dir.join("features").join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("feature.toml"), body).unwrap();
    }

    fn vllm_feedstock(dir: &std::path::Path, matrix: &str) -> anyhow::Result<Feedstock> {
        write_feature(
            dir,
            "base",
            "summary = \"s\"\nlayer = 0\nrpms = [\"git\"]\n",
        );
        write_feature(
            dir,
            "vllm-dev",
            "summary = \"v\"\nlayer = 10\n\n[pins]\nvllm = \"0.28.0\"\n\n[capabilities]\n\"domain.vllm-dev\" = { pin = \"vllm\" }\n",
        );
        fs::write(dir.join("matrix.toml"), matrix).unwrap();
        Feedstock::load(dir)
    }

    const WINDOW: &str = concat!(
        "schema = 1\n\n",
        "[[image]]\nname = \"sandbox-vllm-{ver}\"\nfeatures = [\"vllm-dev\"]\nheavy = true\n",
        "[image.versions]\nvllm = [\"0.28.0\", \"0.27.1\"]\n\n",
        "[[image]]\nname = \"omnibus\"\nfeatures = [\"vllm-dev\"]\n",
    );

    #[test]
    fn versions_expand_with_pin_overrides_and_derived_predicates() {
        let dir = tempfile::tempdir().unwrap();
        let fs_ = vllm_feedstock(dir.path(), WINDOW).unwrap();
        let names: Vec<&str> = fs_.matrix.image.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["sandbox-vllm-0.28", "sandbox-vllm-0.27", "omnibus"]);
        let older = fs_
            .matrix
            .image
            .iter()
            .find(|i| i.name == "sandbox-vllm-0.27")
            .unwrap();
        assert_eq!(
            older.pin_override,
            Some(("vllm".to_string(), "0.27.1".to_string()))
        );
        let resolved = fs_.resolve(older).unwrap();
        assert_eq!(resolved.effective_pin("vllm", "0.28.0"), "0.27.1");
        assert_eq!(resolved.predicates().unwrap()["domain.vllm-dev"], "0.27.1");
        // The unversioned entry keeps the feature default.
        let omnibus = fs_
            .matrix
            .image
            .iter()
            .find(|i| i.name == "omnibus")
            .unwrap();
        let resolved = fs_.resolve(omnibus).unwrap();
        assert_eq!(resolved.predicates().unwrap()["domain.vllm-dev"], "0.28.0");
    }

    #[test]
    fn versioned_entries_are_validated() {
        for (matrix, needle) in [
            (
                "schema = 1\n\n[[image]]\nname = \"no-template\"\nfeatures = [\"vllm-dev\"]\n[image.versions]\nvllm = [\"0.28.0\"]\n",
                "{ver}",
            ),
            (
                "schema = 1\n\n[[image]]\nname = \"x-{ver}\"\nfeatures = [\"vllm-dev\"]\n[image.versions]\nnope = [\"0.28.0\"]\n",
                "not pinned",
            ),
            (
                "schema = 1\n\n[[image]]\nname = \"x-{ver}\"\nfeatures = [\"vllm-dev\"]\n[image.versions]\nvllm = [\"0.28.0\", \"0.28.1\"]\n",
                "duplicate image name",
            ),
            (
                "schema = 1\n\n[[image]]\nname = \"x-{ver}\"\nfeatures = [\"vllm-dev\"]\n[image.versions]\nvllm = [\"latest\"]\n",
                "not x.y",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let err = vllm_feedstock(dir.path(), matrix).unwrap_err().to_string();
            assert!(err.contains(needle), "wanted {needle:?} in: {err}");
        }
    }

    #[test]
    fn from_pin_capability_requires_the_pin_on_the_same_feature() {
        let dir = tempfile::tempdir().unwrap();
        write_feature(dir.path(), "base", "summary = \"s\"\nlayer = 0\n");
        write_feature(
            dir.path(),
            "broken",
            "summary = \"b\"\nlayer = 10\n\n[capabilities]\n\"x.y\" = { pin = \"ghost\" }\n",
        );
        fs::write(
            dir.path().join("matrix.toml"),
            "schema = 1\n\n[[image]]\nname = \"img\"\nfeatures = [\"broken\"]\n",
        )
        .unwrap();
        let err = Feedstock::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn custom_base_skips_substrate_and_rejects_dnf_features() {
        let dir = tempfile::tempdir().unwrap();
        write_feature(
            dir.path(),
            "base",
            "summary = \"s\"\nlayer = 0\nrpms = [\"git\"]\n",
        );
        write_feature(
            dir.path(),
            "pipeline",
            "summary = \"p\"\nlayer = 40\n\n[capabilities]\n\"pipeline.x\" = \"1\"\n",
        );
        fs::write(
            dir.path().join("matrix.toml"),
            concat!(
                "schema = 1\n\n",
                "[[image]]\nname = \"custom\"\nbase = \"docker.io/x/y@sha256:abc\"\n",
                "features = [\"pipeline\"]\n\n[image.provides]\n\"ml.torch\" = \"2.11\"\n",
            ),
        )
        .unwrap();
        let fs_ = Feedstock::load(dir.path()).unwrap();
        let spec = &fs_.matrix.image[0];
        let resolved = fs_.resolve(spec).unwrap();
        // The dnf substrate feature is not composed onto a custom base.
        assert!(resolved.features.iter().all(|f| f.name != "base"));
        let predicates = resolved.predicates().unwrap();
        assert_eq!(predicates["ml.torch"], "2.11");
        assert_eq!(predicates["pipeline.x"], "1");

        // A dnf-needing feature on a custom base is refused.
        fs::write(
            dir.path().join("matrix.toml"),
            "schema = 1\n\n[[image]]\nname = \"bad\"\nbase = \"docker.io/x/y@sha256:abc\"\nfeatures = [\"base\"]\n",
        )
        .unwrap();
        let err = Feedstock::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("needs dnf"), "{err}");
    }

    #[test]
    fn minor_truncates_to_two_components() {
        assert_eq!(minor("0.28.0"), "0.28");
        assert_eq!(minor("1.25"), "1.25");
    }
}

#[cfg(test)]
pub mod testutil {
    use std::fs;
    use std::path::Path;

    use crate::model::Feedstock;

    /// A feedstock with base/go/claude-code features and the given matrix.toml body.
    pub fn feedstock(dir: &Path, matrix: &str) -> Feedstock {
        let features = dir.join("features");
        for (name, spec, install) in [
            (
                "base",
                "summary = \"substrate\"\nlayer = 0\nrpms = [\"git\", \"jq\"]\n\n[capabilities]\n\"vcs.git\" = \"2\"\n",
                false,
            ),
            (
                "go",
                "summary = \"go\"\nlayer = 20\n\n[pins]\ngo = \"1.25.11\"\n\n[capabilities]\n\"toolchain.go\" = \"1.25\"\n\n[env]\nGOTOOLCHAIN = \"local\"\n",
                true,
            ),
            (
                "claude-code",
                "summary = \"cc\"\nlayer = 90\n\n[pins]\nclaude-code = \"2.0.1\"\n\n[capabilities]\n\"agent.claude-code\" = \"2\"\n",
                true,
            ),
        ] {
            let d = features.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("feature.toml"), spec).unwrap();
            if install {
                fs::write(d.join("install.sh"), "true\n").unwrap();
            }
        }
        fs::write(dir.join("matrix.toml"), matrix).unwrap();
        Feedstock::load(dir).unwrap()
    }
}
