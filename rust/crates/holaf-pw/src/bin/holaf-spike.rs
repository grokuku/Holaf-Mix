//! `holaf-spike` — J1 phase-2 end-to-end PipeWire spike.
//!
//! Proves, **against a real (headless) PipeWire daemon**, every technical
//! brick the Holaf-Mix mixer core needs, mirroring the Python
//! `audio_engine.py` where it matters:
//!
//! ```text
//! step 1  connect + registry discovery
//! step 2  create null-sink "Holaf_Strip_<uid>"   (adapter support.null-audio-sink)
//! step 3  host libpipewire-module-filter-chain in a LOCAL context
//!         (FFI shim pw_context_load_module — pw-cli behaviour, see sys.rs),
//!         with the exact SPA-JSON config of _create_fx_chain
//! step 4  link null-sink input  <-  filter-chain output (link-factory)
//! step 5  EQ hot-reload: Node::set_param(Props) on the input.Holaf_FX_<uid>
//!         stream node with the real mbeq port names
//! step 6  external verification (pw-dump / pw-link / pw-cli enum-params)
//! cleanup remove the objects we created (spike must not leave debris)
//! ```
//!
//! Only **one simple dual-mono EQ chain** is exercised against a real daemon
//! (the full multi-plugin chains, dual-mono *and* true-stereo handling, and
//! the routing reconciler are J2).  Since J2 the filter.graph and the flat
//! Props pairs are produced by the `holaf-core` builders
//! (`holaf_core::spa::{build_graph_with, build_props_with, control_pairs_with}`
//! on an EQ-only active strip), so this spike also validates the code the
//! reconciler will use.
//!
//! Usage (headless environment):
//!
//! ```bash
//! export XDG_RUNTIME_DIR=/tmp/pw-runtime PIPEWIRE_RUNTIME_DIR=/tmp/pw-runtime
//! cargo run -p holaf-pw --bin holaf-spike -- --uid demo1 --action demo
//! # with the module's own INFO logs (proves control values reach the graph):
//! PIPEWIRE_DEBUG=3 cargo run -p holaf-pw --bin holaf-spike -- ...
//! ```
//!
//! Defaults: `uid=demo1`, EQ bands `50Hz=4.1 dB`, `10000Hz=-8.5 dB`.

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;

use pipewire as pw;
use pipewire::properties::properties;
use pw::registry::GlobalObject;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value};

use holaf_core::fx::{self as core_fx, PluginSet};
use holaf_core::model::{EffectKey, EffectState, Strip, StripType};
use holaf_core::spa::{build_graph_with, build_props_with, control_pairs_with};
use holaf_pw::sys::{self, LocalModule};
use holaf_pw::{build_filter_chain_config, build_props_python_style};

/// Registry-global snapshot used for discovery.
type OwnedGlobal = GlobalObject<pw::properties::Properties>;

/// PipeWire objects we create during the run (kept to destroy at cleanup).
#[derive(Default)]
struct OwnedObjects {
    /// null-sink + stream-side proxies we created remotely.
    nodes: Vec<pw::node::Node>,
    /// links we created via link-factory.
    links: Vec<pw::link::Link>,
    /// the locally-hosted filter-chain module.
    module: Option<LocalModule>,
}

struct Session {
    _core: pw::core::Core,
    _mainloop: pw::main_loop::MainLoop,
}

impl Session {
    fn roundtrip(&self) {
        let mainloop = self._mainloop.clone();
        let core = &self._core;
        let done = Rc::new(std::cell::Cell::new(false));
        let done_clone = done.clone();
        let loop_clone = mainloop.clone();
        let pending = core.sync(0).expect("sync failed");
        let _l = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done_clone.set(true);
                    loop_clone.quit();
                }
            })
            .register();
        while !done.get() {
            mainloop.run();
        }
    }

    /// Take a fresh registry snapshot (all current globals).
    ///
    /// A *new* registry proxy is created every call: a PipeWire registry only
    /// replays its current globals to a listener attached at creation time, so
    /// reusing one registry would return stale/empty snapshots.
    fn snapshot(&self) -> Vec<OwnedGlobal> {
        let registry = match self._core.get_registry() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("snapshot: get_registry failed: {e}");
                return Vec::new();
            }
        };
        let list: Rc<RefCell<Vec<OwnedGlobal>>> = Rc::new(RefCell::new(Vec::new()));
        let l2 = list.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |g| {
                l2.borrow_mut().push(g.to_owned());
            })
            .register();
        self.roundtrip();
        std::mem::drop(_listener);
        Rc::try_unwrap(list).ok().unwrap().into_inner()
    }

    /// Bind a registry global to a typed proxy.
    fn bind<T: pw::proxy::ProxyT>(&self, global: &OwnedGlobal) -> Result<T, pw::Error> {
        let registry = self._core.get_registry()?;
        registry.bind(global)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let mut uid = "demo1".to_string();
    let mut eq50 = 4.1f32;
    let mut eq10000 = -8.5f32;
    let mut is_output = true;
    let mut reuse_core = false;
    let mut wp_target = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--uid" => {
                i += 1;
                uid = args[i].clone();
            }
            "--eq50" => {
                i += 1;
                eq50 = args[i].parse()?;
            }
            "--eq10000" => {
                i += 1;
                eq10000 = args[i].parse()?;
            }
            "--flavour" => {
                i += 1;
                is_output = args[i] == "output";
            }
            "--reuse-core" => reuse_core = true,
            "--wp-target" => wp_target = true,
            "--action" => {
                i += 1;
                let _action = &args[i]; // only "demo" exists in this spike
            }
            "--help" | "-h" => {
                println!(
                    "holaf-spike [--uid demo1] [--eq50 4.1] [--eq10000 -8.5] \
                     [--flavour output|input] [--reuse-core] [--wp-target] [--action demo]"
                );
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                return Ok(());
            }
        }
        i += 1;
    }

    demo(&uid, is_output, reuse_core, eq50, eq10000, wp_target)
}

fn demo(
    uid: &str,
    is_output: bool,
    reuse_core: bool,
    eq50: f32,
    eq10000: f32,
    wp_target: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let strip_name = format!("Holaf_Strip_{uid}");
    let fx_name = format!("Holaf_FX_{uid}");
    let in_node = format!("input.{fx_name}");
    let out_node = format!("output.{fx_name}");

    banner(&format!("STEP 1/6 — connect to the PipeWire daemon + discovery (uid={uid}, flavour={}, reuse-core={reuse_core})", if is_output {"output"} else {"input"}));

    pw::init();
    let mainloop = pw::main_loop::MainLoop::new(None)?;
    let context = pw::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;

    if reuse_core {
        // SAFETY: context + core are alive and `core` belongs to `context`.
        unsafe { sys::set_context_core(&context, &core) };
        println!("  [sys] context default core set -> module will reuse OUR connection");
    }

    let session = Session {
        _core: core,
        _mainloop: mainloop,
    };

    // A few roundtrips let discovery / module events settle.
    session.roundtrip();

    // ---------------- STEP 1 : discovery --------------------------------
    let snap = session.snapshot();
    let factories: Vec<&OwnedGlobal> = snap
        .iter()
        .filter(|g| g.type_ == pw::types::ObjectType::Factory)
        .collect();
    let has_adapter = factories
        .iter()
        .any(|g| g.props.as_ref().and_then(|p| p.get("factory.name")) == Some("adapter"));
    let has_link_factory = factories
        .iter()
        .any(|g| g.props.as_ref().and_then(|p| p.get("factory.name")) == Some("link-factory"));
    println!(
        "  discovered {} globals — factory adapter: {}, link-factory: {}",
        snap.len(),
        has_adapter,
        has_link_factory
    );

    // ---------------- cleanup leftovers of previous runs ---------------
    cleanup_stale(&session, uid);
    session.roundtrip();

    // ---------------- STEP 2 : null-sink -------------------------------
    banner("STEP 2/6 — create the null-sink strip node (adapter support.null-audio-sink)");

    let sink = session._core.create_object::<pw::node::Node>(
        "adapter",
        &properties! {
            "factory.name" => "support.null-audio-sink",
            "node.name" => strip_name.clone(),
            "media.class" => "Audio/Sink",
            "audio.channels" => "2",
            "audio.position" => "[ FL, FR ]",
            "object.linger" => "1",
        },
    )?;
    println!("  create_object(adapter, {strip_name}) OK");
    let sink_id = wait_node(&session, &strip_name, 30, 100)
        .ok_or("null-sink node never appeared in the registry")?
        .id;
    println!("  null-sink node id = {sink_id} ({strip_name})");

    let sink_ports = wait_ports(&session, sink_id, true, 30, 100);
    println!(
        "  null-sink input ports (playback): {}",
        fmt_ports(&sink_ports)
    );
    let mut owned = OwnedObjects::default();
    owned.nodes.push(sink);

    // ---------------- STEP 3 : local filter-chain ----------------------
    banner("STEP 3/6 — host libpipewire-module-filter-chain in a LOCAL context (FFI shim)");

    // The chain is loaded with a FLAT EQ (all 0.0 dB, the Python default).
    // STEP 5 will hot-update two bands via set-param so the applied change is
    // visible (0.0 -> eq50 / eq10000) in the module log and enum-params.
    //
    // The filter.graph itself now comes from the J2 holaf-core builder
    // (`build_graph_with` on an EQ-only active strip) instead of the local
    // holaf-pw EQ helpers; plugin availability is restricted to mbeq_1197 so
    // the chain contains exactly the dual-mono EQ (input kind).
    let avail = PluginSet::from_files(&["mbeq_1197"]);
    let initial_strip = eq_strip(&format!("{uid}-initial"), &[]);
    let graph = build_graph_with(&initial_strip, true, &avail);
    // By default the spike does NOT set playback.props target.object, so the
    // link-factory step below is exercised deterministically (WirePlumber
    // auto-links the playback stream to the sink when target.object is set).
    // --wp-target re-enables the faithful Python prop (and the spike then
    // detects/keeps the WirePlumber-created links instead of duplicating).
    let cfg = build_filter_chain_config(
        &fx_name,
        &format!("Holaf FX {uid}"),
        &graph,
        is_output,
        // For an input-flavour chain there is no sensible capture target in
        // this headless spike (no real source).
        if is_output && wp_target {
            Some(&strip_name)
        } else {
            None
        },
    );

    let module = {
        // SAFETY: `context` is alive for the whole demo; the module is
        // destroyed in cleanup before `context` is dropped.
        unsafe {
            sys::load_module(
                &context,
                "libpipewire-module-filter-chain",
                Some(&cfg),
                None,
            )?
        }
    };
    println!("  pw_context_load_module(libpipewire-module-filter-chain) OK");
    println!("  module args (SPA-JSON, matches _create_fx_chain):\n    {cfg}");

    let in_id = wait_node(&session, &in_node, 40, 100)
        .ok_or("filter-chain input stream node never appeared")?
        .id;
    let out_id = wait_node(&session, &out_node, 40, 100)
        .ok_or("filter-chain output stream node never appeared")?
        .id;
    println!("  filter-chain stream nodes: {in_node} -> id {in_id} | {out_node} -> id {out_id}");

    let out_ports = wait_ports(&session, out_id, false, 40, 100);
    println!(
        "  filter-chain output ports (to link): {}",
        fmt_ports(&out_ports)
    );
    owned.module = Some(module);

    // ---------------- STEP 4 : links ------------------------------------
    banner("STEP 4/6 — link null-sink input  <-  filter-chain output (link-factory)");
    let mut created = 0usize;
    for (op, ip) in out_ports.iter().zip(sink_ports.iter()) {
        if link_exists(&session, *op, *ip) {
            println!(
                "  link {}:{op} -> {}:{ip} already present (WirePlumber target.object) — skip",
                out_node, strip_name
            );
            continue;
        }
        let link = session._core.create_object::<pw::link::Link>(
            "link-factory",
            &properties! {
                "link.output.node" => out_id.to_string(),
                "link.output.port" => op.to_string(),
                "link.input.node" => sink_id.to_string(),
                "link.input.port" => ip.to_string(),
                "object.linger" => "1",
            },
        )?;
        println!("  created link {out_node} port {op} -> {strip_name} port {ip}");
        owned.links.push(link);
        created += 1;
    }
    session.roundtrip();
    println!("  link step done ({created} created via link-factory)");

    // ---------------- STEP 5 : EQ hot-reload set-param ------------------
    banner(&format!(
        "STEP 5/6 — EQ hot-reload: set-param Props on {in_node} node id {in_id} \
         (50Hz={eq50}, 10000Hz={eq10000} dB)"
    ));
    let hot_strip = eq_strip(
        &format!("{uid}-hot"),
        &[
            ("50Hz gain (low shelving)", eq50),
            ("10000Hz gain", eq10000),
        ],
    );
    println!(
        "  [python-style nested Props — DOC ONLY, silent no-op on 0.3.65]: {}",
        build_props_python_style(&hot_strip.effects[&EffectKey::Eq].params)
    );
    println!(
        "  [flat params form sent by the Rust spike (J2 holaf-core builder)]: {}",
        build_props_with(&hot_strip, &avail)
    );

    set_chain_params(&session, in_id, &hot_strip, &avail)?;
    session.roundtrip();
    std::thread::sleep(std::time::Duration::from_millis(300));

    // verify the module acknowledged and re-emitted its Props param
    println!(
        "  module INFO log (stderr, PIPEWIRE_DEBUG=3) should contain lines like\n\
         \x20    control 0 ('eq_L:50Hz gain (low shelving)') from 0.000000 to 4.100000\n\
         \x20    control 13 ('eq_L:10000Hz gain') from 0.000000 to -8.500000\n\
         \x20  --- run with PIPEWIRE_DEBUG=3 to see them (they are captured in the\n\
         \x20      process stderr of this run if enabled) ---"
    );
    println!("  external read-back (pw-cli enum-params on node {in_id}):");
    run_enum_show("pw-cli enum-params <in> Props", &in_id.to_string());

    // ---------------- STEP 6 : external verification ---------------------
    banner("STEP 6/6 — external verification (pw-dump / pw-link / pw-cli enum-params)");
    std::thread::sleep(std::time::Duration::from_millis(200));
    run_show("pw-dump | grep Holaf", "pw-dump", &[]);
    run_show("pw-link -l (Holaf lines)", "pw-link", &["-l"]);
    run_enum_show(
        "pw-cli enum-params (repeat, after set-param)",
        &in_id.to_string(),
    );

    // ---------------- cleanup -------------------------------------------
    banner("cleanup — destroy links/nodes + unload local module");

    // 1) unload the local filter-chain module first: its streams (and any
    //    WirePlumber links) disappear with it.
    if let Some(m) = owned.module.take() {
        // SAFETY: deferred destroy from outside a module callback; the main
        // loop iterations below let the context finish the teardown.
        unsafe { m.schedule_destroy() };
    }
    for _ in 0..4 {
        session.roundtrip();
        std::thread::sleep(std::time::Duration::from_millis(40));
    }

    // 2) destroy the links we created via link-factory.
    for link in owned.links {
        if let Err(e) = session._core.destroy_object(link) {
            println!("  [cleanup] destroy link failed: {e}");
        }
    }

    // 3) destroy the nodes we created (null-sink) via the proxies, then sweep
    //    any remaining object matching this run's uid with registry destroy
    //    (robust even if a proxy was already auto-destroyed).
    for node in owned.nodes {
        if let Err(e) = session._core.destroy_object(node) {
            println!("  [cleanup] destroy node failed: {e}");
        }
    }
    for _ in 0..4 {
        session.roundtrip();
        std::thread::sleep(std::time::Duration::from_millis(40));
    }

    sweep_uid(&session, uid);

    std::thread::sleep(std::time::Duration::from_millis(150));
    let snap = session.snapshot();
    let leftovers: Vec<&OwnedGlobal> = snap
        .iter()
        .filter(|g| {
            (g.type_ == pw::types::ObjectType::Node)
                && g.props
                    .as_ref()
                    .and_then(|p| p.get("node.name"))
                    .map(|n| n.contains(uid))
                    .unwrap_or(false)
        })
        .collect();
    println!(
        "  cleanup complete — remaining {} node(s) matching uid '{uid}'",
        leftovers.len()
    );
    if leftovers.is_empty() {
        println!("  CLEAN: no debris left in the daemon.");
    } else {
        for g in &leftovers {
            println!(
                "  leftover: id {} node.name={}",
                g.id,
                prop(g, "node.name").unwrap_or("?")
            );
        }
    }

    // NOTE: pw::deinit() is deliberately NOT called here — PipeWire 0.3.65
    // segfaults during deinit when a context still holds a scheduled-destroy
    // module work item.  Dropping the session (core + mainloop) and context
    // below tears everything down cleanly; the process exits right after.
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn banner(title: &str) {
    println!("\n================ {title} ================");
}

fn prop<'a>(g: &'a OwnedGlobal, key: &str) -> Option<&'a str> {
    g.props.as_ref().and_then(|p| p.get(key))
}

/// Find the registry node global matching `node_name`.
fn find_node<'a>(snap: &'a [OwnedGlobal], node_name: &str) -> Option<&'a OwnedGlobal> {
    snap.iter()
        .find(|g| g.type_ == pw::types::ObjectType::Node && prop(g, "node.name") == Some(node_name))
}

/// Poll for a node global to appear.
fn wait_node(
    session: &Session,
    node_name: &str,
    attempts: u32,
    delay_ms: u64,
) -> Option<OwnedGlobal> {
    for _ in 0..attempts {
        let snap = session.snapshot();
        if let Some(g) = find_node(&snap, node_name) {
            return Some(g.to_owned());
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    None
}

/// Port ids of a node with the given direction ("in" = sink side ports we
/// link *to*, "out" = chain output ports we link *from*).
fn ports_of(snap: &[OwnedGlobal], node_id: u32, direction: &str) -> Vec<u32> {
    let mut v: Vec<(u32, u32)> = snap
        .iter()
        .filter(|g| {
            g.type_ == pw::types::ObjectType::Port
                && prop(g, "node.id") == Some(&node_id.to_string())
                && prop(g, "port.direction") == Some(direction)
        })
        .filter_map(|g| {
            let port_id = prop(g, "port.id")?.parse::<u32>().ok()?;
            Some((g.id, port_id))
        })
        .collect();
    v.sort_by_key(|(_, port_id)| *port_id);
    v.into_iter().map(|(global_id, _)| global_id).collect()
}

fn wait_ports(
    session: &Session,
    node_id: u32,
    direction_in: bool,
    attempts: u32,
    delay_ms: u64,
) -> Vec<u32> {
    let dir = if direction_in { "in" } else { "out" };
    for _ in 0..attempts {
        let snap = session.snapshot();
        let ports = ports_of(&snap, node_id, dir);
        if !ports.is_empty() {
            return ports;
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    Vec::new()
}

fn fmt_ports(ids: &[u32]) -> String {
    format!(
        "[{}]",
        ids.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// An EQ-only active strip used to drive the J2 holaf-core builders.
///
/// `uid` is only the *model* uid (node names are chosen separately in the
/// spike); kind is `Input` so the chain may contain gate…compressor but the
/// availability set restricts it to the dual-mono EQ.  `overrides` sets the
/// user bands (e.g. `50Hz gain (low shelving)` = 4.1) on top of the flat
/// defaults.
fn eq_strip(uid: &str, overrides: &[(&str, f32)]) -> Strip {
    let mut params = core_fx::default_params(EffectKey::Eq);
    for (port, value) in overrides {
        params.insert((*port).to_string(), *value);
    }
    let mut strip = Strip::new(uid, StripType::Input);
    strip.effects.clear();
    strip.effects.insert(
        EffectKey::Eq,
        EffectState {
            active: true,
            params,
        },
    );
    strip
}

fn link_exists(session: &Session, out_port: u32, in_port: u32) -> bool {
    let snap = session.snapshot();
    snap.iter().any(|g| {
        g.type_ == pw::types::ObjectType::Link
            && prop(g, "link.output.port") == Some(&out_port.to_string())
            && prop(g, "link.input.port") == Some(&in_port.to_string())
    })
}

/// Destroy any node global (from a previous run or our own) whose name
/// contains `uid`.  Only works when the current client owns the object (or
/// the daemon allows it) — foreign linger objects from a crashed run may
/// resist; the authoritative reset is restarting the headless daemon.
fn sweep_uid(session: &Session, uid: &str) {
    for _ in 0..4 {
        let snap = session.snapshot();
        let targets: Vec<u32> = snap
            .iter()
            .filter(|g| {
                g.type_ == pw::types::ObjectType::Node
                    && prop(g, "node.name")
                        .map(|n| {
                            (n.starts_with("Holaf_Strip_") || n.starts_with("Holaf_FX_"))
                                && n.contains(uid)
                        })
                        .unwrap_or(false)
            })
            .map(|g| g.id)
            .collect();
        if targets.is_empty() {
            break;
        }
        for id in &targets {
            println!("  cleanup: destroying object id {id}");
            if let Ok(reg) = session._core.get_registry() {
                let _ = reg.destroy_global(*id);
            }
        }
        session.roundtrip();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Destroy any stale node global of a previous run whose name contains `uid`.
/// Only adapter null-sinks survive their creator (object.linger); stream
/// nodes of dead clients are already gone.
fn cleanup_stale(session: &Session, uid: &str) {
    sweep_uid(session, uid);
    session.roundtrip();
}

/// Build the flat `Props` Pod and `set_param(Props)` on the node proxy.
///
/// Format (discovered empirically in phase 2 and confirmed in the
/// `module-filter-chain.c` source, both 0.3.65 and 1.2.5): the module reads
/// `Props.params` — a *struct* of `"<node>:<port>"` / value pairs.  The
/// per-node nested shape the Python `_build_fx_props` emits is a no-op
/// (`spa_json_to_pod` drops unknown keys).  The pairs come from the J2
/// `holaf-core` builder ([`control_pairs_with`]) so the spike exercises the
/// same code the reconciler will use.
fn set_chain_params(
    session: &Session,
    node_id: u32,
    strip: &Strip,
    avail: &PluginSet,
) -> Result<(), Box<dyn std::error::Error>> {
    // Bind the stream node global and hold the proxy for the set_param call.
    let snap = session.snapshot();
    let global = snap
        .iter()
        .find(|g| g.id == node_id && g.type_ == pw::types::ObjectType::Node)
        .ok_or("node to set-param not in registry snapshot")?;

    let node: pw::node::Node = session.bind(global)?;

    let pairs = control_pairs_with(strip, avail);
    let mut struct_values: Vec<Value> = Vec::with_capacity(pairs.len() * 2);
    for (name, value) in &pairs {
        struct_values.push(Value::String(name.clone()));
        struct_values.push(Value::Float(*value));
    }

    let object = Value::Object(pw::spa::pod::Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props as u32,
        id: pw::spa::sys::SPA_PARAM_Props as u32,
        properties: vec![pw::spa::pod::Property::new(
            pw::spa::sys::SPA_PROP_params as u32,
            Value::Struct(struct_values),
        )],
    });

    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .map_err(|e| format!("PodSerializer failed: {e}"))?
        .0
        .into_inner();
    let pod: &Pod = Pod::from_bytes(&bytes).ok_or("built invalid Pod")?;

    node.set_param(pw::spa::param::ParamType::Props, 0, pod);
    println!(
        "  Node::set_param(Props) sent to node {node_id} ({} control pairs)",
        pairs.len()
    );
    Ok(())
}

/// Run `pw-cli enum-params <id> Props` and show the two bands we hot-updated
/// (port name + the value line that follows it).
fn run_enum_show(label: &str, node_id: &str) {
    let out = Command::new("pw-cli")
        .args(["enum-params", node_id, "Props"])
        .output();
    match out {
        Ok(o) => {
            let body = String::from_utf8_lossy(&o.stdout).into_owned();
            let body = if body.is_empty() {
                String::from_utf8_lossy(&o.stderr).into_owned()
            } else {
                body
            };
            let lines: Vec<&str> = body.lines().collect();
            println!("  [{label}]");
            let mut printed = 0usize;
            for (i, l) in lines.iter().enumerate() {
                let is_band = l.contains("eq_L:50Hz gain (low shelving)")
                    || l.contains("eq_R:50Hz gain (low shelving)")
                    || l.contains("eq_L:10000Hz gain")
                    || l.contains("eq_R:10000Hz gain");
                if is_band {
                    // print the String name line + the following value line
                    println!("     {}", l.trim());
                    if let Some(next) = lines.get(i + 1) {
                        if next.contains("Float") || next.contains("Int") {
                            println!("     {}", next.trim());
                        }
                    }
                    printed += 1;
                }
            }
            if printed == 0 {
                println!("     (no band line found — full output below)");
                for l in lines.iter().take(14) {
                    println!("     {l}");
                }
            }
        }
        Err(e) => println!("  [{label}] could not run pw-cli: {e}"),
    }
}

/// Run an external verification command, grepping out the relevant lines.
fn run_show(label: &str, bin: &str, args: &[&str]) {
    let out = Command::new(bin).args(args).output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            let body = if text.is_empty() { err } else { text };
            let interesting: Vec<&str> = body
                .lines()
                .filter(|l| {
                    l.contains("Holaf")
                        || l.contains("spike")
                        || l.contains("eq_L")
                        || l.contains("eq_R")
                })
                .collect();
            println!("  [{label}]");
            if interesting.is_empty() {
                println!("     (no matching line — full output follows)");
                for l in body.lines().take(12) {
                    println!("     {l}");
                }
            } else {
                for l in interesting.iter().take(24) {
                    println!("     {l}");
                }
            }
        }
        Err(e) => println!("  [{label}] could not run '{bin}': {e}"),
    }
}
