use std::sync::{Arc, Mutex};

use tokio::{sync::mpsc::Sender, task};

use crate::container_builder::{AllPackages, ContainerBuilder};
use ratatui::widgets::ListState;
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq)]
pub enum Focus {
    Distro,
    Apps,
    Container,
    Logs,
}
#[derive(Debug)]
pub struct App {
    pub builder: ContainerBuilder,
    pub token: CancellationToken,
    pub process_running: bool,
    pub process_started: bool,
    pub focus: Focus,
    pub should_quit: bool,
    pub apps_list: AllPackages,
    pub distro_list: Vec<String>,
    pub apps_list_state: ListState,
    pub distro_list_state: ListState,
    pub apps_selected: Vec<usize>,
    pub distros_selected: Vec<usize>,
    pub logs: Arc<Mutex<String>>,
    pub tx: Sender<String>,
}

/// Returns true if the list wasn't on the list and false otherwise
fn toggle_list(selected_list: &mut Vec<usize>, index: usize) -> bool {
    if selected_list.contains(&index) {
        let id = selected_list.iter().position(&|x| x == &index).unwrap();
        selected_list.remove(id);
        false
    } else {
        selected_list.push(index);
        true
    }
}
fn remove_from_list(selected_list: &mut Vec<usize>, index: usize) {
    if selected_list.contains(&index) {
        let id = selected_list.iter().position(&|x| x == &index).unwrap();
        selected_list.remove(id);
    }
}
fn add_to_list(selected_list: &mut Vec<usize>, index: usize) {
    if selected_list.contains(&index) {
    } else {
        selected_list.push(index);
    }
}
fn manage_dependencies(
     selected_list: &mut Vec<usize>,
    all_packages: AllPackages,
    index: usize,
    add: bool,
) {
    if all_packages.packages[index].requires.is_some() {
        for pkg in all_packages.packages[index]
            .requires
            .clone()
            .unwrap()
            .iter()
        {
            let index = all_packages
                .packages
                .iter()
                .position(|x| x.name == pkg.name)
                .unwrap();
            if add {
                add_to_list(&mut *selected_list, index);
            } else {
                remove_from_list(&mut *selected_list, index);
            }
        }
    }
}
impl App {
    pub fn new(tx: Sender<String>) -> Self {
        Self {
            tx,
            builder: ContainerBuilder::default(),
            token: CancellationToken::new(),
            process_running: false,
            process_started: false,
            focus: Focus::Distro,
            should_quit: false,
            apps_selected: vec![],
            distros_selected: vec![],
            logs: Arc::new(Mutex::new(String::new())),
            apps_list_state: ListState::default().with_selected(Some(0)),
            distro_list_state: ListState::default().with_selected(Some(0)),
            distro_list: vec![
                "jazzy".to_string(),
                "kilted".to_string(),
                "lyrical".to_string(),
            ],
            apps_list: AllPackages::default(),
        }
    }

    pub fn tick(&self) {}

    pub fn quit(&mut self) {
        self.should_quit = true;
    }
    pub fn append_log(&self, log: &str) {
        let mut guard = self.logs.lock().unwrap();
        guard.push_str(log);
        guard.push('\n');
    }
    pub fn focus_right(&mut self) {
        match self.focus {
            Focus::Distro => self.focus = Focus::Apps,
            Focus::Apps => self.focus = Focus::Container,
            Focus::Container => self.focus = Focus::Logs,
            Focus::Logs => self.focus = Focus::Distro,
        }
    }
    pub fn focus_left(&mut self) {
        match self.focus {
            Focus::Distro => self.focus = Focus::Logs,
            Focus::Logs => self.focus = Focus::Container,
            Focus::Container => self.focus = Focus::Apps,
            Focus::Apps => self.focus = Focus::Distro,
        }
    }

    pub fn move_up(&mut self) {
        match self.focus {
            Focus::Distro => self.distro_list_state.select_next(),
            Focus::Apps => self.apps_list_state.select_next(),
            _ => {}
        }
    }

    pub fn move_down(&mut self) {
        match self.focus {
            Focus::Distro => self.distro_list_state.select_previous(),
            Focus::Apps => self.apps_list_state.select_previous(),
            _ => {}
        }
    }
    pub fn create_container_file(&mut self) {
        if !self.distros_selected.is_empty() {
            let distro = &self.distro_list[self.distros_selected[0]];
            self.builder.set_distr(distro.to_string());
            self.builder.packages = vec![];
            for pkg_id in &self.apps_selected {
                self.builder
                    .add_package(self.apps_list.packages[*pkg_id].clone());
            }
            self.builder.create_container_file();
        }
    }

    pub fn toggle_option(&mut self) {
        match self.focus {
            Focus::Distro => {
                // we can only have one selection here so we clear previous ones
                self.distros_selected = vec![];
                toggle_list(
                    &mut self.distros_selected,
                    self.distro_list_state.selected().unwrap(),
                );
                self.create_container_file();
            }
            Focus::Apps => {
                let idx = self.apps_list_state.selected().unwrap();
                let toggled = toggle_list(&mut self.apps_selected, idx);
                // toggle apps that are dependencies
                manage_dependencies(
                    &mut self.apps_selected,
                    self.apps_list.clone(),
                    idx,
                    toggled,
                );
                self.create_container_file();
            }
            _ => {}
        }
    }
    pub fn run_build(&mut self) {
        if !self.process_running {
            self.process_running = true;
            self.token = CancellationToken::new();
            let token = self.token.clone();
            let tx = self.tx.clone();
            let builder = self.builder.clone();
            task::spawn(async move {
                if let Err(e) = builder.run_build(tx, token).await {
                    eprintln!("Error in run_podman_build: {}", e);
                }
            });
        } else {
            self.token.cancel();
            self.process_running = false;
        }
    }
}
