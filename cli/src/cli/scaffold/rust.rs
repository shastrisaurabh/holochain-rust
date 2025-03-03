use crate::{
    cli::{package, scaffold::Scaffold},
    config_files::Build,
    error::DefaultResult,
    util,
};
use colored::*;
use holochain_common::env_vars::EnvVar;
use holochain_core_types::hdk_version::HDK_VERSION;
use holochain_wasm_utils::wasm_target_dir;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use toml::{self, value::Value};
pub const CARGO_FILE_NAME: &str = "Cargo.toml";
pub const LIB_RS_PATH: &str = "src/lib.rs";

pub enum HdkMacroStyle {
    Declarative,
    Procedural,
}

pub struct RustScaffold {
    build_template: Build,
    package_name: String,
    macro_style: HdkMacroStyle,
}

/**
 * @brief      Creates a Cargo.toml string from a Cargo.template.toml template
 * Takes a name and pulls the authors and edition out of the existing toml file.
 * The Cargo.template.toml should contain tags <<NAME>>, <<AUTHORS>>, <<EDITION>>, <<VERSION>>
 *
 * @param      name      The name for the zome/lib
 * @param      contents  Existing Cargo.toml as a string with authors and edition fields filled
 * @param      template  The Cargo.template.toml, looks like a Cargo.toml with tags in place of some fields
 *
 * @return     { Returns filled template Cargo.toml as a string if successful }
 */
fn generate_cargo_toml(name: &str, contents: &str, template: &str) -> DefaultResult<String> {
    let config: Value = toml::from_str(contents)?;

    let authors_default = Value::from("[\"TODO\"]");
    let edition_default = Value::from("\"TODO\"");

    let maybe_version = EnvVar::ScaffoldVersion.value().ok();
    let version_default =
        maybe_version.unwrap_or(vec!["tag = \"v", &*HDK_VERSION.to_string(), "\""].join(""));
    let maybe_package = config.get("package");

    let name = Value::from(name);
    let authors = maybe_package
        .and_then(|p| p.get("authors"))
        .unwrap_or(&authors_default);
    let edition = maybe_package
        .and_then(|p| p.get("edition"))
        .unwrap_or(&edition_default);

    interpolate_cargo_template(template, &name, authors, edition, version_default)
}

/// Use the Cargo.toml.template file and interpolate values into the placeholders
/// TODO: consider using an actual templating engine such as https://github.com/Keats/tera
fn interpolate_cargo_template(
    template: &str,
    name: &Value,
    authors: &Value,
    edition: &Value,
    version: String,
) -> DefaultResult<String> {
    Ok(template
        .replace("<<NAME>>", toml::to_string(name)?.as_str())
        .replace("<<AUTHORS>>", toml::to_string(authors)?.as_str())
        .replace("<<EDITION>>", toml::to_string(edition)?.as_str())
        .replace("<<VERSION>>", &version))
}

impl RustScaffold {
    pub fn new(package_name: &str, macro_style: HdkMacroStyle) -> RustScaffold {
        let target_dir = wasm_target_dir(&package_name.into(), &String::new().into());
        let mut artifact_name = target_dir.clone();
        let artifact_path_component: PathBuf = [
            String::from("wasm32-unknown-unknown"),
            String::from("release"),
            format!("{}.wasm", &package_name),
        ]
        .iter()
        .collect();
        artifact_name.push(artifact_path_component);

        let target_dir_flag = &match target_dir.to_str() {
            Some(dir) => format!("--target-dir={}", dir),
            None => String::new(),
        };

        RustScaffold {
            build_template: Build::with_artifact(artifact_name).cmd(
                "cargo",
                &[
                    "build",
                    "--release",
                    "--target=wasm32-unknown-unknown",
                    target_dir_flag,
                ],
            ),
            package_name: package_name.to_string(),
            macro_style,
        }
    }

    /// Modify Cargo.toml in place, using pieces of the original
    fn rewrite_cargo_toml(&self, base_path: &Path) -> DefaultResult<()> {
        let cargo_file_path = base_path.join(CARGO_FILE_NAME);
        let mut cargo_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(cargo_file_path)?;
        let mut contents = String::new();
        cargo_file.read_to_string(&mut contents)?;

        let template = match self.macro_style {
            HdkMacroStyle::Declarative => include_str!("rust/Cargo.template.toml"),
            HdkMacroStyle::Procedural => include_str!("rust-proc-macro/Cargo.template.toml"),
        };

        // create new Cargo.toml using pieces of the original
        let new_toml =
            generate_cargo_toml(self.package_name.as_str(), contents.as_str(), template)?;
        cargo_file.seek(SeekFrom::Start(0))?;
        cargo_file.write_all(new_toml.as_bytes())?;
        Ok(())
    }

    /// Completely rewrite src/lib.rs with custom scaffold file
    fn rewrite_lib_rs(&self, base_path: &Path) -> DefaultResult<()> {
        let file_path = base_path.join(LIB_RS_PATH);
        let mut cargo_file = OpenOptions::new()
            .truncate(true)
            .write(true)
            .open(file_path)?;
        let contents = match self.macro_style {
            HdkMacroStyle::Declarative => include_str!("rust/lib.rs"),
            HdkMacroStyle::Procedural => include_str!("rust-proc-macro/lib.rs"),
        };
        cargo_file.write_all(contents.as_bytes())?;
        Ok(())
    }
}

impl Scaffold for RustScaffold {
    fn gen<P: AsRef<Path>>(&self, base_path: P) -> DefaultResult<()> {
        // First, check whether they have `cargo` installed
        let should_continue = util::check_for_cargo(
            "Generating a Rust based Zome depends on having Rust installed.",
            None,
        )?;
        if !should_continue {
            // early exit, but user will have received feedback within check_for_cargo about why
            return Ok(());
        }

        fs::create_dir_all(&base_path)?;

        // use cargo to initialise a library Rust crate without any version control
        util::run_cmd(
            base_path.as_ref().to_path_buf(),
            "cargo".into(),
            &["init", "--lib", "--vcs", "none"],
        )?;

        // immediately rewrite the generated Cargo file, using some values
        // and throwing away the rest
        self.rewrite_cargo_toml(base_path.as_ref())?;

        // and clobber the autogenerated lib.rs with our own boilerplate
        self.rewrite_lib_rs(base_path.as_ref())?;

        // create and fill in a build file appropriate for Rust
        let build_file_path = base_path.as_ref().join(package::BUILD_CONFIG_FILE_NAME);
        self.build_template.save_as(build_file_path)?;

        // CLI feedback
        println!(
            "{} {:?} Zome",
            "Generated".green().bold(),
            self.package_name
        );

        Ok(())
    }
}
