use crate::app::{App, View};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn update(app: &mut App, key_event: KeyEvent) {
    // The input overlay swallows everything while it is open.
    if app.prompt_is_open() {
        match key_event.code {
            KeyCode::Esc => app.prompt_cancel(),
            KeyCode::Enter => app.prompt_submit(),
            KeyCode::Backspace => app.prompt_backspace(),
            KeyCode::Tab | KeyCode::Right => app.prompt_cycle_family(true),
            KeyCode::Left | KeyCode::BackTab => app.prompt_cycle_family(false),
            KeyCode::Char('u') | KeyCode::Char('U')
                if key_event.modifiers == KeyModifiers::CONTROL =>
            {
                app.prompt_clear()
            }
            KeyCode::Char('c') | KeyCode::Char('C')
                if key_event.modifiers == KeyModifiers::CONTROL =>
            {
                app.prompt_cancel()
            }
            KeyCode::Char(character) => app.prompt_insert(character),
            _ => {}
        }
        return;
    }

    let builder = app.view == View::Builder;
    let toolboxes = app.view == View::Toolboxes;
    let info = app.info_open;

    match key_event.code {
        KeyCode::Esc => {
            // Esc closes the info popup before it quits.
            if !app.close_info() {
                app.quit();
            }
        }
        KeyCode::Char('q') => app.quit(),
        KeyCode::Char('c') | KeyCode::Char('C') if key_event.modifiers == KeyModifiers::CONTROL => {
            app.quit();
        }
        KeyCode::Tab => app.next_view(),
        KeyCode::BackTab => app.previous_view(),

        KeyCode::Up | KeyCode::Char('k') => app.move_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_down(),
        KeyCode::Char('L') => app.toggle_logs_view(),
        KeyCode::Char('t') | KeyCode::Char('T') => app.cycle_theme(),
        KeyCode::Right | KeyCode::Char('l') => app.focus_right(),
        KeyCode::Left | KeyCode::Char('h') => app.focus_left(),
        KeyCode::PageDown => app.scroll_toolbox_details(true),
        KeyCode::PageUp => app.scroll_toolbox_details(false),
        KeyCode::Enter => app.toggle_option(),
        KeyCode::Backspace => app.run_build(),

        // Info popup: description plus install overrides.
        KeyCode::Char('i') | KeyCode::Char('I') => app.toggle_info(),
        KeyCode::Char('m') | KeyCode::Char('M') if info => app.toggle_install_method(),
        KeyCode::Char('e') | KeyCode::Char('E') if info => app.open_script_prompt(),
        KeyCode::Char('r') | KeyCode::Char('R') if info => app.reset_install_override(),

        // Rescan whatever the active tab is about.
        KeyCode::Char('r') | KeyCode::Char('R') if builder => app.refresh_distros(),
        KeyCode::Char('r') | KeyCode::Char('R') if toolboxes => app.refresh_toolboxes(),

        // Select / clear every package available on the current distro.
        KeyCode::Char('a') | KeyCode::Char('A')
            if key_event.modifiers == KeyModifiers::CONTROL && builder =>
        {
            app.toggle_all_packages()
        }

        // Name the toolbox the next build should create.
        KeyCode::Char('n') | KeyCode::Char('N') if builder => app.open_pending_toolbox_prompt(),

        // User-added base images.
        KeyCode::Char('a') | KeyCode::Char('A') if builder => app.open_image_prompt(),
        KeyCode::Char('d') | KeyCode::Char('D') if builder => app.remove_custom_distro(),

        // Toolbox containers / manifests.
        KeyCode::Char('c') | KeyCode::Char('C') if toolboxes => app.create_toolbox(),
        KeyCode::Char('x') | KeyCode::Char('X') if toolboxes => app.remove_toolboxes(),
        KeyCode::Char('d') | KeyCode::Char('D') if toolboxes => app.forget_selected_toolbox(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::{Catalog, DEFAULT_INSTALL_KEY, InstallSpec, Package},
        distro::Distro,
        manifest::Manifest,
        toolbox::{Toolbox, ToolboxScan, ToolboxStatus},
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    struct Harness {
        app: App,
        distro_rx: mpsc::Receiver<crate::app::DistroRefresh>,
        toolbox_rx: mpsc::Receiver<ToolboxScan>,
    }

    fn harness() -> Harness {
        let catalog = Catalog {
            packages: vec![Package {
                name: "htop".to_string(),
                version: String::new(),
                summary: String::new(),
                category: "Shell".to_string(),
                requires: vec![],
                install: BTreeMap::from([(
                    DEFAULT_INSTALL_KEY.to_string(),
                    InstallSpec::PackageManager {
                        pkg: "htop".to_string(),
                    },
                )]),
            }],
            warnings: vec![],
        };
        let (log_tx, _log_rx) = mpsc::channel(64);
        let (distro_tx, distro_rx) = mpsc::channel(8);
        let (toolbox_tx, toolbox_rx) = mpsc::channel(8);
        Harness {
            app: App::new(catalog, Distro::predefined(), log_tx, distro_tx, toolbox_tx),
            distro_rx,
            toolbox_rx,
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        update(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn toolbox(image: &str) -> Toolbox {
        use crate::manifest::{ManifestDistro, ManifestPackage};
        Toolbox::built(
            Manifest {
                version: crate::manifest::MANIFEST_VERSION,
                image: image.to_string(),
                image_id: None,
                distro: ManifestDistro {
                    name: "jazzy".to_string(),
                    image: "docker.io/library/ros:jazzy-ros-base".to_string(),
                    family: "ros".to_string(),
                },
                packages: vec![ManifestPackage {
                    name: "htop".to_string(),
                    version: String::new(),
                    pkg: Some("htop".to_string()),
                    script: None,
                }],
                created_at: 0,
                tool_version: "test".to_string(),
            },
            ToolboxStatus::Present,
            vec![],
        )
    }

    #[test]
    fn tab_switches_views_and_q_quits() {
        let mut harness = harness();
        assert_eq!(harness.app.view, View::Builder);

        press(&mut harness.app, KeyCode::Tab);
        assert_eq!(harness.app.view, View::Toolboxes);
        press(&mut harness.app, KeyCode::Tab);
        assert_eq!(harness.app.view, View::Logs);
        press(&mut harness.app, KeyCode::BackTab);
        assert_eq!(harness.app.view, View::Toolboxes);
        press(&mut harness.app, KeyCode::BackTab);
        assert_eq!(harness.app.view, View::Builder);

        press(&mut harness.app, KeyCode::Char('q'));
        assert!(harness.app.should_quit);
    }

    #[test]
    fn shift_l_toggles_the_logs_view_from_anywhere() {
        let mut harness = harness();
        press(&mut harness.app, KeyCode::Char('L'));
        assert_eq!(harness.app.view, View::Logs);

        press(&mut harness.app, KeyCode::Char('L'));
        assert_eq!(harness.app.view, View::Builder);

        press(&mut harness.app, KeyCode::Tab); // toolboxes
        press(&mut harness.app, KeyCode::Char('L'));
        assert_eq!(harness.app.view, View::Logs);
        press(&mut harness.app, KeyCode::Char('L'));
        assert_eq!(harness.app.view, View::Builder);
    }

    #[test]
    fn backspace_builds_only_in_the_builder_tab() {
        let mut harness = harness();
        press(&mut harness.app, KeyCode::Tab);
        press(&mut harness.app, KeyCode::Backspace);
        assert!(!harness.app.process_running);
        assert!(
            harness
                .app
                .logs
                .iter()
                .all(|line| !line.contains("Nothing to build"))
        );
    }

    #[test]
    fn enter_is_ignored_in_the_toolbox_tab() {
        let mut harness = harness();
        harness.app.apply_toolbox_scan(ToolboxScan {
            toolboxes: vec![toolbox("localhost/toolshop:jazzy-1a2b3c")],
            warnings: vec![],
        });
        press(&mut harness.app, KeyCode::Tab);
        press(&mut harness.app, KeyCode::Enter);
        assert!(harness.app.selected_packages.is_empty());
        assert!(harness.app.selected_distro.is_none());
    }

    #[test]
    fn d_forgets_the_selected_toolbox_only_in_its_tab() {
        let dir = std::env::temp_dir().join(format!("toolshop-update-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = Manifest::from_selection(
            &Distro::predefined()[0],
            &[],
            "localhost/toolshop:jazzy-1a2b3c".to_string(),
        );
        let path = crate::manifest::save_to(&dir, &manifest).unwrap();

        let mut harness = harness();
        harness.app.cache_dir = dir.clone();
        harness.app.apply_toolbox_scan(ToolboxScan {
            toolboxes: vec![toolbox("localhost/toolshop:jazzy-1a2b3c")],
            warnings: vec![],
        });

        // In the builder tab `d` concerns user images, not manifests.
        press(&mut harness.app, KeyCode::Char('d'));
        assert!(path.exists());

        press(&mut harness.app, KeyCode::Tab);
        press(&mut harness.app, KeyCode::Char('d'));
        assert!(!path.exists());
        assert!(harness.app.toolboxes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_opens_the_image_prompt_and_typing_goes_into_it() {
        let mut harness = harness();
        press(&mut harness.app, KeyCode::Char('a'));
        assert!(harness.app.prompt_is_open());

        for character in "localhost/dev:ubuntu24".chars() {
            press(&mut harness.app, KeyCode::Char(character));
        }
        // Keys that normally switch views are swallowed by the prompt: Tab cycles the
        // image type instead of changing tabs.
        let family_before = harness.app.prompt.as_ref().unwrap().family();
        press(&mut harness.app, KeyCode::Tab);
        assert_eq!(harness.app.view, View::Builder);
        assert_ne!(harness.app.prompt.as_ref().unwrap().family(), family_before);
        assert_eq!(
            harness.app.prompt.as_ref().unwrap().input,
            "localhost/dev:ubuntu24"
        );

        // Esc closes the prompt without quitting.
        press(&mut harness.app, KeyCode::Esc);
        assert!(!harness.app.prompt_is_open());
        assert!(!harness.app.should_quit);
    }

    #[test]
    fn ctrl_a_selects_all_packages_in_the_builder() {
        let mut harness = harness();
        harness.app.selected_distro = Some(0);
        harness
            .app
            .builder
            .set_distro(Distro::predefined()[0].clone());

        update(
            &mut harness.app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(harness.app.selected_packages.len(), 1);

        // Plain `a` still opens the add-image prompt.
        press(&mut harness.app, KeyCode::Char('a'));
        assert!(harness.app.prompt_is_open());
        press(&mut harness.app, KeyCode::Esc);

        // Not bound in the Toolboxes tab.
        press(&mut harness.app, KeyCode::Tab);
        update(
            &mut harness.app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(harness.app.selected_packages.len(), 1, "unchanged");
    }

    #[test]
    fn prompt_shortcuts_clear_and_cancel() {
        let mut harness = harness();
        press(&mut harness.app, KeyCode::Char('a'));
        for character in "abc".chars() {
            press(&mut harness.app, KeyCode::Char(character));
        }
        assert_eq!(harness.app.prompt.as_ref().unwrap().input, "abc");

        // Ctrl+U empties the field instead of switching views/typing.
        update(
            &mut harness.app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(harness.app.prompt.as_ref().unwrap().input, "");

        // Ctrl+C closes the prompt rather than quitting.
        update(
            &mut harness.app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(!harness.app.prompt_is_open());
        assert!(!harness.app.should_quit);
    }

    #[test]
    fn install_override_keys_need_the_info_popup() {
        let mut harness = harness();
        harness.app.selected_distro = Some(0);
        harness
            .app
            .builder
            .set_distro(Distro::predefined()[0].clone());
        press(&mut harness.app, KeyCode::Char('j')); // still fine without info
        press(&mut harness.app, KeyCode::Char('m'));
        assert!(harness.app.overrides.is_empty(), "m ignored without info");

        press(&mut harness.app, KeyCode::Char('i'));
        press(&mut harness.app, KeyCode::Char('m'));
        assert_eq!(harness.app.overrides.len(), 1);

        // `r` resets while the popup is open, and rescans once it is closed.
        press(&mut harness.app, KeyCode::Char('r'));
        assert!(harness.app.overrides.is_empty());
    }

    #[test]
    fn e_opens_the_script_prompt_for_the_cursor_package() {
        let mut harness = harness();
        harness.app.selected_distro = Some(0);
        harness
            .app
            .builder
            .set_distro(Distro::predefined()[0].clone());
        press(&mut harness.app, KeyCode::Char('i'));
        press(&mut harness.app, KeyCode::Char('m')); // switch to a script
        press(&mut harness.app, KeyCode::Char('e'));

        let prompt = harness.app.prompt.as_ref().expect("script prompt");
        assert!(prompt.title.contains("htop"));

        press(&mut harness.app, KeyCode::Backspace);
        press(&mut harness.app, KeyCode::Char('X'));
        press(&mut harness.app, KeyCode::Enter);
        assert!(
            harness
                .app
                .overrides
                .get("htop")
                .is_some_and(|over| over.value.ends_with('X'))
        );
    }

    #[tokio::test]
    async fn r_triggers_the_rescan_of_the_active_tab() {
        let mut harness = harness();

        // Toolbox tab: R spawns a toolbox scan.
        press(&mut harness.app, KeyCode::Tab);
        press(&mut harness.app, KeyCode::Char('R'));
        let scan = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            harness.toolbox_rx.recv(),
        )
        .await
        .expect("toolbox scan should be sent")
        .expect("channel open");
        assert!(scan.warnings.is_empty() || !scan.warnings.is_empty());

        // Builder tab: R spawns a distro scan instead.
        press(&mut harness.app, KeyCode::BackTab);
        press(&mut harness.app, KeyCode::Char('R'));
        let refresh =
            tokio::time::timeout(std::time::Duration::from_secs(10), harness.distro_rx.recv())
                .await
                .expect("distro scan should be sent")
                .expect("channel open");
        assert!(refresh.distros.is_empty() || !refresh.distros.is_empty());
    }
}
