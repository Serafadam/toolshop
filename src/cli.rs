//! Command line interface.
//!
//! The TUI is the default; every mode additionally has a headless counterpart so the tool
//! can be scripted and CI can verify it without a terminal:
//!
//! ```text
//! toolshop                         # interactive TUI
//! toolshop --print -d jazzy -a      # write the Containerfile to stdout
//! toolshop --list-packages          # catalog as text
//! toolshop --list-distros           # predefined + local images
//! ```

use std::collections::{BTreeMap, BTreeSet};

use color_eyre::{
    Result,
    eyre::{WrapErr, bail, eyre},
};

use crate::{
    catalog::{self, Catalog, Package},
    container_builder::ContainerBuilder,
    distro::{self, Distro},
    theme::Flavour,
};

pub const USAGE: &str = "\
toolshop — build ROS/C++ toolboxes from a TUI, or from the command line

USAGE:
    toolshop [OPTIONS]

OPTIONS:
    -h, --help                 print this help
    -V, --version              print the version
        --print                print the generated Containerfile and exit
    -d, --distro <NAME>        distro for --print (name or image, default: first predefined)
    -p, --package <NAME>       package to install; repeatable, comma separated allowed
    -a, --all                  with --print: select every package the distro offers
    -u, --user <NAME>          user the image is prepared for (default: $USER)
        --package-version <PKG=VERSION>
                               pin a package version, e.g. neovim=0.11.2
        --env <NAME=VALUE>     extra ENV line in the generated Containerfile; repeatable
        --hide-username        record a placeholder user instead of $USER
                               (for Containerfiles you commit or share)
        --list-packages        list the package catalog and exit
        --list-distros         list predefined and local distros and exit
        --theme <FLAVOUR>      latte, frappe, macchiato or mocha
        --no-config            do not read or write the saved theme and configuration
                               (--no-theme-config is accepted as the old name)

EXAMPLES:
    toolshop --print -d jazzy -p htop -p fzf
    toolshop --print -d ros:kilted-ros-base -a > Containerfile
    toolshop --list-packages | grep -i clang
";

/// What the process should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start the TUI.
    Interactive,
    /// Write a Containerfile for a selection to stdout.
    Print {
        distro: Option<String>,
        packages: Vec<String>,
        all: bool,
        user: Option<String>,
        hide_username: bool,
        versions: BTreeMap<String, String>,
        env: BTreeMap<String, String>,
    },
    ListPackages,
    ListDistros,
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub command: Command,
    pub theme: Option<Flavour>,
    pub use_config: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            command: Command::Interactive,
            theme: None,
            use_config: true,
        }
    }
}

/// Parse command line arguments (without the program name).
pub fn parse<I, S>(args: I) -> Result<Cli>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut cli = Cli::default();

    let mut distro = None;
    let mut packages: Vec<String> = Vec::new();
    let mut all = false;
    let mut user = None;
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut hide_username = false;

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        index += 1;

        // `--option=value` is accepted as well as `--option value`.
        let (flag, inline_value) = match argument.split_once('=') {
            Some((flag, value)) if flag.starts_with('-') => (flag, Some(value.to_string())),
            _ => (argument, None),
        };

        let mut value = |name: &str| -> Result<String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            let Some(value) = args.get(index) else {
                bail!("{name} needs a value");
            };
            index += 1;
            Ok(value.clone())
        };

        match flag {
            "-h" | "--help" => cli.command = Command::Help,
            "-V" | "--version" => cli.command = Command::Version,
            "--print" => {
                if cli.command == Command::Interactive {
                    cli.command = Command::Print {
                        distro: None,
                        packages: Vec::new(),
                        all: false,
                        user: None,
                        hide_username: false,
                        versions: BTreeMap::new(),
                        env: BTreeMap::new(),
                    };
                }
            }
            "-d" | "--distro" => distro = Some(value("--distro")?),
            "-p" | "--package" | "--packages" => {
                for name in value("--package")?.split(',') {
                    let name = name.trim();
                    if !name.is_empty() {
                        packages.push(name.to_string());
                    }
                }
            }
            "-a" | "--all" => all = true,
            "-u" | "--user" => user = Some(value("--user")?),
            "--env" => {
                let pair = value("--env")?;
                let Some((key, val)) = pair.split_once('=') else {
                    bail!("--env needs NAME=VALUE, got {pair:?}");
                };
                let (key, val) = (key.trim(), val.trim());
                if let Err(err) = catalog::validate_env(key, val) {
                    bail!("--env: {err}");
                }
                env.insert(key.to_string(), val.to_string());
            }
            "--package-version" => {
                let pair = value("--package-version")?;
                let Some((name, version)) = pair.split_once('=') else {
                    bail!("--package-version needs NAME=VERSION, got {pair:?}");
                };
                let (name, version) = (name.trim(), version.trim());
                if name.is_empty() || version.is_empty() {
                    bail!("--package-version needs NAME=VERSION, got {pair:?}");
                }
                versions.insert(name.to_string(), version.to_string());
            }
            "--list-packages" => cli.command = Command::ListPackages,
            "--list-distros" => cli.command = Command::ListDistros,
            "--theme" => {
                let name = value("--theme")?;
                let Some(flavour) = Flavour::from_key(&name) else {
                    bail!("unknown theme {name:?} (try latte, frappe, macchiato or mocha)");
                };
                cli.theme = Some(flavour);
            }
            "--hide-username" => hide_username = true,
            "--no-config" | "--no-theme-config" => cli.use_config = false,
            other => return Err(eyre!("unknown argument {other:?} (try --help)")),
        }
    }

    // Selection flags only make sense with --print.
    let has_selection = distro.is_some()
        || !packages.is_empty()
        || all
        || user.is_some()
        || hide_username
        || !versions.is_empty()
        || !env.is_empty();
    if has_selection
        && cli.command == Command::Interactive
        && !matches!(cli.command, Command::Print { .. })
    {
        bail!("--distro/--package/--all/--user/--env only apply to --print");
    }

    if let Command::Print { .. } = cli.command {
        cli.command = Command::Print {
            distro,
            packages,
            all,
            user,
            hide_username,
            versions,
            env,
        };
    }

    Ok(cli)
}

/// Render a Containerfile for a CLI selection.
///
/// Returns the file plus the warnings gathered on the way (unknown packages, …).
pub fn render_container_file(
    catalog: &Catalog,
    distro: &Distro,
    package_names: &[String],
    all: bool,
    user: Option<&str>,
    versions: &BTreeMap<String, String>,
    options: crate::container_builder::BuildOptions,
) -> Result<(String, Vec<String>)> {
    let mut warnings = Vec::new();

    let selected: Vec<Package> = if all {
        catalog
            .available_on(Some(distro))
            .into_iter()
            .cloned()
            .collect()
    } else {
        let mut selected = Vec::new();
        for name in package_names {
            match catalog.get(name) {
                Some(package) => {
                    if package.is_available_on(distro) {
                        selected.push(package.clone());
                    } else {
                        warnings.push(format!("{name} is not available on {}", distro.name));
                    }
                }
                None => warnings.push(format!("unknown package {name:?}")),
            }
        }
        selected
    };

    for (name, version) in versions {
        match catalog.get(name) {
            Some(package) if catalog::accepts_version(package) => {
                if !selected.iter().any(|chosen| &chosen.name == name) {
                    warnings.push(format!(
                        "{name} is not selected, so its version {version} has no effect"
                    ));
                }
            }
            Some(_) => warnings.push(format!("{name} has no selectable version")),
            None => warnings.push(format!("unknown package {name:?}")),
        }
    }

    // No packages at all is legitimate: it yields the bare Containerfile for that distro.
    if selected.is_empty() && !all && !package_names.is_empty() {
        // Say *why* nothing was selected; a bare "no packages" hides typos and distro
        // mismatches.
        let detail = if warnings.is_empty() {
            String::new()
        } else {
            format!(": {}", warnings.join("; "))
        };
        bail!("no packages selected (use --package or --all){detail}");
    }

    // Requirements are pulled in like the TUI does, otherwise `--print -p isaac-sim` would
    // emit a script that calls `uv` without installing it first.
    let mut names: BTreeSet<String> = selected
        .iter()
        .map(|package| package.name.clone())
        .collect();
    let mut queue: Vec<String> = selected
        .iter()
        .flat_map(|package| package.requires.clone())
        .collect();
    while let Some(name) = queue.pop() {
        if names.contains(&name) {
            continue;
        }
        match catalog.get(&name) {
            Some(requirement) if requirement.is_available_on(distro) => {
                names.insert(name.clone());
                queue.extend(requirement.requires.clone());
            }
            Some(_) => warnings.push(format!(
                "{name} (required by the selection) is not available on {}",
                distro.name
            )),
            None => warnings.push(format!(
                "unknown package {name:?} (required by the selection)"
            )),
        }
    }

    let selected: Vec<Package> = catalog
        .packages
        .iter()
        .filter(|package| names.contains(&package.name))
        .cloned()
        .collect();

    let mut builder = ContainerBuilder::default();
    if let Some(user) = user {
        builder.set_docker_user(user);
    }
    builder.set_options(options);
    builder.set_distro(distro.clone());
    builder.set_packages(selected);
    builder.set_versions(versions.clone());
    builder.create_container_file();

    Ok((builder.get_container_file().to_string(), warnings))
}

/// Pick the distro a `--print` run should use.
pub fn pick_distro(distros: &[Distro], wanted: Option<&str>) -> Result<Distro> {
    let Some(wanted) = wanted else {
        return distros
            .first()
            .cloned()
            .ok_or_else(|| eyre!("no distros available"));
    };

    distros
        .iter()
        .find(|distro| {
            distro.name.eq_ignore_ascii_case(wanted)
                || distro.image.eq_ignore_ascii_case(wanted)
                || crate::engine::normalize_image(&distro.image).eq_ignore_ascii_case(wanted)
        })
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = distros.iter().map(|distro| distro.name.as_str()).collect();
            eyre!("unknown distro {wanted:?} (known: {})", known.join(", "))
        })
}

/// `--list-packages` output.
pub fn list_packages(catalog: &Catalog) -> String {
    let mut out = String::new();
    for package in &catalog.packages {
        let category = catalog::category_of(package);
        let availability = if package.install.contains_key(catalog::DEFAULT_INSTALL_KEY) {
            "all distros".to_string()
        } else {
            let mut names: Vec<&str> = package.install.keys().map(String::as_str).collect();
            names.sort_unstable();
            names.join(",")
        };
        out.push_str(&format!(
            "{:<24} {:<10} {:<18} {}\n",
            package.name, category, availability, package.summary
        ));
    }
    out
}

/// `--list-distros` output.
pub fn list_distros(distros: &[Distro]) -> String {
    let mut out = String::new();
    for distro in distros {
        out.push_str(&format!(
            "{:<10} {:<10} {:<40} {}\n",
            distro.name,
            distro.family.label(),
            distro.image,
            match distro.source {
                distro::DistroSource::Predefined => "predefined",
                distro::DistroSource::Local => "local",
                distro::DistroSource::Custom => "custom",
            }
        ));
    }
    out
}

/// Load the catalog the CLI works from, reporting warnings on stderr.
pub fn load_catalog() -> Result<Catalog> {
    let catalog = Catalog::load().wrap_err("cannot load the package catalog")?;
    for warning in &catalog.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Cli {
        parse(args.iter().map(|arg| arg.to_string())).expect("valid arguments")
    }

    fn parse_error(args: &[&str]) -> String {
        let err = parse(args.iter().map(|arg| arg.to_string())).expect_err("invalid arguments");
        format!("{err:#}")
    }

    #[test]
    fn no_arguments_starts_the_tui() {
        let cli = parse_args(&[]);
        assert_eq!(cli.command, Command::Interactive);
        assert!(cli.use_config);
        assert!(cli.theme.is_none());
    }

    #[test]
    fn help_and_version_are_recognised() {
        assert_eq!(parse_args(&["--help"]).command, Command::Help);
        assert_eq!(parse_args(&["-h"]).command, Command::Help);
        assert_eq!(parse_args(&["--version"]).command, Command::Version);
        assert_eq!(parse_args(&["-V"]).command, Command::Version);
    }

    #[test]
    fn print_collects_distro_packages_and_user() {
        let cli = parse_args(&[
            "--print",
            "--distro",
            "jazzy",
            "-p",
            "htop,fzf",
            "--package=curl",
            "--user",
            "dev",
        ]);
        assert_eq!(
            cli.command,
            Command::Print {
                distro: Some("jazzy".to_string()),
                packages: vec!["htop".to_string(), "fzf".to_string(), "curl".to_string()],
                all: false,
                user: Some("dev".to_string()),
                hide_username: false,
                versions: BTreeMap::new(),
                env: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn all_flag_is_carried_through() {
        let cli = parse_args(&["--print", "-a"]);
        assert!(matches!(cli.command, Command::Print { all: true, .. }));
    }

    #[test]
    fn theme_and_config_flags_are_parsed() {
        let cli = parse_args(&["--theme", "latte", "--no-config"]);
        assert_eq!(cli.theme, Some(Flavour::Latte));
        assert!(!cli.use_config);
        // The old flag name still works.
        assert!(!parse_args(&["--no-theme-config"]).use_config);
        assert!(parse_args(&["--theme=frappe"]).theme == Some(Flavour::Frappe));
    }

    #[test]
    fn package_versions_are_parsed_and_validated() {
        let cli = parse_args(&[
            "--print",
            "-p",
            "neovim",
            "--package-version",
            "neovim=0.11.2",
        ]);
        match cli.command {
            Command::Print { versions, .. } => {
                assert_eq!(versions.get("neovim").map(String::as_str), Some("0.11.2"));
            }
            other => panic!("expected --print, got {other:?}"),
        }

        // Repeatable, and the short syntax works too.
        let cli = parse_args(&[
            "--print",
            "--package-version=neovim=0.11.2",
            "--package-version=htop=3.3",
        ]);
        match cli.command {
            Command::Print { versions, .. } => assert_eq!(versions.len(), 2),
            other => panic!("expected --print, got {other:?}"),
        }

        assert!(
            parse(["--package-version", "neovim"]).is_err(),
            "NAME=VERSION required"
        );
        assert!(parse(["--package-version", "=1.0"]).is_err());
        assert!(parse(["--package-version", "neovim="]).is_err());
    }

    #[test]
    fn env_flags_are_parsed_validated_and_rendered() {
        let cli = parse_args(&[
            "--print",
            "-p",
            "htop",
            "--env",
            "FOO=bar",
            "--env=WITH_SPACE=hello world",
        ]);
        match &cli.command {
            Command::Print { env, .. } => {
                assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
                assert_eq!(
                    env.get("WITH_SPACE").map(String::as_str),
                    Some("hello world")
                );
            }
            other => panic!("expected --print, got {other:?}"),
        }

        // A pair without `=` and a bad name are refused at parse time.
        assert!(parse_error(&["--print", "--env", "JUSTANAME"]).contains("NAME=VALUE"));
        assert!(
            parse_error(&["--print", "--env", "1BAD=x"]).contains("not a usable variable name")
        );
        // `--env` needs `--print`, like the other selection flags.
        assert!(parse_error(&["--env", "FOO=bar"]).contains("only apply to --print"));

        let cli = parse_args(&["--print", "-p", "htop", "--env", "FOO=bar"]);
        let Command::Print { env, .. } = &cli.command else {
            panic!("expected --print");
        };
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let (file, _) = render_container_file(
            &catalog,
            &distro,
            &["htop".to_string()],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: env.to_owned(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(file.contains("ENV FOO=bar"), "{file}");
    }

    #[test]
    fn a_chosen_version_lands_in_the_container_file() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let versions = BTreeMap::from([("neovim".to_string(), "0.11.2".to_string())]);

        let (file, warnings) = render_container_file(
            &catalog,
            &distro,
            &["neovim".to_string()],
            false,
            None,
            &versions,
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(file.contains("/v0.11.2/"), "{file}");
        assert!(!file.contains("{{"), "{file}");

        // A version for a package that is not selected is reported.
        let versions = BTreeMap::from([("htop".to_string(), "3.3".to_string())]);
        let (_, warnings) = render_container_file(
            &catalog,
            &distro,
            &["neovim".to_string()],
            false,
            None,
            &versions,
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("has no selectable version")),
            "{warnings:?}"
        );
    }

    #[test]
    fn list_commands_are_recognised() {
        assert_eq!(
            parse_args(&["--list-packages"]).command,
            Command::ListPackages
        );
        assert_eq!(
            parse_args(&["--list-distros"]).command,
            Command::ListDistros
        );
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse(["--nope"]).is_err());
        assert!(parse(["--distro"]).is_err(), "missing value");
        assert!(parse(["--theme", "dracula"]).is_err());
        // Selection flags without --print are a mistake, not a silent no-op.
        assert!(parse(["--package", "htop"]).is_err());
    }

    #[test]
    fn a_container_file_can_be_rendered_headless() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);

        let (file, warnings) = render_container_file(
            &catalog,
            &distro,
            &["htop".to_string(), "fzf".to_string()],
            false,
            Some("ci"),
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(file.starts_with("FROM docker.io/library/ros:jazzy-ros-base"));
        assert!(file.contains("    htop \\"));
        assert!(file.contains("# --- install fzf ---"));
        assert!(file.contains("ARG DOCKER_USER=ci"), "{file}");
        assert!(file.contains("chown -R 1000:1000 /opt/toolbox"));

        // Unknown and unavailable packages are reported, not silently dropped.
        let err = render_container_file(
            &catalog,
            &distro,
            &["does-not-exist".to_string()],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .expect_err("nothing selectable");
        assert!(format!("{err:#}").contains("unknown package"), "{err:#}");

        // `isaac-sim` is only offered for Ubuntu-based images.
        let fedora = crate::distro::Distro::predefined()
            .into_iter()
            .find(|distro| distro.family == crate::distro::DistroFamily::Fedora)
            .expect("a Fedora distro");
        let err = render_container_file(
            &catalog,
            &fedora,
            &["isaac-sim".to_string()],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .expect_err("not available on Fedora");
        let message = format!("{err:#}");
        assert!(message.contains("not available on 41"), "{message}");
    }

    #[test]
    fn hide_username_keeps_the_account_out_of_the_file() {
        let cli = parse_args(&["--print", "-p", "htop", "--hide-username"]);
        assert!(
            matches!(
                cli.command,
                Command::Print {
                    hide_username: true,
                    ..
                }
            ),
            "{:?}",
            cli.command
        );

        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let (file, _) = render_container_file(
            &catalog,
            &distro,
            &["htop".to_string()],
            false,
            Some("real-person"),
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: true,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(file.contains("ARG DOCKER_USER=ubuntu"), "{file}");
        assert!(!file.contains("real-person"), "the account stays out");
    }

    #[test]
    fn a_distro_on_its_own_prints_a_bare_containerfile() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let (file, warnings) = render_container_file(
            &catalog,
            &distro,
            &[],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert!(
            file.starts_with("FROM docker.io/library/ros:jazzy-ros-base\n"),
            "{file}"
        );
        for absent in ["ENV PATH=", "CARGO_HOME", "NVM_DIR", "mkdir", "chown"] {
            assert!(
                !file.contains(absent),
                "{absent} should not appear:\n{file}"
            );
        }
    }

    #[test]
    fn requirements_of_the_selection_are_pulled_in() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);

        // Isaac Sim's script calls `uv`, which it declares in `requires`.
        let (file, warnings) = render_container_file(
            &catalog,
            &distro,
            &["isaac-sim".to_string()],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let uv = file.find("# --- install uv ---").expect("uv pulled in");
        let isaac = file
            .find("# --- install isaac-sim ---")
            .expect("isaac present");
        assert!(uv < isaac, "uv installs first:\n{file}");

        // A single package that requires another gets both, in dependency order.
        let (file, _) = render_container_file(
            &catalog,
            &distro,
            &["rust".to_string()],
            false,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            file.contains("    curl \\"),
            "curl pulled into the apt layer: {file}"
        );
    }

    #[test]
    fn all_selects_every_package_the_distro_offers() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let (file, warnings) = render_container_file(
            &catalog,
            &distro,
            &[],
            true,
            None,
            &BTreeMap::new(),
            crate::container_builder::BuildOptions {
                hide_username: false,
                env: BTreeMap::new(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(warnings.is_empty());
        for expected in ["cppcheck", "htop", "git", "clang-tidy-18"] {
            assert!(file.contains(expected), "{expected} missing:\\n{file}");
        }
    }

    #[test]
    fn distros_are_picked_by_name_or_image() {
        let distros = Distro::predefined();
        assert_eq!(pick_distro(&distros, Some("jazzy")).unwrap().name, "jazzy");
        assert_eq!(
            pick_distro(&distros, Some("docker.io/library/ros:kilted-ros-base"))
                .unwrap()
                .name,
            "kilted"
        );
        // Names are matched case-insensitively and without the registry prefix.
        assert_eq!(
            pick_distro(&distros, Some("ROS:kilted-ros-base"))
                .unwrap()
                .name,
            "kilted"
        );
        assert_eq!(pick_distro(&distros, None).unwrap().name, "jazzy");
        assert!(pick_distro(&distros, Some("gentoo")).is_err());
    }

    #[test]
    fn listings_are_plain_text() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let packages = list_packages(&catalog);
        assert!(
            packages.lines().count() >= 38,
            "{}",
            packages.lines().count()
        );
        assert!(packages.contains("cppcheck"));
        assert!(packages.lines().all(|line| line.contains("  ")));

        let distros = list_distros(&Distro::predefined());
        assert!(distros.contains("jazzy"));
        assert!(distros.contains("predefined"));
        assert!(distros.contains("docker.io/library/fedora:41"));
    }
}
