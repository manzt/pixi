use std::path::{Path, PathBuf};

use miette::Diagnostic;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::{
    TomlError, Warning, WorkspaceManifest, pyproject::PyProjectManifest, toml::FromTomlStr,
};

/// A Python script containing a PEP 723 metadata block.
#[derive(Debug, Clone)]
pub struct ScriptManifest {
    path: PathBuf,
    metadata: String,
    prelude: String,
    postlude: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ScriptWorkspaceConfig {
    pub channels_explicit: bool,
    pub platforms_explicit: bool,
}

impl ScriptManifest {
    /// Add a PEP 723 metadata block to a new or existing Python script.
    pub fn initialize(
        path: impl AsRef<Path>,
        channels: &[String],
        platforms: &[String],
    ) -> Result<Self, ScriptManifestError> {
        let path = std::path::absolute(path)?;
        script_name(&path)?;

        let contents = match fs_err::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if ScriptBlock::parse(&contents)?.is_some() {
            return Err(ScriptManifestError::AlreadyInitialized { path });
        }

        let (bom, shebang, body) = extract_script_header(&contents)?;
        // Keep the Python requirement aligned with the new pyproject.toml template.
        let mut metadata =
            "requires-python = \">= 3.11\"\ndependencies = []\n\n[tool.pixi.workspace]\n"
                .parse::<DocumentMut>()
                .expect("the default script metadata is valid TOML");
        metadata["tool"]["pixi"]["workspace"]["channels"] =
            Item::Value(Value::Array(string_array(channels)));
        metadata["tool"]["pixi"]["workspace"]["platforms"] =
            Item::Value(Value::Array(string_array(platforms)));

        let mut output = String::new();
        output.push_str(bom);
        if let Some(shebang) = shebang {
            output.push_str(shebang);
            output.push_str("\n#\n");
        }
        output.push_str(&serialize_metadata(&metadata.to_string()));
        if !body.is_empty() {
            output.push('\n');
            output.push_str(body);
        }

        fs_err::create_dir_all(
            path.parent()
                .expect("an absolute script path always has a parent"),
        )?;
        fs_err::write(&path, output)?;

        Ok(Self::from_path(path)?
            .expect("metadata serialized by the script initializer must be parseable"))
    }

    /// Read the PEP 723 metadata block from a script.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Option<Self>, ScriptManifestError> {
        let contents = fs_err::read(&path)?;
        let Some(block) = ScriptBlock::parse(&contents)? else {
            return Ok(None);
        };
        block.metadata.parse::<DocumentMut>()?;

        Ok(Some(Self {
            path: std::path::absolute(path)?,
            metadata: block.metadata,
            prelude: block.prelude,
            postlude: block.postlude,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &str {
        &self.metadata
    }

    pub fn metadata_document(&self) -> Result<DocumentMut, ScriptManifestError> {
        Ok(self.metadata.parse()?)
    }

    /// Present the script metadata as a pyproject document for Pixi's manifest editors.
    pub fn pyproject_document(&self) -> Result<DocumentMut, ScriptManifestError> {
        inline_pyproject(self.metadata(), script_name(&self.path)?)
    }

    pub fn workspace_config(&self) -> Result<ScriptWorkspaceConfig, ScriptManifestError> {
        let metadata = self.metadata_document()?;
        let workspace = metadata
            .get("tool")
            .and_then(Item::as_table_like)
            .and_then(|tool| tool.get("pixi"))
            .and_then(Item::as_table_like)
            .and_then(|pixi| pixi.get("workspace").or_else(|| pixi.get("project")))
            .and_then(Item::as_table_like);

        Ok(ScriptWorkspaceConfig {
            channels_explicit: workspace.is_some_and(|table| table.contains_key("channels")),
            platforms_explicit: workspace.is_some_and(|table| table.contains_key("platforms")),
        })
    }

    /// Parse the inline metadata using the same semantics as `pyproject.toml`.
    pub fn into_workspace_manifest(
        self,
    ) -> Result<(WorkspaceManifest, Vec<Warning>), ScriptManifestError> {
        let root_directory = self
            .path
            .parent()
            .expect("an absolute script path always has a parent");
        let pyproject = self.pyproject_document()?;
        let (workspace, package, warnings) =
            PyProjectManifest::from_toml_str(&pyproject.to_string())?
                .into_workspace_manifest(root_directory)?;

        debug_assert!(package.is_none(), "script manifests cannot define packages");
        Ok((workspace, warnings))
    }

    /// Replace the metadata block while preserving the Python around it.
    pub fn write_metadata(&self, metadata: &DocumentMut) -> Result<(), ScriptManifestError> {
        let contents = format!(
            "{}{}{}",
            self.prelude,
            serialize_metadata(&metadata.to_string()),
            self.postlude
        );
        fs_err::write(&self.path, contents)?;
        Ok(())
    }

    /// Render changes made through a synthetic pyproject document back into the script.
    pub fn render_pyproject_document(
        &self,
        pyproject: &DocumentMut,
    ) -> Result<String, ScriptManifestError> {
        let mut pyproject = pyproject.clone();
        let mut project = pyproject
            .remove("project")
            .and_then(|item| item.into_table().ok())
            .ok_or(ScriptManifestError::InvalidEditableDocument)?;
        let dependencies = project
            .remove("dependencies")
            .unwrap_or_else(|| Item::Value(Value::Array(Array::new())));

        let mut metadata = self.metadata_document()?;
        metadata["dependencies"] = dependencies;
        if let Some(requires_python) = project.remove("requires-python") {
            metadata["requires-python"] = requires_python;
        } else {
            metadata.remove("requires-python");
        }

        let pixi = pyproject
            .get_mut("tool")
            .and_then(Item::as_table_like_mut)
            .and_then(|tool| tool.remove("pixi"));
        if let Some(pixi) = pixi {
            if metadata.get("tool").is_none() {
                metadata["tool"] = Item::Table(Table::new());
            }
            metadata
                .get_mut("tool")
                .and_then(Item::as_table_like_mut)
                .expect("the tool table was just created or parsed")
                .insert("pixi", pixi);
        }

        Ok(format!(
            "{}{}{}",
            self.prelude,
            serialize_metadata(&metadata.to_string()),
            self.postlude
        ))
    }
}

fn string_array(values: &[String]) -> Array {
    let mut array = Array::new();
    array.extend(values.iter().map(String::as_str));
    array
}

fn extract_script_header(
    contents: &[u8],
) -> Result<(&str, Option<&str>, &str), ScriptManifestError> {
    let contents = std::str::from_utf8(contents)?;
    let (bom, contents) = contents
        .strip_prefix('\u{feff}')
        .map_or(("", contents), |contents| ("\u{feff}", contents));
    if !contents.starts_with("#!") {
        return Ok((bom, None, contents));
    }

    let bytes = contents.as_bytes();
    let end = bytes
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    let newline_width = match bytes.get(end..) {
        Some([b'\r', b'\n', ..]) => 2,
        Some([b'\r' | b'\n', ..]) => 1,
        _ => 0,
    };

    Ok((
        bom,
        Some(&contents[..end]),
        &contents[end + newline_width..],
    ))
}

fn script_name(path: &Path) -> Result<&str, ScriptManifestError> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ScriptManifestError::InvalidFilename {
            path: path.to_path_buf(),
        })
}

fn inline_pyproject(
    metadata: &str,
    project_name: &str,
) -> Result<DocumentMut, ScriptManifestError> {
    let mut metadata = metadata.parse::<DocumentMut>()?;
    validate_subset(&metadata)?;

    let dependencies = metadata
        .remove("dependencies")
        .unwrap_or_else(|| Item::Value(Value::Array(Array::new())));
    let requires_python = metadata.remove("requires-python");
    let pixi = metadata
        .remove("tool")
        .map(|tool| {
            tool.into_table()
                .map_err(|_| ScriptManifestError::InvalidToolTable)
        })
        .transpose()?
        .and_then(|mut tool| tool.remove("pixi"));

    let mut pyproject = DocumentMut::new();
    pyproject["project"]["name"] = value(project_name);
    pyproject["project"]["dependencies"] = dependencies;
    if let Some(requires_python) = requires_python {
        pyproject["project"]["requires-python"] = requires_python;
    }
    if let Some(pixi) = pixi {
        pyproject["tool"] = Item::Table(Table::new());
        pyproject["tool"]
            .as_table_mut()
            .expect("the tool table was just created")
            .insert("pixi", pixi);
    }

    ensure_pixi_workspace(&mut pyproject)?;
    Ok(pyproject)
}

fn ensure_pixi_workspace(pyproject: &mut DocumentMut) -> Result<(), ScriptManifestError> {
    if pyproject.get("tool").is_none() {
        pyproject["tool"] = Item::Table(Table::new());
    }
    if pyproject["tool"].get("pixi").is_none() {
        pyproject["tool"]["pixi"] = Item::Table(Table::new());
    }
    let workspace_key = if pyproject["tool"]["pixi"].get("workspace").is_some() {
        "workspace"
    } else if pyproject["tool"]["pixi"].get("project").is_some() {
        "project"
    } else {
        pyproject["tool"]["pixi"]["workspace"] = Item::Table(Table::new());
        "workspace"
    };
    if !pyproject["tool"]["pixi"][workspace_key].is_table() {
        return Err(ScriptManifestError::InvalidPixiWorkspace);
    }
    let workspace = pyproject["tool"]["pixi"][workspace_key]
        .as_table_mut()
        .expect("workspace was checked to be a table");
    for key in ["channels", "platforms"] {
        if !workspace.contains_key(key) {
            workspace.insert(key, Item::Value(Value::Array(Array::new())));
        }
    }
    Ok(())
}

fn validate_subset(metadata: &DocumentMut) -> Result<(), ScriptManifestError> {
    let unsupported_root = metadata
        .as_table()
        .iter()
        .map(|(key, _)| key)
        .filter(|key| !matches!(*key, "dependencies" | "requires-python" | "tool"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !unsupported_root.is_empty() {
        return Err(ScriptManifestError::UnsupportedFields(unsupported_root));
    }

    let Some(pixi) = metadata
        .get("tool")
        .and_then(Item::as_table_like)
        .and_then(|tool| tool.get("pixi"))
        .and_then(Item::as_table_like)
    else {
        return Ok(());
    };

    let mut unsupported = Vec::new();
    for key in ["environments", "feature"] {
        if pixi.contains_key(key) {
            unsupported.push(format!("tool.pixi.{key}"));
        }
    }
    for key in [
        "build-backend",
        "build-dependencies",
        "dev-dependencies",
        "host-dependencies",
        "package",
        "run-dependencies",
        "tasks",
    ] {
        collect_nested_key(pixi, key, "tool.pixi", &mut unsupported);
    }
    if pixi
        .get("workspace")
        .and_then(Item::as_table_like)
        .is_some_and(|workspace| workspace.contains_key("dependencies"))
    {
        unsupported.push("tool.pixi.workspace.dependencies".to_owned());
    }

    if unsupported.is_empty() {
        Ok(())
    } else {
        unsupported.sort();
        unsupported.dedup();
        Err(ScriptManifestError::UnsupportedFields(unsupported))
    }
}

fn collect_nested_key(
    table: &dyn toml_edit::TableLike,
    needle: &str,
    prefix: &str,
    found: &mut Vec<String>,
) {
    for (key, item) in table.iter() {
        let path = format!("{prefix}.{key}");
        if key == needle {
            found.push(path.clone());
        }
        if let Some(table) = item.as_table_like() {
            collect_nested_key(table, needle, &path, found);
        }
    }
}

#[derive(Debug, Error, Diagnostic)]
pub enum ScriptManifestError {
    #[error(transparent)]
    TomlEdit(#[from] toml_edit::TomlError),

    #[error(transparent)]
    Toml(#[from] TomlError),

    #[error("the script filename cannot be used as a project name: {}", path.display())]
    InvalidFilename { path: PathBuf },

    #[error("{} is already a PEP 723 script", path.display())]
    AlreadyInitialized { path: PathBuf },

    #[error("`tool.pixi.workspace` must be a table")]
    InvalidPixiWorkspace,

    #[error("`tool` must be a table")]
    InvalidToolTable,

    #[error("the editable script document is missing its project table")]
    InvalidEditableDocument,

    #[error("PEP 723 scripts do not support: {}", .0.join(", "))]
    #[diagnostic(help("A script represents one implicit default environment."))]
    UnsupportedFields(Vec<String>),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("the PEP 723 metadata block is not valid UTF-8")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("the opening `# /// script` marker has no closing `# ///` marker")]
    UnclosedBlock,

    #[error("the script contains multiple PEP 723 metadata blocks")]
    DuplicateBlock,
}

// Keep this envelope parser aligned with uv's `uv-scripts` implementation. The
// TOML model above remains Pixi-owned so script and pyproject semantics cannot drift.
struct ScriptBlock {
    prelude: String,
    metadata: String,
    postlude: String,
}

impl ScriptBlock {
    fn parse(contents: &[u8]) -> Result<Option<Self>, ScriptManifestError> {
        const OPENING: &[u8] = b"# /// script";
        const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
        let Some(index) = contents
            .windows(OPENING.len())
            .position(|window| window == OPENING)
        else {
            return Ok(None);
        };
        let follows_bom = index == UTF8_BOM.len() && contents.starts_with(UTF8_BOM);
        if index != 0 && !follows_bom && !matches!(contents[index - 1], b'\r' | b'\n') {
            return Ok(None);
        }

        let prelude = std::str::from_utf8(&contents[..index])?;
        let contents = std::str::from_utf8(&contents[index..])?;
        let mut lines = contents.split_inclusive('\n');
        let Some(opening) = lines.next() else {
            return Ok(None);
        };
        if without_line_ending(opening) != "# /// script" {
            return Ok(None);
        }

        let mut toml = Vec::new();
        let mut offset = opening.len();
        let mut line_end_offsets = Vec::new();
        for raw_line in lines {
            let line = without_line_ending(raw_line);
            let Some(line) = line.strip_prefix('#') else {
                break;
            };
            if line.is_empty() {
                toml.push("");
            } else if let Some(line) = line.strip_prefix(' ') {
                toml.push(line);
            } else {
                break;
            }
            offset += raw_line.len();
            line_end_offsets.push(offset);
        }

        let Some(reverse_index) = toml.iter().rev().position(|line| *line == "///") else {
            return Err(ScriptManifestError::UnclosedBlock);
        };
        let closing_index = toml.len() - reverse_index;
        let postlude = &contents[line_end_offsets[closing_index - 1]..];
        toml.truncate(closing_index - 1);

        reject_duplicate_block(&postlude.lines().collect::<Vec<_>>())?;

        Ok(Some(Self {
            prelude: prelude.to_owned(),
            metadata: toml.join("\n") + "\n",
            postlude: postlude.to_owned(),
        }))
    }
}

fn without_line_ending(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn reject_duplicate_block(lines: &[&str]) -> Result<(), ScriptManifestError> {
    for (index, line) in lines.iter().enumerate() {
        if *line != "# /// script" {
            continue;
        }
        if lines[index + 1..]
            .iter()
            .take_while(|line| {
                line.strip_prefix('#')
                    .is_some_and(|content| content.is_empty() || content.starts_with(' '))
            })
            .any(|line| *line == "# ///")
        {
            return Err(ScriptManifestError::DuplicateBlock);
        }
    }
    Ok(())
}

fn serialize_metadata(metadata: &str) -> String {
    let mut output = String::with_capacity(metadata.len() + 32);
    output.push_str("# /// script\n");
    for line in metadata.lines() {
        output.push('#');
        if !line.is_empty() {
            output.push(' ');
            output.push_str(line);
        }
        output.push('\n');
    }
    output.push_str("# ///\n");
    output
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use pixi_pypi_spec::PypiPackageName;
    use rattler_conda_types::PackageName;
    use tempfile::TempDir;

    use super::*;
    use crate::SpecType;

    fn script(source: &str) -> (TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("example.py");
        fs_err::write(&path, source).unwrap();
        (directory, path)
    }

    #[test]
    fn initializes_a_script_without_replacing_its_python() {
        let (directory, path) = script("#!/usr/bin/env python\r\nprint('hello')\r\n");

        let script = ScriptManifest::initialize(
            &path,
            &["conda-forge".to_owned()],
            &["linux-64".to_owned()],
        )
        .unwrap();

        assert_eq!(script.path(), path);
        assert_eq!(
            fs_err::read_to_string(&path).unwrap(),
            r#"#!/usr/bin/env python
#
# /// script
# requires-python = ">= 3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
# ///

print('hello')"#
                .to_owned()
                + "\r\n"
        );
        assert!(!directory.path().join("pixi.toml").exists());
    }

    #[test]
    fn initializing_preserves_a_utf8_bom_at_the_start_of_the_script() {
        let (_directory, path) = script("\u{feff}print('hello')\r\n");

        ScriptManifest::initialize(&path, &[], &["linux-64".to_owned()]).unwrap();

        let contents = fs_err::read_to_string(&path).unwrap();
        assert!(contents.starts_with("\u{feff}# /// script\n"));
        assert_eq!(contents.matches('\u{feff}').count(), 1);
        assert!(contents.ends_with("\n\nprint('hello')\r\n"));

        assert!(matches!(
            ScriptManifest::initialize(&path, &[], &[]),
            Err(ScriptManifestError::AlreadyInitialized { .. })
        ));
    }

    #[test]
    fn initializes_a_new_script_and_its_parent_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/example.py");

        ScriptManifest::initialize(&path, &[], &["osx-arm64".to_owned()]).unwrap();

        assert_eq!(
            fs_err::read_to_string(path).unwrap(),
            r#"# /// script
# requires-python = ">= 3.11"
# dependencies = []
#
# [tool.pixi.workspace]
# channels = []
# platforms = ["osx-arm64"]
# ///
"#
        );
    }

    #[test]
    fn refuses_to_initialize_an_existing_script_manifest() {
        let (_directory, path) = script("# /// script\n# dependencies = []\n# ///\n");

        assert!(matches!(
            ScriptManifest::initialize(&path, &[], &[]),
            Err(ScriptManifestError::AlreadyInitialized { .. })
        ));
    }

    #[test]
    fn parses_standard_and_pixi_dependencies_with_pyproject_semantics() {
        let (_directory, path) = script(
            r#"#!/usr/bin/env python
# /// script
# requires-python = ">=3.11"
# dependencies = ["requests>=2"]
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
#
# [tool.pixi.dependencies]
# python = "3.12.*"
# zlib = "*"
#
# [tool.some-future-runner]
# option = true
# ///
print("hello")
"#,
        );

        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        let (manifest, warnings) = script.into_workspace_manifest().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(manifest.workspace.name.as_deref(), Some("example"));
        assert_eq!(
            manifest
                .workspace
                .platforms
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["linux-64"]
        );
        assert_eq!(
            manifest
                .workspace
                .channels
                .iter()
                .map(|channel| channel.channel.to_string())
                .collect::<Vec<_>>(),
            ["conda-forge"]
        );

        let target = manifest.default_feature().targets.default();
        let python = PackageName::from_str("python").unwrap();
        assert_eq!(
            target
                .run_dependencies()
                .unwrap()
                .get_single(&python)
                .unwrap()
                .unwrap()
                .to_string(),
            "3.12.*"
        );
        assert!(target.has_dependency(
            &PackageName::from_str("zlib").unwrap(),
            SpecType::Run,
            None
        ));
        assert!(
            target
                .pypi_dependencies
                .as_ref()
                .unwrap()
                .contains_key(&PypiPackageName::from_str("requests").unwrap())
        );
    }

    #[test]
    fn resolves_relative_paths_from_the_script_directory() {
        let (directory, path) = script(
            r#"# /// script
# dependencies = ["demo @ ./demo"]
# ///
"#,
        );
        fs_err::create_dir(directory.path().join("demo")).unwrap();

        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        let (manifest, _) = script.into_workspace_manifest().unwrap();
        let dependency = manifest
            .default_feature()
            .targets
            .default()
            .pypi_dependencies
            .as_ref()
            .unwrap()
            .get_single(&PypiPackageName::from_str("demo").unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(
            dependency.source.as_path(),
            Some(&directory.path().join("demo"))
        );
    }

    #[test]
    fn an_empty_standard_script_gets_one_implicit_workspace() {
        let (_directory, path) = script(
            r#"# /// script
# dependencies = []
# ///
print("hello")
"#,
        );

        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        let (manifest, _) = script.into_workspace_manifest().unwrap();

        assert_eq!(manifest.workspace.name.as_deref(), Some("example"));
        assert_eq!(manifest.all_features().count(), 1);
        assert_eq!(manifest.environments.iter().count(), 1);
    }

    #[test]
    fn preserves_the_pyproject_project_alias() {
        let (_directory, path) = script(
            r#"# /// script
# dependencies = []
#
# [tool.pixi.project]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
# ///
"#,
        );

        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        let config = script.workspace_config().unwrap();
        assert!(config.channels_explicit);
        assert!(config.platforms_explicit);
        let (manifest, warnings) = script.into_workspace_manifest().unwrap();

        assert_eq!(manifest.workspace.channels.len(), 1);
        assert_eq!(manifest.workspace.platforms.len(), 1);
        assert_eq!(warnings.len(), 1, "the existing alias remains deprecated");
    }

    #[test]
    fn rejects_workspace_only_concepts() {
        let (_directory, path) = script(
            r#"# /// script
# dependencies = []
#
# [tool.pixi.target.linux-64.tasks]
# test = "pytest"
#
# [tool.pixi.feature.test.dependencies]
# pytest = "*"
#
# [tool.pixi.target.linux-64.host-dependencies]
# python = "*"
# ///
"#,
        );

        let error = ScriptManifest::from_path(path)
            .unwrap()
            .unwrap()
            .into_workspace_manifest()
            .unwrap_err();
        let ScriptManifestError::UnsupportedFields(fields) = error else {
            panic!("unexpected error: {error}");
        };
        assert_eq!(
            fields,
            [
                "tool.pixi.feature",
                "tool.pixi.target.linux-64.host-dependencies",
                "tool.pixi.target.linux-64.tasks"
            ]
        );
    }

    #[test]
    fn rejects_invalid_and_duplicate_blocks() {
        let (_directory, unclosed) = script(
            r#"# /// script
# dependencies = []
print("hello")
"#,
        );
        assert!(matches!(
            ScriptManifest::from_path(unclosed),
            Err(ScriptManifestError::UnclosedBlock)
        ));

        let (_directory, duplicate) = script(
            r#"# /// script
# dependencies = []
# ///
print("first")
# /// script
# dependencies = []
# ///
"#,
        );
        assert!(matches!(
            ScriptManifest::from_path(duplicate),
            Err(ScriptManifestError::DuplicateBlock)
        ));
    }

    #[test]
    fn metadata_edits_preserve_the_python_and_other_tools() {
        let (_directory, path) = script(
            r#"#!/usr/bin/env -S uv run --script
# /// script
# dependencies = ["requests"]
#
# [tool.uv]
# prerelease = "allow"
# ///

print("hello")
"#,
        );
        let script = ScriptManifest::from_path(&path).unwrap().unwrap();
        let mut metadata = script.metadata_document().unwrap();
        metadata["dependencies"]
            .as_array_mut()
            .unwrap()
            .push("rich");

        script.write_metadata(&metadata).unwrap();

        assert_eq!(
            fs_err::read_to_string(path).unwrap(),
            r#"#!/usr/bin/env -S uv run --script
# /// script
# dependencies = ["requests", "rich"]
#
# [tool.uv]
# prerelease = "allow"
# ///

print("hello")
"#
        );
    }

    #[test]
    fn metadata_edits_preserve_bom_crlf_and_missing_final_newline() {
        let (_directory, path) = script(
            "\u{feff}#!/usr/bin/env python\r\n# /// script\r\n# dependencies = []\r\n# ///\r\n\r\nprint('first')\r\nprint('last')",
        );
        let script = ScriptManifest::from_path(&path).unwrap().unwrap();
        let mut pyproject = script.pyproject_document().unwrap();
        pyproject["project"]["dependencies"]
            .as_array_mut()
            .unwrap()
            .push("requests");

        let contents = script.render_pyproject_document(&pyproject).unwrap();

        assert!(contents.starts_with("\u{feff}#!/usr/bin/env python\r\n# /// script\n"));
        assert!(contents.contains("# dependencies = [\"requests\"]\n"));
        assert!(contents.ends_with("\r\nprint('first')\r\nprint('last')"));
        assert!(!contents.ends_with('\n'));
    }

    #[test]
    fn pyproject_edits_round_trip_through_script_metadata() {
        let (_directory, path) = script(
            r#"# /// script
# requires-python = ">= 3.11"
# dependencies = ["requests"]
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
# ///
print("hello")
"#,
        );
        let script = ScriptManifest::from_path(path).unwrap().unwrap();
        let mut pyproject = script.pyproject_document().unwrap();
        pyproject["project"]["dependencies"]
            .as_array_mut()
            .unwrap()
            .push("rich");
        pyproject["tool"]["pixi"]["dependencies"] = Item::Table(Table::new());
        pyproject["tool"]["pixi"]["dependencies"]["python"] = value("*");

        assert_eq!(
            script.render_pyproject_document(&pyproject).unwrap(),
            r#"# /// script
# requires-python = ">= 3.11"
# dependencies = ["requests", "rich"]
#
# [tool.uv]
# prerelease = "allow"
#
# [tool.pixi.workspace]
# channels = ["conda-forge"]
# platforms = ["linux-64"]
#
# [tool.pixi.dependencies]
# python = "*"
# ///
print("hello")
"#
        );
    }
}
