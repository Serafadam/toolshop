use std::{borrow::Cow, error::Error, fs::File, io::Write};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc::Sender,
    task,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default, Clone, PartialEq)]
pub enum InstallationType {
    #[default]
    PkgManager,
    Script,
}
#[derive(Debug, Default, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub pkg_name: String,
    pub installation_type: InstallationType,
    pub installation_command: String,
    pub requires: Option<Vec<Box<Package>>>,
}
impl Package {
    pub fn new(
        name: String,
        version: String,
        pkg_name: String,
        installation_type: InstallationType,
        installation_command: String,
        requires: Option<Vec<Box<Package>>>,
    ) -> Self {
        Self {
            name,
            version,
            pkg_name,
            installation_type,
            installation_command,
            requires,
        }
    }
}
impl From<Package> for Cow<'_, str> {
    fn from(package: Package) -> Self {
        Cow::Owned(package.name)
    }
}
#[derive(Debug, Clone)]
pub struct AllPackages {
    pub packages: Vec<Package>,
}

#[derive(Debug, Default, Clone)]
pub struct ContainerBuilder {
    pub packages: Vec<Package>,
    distro: String,
    container_file: String,
}
impl ContainerBuilder {
    pub async fn run_build(
        &self,
        tx: Sender<String>,
        token: CancellationToken,
    ) -> Result<(), Box<dyn Error + Send>> {
        tx.send("Starting...".to_string()).await.unwrap();

        let mut file = File::create("container").unwrap();
        file.write_all(self.container_file.as_bytes()).unwrap();
        let mut child = tokio::process::Command::new("podman")
            .args(["build", "-f", "container"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let stdout_handle = if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let tx_clone = tx.clone();
            task::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_clone.send(line).await.is_err() {
                        break;
                    }
                }
            })
        } else {
            task::spawn(async {})
        };

        let stderr_handle = if let Some(stderr) = child.stderr.take() {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let tx_clone = tx.clone();
            task::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_clone.send(line).await.is_err() {
                        break;
                    }
                }
            })
        } else {
            task::spawn(async {})
        };

        tokio::select! {
            _ = token.cancelled() => {
                child.kill().await.expect("Kill failed");
                tx.send("Process killed by cancellation token.".to_string()).await.unwrap();
            }
            status = child.wait() => {
                tx.send(format!("Process exited with status: {:?}", status)).await.unwrap();
            }
        }

        stdout_handle.await.unwrap();
        stderr_handle.await.unwrap();

        Ok(())
    }
    pub fn add_package(&mut self, package: Package) {
        self.packages.push(package);
    }
    pub fn set_distr(&mut self, distro: String) {
        self.distro = distro;
    }
    pub fn get_container_file(&mut self) -> String {
        self.container_file.clone()
    }
    pub fn get_packages(self) -> Vec<Package> {
        self.packages
    }
    pub fn create_container_file(&mut self) {
        self.container_file = "".to_string();
        self.container_file = format!("FROM docker.io/library/ros:{}-ros-base \n", self.distro);
        self.container_file
            .push_str("ENV DEBIAN_FRONTEND=noninteractive\n");
        let mut found_pkg_man_pkg = false;
        for pkg in &self.packages {
            if pkg.installation_type == InstallationType::PkgManager {
                if !found_pkg_man_pkg {
                    self.container_file.push_str("RUN apt update\n");
                    self.container_file
                        .push_str(format!("RUN apt install -y {}", pkg.name).as_str());
                    found_pkg_man_pkg = true;
                } else {
                    self.container_file
                        .push_str(format!(" \\ \n \t && {}", pkg.pkg_name).as_str());
                }
            }
        }

        for pkg in &self.packages {
            if pkg.installation_type == InstallationType::Script {
                self.container_file.push_str(
                    format!("\n \n# \\\\\\\\\\\\\\\\\\\\\\\\\\\\ Installing {} \n", pkg.name).as_str(),
                );
                self.container_file
                    .push_str(format!("\nRUN {} ", pkg.installation_command).as_str());
                self.container_file
                    .push_str("\n\n# //////////////")
            }
        }
    }
}
impl Default for AllPackages {
    fn default() -> Self {
        let htop = Package::new(
            "htop".to_string(),
            "".to_string(),
            "htop".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let wget = Package::new(
            "wget".to_string(),
            "".to_string(),
            "wget".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let cmake = Package::new(
            "cmake".to_string(),
            "".to_string(),
            "cmake".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let clangd = Package::new(
            "clangd".to_string(),
            "".to_string(),
            "clangd".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let clang_tidy = Package::new(
            "clang-tidy".to_string(),
            "".to_string(),
            "clang-tidy".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let curl = Package::new(
            "curl".to_string(),
            "".to_string(),
            "curl".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let ssh = Package::new(
            "ssh".to_string(),
            "".to_string(),
            "ssh".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let tmux = Package::new(
            "tmux".to_string(),
            "".to_string(),
            "tmux".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let git = Package::new(
            "git".to_string(),
            "".to_string(),
            "git".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let ripgrep = Package::new(
            "ripgrep".to_string(),
            "".to_string(),
            "ripgrep".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let zsh = Package::new(
            "zsh".to_string(),
            "".to_string(),
            "zsh".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let pip = Package::new(
            "pip".to_string(),
            "".to_string(),
            "python3-pip".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let fzf = Package::new(
            "fzf".to_string(),
            "".to_string(),
            "fzf".to_string(),
            InstallationType::Script,
            "git clone https://github.com/junegunn/fzf.git /opt/.fzf \\
    && cd /opt/.fzf && PREFIX=/fzf make install && ./install --all"
                .to_string(),
            Some(vec![Box::new(wget.clone())]),
        );
        let ping = Package::new(
            "ping".to_string(),
            "".to_string(),
            "iputils-ping".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let nvim = Package::new(
            "neovim".to_string(),
            "0.12.1".to_string(),
            "nvim".to_string(),
            InstallationType::Script,
            "cd /usr/local/bin/ \\
    && wget https://github.com/neovim/neovim/releases/download/v0.12.1/nvim-linux-x86_64.tar.gz  \\
    && tar zxfv nvim-linux-x86_64.tar.gz \\
    && rm -rf nvim-linux-x86_64.tar.gz \\
    && ln -sf /usr/local/bin/nvim-linux-x86_64/bin/nvim /usr/local/bin/"
                .to_string(),
            Some(vec![Box::new(wget.clone()), Box::new(git.clone())]),
        );
        let gdb = Package::new(
            "gdb".to_string(),
            "latetst".to_string(),
            "gdb".to_string(),
            InstallationType::PkgManager,
            "".to_string(),
            None,
        );
        let rust = Package::new(
            "rust".to_string(),
            "".to_string(),
            "rust".to_string(),
            InstallationType::Script,
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y".to_string(),
            Some(vec![Box::new(curl.clone())]),
        );
        let uv = Package::new(
            "uv".to_string(),
            "".to_string(),
            "uv".to_string(),
            InstallationType::Script,
            "pip3 install uv --break-system-packages".to_string(),
            Some(vec![Box::new(pip.clone())]),
        );
        Self {
            packages: vec![
                htop, fzf, ripgrep, nvim, gdb, git, ping, wget, curl, tmux, uv, cmake, clangd,
                clang_tidy, ssh, rust, pip, zsh,
            ],
        }
    }
}
