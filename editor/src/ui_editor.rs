//! Visual authoring for the same validated document and paint used by the player.
use crate::project::OpenProject;
use crate::state::EditorState;
use egui::{Color32, Pos2, Rect, Vec2, pos2, vec2};
use engine_gui::document::*;
use engine_gui::input::UiInput;
use engine_gui::layout::{canvas_scale, canvas_size, layout, resolve_rect};
use engine_gui::paint::UiTexture;
use engine_gui::system::UiSystem;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Draft {
    canvas: CanvasSettings,
    #[serde(default)]
    assets: Vec<UiAsset>,
    #[serde(default)]
    elements: Vec<UiElement>,
}
impl Draft {
    fn from_document(doc: &UiDocument) -> Self {
        Self {
            canvas: doc.canvas.clone(),
            assets: doc.assets().to_vec(),
            elements: doc.elements().to_vec(),
        }
    }
    fn document(&self) -> Result<UiDocument, String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        UiDocument::parse("UI Editor", &text).map_err(|e| e.to_string())
    }
    fn unique_id(&self, prefix: &str) -> String {
        (1..)
            .map(|n| format!("{prefix}_{n}"))
            .find(|id| !self.elements.iter().any(|e| &e.id == id))
            .unwrap()
    }
    fn add(&mut self, kind: &str, parent: Option<String>) -> String {
        let id = self.unique_id(kind);
        let element = UiElement {
            id: id.clone(),
            parent,
            visible: true,
            interactable: true,
            nav: NavSpec::default(),
            rect: RectSpec {
                anchor_min: [0.0; 2],
                anchor_max: [0.0; 2],
                offset_min: [24.0; 2],
                offset_max: [224.0, 80.0],
                ..Default::default()
            },
            kind: match kind {
                "text" => ElementKind::Text(TextSpec {
                    string: "Text".into(),
                    ..Default::default()
                }),
                "button" => ElementKind::Button(ButtonSpec::default()),
                "toggle" => ElementKind::Toggle(ToggleSpec::default()),
                "slider" => ElementKind::Slider(SliderSpec::default()),
                "progress_bar" => ElementKind::ProgressBar(ProgressBarSpec::default()),
                "input_field" => ElementKind::InputField(InputFieldSpec::default()),
                "scroll_view" => ElementKind::ScrollView(ScrollViewSpec::default()),
                "dropdown" => ElementKind::Dropdown(DropdownSpec {
                    options: vec!["First".into(), "Second".into()],
                    selected: Some(0),
                    ..Default::default()
                }),
                "grid" => ElementKind::Grid(GridSpec::default()),
                "horizontal" => ElementKind::Horizontal(StackSpec::default()),
                "vertical" => ElementKind::Vertical(StackSpec::default()),
                _ => ElementKind::Panel(PanelSpec {
                    color: [40, 45, 55, 255],
                    ..Default::default()
                }),
            },
        };
        self.elements.push(element);
        if kind == "scroll_view" {
            self.add("panel", Some(id.clone()));
        }
        if kind == "button" {
            let child = self.add("text", Some(id.clone()));
            let e = self.elements.iter_mut().find(|e| e.id == child).unwrap();
            e.rect = RectSpec::default();
            e.kind = ElementKind::Text(TextSpec {
                string: "Button".into(),
                color: [25, 25, 25, 255],
                h_align: HAlign::Center,
                v_align: VAlign::Middle,
                ..Default::default()
            });
        }
        id
    }
    fn rename(&mut self, old: &str, new: String) {
        for e in &mut self.elements {
            if e.id == old {
                e.id = new.clone();
            }
            if e.parent.as_deref() == Some(old) {
                e.parent = Some(new.clone());
            }
            for link in [
                &mut e.nav.up,
                &mut e.nav.down,
                &mut e.nav.left,
                &mut e.nav.right,
            ] {
                if link.as_deref() == Some(old) {
                    *link = Some(new.clone());
                }
            }
        }
        if self.canvas.default_focus.as_deref() == Some(old) {
            self.canvas.default_focus = Some(new);
        }
    }
    fn subtree(&self, id: &str) -> HashSet<String> {
        let mut ids = HashSet::from([id.to_owned()]);
        loop {
            let before = ids.len();
            for e in &self.elements {
                if e.parent.as_ref().is_some_and(|p| ids.contains(p)) {
                    ids.insert(e.id.clone());
                }
            }
            if ids.len() == before {
                return ids;
            }
        }
    }
    fn remove(&mut self, id: &str) {
        let ids = self.subtree(id);
        self.elements.retain(|e| !ids.contains(&e.id));
        for e in &mut self.elements {
            for link in [
                &mut e.nav.up,
                &mut e.nav.down,
                &mut e.nav.left,
                &mut e.nav.right,
            ] {
                if link.as_ref().is_some_and(|id| ids.contains(id)) {
                    *link = None;
                }
            }
        }
        if self
            .canvas
            .default_focus
            .as_ref()
            .is_some_and(|id| ids.contains(id))
        {
            self.canvas.default_focus = None;
        }
    }
    fn duplicate(&mut self, id: &str) -> Option<String> {
        let ids = self.subtree(id);
        let originals: Vec<_> = self
            .elements
            .iter()
            .filter(|e| ids.contains(&e.id))
            .cloned()
            .collect();
        let mut remap = HashMap::new();
        for e in &originals {
            let next = self.unique_id(&e.id);
            // Reserve each name immediately, so descendants cannot collide with a preceding copy.
            let mut copy = e.clone();
            copy.id = next.clone();
            self.elements.push(copy);
            remap.insert(e.id.clone(), next);
        }
        let start = self.elements.len() - originals.len();
        for e in &mut self.elements[start..] {
            if let Some(parent) = &e.parent
                && let Some(new) = remap.get(parent)
            {
                e.parent = Some(new.clone());
            }
            for link in [
                &mut e.nav.up,
                &mut e.nav.down,
                &mut e.nav.left,
                &mut e.nav.right,
            ] {
                if let Some(new) = link.as_ref().and_then(|id| remap.get(id)) {
                    *link = Some(new.clone());
                }
            }
        }
        remap.remove(id)
    }
}

/// Owns GPU-independent editing history and managed egui textures (released on drop).
pub struct CanvasEditor {
    pub path: String,
    draft: Draft,
    saved: Draft,
    undo: Vec<Draft>,
    redo: Vec<Draft>,
    selected: Option<String>,
    screen: Vec2,
    zoom: f32,
    fit_requested: bool,
    pan: Vec2,
    error: Option<String>,
    system: UiSystem,
    preview_document: Option<UiDocument>,
    atlas: Option<(u64, egui::TextureHandle)>,
    images: HashMap<String, egui::TextureHandle>,
    disk_text: String,
    external_conflict: Option<String>,
    source: String,
    drag: Option<(Draft, Rect, usize)>,
}
impl CanvasEditor {
    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }
    fn open(project: &OpenProject, path: &str) -> Result<Self, String> {
        let file = project
            .assets_dir
            .resolve(path)
            .map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
        let doc = UiDocument::parse(path, &text).map_err(|e| e.to_string())?;
        let draft = Draft::from_document(&doc);
        Ok(Self {
            path: path.into(),
            saved: draft.clone(),
            draft,
            undo: vec![],
            redo: vec![],
            selected: None,
            screen: vec2(1280.0, 720.0),
            zoom: 0.5,
            fit_requested: true,
            pan: Vec2::ZERO,
            error: None,
            system: UiSystem::default(),
            preview_document: None,
            atlas: None,
            images: HashMap::new(),
            disk_text: text.clone(),
            external_conflict: None,
            source: text,
            drag: None,
        })
    }
    pub fn save(&mut self, project: &OpenProject) -> Result<(), String> {
        let doc = self.draft.document()?;
        let file = project
            .assets_dir
            .resolve(&self.path)
            .map_err(|e| e.to_string())?;
        let current = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        if current != self.disk_text {
            self.external_conflict = Some(current);
            return Err("UI изменён извне. Сначала выберите Reload или Keep edits.".into());
        }
        let text = doc.to_toml().map_err(|e| e.to_string())?;
        std::fs::write(file, &text).map_err(|e| e.to_string())?;
        self.disk_text = text.clone();
        self.source = text;
        self.saved = self.draft.clone();
        Ok(())
    }
    fn commit(&mut self, before: Draft) {
        if before == self.draft {
            return;
        }
        if let Err(error) = self.draft.document() {
            self.draft = before;
            self.error = Some(error);
            return;
        }
        self.undo.push(before);
        if self.undo.len() > 128 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.error = None;
    }
    fn undo(&mut self) {
        if let Some(old) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.draft, old));
        }
    }
    fn redo(&mut self) {
        if let Some(new) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.draft, new));
        }
    }
    fn refresh_assets(&mut self, ctx: &egui::Context, project: &OpenProject) -> Result<(), String> {
        self.system = UiSystem::default();
        self.atlas = None;
        self.images.clear();
        self.preview_document = None;
        for asset in &self.draft.assets {
            let path = asset.path.as_str();
            let file = project
                .assets_dir
                .resolve(path)
                .map_err(|e| e.to_string())?;
            match asset_kind(path) {
                Some(AssetKind::Font) => self
                    .system
                    .register_font(path, std::fs::read(file).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
                Some(AssetKind::Image) => {
                    let image = project
                        .assets
                        .load_texture(path)
                        .map_err(|e| e.to_string())?;
                    self.system
                        .register_image(path, [image.width, image.height])
                        .map_err(|e| e.to_string())?;
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width as usize, image.height as usize],
                        &image.rgba,
                    );
                    self.images.insert(
                        path.into(),
                        ctx.load_texture(path, color, Default::default()),
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }
    /// Called from the existing debounced asset watcher. Never overwrites dirty edits.
    pub fn refresh_external(
        &mut self,
        project: &OpenProject,
        changes: &crate::asset_watch::AssetChanges,
    ) {
        let Ok(path) = engine_files::VirtualPath::parse(&self.path) else {
            return;
        };
        let changed = changes.rescan || changes.contains_path_or_parent(&path);
        if changed {
            let result = project
                .assets_dir
                .resolve(&self.path)
                .map_err(|e| e.to_string())
                .and_then(|p| std::fs::read_to_string(p).map_err(|e| e.to_string()));
            match result {
                Ok(text) if text != self.disk_text => {
                    if self.is_dirty() {
                        self.external_conflict = Some(text);
                    } else {
                        self.reload(text);
                    }
                }
                Err(e) => self.error = Some(e),
                _ => {}
            }
        }
        if changes.rescan
            || self.draft.assets.iter().any(|a| {
                engine_files::VirtualPath::parse(a.path.as_str())
                    .is_ok_and(|p| changes.contains_path_or_parent(&p))
            })
        {
            self.preview_document = None;
            self.images.clear();
        }
    }
    fn reload(&mut self, text: String) {
        match UiDocument::parse(&self.path, &text) {
            Ok(doc) => {
                self.draft = Draft::from_document(&doc);
                self.saved = self.draft.clone();
                self.undo.clear();
                self.redo.clear();
                self.disk_text = text.clone();
                self.source = text;
                self.preview_document = None;
                self.images.clear();
                self.external_conflict = None;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
}

const KINDS: [&str; 12] = [
    "panel",
    "text",
    "button",
    "toggle",
    "slider",
    "progress_bar",
    "input_field",
    "scroll_view",
    "dropdown",
    "grid",
    "horizontal",
    "vertical",
];

/// Entry point from Project; unsaved documents require a choice before replacement.
pub fn open(state: &mut EditorState, path: &str) {
    if state.ui_document.as_ref().is_some_and(|e| e.path == path) {
        state.ui_editor_requested = true;
        return;
    }
    if state
        .ui_document
        .as_ref()
        .is_some_and(CanvasEditor::is_dirty)
    {
        state.ui_pending_path = Some(path.into());
        return;
    }
    if let Some(project) = &state.project {
        match CanvasEditor::open(project, path) {
            Ok(editor) => {
                state.ui_document = Some(editor);
                state.ui_editor_requested = true;
            }
            Err(e) => state.log.error(e),
        }
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(mut editor) = state.ui_document.take() else {
        ui.label("Double-click a .rveui asset or create a New UI Canvas in Project.");
        return;
    };
    let Some(project) = state.project.as_ref() else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(format!(
            "{}{}",
            editor.path,
            if editor.is_dirty() { " *" } else { "" }
        ));
        if ui.button("Save").clicked() {
            editor.error = editor.save(project).err();
        }
        if ui
            .add_enabled(!editor.undo.is_empty(), egui::Button::new("Undo"))
            .clicked()
        {
            editor.undo();
        }
        if ui
            .add_enabled(!editor.redo.is_empty(), egui::Button::new("Redo"))
            .clicked()
        {
            editor.redo();
        }
        egui::ComboBox::from_id_salt("ui_resolution")
            .selected_text("Resolution")
            .show_ui(ui, |ui| {
                for (w, h) in [(1280, 720), (1920, 1080), (800, 600), (720, 1280)] {
                    if ui.button(format!("{w}×{h}")).clicked() {
                        editor.screen = vec2(w as f32, h as f32);
                        ui.close();
                    }
                }
            });
        ui.add(egui::DragValue::new(&mut editor.screen.x).range(1.0..=16384.0));
        ui.add(egui::DragValue::new(&mut editor.screen.y).range(1.0..=16384.0));
        ui.add(egui::Slider::new(&mut editor.zoom, 0.1..=3.0).text("Zoom"));
        if ui.button("Fit").clicked() {
            editor.fit_requested = true;
        }
    });
    if let Some(error) = &editor.error {
        ui.colored_label(Color32::LIGHT_RED, error);
    }
    if editor.external_conflict.is_some() {
        ui.horizontal(|ui| {
            ui.colored_label(Color32::YELLOW, "File changed externally");
            if ui.button("Reload from disk").clicked() {
                let text = editor.external_conflict.take().unwrap();
                editor.reload(text);
            }
            if ui.button("Keep edits").clicked() {
                editor.disk_text = editor.external_conflict.take().unwrap();
            }
        });
    }
    egui::CentralPanel::default().show(ui, |ui| {
        canvas(ui, &mut editor, project);
    });
    state.ui_document = Some(editor);
}

/// Called before scene shortcuts, so Ctrl+S/Z affect the focused UI Editor.
pub fn shortcuts(ctx: &egui::Context, state: &mut EditorState) -> bool {
    if !state.ui_authoring {
        return false;
    }
    let Some(editor) = &mut state.ui_document else {
        return false;
    };
    let cmd = egui::Modifiers::COMMAND;
    if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(cmd, egui::Key::S)))
        && let Some(project) = &state.project
    {
        editor.error = editor.save(project).err();
    }
    if !ctx.egui_wants_keyboard_input() {
        if ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                cmd.plus(egui::Modifiers::SHIFT),
                egui::Key::Z,
            ))
        }) {
            editor.redo();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(cmd, egui::Key::Z))) {
            editor.undo();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(cmd, egui::Key::Y))) {
            editor.redo();
        }
    }
    true
}

pub fn pending_dialog(ctx: &egui::Context, state: &mut EditorState) {
    if state.ui_pending_path.is_none() {
        return;
    }
    let mut choice = 0;
    egui::Modal::new(egui::Id::new("ui_unsaved")).show(ctx, |ui| {
        ui.heading("Unsaved UI changes");
        ui.label("Save this UI before opening another document?");
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                choice = 1;
            }
            if ui.button("Discard").clicked() {
                choice = 2;
            }
            if ui.button("Cancel").clicked() {
                choice = 3;
            }
        });
    });
    if choice == 1
        && let (Some(e), Some(p)) = (&mut state.ui_document, &state.project)
        && let Err(error) = e.save(p)
    {
        e.error = Some(error);
        return;
    }
    if choice == 1 || choice == 2 {
        let path = state.ui_pending_path.take().unwrap();
        state.ui_document = None;
        open(state, &path);
    }
    if choice == 3 {
        state.ui_pending_path = None;
    }
}

pub fn show_hierarchy(ui: &mut egui::Ui, state: &mut EditorState) {
    if let Some(e) = &mut state.ui_document {
        hierarchy(ui, e);
    }
}
pub fn show_inspector(ui: &mut egui::Ui, state: &mut EditorState) {
    if let Some(e) = &mut state.ui_document {
        egui::ScrollArea::vertical().show(ui, |ui| inspector(ui, e));
    }
}
fn hierarchy(ui: &mut egui::Ui, e: &mut CanvasEditor) {
    let before = e.draft.clone();
    ui.menu_button("+ Element", |ui| {
        for kind in KINDS {
            if ui.button(kind).clicked() {
                e.selected = Some(e.draft.add(kind, e.selected.clone()));
                ui.close();
            }
        }
    });
    if ui
        .selectable_label(e.selected.is_none(), "Canvas")
        .clicked()
    {
        e.selected = None;
    }
    let elements = e.draft.elements.clone();
    egui::ScrollArea::vertical().show(ui, |ui| {
        tree_rows(ui, e, &elements, None, 0);
    });
    if let Some(id) = e.selected.clone() {
        ui.horizontal(|ui| {
            if ui.button("Duplicate").clicked() {
                e.selected = e.draft.duplicate(&id);
            }
            if ui.button("Delete").clicked() {
                e.draft.remove(&id);
                e.selected = None;
            }
        });
        ui.horizontal(|ui| {
            for (label, up) in [("↑", true), ("↓", false)] {
                if ui.button(label).clicked() {
                    reorder(&mut e.draft, &id, up);
                }
            }
        });
    }
    e.commit(before);
}
fn tree_rows(
    ui: &mut egui::Ui,
    e: &mut CanvasEditor,
    elements: &[UiElement],
    parent: Option<&str>,
    depth: usize,
) {
    for element in elements.iter().filter(|e| e.parent.as_deref() == parent) {
        let row = ui
            .horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                ui.selectable_label(e.selected.as_deref() == Some(&element.id), &element.id)
            })
            .inner;
        if row.clicked() {
            e.selected = Some(element.id.clone());
        }
        row.dnd_set_drag_payload(element.id.clone());
        if let Some(source) = row.dnd_release_payload::<String>()
            && let Some(item) = e.draft.elements.iter_mut().find(|i| i.id == *source)
        {
            item.parent = Some(element.id.clone());
        }
        tree_rows(ui, e, elements, Some(&element.id), depth + 1);
    }
}
fn reorder(draft: &mut Draft, id: &str, up: bool) {
    let Some(index) = draft.elements.iter().position(|e| e.id == id) else {
        return;
    };
    let parent = &draft.elements[index].parent;
    let siblings: Vec<_> = draft
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| &e.parent == parent)
        .map(|(i, _)| i)
        .collect();
    let pos = siblings.iter().position(|&i| i == index).unwrap();
    let target = if up {
        pos.checked_sub(1)
    } else {
        (pos + 1 < siblings.len()).then_some(pos + 1)
    };
    if let Some(target) = target {
        draft.elements.swap(index, siblings[target]);
    }
}

fn vector(
    ui: &mut egui::Ui,
    name: &str,
    value: &mut [f32; 2],
    range: std::ops::RangeInclusive<f32>,
) {
    ui.horizontal(|ui| {
        ui.label(name);
        for v in value {
            ui.add(egui::DragValue::new(v).speed(0.5).range(range.clone()));
        }
    });
}
fn inspector(ui: &mut egui::Ui, e: &mut CanvasEditor) {
    let before = e.draft.clone();
    let doc = e.draft.document().ok();
    let scale = canvas_scale(&e.draft.canvas, e.screen);
    let size = canvas_size(e.screen, scale);
    let computed = doc.as_ref().map(|d| {
        layout(
            d.tree(),
            d.elements(),
            size,
            &vec![Vec2::ZERO; d.elements().len()],
        )
    });
    if let Some(index) = e
        .selected
        .as_ref()
        .and_then(|id| e.draft.elements.iter().position(|i| &i.id == id))
    {
        let old_id = e.draft.elements[index].id.clone();
        if let Some(new_id) = crate::ui::committed_text_field(
            ui,
            ui.id().with("element_id"),
            &old_id,
            "Element id",
            180.0,
        ) {
            e.draft.rename(&old_id, new_id.clone());
            e.selected = Some(new_id);
        }
        let ids: Vec<_> = e.draft.elements.iter().map(|e| e.id.clone()).collect();
        let parent = e.draft.elements[index]
            .parent
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .and_then(|i| computed.as_ref().map(|l| l.rects[i]))
            .unwrap_or(Rect::from_min_size(Pos2::ZERO, size));
        let element = &mut e.draft.elements[index];
        ui.heading(&element.id);
        ui.checkbox(&mut element.visible, "Visible");
        ui.checkbox(&mut element.interactable, "Interactable");
        egui::ComboBox::from_id_salt("ui_parent")
            .selected_text(element.parent.as_deref().unwrap_or("Canvas"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut element.parent, None, "Canvas");
                for id in &ids {
                    if id != &element.id {
                        ui.selectable_value(&mut element.parent, Some(id.clone()), id);
                    }
                }
            });
        ui.label("Anchor presets (Shift: pivot, Alt: position)");
        egui::Grid::new("anchor_presets").show(ui, |ui| {
            for y in 0..3 {
                for x in 0..3 {
                    if ui
                        .button(["↖", "↑", "↗", "←", "·", "→", "↙", "↓", "↘"][y * 3 + x])
                        .clicked()
                    {
                        let modifiers = ui.input(|i| i.modifiers);
                        anchor_preset(
                            &mut element.rect,
                            parent,
                            [x as f32 * 0.5, y as f32 * 0.5],
                            modifiers.shift,
                            modifiers.alt,
                        );
                    }
                }
                ui.end_row();
            }
        });
        if ui.button("Stretch to parent").clicked() {
            element.rect = RectSpec::default();
        }
        vector(ui, "Anchor min", &mut element.rect.anchor_min, 0.0..=1.0);
        vector(ui, "Anchor max", &mut element.rect.anchor_max, 0.0..=1.0);
        vector(ui, "Pivot", &mut element.rect.pivot, 0.0..=1.0);
        vector(
            ui,
            "Offset min",
            &mut element.rect.offset_min,
            -100000.0..=100000.0,
        );
        vector(
            ui,
            "Offset max",
            &mut element.rect.offset_max,
            -100000.0..=100000.0,
        );
        let (pos, size) = element.rect.position_size(parent);
        let mut p = [pos.x, pos.y];
        let mut s = [size.x, size.y];
        vector(ui, "Position", &mut p, -100000.0..=100000.0);
        vector(ui, "Size", &mut s, 0.0..=100000.0);
        if p != [pos.x, pos.y] || s != [size.x, size.y] {
            element
                .rect
                .set_position_size(parent, pos2(p[0], p[1]), vec2(s[0], s[1]));
        }
        ui.separator();
        // Structured fields follow the serialized widget schema, so every widget uses the same
        // validation and optional state tables remain accessible through Advanced element JSON.
        let mut value = serde_json::to_value(&*element).unwrap();
        if let Some(object) = value.as_object_mut() {
            for (key, value) in object.iter_mut() {
                if !["id", "parent", "rect", "visible", "interactable", "nav"]
                    .contains(&key.as_str())
                {
                    ui.push_id(key, |ui| {
                        fields(ui, key, value);
                    });
                }
            }
        }
        if let Ok(changed) = serde_json::from_value::<UiElement>(value) {
            *element = changed;
        }
        ui.collapsing("Advanced element JSON (assets must be declared)", |ui| {
            let id = ui.id().with(("json", &element.id));
            let mut text = ui
                .ctx()
                .data(|d| d.get_temp::<String>(id))
                .unwrap_or_else(|| serde_json::to_string_pretty(element).unwrap());
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_rows(12)
                    .desired_width(f32::INFINITY),
            );
            if ui.button("Apply element").clicked() {
                match serde_json::from_str(&text) {
                    Ok(new) => *element = new,
                    Err(error) => e.error = Some(error.to_string()),
                }
            }
            ui.ctx().data_mut(|d| d.insert_temp(id, text));
        });
    } else {
        ui.heading("Canvas");
        let mut value = serde_json::to_value(&e.draft.canvas).unwrap();
        fields(ui, "canvas", &mut value);
        if let Ok(canvas) = serde_json::from_value(value) {
            e.draft.canvas = canvas;
        }
    }
    ui.separator();
    ui.collapsing("Images and fonts", |ui| {
        for asset in &e.draft.assets {
            ui.label(asset.path.as_str());
        }
        let id = ui.id().with("asset_path");
        let mut path = ui
            .ctx()
            .data(|d| d.get_temp::<String>(id))
            .unwrap_or_default();
        ui.text_edit_singleline(&mut path);
        if ui.button("Declare PNG / TTF / OTF").clicked() {
            match UiPath::parse(&path) {
                Ok(path) => e.draft.assets.push(UiAsset { path }),
                Err(error) => e.error = Some(error),
            }
        }
        if ui.button("Add image element").clicked() {
            match UiPath::parse(&path) {
                Ok(path) => {
                    if !e.draft.assets.iter().any(|a| a.path == path) {
                        e.draft.assets.push(UiAsset { path: path.clone() });
                    }
                    let id = e.draft.add("panel", e.selected.clone());
                    e.draft.elements.last_mut().unwrap().kind = ElementKind::Image(ImageSpec {
                        path,
                        tint: [255; 4],
                        slice: None,
                        preserve_aspect: true,
                    });
                    e.selected = Some(id);
                }
                Err(error) => e.error = Some(error),
            }
        }
        ui.ctx().data_mut(|d| d.insert_temp(id, path));
    });
    ui.collapsing("Document source", |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut e.source)
                .code_editor()
                .desired_rows(18)
                .desired_width(f32::INFINITY),
        );
        ui.horizontal(|ui| {
            if ui.button("From visual document").clicked()
                && let Ok(doc) = e.draft.document()
            {
                e.source = doc.to_toml().unwrap();
            }
            if ui.button("Apply TOML").clicked() {
                match UiDocument::parse(&e.path, &e.source) {
                    Ok(doc) => e.draft = Draft::from_document(&doc),
                    Err(error) => e.error = Some(error.to_string()),
                }
            }
        });
    });
    e.commit(before);
}
fn fields(ui: &mut egui::Ui, name: &str, value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Bool(v) => {
            ui.checkbox(v, name);
        }
        Value::String(v) => {
            let choices: &[&str] = match name {
                "scale_mode" => &["constant", "screen"],
                "direction" => &["horizontal", "vertical"],
                "h_align" => &["left", "center", "right"],
                "v_align" => &["top", "middle", "bottom"],
                "content" => &["standard", "integer", "decimal", "password"],
                "child_align" => &["start", "center", "end", "stretch"],
                "start_corner" => &["top_left", "top_right", "bottom_left", "bottom_right"],
                _ => &[],
            };
            ui.horizontal(|ui| {
                ui.label(name);
                if choices.is_empty() {
                    ui.text_edit_singleline(v);
                } else {
                    egui::ComboBox::from_id_salt(name)
                        .selected_text(&*v)
                        .show_ui(ui, |ui| {
                            for choice in choices {
                                ui.selectable_value(v, (*choice).into(), *choice);
                            }
                        });
                }
            });
        }
        Value::Number(v) => {
            let mut number = v.as_f64().unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(name);
                if ui
                    .add(egui::DragValue::new(&mut number).speed(0.1))
                    .changed()
                {
                    *v = if v.is_i64() {
                        serde_json::Number::from(number as i64)
                    } else if v.is_u64() {
                        serde_json::Number::from(number.max(0.0) as u64)
                    } else {
                        serde_json::Number::from_f64(number).unwrap_or_else(|| 0.into())
                    };
                }
            });
        }
        Value::Array(values)
            if values.len() == 4
                && (name.contains("color")
                    || ["tint", "normal", "hover", "pressed", "disabled", "focused"]
                        .contains(&name))
                && values.iter().all(|v| v.as_u64().is_some()) =>
        {
            let mut color = [0u8; 4];
            for (out, v) in color.iter_mut().zip(values.iter()) {
                *out = v.as_u64().unwrap().min(255) as u8;
            }
            ui.horizontal(|ui| {
                ui.label(name);
                if ui
                    .color_edit_button_srgba_unmultiplied(&mut color)
                    .changed()
                {
                    *values = color
                        .into_iter()
                        .map(|v| serde_json::Value::from(v as u64))
                        .collect();
                }
            });
        }
        Value::Array(values) => {
            ui.label(name);
            for (i, v) in values.iter_mut().enumerate() {
                ui.push_id((name, i), |ui| {
                    fields(ui, &i.to_string(), v);
                });
            }
        }
        Value::Object(values) => {
            if name == "tint" {
                for state in ["normal", "hover", "pressed", "disabled", "focused"] {
                    if !values.contains_key(state)
                        && ui.small_button(format!("+ {state} tint")).clicked()
                    {
                        values.insert(state.into(), serde_json::json!([200, 200, 200, 255]));
                    }
                }
            }
            ui.collapsing(name, |ui| {
                for (key, v) in values {
                    ui.push_id(key, |ui| fields(ui, key, v));
                }
            });
        }
        Value::Null => {}
    }
}
/// Preserve the visual bounds when changing anchors, unless Alt explicitly resets position.
fn anchor_preset(spec: &mut RectSpec, parent: Rect, anchor: [f32; 2], pivot: bool, position: bool) {
    let rect = resolve_rect(spec, parent);
    spec.anchor_min = anchor;
    spec.anchor_max = anchor;
    if pivot {
        spec.pivot = anchor;
    }
    let point = if position {
        parent.min + parent.size() * vec2(anchor[0], anchor[1])
    } else {
        rect.min + rect.size() * vec2(spec.pivot[0], spec.pivot[1])
    };
    spec.set_position_size(parent, point, rect.size());
}
fn resize_rect(rect: Rect, handle: usize, delta: Vec2) -> Rect {
    let mut min = rect.min;
    let mut max = rect.max;
    if [0, 6, 7].contains(&handle) {
        min.x = (min.x + delta.x).min(max.x);
    }
    if [2, 3, 4].contains(&handle) {
        max.x = (max.x + delta.x).max(min.x);
    }
    if [0, 1, 2].contains(&handle) {
        min.y = (min.y + delta.y).min(max.y);
    }
    if [4, 5, 6].contains(&handle) {
        max.y = (max.y + delta.y).max(min.y);
    }
    Rect::from_min_max(min, max)
}
fn handle_points(r: Rect) -> [Pos2; 8] {
    [
        r.left_top(),
        r.center_top(),
        r.right_top(),
        r.right_center(),
        r.right_bottom(),
        r.center_bottom(),
        r.left_bottom(),
        r.left_center(),
    ]
}
fn canvas(ui: &mut egui::Ui, e: &mut CanvasEditor, project: &OpenProject) {
    let (area, response) = ui.allocate_exact_size(
        ui.available_size().max(vec2(1.0, 1.0)),
        egui::Sense::click_and_drag(),
    );
    if response.dragged_by(egui::PointerButton::Middle) {
        e.pan += ui.input(|i| i.pointer.delta());
    }
    if response.hovered() {
        let zoom = ui.input(|i| i.zoom_delta());
        e.zoom = (e.zoom * zoom).clamp(0.1, 3.0);
    }
    if e.fit_requested {
        e.zoom = ((area.width() - 32.0) / e.screen.x)
            .min((area.height() - 32.0) / e.screen.y)
            .clamp(0.1, 3.0);
        e.pan = Vec2::ZERO;
        e.fit_requested = false;
    }
    let origin = area.min + vec2(16.0, 16.0) + e.pan;
    let screen_rect = Rect::from_min_size(origin, e.screen * e.zoom);
    let painter = ui.painter().with_clip_rect(area);
    painter.rect_filled(screen_rect, 0.0, Color32::from_rgb(20, 23, 28));
    let Ok(doc) = e.draft.document() else {
        return;
    };
    if e.preview_document.as_ref() != Some(&doc) {
        let assets_changed = e
            .preview_document
            .as_ref()
            .is_none_or(|d| d.assets() != doc.assets());
        if assets_changed && let Err(error) = e.refresh_assets(ui.ctx(), project) {
            e.error = Some(error);
        }
        e.system = if assets_changed {
            std::mem::take(&mut e.system)
        } else {
            let mut system = std::mem::take(&mut e.system);
            system.close_owned(0);
            system
        };
        e.system.open(Arc::new(doc.clone()), 0);
        e.preview_document = Some(doc.clone());
    }
    e.system.update(&UiInput::default(), 0.0, e.screen);
    let paint = e.system.paint();
    if e.atlas
        .as_ref()
        .is_none_or(|(version, _)| *version != paint.atlas.version)
        && !paint.atlas.pixels.is_empty()
    {
        let pixels = paint
            .atlas
            .pixels
            .iter()
            .map(|c| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
            .collect();
        let image = egui::ColorImage::new(paint.atlas.size, pixels);
        e.atlas = Some((
            paint.atlas.version,
            ui.ctx()
                .load_texture("ui_document_atlas", image, Default::default()),
        ));
    }
    for batch in &paint.draw_list.batches {
        let texture = match &batch.texture {
            UiTexture::FontAtlas => e.atlas.as_ref().map(|(_, t)| t.id()),
            UiTexture::Image(path) => e.images.get(path).map(|t| t.id()),
        };
        let Some(texture) = texture else {
            continue;
        };
        let mut mesh = egui::Mesh::with_texture(texture);
        mesh.vertices.extend(
            paint.draw_list.vertices[batch.vertices.start as usize..batch.vertices.end as usize]
                .iter()
                .map(|v| egui::epaint::Vertex {
                    pos: origin + vec2(v.pos[0], v.pos[1]) * e.zoom,
                    uv: pos2(v.uv[0], v.uv[1]),
                    color: Color32::from_rgba_premultiplied(
                        v.color[0], v.color[1], v.color[2], v.color[3],
                    ),
                }),
        );
        mesh.indices.extend_from_slice(
            &paint.draw_list.indices[batch.indices.start as usize..batch.indices.end as usize],
        );
        let c = batch.clip_rect;
        let clip = Rect::from_min_max(
            origin + vec2(c[0], c[1]) * e.zoom,
            origin + vec2(c[2], c[3]) * e.zoom,
        );
        painter
            .with_clip_rect(clip.intersect(area))
            .add(egui::Shape::mesh(mesh));
    }
    let scale = canvas_scale(&doc.canvas, e.screen);
    let size = canvas_size(e.screen, scale);
    let factor = e.zoom * scale;
    let computed = layout(
        doc.tree(),
        doc.elements(),
        size,
        &vec![Vec2::ZERO; doc.elements().len()],
    );
    let to_screen = |r: Rect| {
        Rect::from_min_max(
            origin + r.min.to_vec2() * factor,
            origin + r.max.to_vec2() * factor,
        )
    };
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let mut hit_handle = None;
    if let Some(index) = e.selected.as_ref().and_then(|id| doc.element_index(id)) {
        let rect = computed.rects[index];
        let r = to_screen(rect);
        painter.rect_stroke(
            r,
            0.0,
            (1.0, Color32::LIGHT_BLUE),
            egui::StrokeKind::Outside,
        );
        let element = &doc.elements()[index];
        let parent = element
            .parent
            .as_ref()
            .and_then(|id| doc.element_index(id))
            .map(|i| computed.rects[i])
            .unwrap_or(Rect::from_min_size(Pos2::ZERO, size));
        for anchor in [element.rect.anchor_min, element.rect.anchor_max] {
            let point = origin
                + (parent.min.to_vec2() + parent.size() * vec2(anchor[0], anchor[1])) * factor;
            painter.circle_stroke(point, 5.0, (1.0, Color32::YELLOW));
        }
        for (i, point) in handle_points(r).into_iter().enumerate() {
            let handle = Rect::from_center_size(point, Vec2::splat(8.0));
            painter.rect_filled(handle, 0.0, Color32::LIGHT_BLUE);
            if pointer.is_some_and(|p| handle.expand(3.0).contains(p)) {
                hit_handle = Some(i);
            }
        }
    }
    if response.drag_started_by(egui::PointerButton::Primary) || response.clicked() {
        if let Some(index) = e.selected.as_ref().and_then(|id| doc.element_index(id))
            && let Some(handle) = hit_handle
        {
            e.drag = Some((e.draft.clone(), computed.rects[index], handle));
        } else if let Some(point) = pointer {
            e.selected = computed
                .order
                .iter()
                .rev()
                .find(|&&i| {
                    to_screen(computed.rects[i].intersect(computed.clips[i])).contains(point)
                })
                .map(|&i| doc.elements()[i].id.clone());
            if response.drag_started_by(egui::PointerButton::Primary)
                && let Some(index) = e.selected.as_ref().and_then(|id| doc.element_index(id))
            {
                e.drag = Some((e.draft.clone(), computed.rects[index], 8));
            }
        }
    }
    if response.dragged_by(egui::PointerButton::Primary)
        && let Some((before, rect, handle)) = &e.drag
        && let Some(id) = &e.selected
        && let Some(index) = before.elements.iter().position(|i| &i.id == id)
    {
        let delta = response.total_drag_delta().unwrap_or_default() / factor;
        let changed = if *handle == 8 {
            rect.translate(delta)
        } else {
            resize_rect(*rect, *handle, delta)
        };
        let parent = before.elements[index]
            .parent
            .as_ref()
            .and_then(|id| doc.element_index(id))
            .map(|i| computed.rects[i])
            .unwrap_or(Rect::from_min_size(Pos2::ZERO, size));
        let element = &mut e.draft.elements[index];
        let pivot = vec2(element.rect.pivot[0], element.rect.pivot[1]);
        element.rect.set_position_size(
            parent,
            changed.min + changed.size() * pivot,
            changed.size(),
        );
    }
    if response.drag_stopped()
        && let Some((before, _, _)) = e.drag.take()
    {
        e.commit(before);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft() -> Draft {
        Draft::from_document(&UiDocument::parse("test", "[canvas]\n").unwrap())
    }
    struct ProjectFixture(std::path::PathBuf);
    impl ProjectFixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "rve-ui-editor-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("Assets")).unwrap();
            std::fs::write(root.join("project.rveproject"), "id = \"ui-test\"\nname = \"UI Test\"\nversion = \"1.0.0\"\n[[scenes]]\npath = \"/test.rvescene\"\nenabled = true\n").unwrap();
            std::fs::write(root.join("Assets/test.rveui"), "[canvas]\n").unwrap();
            Self(root)
        }
        fn open(&self) -> OpenProject {
            OpenProject::open(&self.0).unwrap()
        }
    }
    impl Drop for ProjectFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn history_rolls_back_invalid_edits_and_tracks_saved_state() {
        let fixture = ProjectFixture::new();
        let project = fixture.open();
        let mut editor = CanvasEditor::open(&project, "test.rveui").unwrap();
        let before = editor.draft.clone();
        editor.draft.add("button", None);
        editor.commit(before);
        assert!(editor.is_dirty());
        assert_eq!(editor.undo.len(), 1);
        let before = editor.draft.clone();
        editor.draft.elements[0].parent = Some(editor.draft.elements[1].id.clone());
        editor.commit(before.clone());
        assert_eq!(editor.draft, before);
        assert_eq!(editor.undo.len(), 1);
        assert!(editor.error.is_some());
        editor.undo();
        assert!(!editor.is_dirty());
        assert!(editor.draft.elements.is_empty());
        editor.redo();
        assert!(editor.is_dirty());
        editor.save(&project).unwrap();
        assert!(!editor.is_dirty());
        let loaded = CanvasEditor::open(&project, "test.rveui").unwrap();
        assert_eq!(loaded.draft, editor.draft);
    }
    #[test]
    fn external_edits_never_silently_overwrite_an_unsaved_document() {
        let fixture = ProjectFixture::new();
        let project = fixture.open();
        let mut editor = CanvasEditor::open(&project, "test.rveui").unwrap();
        let before = editor.draft.clone();
        editor.draft.add("slider", None);
        editor.commit(before);
        let external = "[canvas]\nlayer=7\n";
        std::fs::write(fixture.0.join("Assets/test.rveui"), external).unwrap();
        let changes = crate::asset_watch::AssetChanges {
            rescan: true,
            ..Default::default()
        };
        editor.refresh_external(&project, &changes);
        assert_eq!(editor.external_conflict.as_deref(), Some(external));
        assert_eq!(editor.draft.elements.len(), 1);
        assert!(editor.save(&project).is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("Assets/test.rveui")).unwrap(),
            external
        );
        editor.reload(external.into());
        assert_eq!(editor.draft.canvas.layer, 7);
        assert!(!editor.is_dirty());
    }
    #[test]
    fn visual_preview_uses_game_meshes_and_reuses_an_unchanged_draw_list() {
        let fixture = ProjectFixture::new();
        let project = fixture.open();
        let mut editor = CanvasEditor::open(&project, "test.rveui").unwrap();
        editor.draft.add("button", None);
        let mut state = EditorState::new(None);
        state.project = Some(project);
        state.ui_document = Some(editor);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                show(ui, &mut state);
            });
            assert!(!output.shapes.is_empty());
            output.textures_delta.clear();
        }
        let e = state.ui_document.as_mut().unwrap();
        assert!(e.error.is_none(), "{:?}", e.error);
        let first = e.system.paint();
        let second = e.system.paint();
        assert!(!first.draw_list.vertices.is_empty());
        assert!(Arc::ptr_eq(&first.draw_list, &second.draw_list));
        assert!(e.atlas.is_some());
    }
    #[test]
    fn every_creation_template_is_valid() {
        for kind in KINDS {
            let mut d = draft();
            d.add(kind, None);
            d.document().unwrap();
        }
    }
    #[test]
    fn duplicate_remaps_descendants_and_internal_navigation() {
        let mut d = draft();
        let id = d.add("button", None);
        let label = d.elements[1].id.clone();
        d.elements[0].nav.down = Some(label.clone());
        let copy = d.duplicate(&id).unwrap();
        let doc = d.document().unwrap();
        let index = doc.element_index(&copy).unwrap();
        let target = doc.elements()[index].nav.down.as_ref().unwrap();
        assert_ne!(target, &label);
        assert_eq!(
            doc.elements()[doc.element_index(target).unwrap()]
                .parent
                .as_ref(),
            Some(&copy)
        );
    }
    #[test]
    fn deleting_subtree_clears_focus_and_navigation() {
        let mut d = draft();
        let button = d.add("button", None);
        let other = d.add("toggle", None);
        d.canvas.default_focus = Some(button.clone());
        d.elements
            .iter_mut()
            .find(|e| e.id == other)
            .unwrap()
            .nav
            .up = Some(button.clone());
        d.remove(&button);
        d.document().unwrap();
        assert_eq!(d.elements.len(), 1);
        assert!(d.canvas.default_focus.is_none());
        assert!(d.elements[0].nav.up.is_none());
    }
    #[test]
    fn invalid_reparent_is_rejected_without_mutating_history() {
        let mut d = draft();
        let id = d.add("button", None);
        d.elements[0].parent = Some(d.elements[1].id.clone());
        assert!(d.document().is_err());
        assert_eq!(id, d.elements[0].id);
    }
    #[test]
    fn presets_preserve_bounds_and_alt_places_pivot_at_anchor() {
        let parent = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 600.0));
        let mut spec = RectSpec {
            offset_min: [20.0, 30.0],
            offset_max: [-40.0, -50.0],
            ..Default::default()
        };
        let old = resolve_rect(&spec, parent);
        anchor_preset(&mut spec, parent, [1.0, 0.0], true, false);
        assert_eq!(resolve_rect(&spec, parent), old);
        anchor_preset(&mut spec, parent, [1.0, 1.0], true, true);
        assert_eq!(spec.position_size(parent).0, parent.max);
    }
    #[test]
    fn resize_handles_clamp_without_inverting_opposite_edge() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(100.0, 50.0));
        for h in 0..8 {
            assert!(
                resize_rect(rect, h, vec2(200.0, 200.0)).is_positive()
                    || resize_rect(rect, h, vec2(200.0, 200.0)).width() == 0.0
                    || resize_rect(rect, h, vec2(200.0, 200.0)).height() == 0.0
            );
        }
        assert_eq!(resize_rect(rect, 0, vec2(5.0, 6.0)).max, rect.max);
        assert_eq!(resize_rect(rect, 4, vec2(5.0, 6.0)).min, rect.min);
    }
}
