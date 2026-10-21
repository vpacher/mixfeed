//! The PipeWire source node "Mixfeed Mic".
//!
//! An output stream with media class `Audio/Source`, 48 kHz, 2 channels,
//! float32. When a client links to it, the stream goes to `Streaming` and the
//! engine starts IO; when the node is idle again, the stream pauses and the
//! engine stops IO (the receiver lingers). The process callback renders on
//! PipeWire's real-time thread with the cycle time (`CLOCK_MONOTONIC`) as
//! host time, the clock the engine uses for the sender estimate.
//!
//! On air: the registry tells which nodes link to this node. The microphone
//! is on air while a node other than the monitor (`monitor::NODE_PREFIX`)
//! takes audio from it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamFlags, StreamState};
use pw::types::ObjectType;
use mixfeed_audio::{CHANNELS, FS};
use spa::pod::Pod;

const NODE_NAME: &str = "mixfeed_mic";

/// Our node's id, once the stream is connected (0 before).
static OUR_NODE: AtomicU32 = AtomicU32::new(0);
static ON_AIR: AtomicBool = AtomicBool::new(false);

/// An application other than the monitor takes audio from the microphone.
pub fn on_air() -> bool {
    ON_AIR.load(Ordering::Relaxed)
}

/// Nodes and links seen in the registry (main loop thread only).
#[derive(Default)]
struct Graph {
    node_names: HashMap<u32, String>,
    /// Link id -> (output node, input node).
    links: HashMap<u32, (u32, u32)>,
}

impl Graph {
    fn update_on_air(&self) {
        let ours = OUR_NODE.load(Ordering::Relaxed);
        let live = ours != 0
            && self.links.values().any(|&(out, inp)| {
                out == ours
                    && !self
                        .node_names
                        .get(&inp)
                        .is_some_and(|n| n.starts_with(crate::monitor::NODE_PREFIX))
            });
        if ON_AIR.swap(live, Ordering::Relaxed) != live {
            eprintln!("on air: {live}");
        }
    }
}
/// Largest cycle the scratch buffer takes without allocating (frames).
const MAX_FRAMES: usize = 8192;

/// Run the PipeWire source until the connection to PipeWire breaks (an error;
/// the caller connects again). Without this, a PipeWire restart (a package
/// update) left the daemon running without its node, and the microphone was
/// gone until a manual restart.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let broken: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let (ml, br) = (mainloop.clone(), broken.clone());
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            // Errors of the core object (id 0) end the connection (e.g. EPIPE).
            if id == 0 {
                br.borrow_mut().get_or_insert_with(|| format!("PipeWire connection: {message} ({res})"));
                ml.quit();
            }
        })
        .register();

    let graph = Rc::new(RefCell::new(Graph::default()));
    let registry = core.get_registry_rc()?;
    let (g_add, g_remove) = (graph.clone(), graph.clone());
    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            let mut g = g_add.borrow_mut();
            let prop = |k: &str| obj.props.as_ref().and_then(|p| p.get(k)).map(str::to_owned);
            match obj.type_ {
                ObjectType::Node => {
                    g.node_names.insert(obj.id, prop("node.name").unwrap_or_default());
                }
                ObjectType::Link => {
                    let id = |k: &str| prop(k).and_then(|v| v.parse::<u32>().ok());
                    if let (Some(out), Some(inp)) = (id("link.output.node"), id("link.input.node")) {
                        g.links.insert(obj.id, (out, inp));
                    }
                }
                _ => return,
            }
            g.update_on_air();
        })
        .global_remove(move |id| {
            let mut g = g_remove.borrow_mut();
            g.node_names.remove(&id);
            g.links.remove(&id);
            g.update_on_air();
        })
        .register();

    let stream = pw::stream::StreamBox::new(
        &core,
        "mixfeed-mic",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "Mixfeed Mic",
            *pw::keys::AUDIO_CHANNELS => "2",
        },
    )?;

    // Each callback owns its state: process runs on the real-time thread,
    // state_changed on the main loop.
    let mut io_running = false;
    let mut scratch = vec![0.0f32; MAX_FRAMES * CHANNELS];
    let (ml, br) = (mainloop.clone(), broken.clone());
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |stream, _, old, new| {
            eprintln!("stream {old:?} -> {new:?}");
            // Back to Unconnected after a connection, or an error: the node is gone.
            let gone = match &new {
                StreamState::Error(e) => Some(format!("stream error: {e}")),
                StreamState::Unconnected if !matches!(old, StreamState::Unconnected) => {
                    Some("stream disconnected".to_owned())
                }
                _ => None,
            };
            if let Some(why) = gone {
                br.borrow_mut().get_or_insert(why);
                ml.quit();
            }
            OUR_NODE.store(stream.node_id(), Ordering::Relaxed);
            let streaming = matches!(new, StreamState::Streaming);
            if streaming && !io_running {
                mixfeed_audio::start_io();
            } else if !streaming && io_running {
                mixfeed_audio::stop_io();
            }
            io_running = streaming;
        })
        .process(move |stream, _| {
            let host_s = stream
                .time()
                .ok()
                .map(|t| t.now())
                .filter(|&ns| ns > 0)
                .map_or_else(mixfeed_audio::host_seconds, |ns| ns as f64 / 1e9);
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let data = &mut buffer.datas_mut()[0];
            let stride = std::mem::size_of::<f32>() * CHANNELS;
            let frames = match data.data() {
                Some(bytes) => {
                    let max = (bytes.len() / stride).min(MAX_FRAMES);
                    let n = if requested > 0 { requested.min(max) } else { max };
                    let out = &mut scratch[..n * CHANNELS];
                    mixfeed_audio::read_input(out, host_s);
                    for (dst, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(out.iter()) {
                        *dst = s.to_le_bytes();
                    }
                    n
                }
                None => 0,
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = (stride * frames) as _;
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(FS);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("audio format serialises")
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).expect("serialised pod is valid")];

    // No AUTOCONNECT: the node waits for clients to link to it.
    stream.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    eprintln!("PipeWire source {NODE_NAME} ready");
    mainloop.run();
    match broken.take() {
        Some(why) => Err(why.into()),
        None => Ok(()),
    }
}
