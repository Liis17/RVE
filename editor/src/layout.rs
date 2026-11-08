//! Расположение панелей и их отображение через egui_dock.

use crate::state::EditorState;
use crate::ui;
use egui_dock::{DockState, NodeIndex, TabViewer};

/// Панель редактора; каждая существует в одном экземпляре.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Tab {
    Scene,
    Game,
    Hierarchy,
    Inspector,
    Project,
    Console,
    /// Подокно `.rveprefab` в `[[assets]]` сцены.
    SceneAssets,
    /// Подокно списка тяжёлых префабов `prefabs.rveprefabs` проекта.
    ProjectPrefabs,
    Profiler,
    MaterialPreview,
}

impl Tab {
    pub const ALL: [Self; 10] = [
        Self::Scene,
        Self::Game,
        Self::Hierarchy,
        Self::Inspector,
        Self::Project,
        Self::Console,
        Self::SceneAssets,
        Self::ProjectPrefabs,
        Self::Profiler,
        Self::MaterialPreview,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Scene => "Scene",
            Self::Game => "Game",
            Self::Hierarchy => "Hierarchy",
            Self::Inspector => "Inspector",
            Self::Project => "Project",
            Self::Console => "Console",
            Self::SceneAssets => "Scene Assets",
            Self::ProjectPrefabs => "Project Prefabs",
            Self::Profiler => "Profiler",
            Self::MaterialPreview => "Material Preview",
        }
    }
}

/// Раскладка в стиле Unity: в центре Scene и Game, слева Hierarchy, справа Inspector, снизу Project и Console.
pub fn default_layout() -> DockState<Tab> {
    let mut dock = DockState::new(vec![Tab::Scene, Tab::Game]);
    let surface = dock.main_surface_mut();
    let [center, inspector] = surface.split_right(NodeIndex::root(), 0.78, vec![Tab::Inspector]);
    surface.split_below(inspector, 0.62, vec![Tab::MaterialPreview]);
    let [center, _hierarchy] = surface.split_left(center, 0.22, vec![Tab::Hierarchy]);
    surface.split_below(center, 0.68, vec![Tab::Project, Tab::Console]);
    dock
}

/// Открытые панели, отсутствующие в раскладке, возвращаются через меню Window.
pub fn is_open(dock: &DockState<Tab>, tab: Tab) -> bool {
    dock.iter_all_tabs().any(|(_, open)| *open == tab)
}

pub struct EditorTabs<'a> {
    pub state: &'a mut EditorState,
}

impl TabViewer for EditorTabs<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        tab.title().into()
    }

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::new(tab.title())
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Scene => ui::scene_view::show(ui, self.state),
            Tab::Game => ui::game_view::show(ui, self.state),
            Tab::Hierarchy => ui::hierarchy::show(ui, self.state),
            Tab::Inspector => ui::inspector::show(ui, self.state),
            Tab::Project => ui::project_panel::show(ui, self.state),
            Tab::Console => ui::console::show(ui, self.state),
            Tab::SceneAssets => ui::scene_assets::show(ui, self.state),
            Tab::ProjectPrefabs => ui::project_prefabs::show(ui, self.state),
            Tab::Profiler => ui::profiler::show(ui, self.state),
            Tab::MaterialPreview => ui::material_preview::show(ui, self.state),
        }
    }

    fn clear_background(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Scene | Tab::Game)
    }

    /// Панели сами управляют прокруткой; изображение сцены занимает всю область.
    fn scroll_bars(&self, _tab: &Tab) -> [bool; 2] {
        [false, false]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_contains_every_panel_once() {
        let dock = default_layout();
        // Profiler открывается из меню вкладок и по умолчанию закрыт.
        for tab in Tab::ALL.into_iter().filter(|tab| *tab != Tab::Profiler) {
            let count = dock
                .iter_all_tabs()
                .filter(|(_, open)| **open == tab)
                .count();
            // Подокна Scene Assets и Project Prefabs открываются из меню Window.
            let optional = matches!(tab, Tab::SceneAssets | Tab::ProjectPrefabs);
            let expected = usize::from(!optional);
            assert_eq!(count, expected, "{tab:?}");
        }
        assert!(!is_open(&dock, Tab::Profiler));
    }

    #[test]
    fn closed_panel_is_reported_and_can_be_reopened() {
        let mut dock = DockState::new(vec![Tab::Scene]);
        assert!(!is_open(&dock, Tab::Console));
        dock.push_to_first_leaf(Tab::Console);
        assert!(is_open(&dock, Tab::Console));
    }
}
