//! A pure, daemon-free view of the PipeWire registry used by the reconciler.
//!
//! The reconciler never talks to raw [`pw::registry::GlobalObject`]s directly
//! (that type is `!Send` and requires a live server).  Instead each operation
//! collects a [`LiveView`] from a registry snapshot via [`collect_view`], and
//! every decision (node present? port FL? link already there?) is answered
//! against that pure snapshot.
//!
//! [`collect_view`] is a pure mapping — a unit test can fabricate
//! [`GlobalObject`]s with [`pw::properties::PropertiesBox`] and assert the exact
//! `LiveView` that results, without any daemon.
//!
//! Property keys mirror what a real registry snapshot exposes (verified live
//! against PipeWire 0.3.65 + WirePlumber 0.4.13, see `docs/j1-spike-phase2.md`):
//!
//! - Node globals carry `node.name` and `media.class`.
//! - Port globals carry `node.id`, `port.direction` (`in`/`out`),
//!   `port.id` (channel index), `audio.channel` (`FL`/`FR`/…) and
//!   `port.name` (`playback_FL`, `monitor_FL`, `output_FL`, …).
//! - Link globals carry `link.output.node`, `link.output.port`,
//!   `link.input.node`, `link.input.port` — all **global** object ids of the
//!   port proxies (verified live).

use pipewire::properties::PropertiesBox;
use pipewire::registry::GlobalObject;
use pipewire::types::ObjectType;

/// Direction of a port, from the driver's point of view (`in` = playback /
/// sink-side, `out` = monitor / source-side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDir {
    In,
    Out,
}

impl PortDir {
    pub fn as_str(self) -> &'static str {
        match self {
            PortDir::In => "in",
            PortDir::Out => "out",
        }
    }
}

/// One node global of interest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveNode {
    pub id: u32,
    pub name: String,
    pub media_class: String,
    /// The `holaf.uid` property a bus node carries (see
    /// [`holaf_core::graph::UID_PROP`]) — **diagnostic only**.
    ///
    /// PipeWire exports just a fixed key subset in registry globals, and an
    /// arbitrary key such as `holaf.uid` is not part of it: this field is
    /// therefore `None` for the buses the daemon itself created (measured on
    /// 0.3.65; the key *is* visible in `pw-dump`, which reads the node info).
    /// It is parsed anyway so that a daemon running on a build/future PipeWire
    /// that does export it gets the identity for free — the reconciler never
    /// depends on it (see `holaf_core::graph` module docs).
    pub uid: Option<String>,
}

impl LiveNode {
    /// Is this one of our bus nodes (it carries a `holaf.uid`)?
    pub fn is_ours(&self) -> bool {
        self.uid.is_some()
    }
}

/// One audio port of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePort {
    /// Global (registry) id of the port object — used to address links.
    pub id: u32,
    /// Id of the owning node.
    pub node_id: u32,
    pub dir: PortDir,
    /// Channel index inside the node (`port.id`), 0-based.
    pub port_idx: u32,
    /// `audio.channel`, e.g. `FL`, `FR`, `MONO`.
    pub channel: Option<String>,
    /// `port.name`, e.g. `playback_FL`, `monitor_FL`, `output_FL`.
    pub name: String,
}

/// One audio link between two ports (by global port ids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLink {
    /// Global (registry) id of the link object.
    pub id: u32,
    pub out_node: u32,
    pub out_port: u32,
    pub in_node: u32,
    pub in_port: u32,
}

/// A registry snapshot reduced to the objects the reconciler reasons about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveView {
    nodes: Vec<LiveNode>,
    ports: Vec<LivePort>,
    links: Vec<LiveLink>,
}

fn prop(g: &GlobalObject<PropertiesBox>, key: &str) -> Option<String> {
    g.props
        .as_ref()
        .and_then(|p| p.get(key))
        .map(str::to_string)
}

/// Convert a raw registry snapshot into a [`LiveView`].
///
/// Pure: given the same globals it always produces the same view, so it is
/// unit-tested with fabricated globals and never needs a PipeWire daemon.
pub fn collect_view(globals: &[GlobalObject<PropertiesBox>]) -> LiveView {
    let mut view = LiveView::default();
    for g in globals {
        match g.type_ {
            ObjectType::Node => {
                let Some(name) = prop(g, "node.name") else {
                    continue;
                };
                view.nodes.push(LiveNode {
                    id: g.id,
                    name,
                    media_class: prop(g, "media.class").unwrap_or_default(),
                    uid: prop(g, holaf_core::graph::UID_PROP),
                });
            }
            ObjectType::Port => {
                let Some(node_id) = prop(g, "node.id").and_then(|s| s.parse().ok()) else {
                    continue;
                };
                let dir = match prop(g, "port.direction").as_deref() {
                    Some("in") => PortDir::In,
                    Some("out") => PortDir::Out,
                    _ => continue,
                };
                view.ports.push(LivePort {
                    id: g.id,
                    node_id,
                    dir,
                    port_idx: prop(g, "port.id").and_then(|s| s.parse().ok()).unwrap_or(0),
                    channel: prop(g, "audio.channel"),
                    name: prop(g, "port.name").unwrap_or_default(),
                });
            }
            ObjectType::Link => {
                let (Some(out_node), Some(out_port)) = (
                    prop(g, "link.output.node").and_then(|s| s.parse().ok()),
                    prop(g, "link.output.port").and_then(|s| s.parse().ok()),
                ) else {
                    continue;
                };
                let (Some(in_node), Some(in_port)) = (
                    prop(g, "link.input.node").and_then(|s| s.parse().ok()),
                    prop(g, "link.input.port").and_then(|s| s.parse().ok()),
                ) else {
                    continue;
                };
                view.links.push(LiveLink {
                    id: g.id,
                    out_node,
                    out_port,
                    in_node,
                    in_port,
                });
            }
            _ => {}
        }
    }
    view
}

impl LiveView {
    /// All nodes captured in the snapshot.
    pub fn nodes(&self) -> &[LiveNode] {
        &self.nodes
    }

    /// Node by exact `node.name`.
    pub fn node_named(&self, name: &str) -> Option<&LiveNode> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The bus node that belongs to strip `uid` (whatever its current name).
    pub fn node_for_strip_uid(&self, uid: &str) -> Option<&LiveNode> {
        self.nodes
            .iter()
            .find(|n| n.uid.as_deref() == Some(uid))
    }

    /// All nodes whose name contains `sub` (cleanup sweeps).
    pub fn nodes_containing(&self, sub: &str) -> Vec<&LiveNode> {
        self.nodes.iter().filter(|n| n.name.contains(sub)).collect()
    }

    /// Ports of a node in one direction, sorted by channel index.
    pub fn ports(&self, node_id: u32, dir: PortDir) -> Vec<&LivePort> {
        let mut ports: Vec<&LivePort> = self
            .ports
            .iter()
            .filter(|p| p.node_id == node_id && p.dir == dir)
            .collect();
        ports.sort_by_key(|p| p.port_idx);
        ports
    }

    /// The FL (left) or FR (right) port of a node in one direction, falling
    /// back to the first/second ports by index when channels are unnamed.
    pub fn stereo_ports(&self, node_id: u32, dir: PortDir) -> (Option<&LivePort>, Option<&LivePort>) {
        let ports = self.ports(node_id, dir);
        let by_channel = |c: &str| ports.iter().find(|p| p.channel.as_deref() == Some(c)).copied();
        let left = by_channel("FL").or_else(|| ports.first().copied());
        let right = by_channel("FR").or_else(|| ports.get(1).copied());
        (left, right)
    }

    /// Port global id of a node's FL port in a direction.
    pub fn port_fl(&self, node_id: u32, dir: PortDir) -> Option<u32> {
        self.stereo_ports(node_id, dir).0.map(|p| p.id)
    }

    /// Port global id of a node's FR port in a direction.
    pub fn port_fr(&self, node_id: u32, dir: PortDir) -> Option<u32> {
        self.stereo_ports(node_id, dir).1.map(|p| p.id)
    }

    /// Whether a link between two port global ids already exists.
    pub fn link_exists(&self, out_port: u32, in_port: u32) -> bool {
        self.links
            .iter()
            .any(|l| l.out_port == out_port && l.in_port == in_port)
    }

    /// Links touching a node (as source or target), used for cleanup.
    pub fn links_of_node(&self, node_id: u32) -> impl Iterator<Item = &LiveLink> {
        self.links
            .iter()
            .filter(move |l| l.out_node == node_id || l.in_node == node_id)
    }

    /// The port pairs to link between one source node and one sink node.
    ///
    /// Pure so the *policy* is unit-tested without a graph (the caller resolves
    /// the ids from a live snapshot).  It reproduces
    /// `_auto_link_ports` of the Python engine, which is the reference the
    /// mixer is compared against:
    ///
    /// | source | target | links |
    /// |---|---|---|
    /// | stereo | stereo | `FL→FL`, `FR→FR` |
    /// | mono (or `force_mono`) | stereo | the one channel into **both** target ports |
    /// | stereo | **mono** | `FL→MONO` (one link: the target has one input) |
    /// | mono | mono | the one link |
    ///
    /// Two source channels are **never** linked into the same target port:
    /// PipeWire sums linked inputs, which would add +6 dB on a correlated
    /// signal.
    ///
    /// # Why this is a function (regression)
    ///
    /// The stereo branch used to accept only `(stereo, stereo)` and
    /// `(mono, mono)`; a stereo source feeding a **mono sink** (a Pro Audio
    /// `pro-output-0` node, a mono USB interface, a mono null-sink — one
    /// `playback_MONO` port) produced **no pair at all**: the route stayed
    /// pending, `degraded` became true, and the strip was simply silent, while
    /// the Python engine linked `left → MONO` and played.  Measured on the
    /// headless sandbox: `0 links` with the old rule, `1 link` with this one.
    pub fn port_pairs(
        src_left: Option<u32>,
        src_right: Option<u32>,
        dst_left: Option<u32>,
        dst_right: Option<u32>,
        force_mono: bool,
    ) -> Vec<(u32, u32)> {
        // A strip in MONO, or a node that only publishes one port, feeds every
        // target port from that single channel.
        let single = src_left.or(src_right);
        let duplicate = force_mono || src_left.is_none() || src_right.is_none();
        if duplicate {
            let Some(single) = single else {
                return Vec::new();
            };
            return match (dst_left, dst_right) {
                (Some(dl), Some(dr)) if dl != dr => vec![(single, dl), (single, dr)],
                (Some(dl), _) => vec![(single, dl)],
                (None, Some(dr)) => vec![(single, dr)],
                _ => Vec::new(),
            };
        }
        let (Some(sl), Some(sr)) = (src_left, src_right) else {
            return Vec::new();
        };
        match (dst_left, dst_right) {
            (Some(dl), Some(dr)) if dl == dr => vec![(sl, dl)],
            (Some(dl), Some(dr)) => vec![(sl, dl), (sr, dr)],
            // A mono target: the left channel reaches the only input port.  The
            // right channel has nowhere to go, and must not be summed in.
            (Some(dl), None) => vec![(sl, dl)],
            (None, Some(dr)) => vec![(sr, dr)],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::properties::properties;

    fn node(id: u32, name: &str, media_class: &str) -> GlobalObject<PropertiesBox> {
        GlobalObject {
            id,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Node,
            version: 3,
            props: Some(properties! {
                "node.name" => name,
                "media.class" => media_class,
            }),
        }
    }

    /// A node of ours: a bus name plus the `holaf.uid` marker it carries.
    fn bus(id: u32, name: &str, uid: &str) -> GlobalObject<PropertiesBox> {
        GlobalObject {
            id,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Node,
            version: 3,
            props: Some(properties! {
                "node.name" => name,
                "media.class" => "Audio/Sink",
                holaf_core::graph::UID_PROP => uid,
                holaf_core::graph::KIND_PROP => holaf_core::graph::KIND_BUS,
            }),
        }
    }

    fn port(id: u32, node_id: u32, dir: &str, idx: u32, channel: &str, name: &str) -> GlobalObject<PropertiesBox> {
        GlobalObject {
            id,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Port,
            version: 3,
            props: Some(properties! {
                "node.id" => node_id.to_string(),
                "port.direction" => dir,
                "port.id" => idx.to_string(),
                "audio.channel" => channel,
                "port.name" => name,
            }),
        }
    }

    fn link(id: u32, out_node: u32, out_port: u32, in_node: u32, in_port: u32) -> GlobalObject<PropertiesBox> {
        GlobalObject {
            id,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Link,
            version: 3,
            props: Some(properties! {
                "link.output.node" => out_node.to_string(),
                "link.output.port" => out_port.to_string(),
                "link.input.node" => in_node.to_string(),
                "link.input.port" => in_port.to_string(),
            }),
        }
    }

    #[test]
    fn collect_view_finds_nodes_ports_links_and_ignores_others() {
        // Fake registry as it appears live for a null-sink + chain + links:
        // nodes 10/11, their ports, two links, plus unrelated objects.
        let globals = vec![
            node(10, "Holaf_Strip_a", "Audio/Sink"),
            node(11, "output.Holaf_FX_a", "Stream/Output/Audio"),
            port(100, 10, "in", 0, "FL", "playback_FL"),
            port(101, 10, "in", 1, "FR", "playback_FR"),
            port(102, 10, "out", 0, "FL", "monitor_FL"),
            port(103, 10, "out", 1, "FR", "monitor_FR"),
            port(110, 11, "out", 0, "FL", "output_FL"),
            port(111, 11, "out", 1, "FR", "output_FR"),
            link(120, 11, 110, 10, 100),
            link(121, 11, 111, 10, 101),
            GlobalObject {
                id: 999,
                permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
                type_: ObjectType::Metadata,
                version: 3,
                props: Some(properties! { "metadata.name" => "default" }),
            },
        ];
        let view = collect_view(&globals);

        assert_eq!(view.nodes.len(), 2);
        let sink = view.node_named("Holaf_Strip_a").expect("sink present");
        assert_eq!(sink.id, 10);
        assert_eq!(sink.media_class, "Audio/Sink");

        assert_eq!(view.ports(10, PortDir::In).len(), 2);
        assert_eq!(view.ports(10, PortDir::Out).len(), 2);
        assert_eq!(view.port_fl(10, PortDir::In), Some(100));
        assert_eq!(view.port_fr(10, PortDir::In), Some(101));
        // Chain output → sink input links exist, both directions.
        assert!(view.link_exists(110, 100));
        assert!(view.link_exists(111, 101));
        assert!(!view.link_exists(102, 100), "monitor link must not exist");
        assert_eq!(view.links_of_node(11).count(), 2);
    }

    /// The `holaf.uid` marker is parsed when the registry exposes it (it is
    /// informational — see [`LiveNode::uid`]; the reconciler recognises its
    /// nodes through the naming plan of `holaf_core::graph`).
    #[test]
    fn the_holaf_uid_marker_is_parsed_when_present() {
        let globals = vec![
            bus(10, "Holaf_Bus_Mix_Out", "u-1"),
            // An older build's null-sink: right uid, no marker at all.
            node(11, "Holaf_Strip_u-2", "Audio/Sink"),
            // …and a node that merely *looks* like ours but is not.
            node(12, "Holaf_Bus_Somebody_Else", "Audio/Sink"),
        ];
        let view = collect_view(&globals);

        let mine = view.node_for_strip_uid("u-1").expect("bus of u-1");
        assert_eq!(mine.name, "Holaf_Bus_Mix_Out");
        assert!(mine.is_ours());
        assert!(
            view.node_for_strip_uid("u-2").is_none(),
            "a legacy node carries no uid marker — it is swept by name instead"
        );
        assert!(!view.node_named("Holaf_Bus_Somebody_Else").unwrap().is_ours());
    }

    #[test]
    fn stereo_ports_fall_back_to_channel_index_when_channels_unnamed() {
        let globals = vec![
            port(200, 10, "out", 0, "", "output_0"),
            port(201, 10, "out", 1, "", "output_1"),
        ];
        let view = collect_view(&globals);
        assert_eq!(view.port_fl(10, PortDir::Out), Some(200));
        assert_eq!(view.port_fr(10, PortDir::Out), Some(201));
        assert_eq!(view.port_fl(10, PortDir::In), None, "no in ports");
    }

    /// A **pro-audio** device (the reported `alsa_output.…pro-output-0`, 4
    /// channels) has no `FL`/`FR` channel at all: its ports are
    /// `playback_AUX0..AUX3` with `audio.channel = AUX0..AUX3`.  The index
    /// fallback then targets AUX0/AUX1 — the first two ports — which is
    /// exactly what the classic Python engine's `_auto_link_ports` does too
    /// (its `FL`/`left` matchers miss as well and it takes ports[0]/[1]).  So
    /// both engines wire the *same* pair; this test pins that, because a
    /// change here would silently move the audio to another physical output.
    #[test]
    fn a_pro_audio_device_resolves_to_its_first_two_channels() {
        let globals = vec![
            port(300, 10, "in", 0, "AUX0", "playback_AUX0"),
            port(301, 10, "in", 1, "AUX1", "playback_AUX1"),
            port(302, 10, "in", 2, "AUX2", "playback_AUX2"),
            port(303, 10, "in", 3, "AUX3", "playback_AUX3"),
        ];
        let view = collect_view(&globals);
        assert_eq!(view.port_fl(10, PortDir::In), Some(300));
        assert_eq!(view.port_fr(10, PortDir::In), Some(301));
        // AUX2/AUX3 exist but are not selected — exactly what the live report
        // showed (`output_FL → playback_AUX0`, `output_FR → playback_AUX1`).
        assert!(view.port_fl(10, PortDir::In) != Some(302));
        assert!(view.port_fr(10, PortDir::In) != Some(303));
        assert_eq!(view.ports(10, PortDir::In).len(), 4);
    }

    /// Ports whose channel **is** named FL/FR win over the index order, even
    /// when the node lists them in another order (a device that publishes RL
    /// first must not send the left channel to the right speaker).
    #[test]
    fn named_channels_win_over_the_port_order() {
        let globals = vec![
            port(400, 10, "in", 0, "FR", "playback_FR"),
            port(401, 10, "in", 1, "FL", "playback_FL"),
        ];
        let view = collect_view(&globals);
        assert_eq!(view.port_fl(10, PortDir::In), Some(401));
        assert_eq!(view.port_fr(10, PortDir::In), Some(400));
    }

    /// **The regression that made a whole strip silent**: a stereo source
    /// (a bus, `monitor_FL`/`monitor_FR`) routed to a **mono** sink (a Pro Audio
    /// `pro-output-0` node: one `playback_MONO` port) used to produce *no link
    /// at all*, so `apply_snapshot` reported the route as pending and nothing
    /// reached the device.  The Python engine linked `left → MONO`.
    #[test]
    fn a_stereo_source_reaches_a_mono_sink() {
        assert_eq!(LiveView::port_pairs(Some(1), Some(2), Some(9), None, false), vec![(1, 9)]);
        // A target that publishes only its right port still gets a link.
        assert_eq!(LiveView::port_pairs(Some(1), Some(2), None, Some(9), false), vec![(2, 9)]);
    }

    /// A mono source (a Pro Audio `pro-input-0` capture node, a mono device)
    /// feeding a stereo target is duplicated into both target ports — one link
    /// per port, never two source channels into one port.
    #[test]
    fn a_mono_source_feeds_both_channels_of_a_stereo_target() {
        assert_eq!(
            LiveView::port_pairs(Some(1), None, Some(8), Some(9), false),
            vec![(1, 8), (1, 9)]
        );
        // Only a right port published: use it, do not lose the signal.
        assert_eq!(
            LiveView::port_pairs(None, Some(2), Some(8), Some(9), false),
            vec![(2, 8), (2, 9)]
        );
        // Mono strip (force_mono) with a stereo node: the left channel wins.
        assert_eq!(
            LiveView::port_pairs(Some(1), Some(2), Some(8), Some(9), true),
            vec![(1, 8), (1, 9)]
        );
    }

    #[test]
    fn stereo_to_stereo_keeps_the_channel_pairs_and_never_sums() {
        assert_eq!(
            LiveView::port_pairs(Some(1), Some(2), Some(8), Some(9), false),
            vec![(1, 8), (2, 9)]
        );
        // Mono to mono: exactly one link.
        assert_eq!(LiveView::port_pairs(Some(1), None, Some(9), None, false), vec![(1, 9)]);
        // Nothing to link: no ports on one side.
        assert!(LiveView::port_pairs(None, None, Some(8), Some(9), false).is_empty());
        assert!(LiveView::port_pairs(Some(1), Some(2), None, None, false).is_empty());
        // A node publishing the same port twice would create a duplicate link.
        assert_eq!(LiveView::port_pairs(Some(1), Some(2), Some(9), Some(9), false), vec![(1, 9)]);
    }

    #[test]
    fn node_without_name_or_port_without_node_id_is_skipped() {
        let no_name = GlobalObject {
            id: 1,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Node,
            version: 3,
            props: Some(properties! { "media.class" => "Audio/Sink" }),
        };
        let orphan_port = GlobalObject {
            id: 2,
            permissions: pipewire::permissions::PermissionFlags::from_bits_retain(0),
            type_: ObjectType::Port,
            version: 3,
            props: Some(properties! { "port.direction" => "in" }),
        };
        let view = collect_view(&[no_name, orphan_port]);
        assert!(view.nodes.is_empty());
        assert!(view.ports.is_empty());
    }
}
