use super::*;
use atspi::zbus::{self, zvariant::OwnedObjectPath};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct PrivateBus(Child);
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn private_bus() -> (tempfile::TempDir, PrivateBus, String) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("bus.conf");
    std::fs::write(&config, format!(
        "<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
         <policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
        dir.path().join("bus").display()
    )).unwrap();
    let mut child = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("tree tests require a private dbus-daemon; never the desktop bus");
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    assert!(!address.trim().is_empty());
    (dir, PrivateBus(child), address.trim().to_owned())
}

#[derive(Clone)]
struct Node {
    role: &'static str,
    name: String,
    state: Option<StateSet>,
    children: Vec<usize>,
    showing_once: bool,
}

#[derive(Default)]
struct Tree {
    nodes: Vec<Node>,
    visits: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}

fn visible(showing: bool) -> StateSet {
    let mut states = StateSet::empty();
    states.insert(State::Visible);
    states.insert(State::Enabled);
    if showing {
        states.insert(State::Showing);
    }
    states
}

impl Tree {
    fn add(
        &mut self,
        parent: Option<usize>,
        role: &'static str,
        name: &str,
        showing: bool,
    ) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node {
            role,
            name: name.into(),
            state: Some(visible(showing)),
            children: vec![],
            showing_once: false,
        });
        if let Some(parent) = parent {
            self.nodes[parent].children.push(id);
        }
        id
    }
    fn new() -> Self {
        let mut tree = Self::default();
        tree.add(None, "application", "Gecko-like fixture", true);
        tree
    }
    fn window(&mut self) -> usize {
        self.add(Some(0), "frame", "Browser", true)
    }
    fn tab(&mut self, window: usize, name: &str, showing: bool, controls: usize) -> usize {
        let tab = self.add(Some(window), "internal frame", "", true);
        let wrapper = self.add(Some(tab), "panel", "", true);
        let doc = self.add(Some(wrapper), "document web", name, showing);
        for n in 0..controls {
            self.add(
                Some(doc),
                "push button",
                &format!("{name}-control-{n}"),
                showing,
            );
        }
        doc
    }
}

fn path(id: usize) -> String {
    format!("/org/a11y/atspi/accessible/n{id}")
}
fn object(bus: &str, id: usize) -> (String, OwnedObjectPath) {
    (bus.to_owned(), OwnedObjectPath::try_from(path(id)).unwrap())
}
struct Desktop(String);
#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl Desktop {
    fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![object(&self.0, 0)]
    }
}
struct Accessible {
    bus: String,
    node: Node,
    state_reads: AtomicUsize,
    visits: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
}
#[zbus::interface(name = "org.a11y.atspi.Accessible")]
impl Accessible {
    fn get_interfaces(&self) -> Vec<&str> {
        self.visits.fetch_add(1, Ordering::SeqCst);
        if self.node.role == "push button" {
            vec!["org.a11y.atspi.Accessible", "org.a11y.atspi.Component"]
        } else {
            vec!["org.a11y.atspi.Accessible"]
        }
    }
    fn get_role_name(&self) -> &str {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.node.role
    }
    fn get_state(&self) -> zbus::fdo::Result<StateSet> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut state = self
            .node
            .state
            .ok_or_else(|| zbus::fdo::Error::Failed("state unavailable".into()))?;
        if self.node.showing_once && self.state_reads.fetch_add(1, Ordering::SeqCst) > 0 {
            state.remove(State::Showing);
        }
        Ok(state)
    }
    fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.node
            .children
            .iter()
            .map(|id| object(&self.bus, *id))
            .collect()
    }
    #[zbus(property)]
    fn child_count(&self) -> i32 {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.node.children.len() as i32
    }
    #[zbus(property)]
    fn name(&self) -> &str {
        &self.node.name
    }
    #[zbus(property)]
    fn description(&self) -> &str {
        ""
    }
}

async fn walk(tree: Tree, cap: usize) -> (Collected<'static>, usize, usize, Duration) {
    let (_dir, _bus, address) = private_bus();
    let service = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("org.a11y.atspi.Registry")
        .unwrap()
        .build()
        .await
        .unwrap();
    let bus_name = service.unique_name().unwrap().to_string();
    service
        .object_server()
        .at("/org/a11y/atspi/accessible/root", Desktop(bus_name.clone()))
        .await
        .unwrap();
    for (id, node) in tree.nodes.iter().cloned().enumerate() {
        service
            .object_server()
            .at(
                path(id),
                Accessible {
                    bus: bus_name.clone(),
                    node,
                    state_reads: AtomicUsize::new(0),
                    visits: tree.visits.clone(),
                    reads: tree.reads.clone(),
                },
            )
            .await
            .unwrap();
    }
    let conn = AccessibilityConnection::from_address(address.parse().unwrap())
        .await
        .unwrap();
    let start = std::time::Instant::now();
    let collected = OP_DEADLINE
        .scope(
            tokio::time::Instant::now() + Duration::from_secs(20),
            collect_visited_bounded(&conn, std::process::id(), 0, Some(cap), None),
        )
        .await
        .unwrap()
        .expect("fixture application resolved by its real bus PID");
    let elapsed = start.elapsed();
    let visits = tree.visits.load(Ordering::SeqCst);
    let reads = tree.reads.load(Ordering::SeqCst);
    conn.connection().clone().close().await.unwrap();
    service.close().await.unwrap();
    (collected, visits, reads, elapsed)
}

fn names<'a>(walk: &'a Collected<'_>) -> Vec<&'a str> {
    walk.visited.iter().map(|node| node.name.as_str()).collect()
}

#[tokio::test(start_paused = true)]
async fn selected_tab_reached_before_cap_and_background_roots_reported() {
    // This owns selected-tab pruning and the element cap, not D-Bus latency.
    // A loaded runner can legitimately exhaust the 250 ms look-ahead deadline
    // before tab 34: without a showing witness, retaining background contents
    // (and eventually reporting node_budget) is the correct fail-open behavior.
    // Freeze Tokio time without auto-advancing while the real private bus does
    // I/O; leave production deadlines and the full 35-tab tree unchanged.
    let keep_clock_frozen = tokio::spawn(async {
        let wall_start = std::time::Instant::now();
        // If the fixture stalls, stop suppressing auto-advance so the existing
        // operation/call deadlines fail the test instead of spinning forever.
        while wall_start.elapsed() < Duration::from_secs(60) {
            tokio::task::yield_now().await;
        }
    });
    let logical_start = tokio::time::Instant::now();
    for (selected, cap) in [(5, 5000), (0, 200), (5, 200), (34, 200)] {
        let mut tree = Tree::new();
        let window = tree.window();
        for tab in 0..35 {
            tree.tab(window, &format!("tab-{tab}"), tab == selected, 120);
        }
        let (walk, visits, reads, elapsed) = walk(tree, cap).await;
        let (md, nodes) = render(&walk.visited, None);
        eprintln!(
            "TAB_BENCH selected={selected} visits={visits} reads={reads} elapsed_us={} indexed={}",
            elapsed.as_micros(),
            nodes.len()
        );
        assert!(walk.truncation.is_none(), "{:?}", walk.truncation);
        assert_eq!(nodes.len(), 120);
        assert!(nodes.iter().all(|node| node
            .name
            .as_ref()
            .unwrap()
            .starts_with(&format!("tab-{selected}-"))));
        assert_eq!(md.matches("background tab; contents skipped").count(), 34);
        assert!(!md.contains("closed menu"));
        assert_eq!(visits, 226); // frame + 35 * (two wrappers + doc) + selected controls
    }
    assert_eq!(tokio::time::Instant::now(), logical_start);
    keep_clock_frozen.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_showing_document_keeps_background_contents() {
    let mut tree = Tree::new();
    let window = tree.window();
    tree.tab(window, "one", false, 2);
    tree.tab(window, "two", false, 2);
    let (walk, _, _, _) = walk(tree, 200).await;
    assert!(names(&walk).contains(&"one-control-0"));
    assert!(names(&walk).contains(&"two-control-0"));
    assert!(!render(&walk.visited, None).0.contains("background tab"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_nonshowing_iframe_and_offscreen_controls_remain_reachable() {
    let mut tree = Tree::new();
    let window = tree.window();
    let selected = tree.tab(window, "selected", true, 1);
    let iframe = tree.add(Some(selected), "document frame", "offscreen iframe", false);
    tree.add(Some(iframe), "push button", "iframe control", false);
    tree.tab(window, "background", false, 1);
    let (walk, _, _, _) = walk(tree, 200).await;
    assert!(names(&walk).contains(&"iframe control"));
    let (_, nodes) = render(&walk.visited, None);
    let iframe_control = nodes
        .iter()
        .find(|node| node.name.as_deref() == Some("iframe control"))
        .unwrap();
    assert!(iframe_control.in_web_content);
    assert!(iframe_control
        .description
        .as_ref()
        .unwrap()
        .contains("off-screen"));
    assert!(!names(&walk).contains(&"background-control-0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_document_state_and_other_window_showing_are_not_pruning_evidence() {
    let mut tree = Tree::new();
    let first = tree.window();
    tree.tab(first, "one", false, 1);
    let second = tree.window();
    tree.tab(second, "showing elsewhere", true, 1);
    let (walked, _, _, _) = walk(tree, 200).await;
    assert!(names(&walked).contains(&"one-control-0"));
    // Frame filtering must consume but never compact application-wide indices.
    let (_, all) = render(&walked.visited, None);
    let (_, scoped) = render(&walked.visited, Some(1));
    assert_eq!(scoped[0].element_index, all[1].element_index);
    assert_eq!(scoped[0].element_index, Some(1));

    let mut tree = Tree::new();
    let window = tree.window();
    let unknown = tree.tab(window, "unknown", false, 1);
    tree.nodes[unknown].state = None;
    tree.tab(window, "selected", true, 1);
    let (walked, _, _, _) = walk(tree, 200).await;
    assert!(names(&walked).contains(&"unknown-control-0"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_document_and_nonbrowser_apps_unchanged() {
    for showing in [true, false] {
        let mut tree = Tree::new();
        let window = tree.window();
        tree.tab(window, "single", showing, 2);
        let (walked, _, _, _) = walk(tree, 200).await;
        assert_eq!(render(&walked.visited, None).1.len(), 2);
        assert!(!render(&walked.visited, None).0.contains("contents skipped"));
    }
    let mut tree = Tree::new();
    let window = tree.window();
    let panel = tree.add(Some(window), "panel", "offscreen panel", false);
    tree.add(Some(panel), "push button", "offscreen button", false);
    tree.add(Some(window), "push button", "showing button", true);
    let (walked, _, _, _) = walk(tree, 200).await;
    assert_eq!(render(&walked.visited, None).1.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_frame_and_unnamed_hidden_background_root_are_reported() {
    let mut tree = Tree::new();
    let window = tree.window();
    let background = tree.tab(window, "", false, 1);
    tree.nodes[background].role = "document frame";
    tree.nodes[background].state = Some(StateSet::empty());
    let selected = tree.tab(window, "selected", true, 1);
    tree.nodes[selected].role = "document frame";
    let (walked, _, _, _) = walk(tree, 200).await;
    let (md, nodes) = render(&walked.visited, None);
    assert_eq!(nodes.len(), 1);
    assert!(md.contains("document frame = \"\" (background tab; contents skipped)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_or_unknown_showing_witness_preserves_contents() {
    for stale in [true, false] {
        let mut tree = Tree::new();
        let window = tree.window();
        tree.tab(window, "first", false, 1);
        let peer = tree.tab(window, "peer", true, 1);
        if stale {
            tree.nodes[peer].showing_once = true;
        } else {
            tree.nodes[peer].state = None;
        }
        let (walked, _, _, _) = walk(tree, 200).await;
        assert!(names(&walked).contains(&"first-control-0"));
        assert!(!render(&walked.visited, None).0.contains("contents skipped"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peek_does_not_enter_nested_documents_or_exceed_node_budget() {
    let mut tree = Tree::new();
    let window = tree.window();
    let doc = tree.tab(window, "outer", false, 1);
    let other = tree.tab(window, "other outer", false, 0);
    let iframe = tree.add(Some(other), "document web", "showing iframe", true);
    tree.add(Some(iframe), "push button", "iframe control", true);
    let (walked, _, _, _) = walk(tree, 200).await;
    assert!(names(&walked).contains(&"outer-control-0"));
    assert!(walked
        .visited
        .iter()
        .find(|node| node.object_ref.path == path(doc))
        .unwrap()
        .collapsed_children
        .is_none());

    let mut tree = Tree::new();
    let window = tree.window();
    tree.tab(window, "first", false, 1);
    let mut parent = window;
    for _ in 0..PEEK_NODES + 1 {
        parent = tree.add(Some(parent), "panel", "", true);
    }
    tree.tab(parent, "beyond peek", true, 1);
    let (walked, _, _, _) = walk(tree, 200).await;
    assert!(names(&walked).contains(&"first-control-0"));
    assert!(names(&walked).contains(&"beyond peek-control-0"));
}
