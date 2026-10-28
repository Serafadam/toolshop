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
        --list-packages        list the package catalog and exit
        --list-distros         list predefined and local distros and exit
        --theme <FLAVOUR>      latte, frappe, macchiato or mocha
        --no-theme-config      do not read or write the saved theme

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
    pub use_theme_config: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            command: Command::Interactive,
            theme: None,
            use_theme_config: true,
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
            "--list-packages" => cli.command = Command::ListPackages,
            "--list-distros" => cli.command = Command::ListDistros,
            "--theme" => {
                let name = value("--theme")?;
                let Some(flavour) = Flavour::from_key(&name) else {
                    bail!("unknown theme {name:?} (try latte, frappe, macchiato or mocha)");
                };
                cli.theme = Some(flavour);
            }
            "--no-theme-config" => cli.use_theme_config = false,
            other => return Err(eyre!("unknown argument {other:?} (try --help)")),
        }
    }

    // Selection flags only make sense with --print.
    let has_selection = distro.is_some() || !packages.is_empty() || all || user.is_some();
    if has_selection
        && cli.command == Command::Interactive
        && !matches!(cli.command, Command::Print { .. })
    {
        bail!("--distro/--package/--all/--user only apply to --print");
    }

    if let Command::Print { .. } = cli.command {
        cli.command = Command::Print {
            distro,
            packages,
            all,
            user,
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

    if selected.is_empty() && !all {
        // Say *why* nothing was selected; a bare "no packages" hides typos and distro
        // mismatches.
        let detail = if warnings.is_empty() {
            String::new()
        } else {
            format!(": {}", warnings.join("; "))
        };
        bail!("no packages selected (use --package or --all){detail}");
    }

    let mut builder = ContainerBuilder::default();
    if let Some(user) = user {
        builder.set_docker_user(user);
    }
    builder.set_distro(distro.clone());
    builder.set_packages(selected);
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

    #[test]
    fn no_arguments_starts_the_tui() {
        let cli = parse_args(&[]);
        assert_eq!(cli.command, Command::Interactive);
        assert!(cli.use_theme_config);
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
            "adaser",
        ]);
        assert_eq!(
            cli.command,
            Command::Print {
                distro: Some("jazzy".to_string()),
                packages: vec!["htop".to_string(), "fzf".to_string(), "curl".to_string()],
                all: false,
                user: Some("adaser".to_string()),
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
        let cli = parse_args(&["--theme", "latte", "--no-theme-config"]);
        assert_eq!(cli.theme, Some(Flavour::Latte));
        assert!(!cli.use_theme_config);
        assert!(parse_args(&["--theme=frappe"]).theme == Some(Flavour::Frappe));
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
        )
        .expect_err("nothing selectable");
        assert!(format!("{err:#}").contains("unknown package"), "{err:#}");

        // `isaac-sim` is only offered for Ubuntu-based images.
        let fedora = crate::distro::Distro::predefined()
            .into_iter()
            .find(|distro| distro.family == crate::distro::DistroFamily::Fedora)
            .expect("a Fedora distro");
        let err = render_container_file(&catalog, &fedora, &["isaac-sim".to_string()], false, None)
            .expect_err("not available on Fedora");
        let message = format!("{err:#}");
        assert!(message.contains("not available on 41"), "{message}");
    }

    #[test]
    fn all_selects_every_package_the_distro_offers() {
        let catalog = Catalog::load_from_dirs(&[]).unwrap();
        let distro = crate::distro::Distro::predefined().remove(0);
        let (file, warnings) = render_container_file(&catalog, &distro, &[], true, None).unwrap();
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
