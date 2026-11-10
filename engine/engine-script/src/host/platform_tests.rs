use super::*;
use crate::platform::{DialogPoll, FileDialogKind, PlatformRequest};
use engine_scene::{SceneDocument, World};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

fn runner_world() -> World {
    let document = SceneDocument::parse(
        "platform.rvescene",
        "[[assets]]\npath = \"/runner.lua\"\n[[objects]]\nid = \"runner\"\n[[objects.lite_scripts]]\npath = \"/runner.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn load(source: &str) -> (World, ScriptHost) {
    let world = runner_world();
    let sources = BTreeMap::from([("runner.lua".to_owned(), source.to_owned())]);
    let host = ScriptHost::load(&world, "platform", &sources, None).unwrap();
    (world, host)
}

fn messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

fn temp_dir() -> PathBuf {
    let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!("rve-script-fs-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Lua-строка с путём: обратные слеши Windows экранируются.
fn lua_path(path: &std::path::Path) -> String {
    format!("{:?}", path.to_str().unwrap())
}

#[test]
fn lua_fs_reads_writes_lists_and_reports_io_errors_as_values() {
    let directory = temp_dir();
    let nested = directory.join("maps");
    let file = nested.join("a.bin");
    let source = format!(
        r#"
local Script = {{}}
function Script:on_update()
  assert(rve.fs.create_dir({nested}))
  assert(rve.fs.is_dir({nested}))
  assert(rve.fs.write({file}, "ab\0c"))
  assert(rve.fs.append({file}, "d"))
  local data = rve.fs.read({file})
  rve.log("bytes " .. #data .. " " .. data:byte(3))
  assert(rve.fs.exists({file}))
  rve.log("list " .. table.concat(rve.fs.list({nested}), ","))
  local missing, error = rve.fs.read({missing})
  rve.log("missing " .. tostring(missing) .. " " .. tostring(error ~= nil))
  rve.log("bad " .. tostring(pcall(rve.fs.read, 42)))
end
return Script
"#,
        nested = lua_path(&nested),
        file = lua_path(&file),
        missing = lua_path(&directory.join("missing.txt")),
    );
    let (mut world, mut host) = load(&source);

    host.update(&mut world, 0.1, ScriptTime::default());

    let messages = messages(&mut host);
    for expected in ["bytes 5 0", "list a.bin", "missing nil true", "bad false"] {
        assert!(messages.contains(&expected.to_owned()), "{messages:?}");
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"ab\0cd");
    std::fs::remove_dir_all(directory).unwrap();
}

const WINDOW_SCRIPT: &str = r#"
local Script = {}
local frame = 0
function Script:on_update()
  frame = frame + 1
  if frame == 1 then
    rve.log("fullscreen " .. tostring(rve.window.fullscreen()))
    rve.window.set_fullscreen(true)
    rve.log("staged " .. tostring(rve.window.fullscreen()))
    rve.window.screenshot()
    rve.window.screenshot("shots/one.png")
    self.dialog = rve.dialog.save_file({
      title = "Save map",
      file_name = "map.json",
      filters = { { name = "Maps", extensions = { ".json" } } },
    })
    rve.log("pending " .. tostring(rve.dialog.poll(self.dialog)))
  elseif self.dialog then
    local paths = rve.dialog.poll(self.dialog)
    if paths then
      rve.log("picked " .. #paths .. " " .. tostring(paths[1]))
      if frame == 2 then error("fail after reading") end
      self.dialog = nil
    else
      rve.log("waiting")
    end
  else
    rve.log("unknown " .. tostring(pcall(rve.dialog.poll, 1)))
  end
end
return Script
"#;

#[test]
fn lua_window_and_dialog_requests_reach_the_caller_and_results_survive_a_failed_call() {
    let (mut world, mut host) = load(WINDOW_SCRIPT);

    host.update(&mut world, 0.1, ScriptTime::default());
    let first = messages(&mut host);
    assert!(first.contains(&"fullscreen false".to_owned()), "{first:?}");
    assert!(first.contains(&"staged true".to_owned()), "{first:?}");
    assert!(first.contains(&"pending nil".to_owned()), "{first:?}");
    let requests = host.take_platform_requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert_eq!(requests[0], PlatformRequest::SetFullscreen(true));
    assert_eq!(requests[1], PlatformRequest::Screenshot(None));
    assert_eq!(
        requests[2],
        PlatformRequest::Screenshot(Some(PathBuf::from("shots/one.png")))
    );
    let PlatformRequest::FileDialog { id, request } = &requests[3] else {
        panic!("expected a file dialog: {requests:?}");
    };
    assert_eq!(request.kind, FileDialogKind::SaveFile);
    assert_eq!(request.title.as_deref(), Some("Save map"));
    assert_eq!(request.filters[0].extensions, ["json"]);
    assert!(host.take_platform_requests().is_empty());

    host.set_window_fullscreen(true);
    host.complete_file_dialog(*id, vec![PathBuf::from("/maps/map.json")]);
    // Кадр 2 читает результат и падает: чтение откатывается, результат остаётся у хоста.
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(messages(&mut host).contains(&"picked 1 /maps/map.json".to_owned()));
    assert_eq!(
        host.platform_state.borrow().poll(*id),
        Ok(DialogPoll::Done(vec![PathBuf::from("/maps/map.json")]))
    );
}

#[test]
fn consumed_dialog_results_and_creation_time_requests_are_rejected() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
local frame = 0
local ok_create = pcall(rve.window.set_fullscreen, true)
rve.log("create " .. tostring(ok_create))
function Script:on_update()
  frame = frame + 1
  if frame == 1 then
    self.dialog = rve.dialog.pick_folder()
  else
    local ok, paths = pcall(rve.dialog.poll, self.dialog)
    rve.log("poll " .. tostring(ok) .. " " .. (ok and paths and #paths or -1))
  end
end
return Script
"#,
    );
    host.update(&mut world, 0.1, ScriptTime::default());
    let first = messages(&mut host);
    assert!(first.contains(&"create false".to_owned()), "{first:?}");
    let requests = host.take_platform_requests();
    let [PlatformRequest::FileDialog { id, .. }] = requests.as_slice() else {
        panic!("expected one dialog: {requests:?}");
    };
    host.complete_file_dialog(*id, Vec::new());
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(messages(&mut host).contains(&"poll true 0".to_owned()));
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(messages(&mut host).contains(&"poll false -1".to_owned()));
}
