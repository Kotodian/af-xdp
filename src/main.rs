use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{thread, time};

use afxdp::buf::Buf;
use afxdp::buf_mmap::BufMmap;
use afxdp::buf_pool::BufPool;
use afxdp::buf_pool_vec::BufPoolVec;
use afxdp::mmap_area::{MmapArea, MmapAreaOptions, MmapError};
use afxdp::socket::{Socket, SocketOptions};
use afxdp::umem::Umem;
use afxdp::{link, PENDING_LEN};
use afxdp::{socket::SocketRx, umem::UmemFillQueue};
use arraydeque::{ArrayDeque, Wrapping};
use cli_table::WithTitle;
use cli_table::{format::Justify, Table};
use pnet::packet::ethernet::EthernetPacket;
use rlimit::{setrlimit, Resource};
use rtrb::{Consumer, PopError, Producer, RingBuffer};
use serde::Deserialize;
use structopt::StructOpt;

const RING_SIZE: u32 = 2048;
const SOCKET_BATCH_SIZE: usize = 64;
const FILL_THRESHOLD: usize = 64;

struct State<'a> {
    fq: UmemFillQueue<'a, BufCustom>,
    rx: SocketRx<'a, BufCustom>,
    fq_deficit: usize,
}

#[derive(Default, Debug, Clone, Copy)]
struct Stats {
    cp_bufs_received: usize,
    fq_bufs_filled: usize,
    rx_packets: usize,
    tx_packets: usize,
}

#[derive(Debug, Copy, Clone)]
enum Command {
    Exit,
    GetStats,
}

#[derive(Debug, Default, Copy, Clone, Table)]
struct StatsRow {
    #[table(name = "CPU Core", justify = "Justify::Right")]
    core: usize,
    #[table(name = "Rx Packets")]
    rx_packets: usize,
    #[table(name = "Rx Packet Rate (PPS)")]
    rx_packet_rate: usize,
}

#[derive(Debug, Clone, Copy)]
struct StatsMessage {
    core: usize,
    stats: Stats,
}

#[derive(Debug)]
struct StatState {
    time: time::SystemTime,
    stats: StatsMessage,
}

impl StatState {
    fn diff(&self, b: &StatState) -> StatsRow {
        let mut ret: StatsRow = Default::default();
        let now = time::SystemTime::now();
        let time_diff = now.duration_since(b.time).unwrap().as_secs();

        ret.core = self.stats.core;
        ret.rx_packets = self.stats.stats.rx_packets;

        let diff = self.stats.stats.rx_packets - b.stats.stats.rx_packets;
        ret.rx_packet_rate = diff / time_diff as usize;

        ret
    }
}

#[derive(Debug, Clone, Copy)]
enum Response {
    Stats(StatsMessage),
}

struct WorkerConfig<'a> {
    core: usize,

    link_rx: SocketRx<'a, BufCustom>,
    link_fq: UmemFillQueue<'a, BufCustom>,
}

struct WorkerQueues {
    command_consumer: Consumer<Command>,
    response_producer: Producer<Response>,
}

struct ControllerQueues {
    command_producer: Producer<Command>,
    response_consumer: Consumer<Response>,
}

#[derive(StructOpt, Debug)]
#[structopt(name = "rxdrop")]
struct Opt {
    /// Default buffer size
    #[structopt(long, default_value = "2048")]
    bufsize: usize,

    /// Number of buffers
    #[structopt(long, default_value = "65535")]
    bufnum: usize,

    /// Use HUGE TLB
    #[structopt(long)]
    huge_tlb: bool,

    /// Zero copy mode
    #[structopt(long)]
    zero_copy: bool,

    /// Copy mode
    #[structopt(long, conflicts_with = "zero-copy")]
    copy: bool,

    /// First link name
    #[structopt(long, default_value = "config.yaml")]
    config_file: std::string::String,
}

#[derive(Debug, Default, Copy, Clone)]
struct BufCustom;

/// The loop for each worker
fn do_worker<F>(
    mut config: WorkerConfig,
    mut queues: WorkerQueues,
    bp: Arc<Mutex<BufPoolVec<BufMmap<BufCustom>, BufCustom>>>,
    func: F,
) where
    F: Fn(&Vec<BufMmap<BufCustom>>),
{
    let core = core_affinity::CoreId { id: config.core };
    core_affinity::set_for_current(core);

    let initial_fill_num: usize = RING_SIZE as usize;

    const START_BUFS: usize = 8192;

    let mut bufs = Vec::with_capacity(START_BUFS);

    let r = bp.lock().unwrap().get(&mut bufs, START_BUFS);
    if r != START_BUFS {
        println!(
            "Failed to get initial bufs. Wanted {} got {}",
            START_BUFS, r
        );
    }

    //
    // umem1
    //
    println!("Filling umem1 with {} buffers", initial_fill_num);
    let r = config.link_fq.fill(&mut bufs, initial_fill_num);
    match r {
        Ok(n) => {
            if n != initial_fill_num {
                panic!(
                    "Initial fill of umem1 incomplete: {} of {}",
                    n, initial_fill_num
                );
            }
        }
        Err(err) => println!("error: {:?}", err),
    }

    //
    // The loop
    let mut pending: ArrayDeque<[BufMmap<BufCustom>; PENDING_LEN], Wrapping> = ArrayDeque::new();

    let mut state = State {
        fq: config.link_fq,
        rx: config.link_rx,
        fq_deficit: 0,
    };

    let mut stats: Stats = Default::default();

    let bc = BufCustom {};
    loop {
        //
        // Look for commands from the controller thread only once per servicing both links
        //
        let r = queues.command_consumer.pop();
        match r {
            Err(err) => {
                match err {
                    PopError::Empty => {
                        // Do nothing. This will mostly be empty.
                    }
                }
            }
            Ok(msg) => match msg {
                Command::Exit => {
                    // Time for the worker to exit
                    break;
                }
                Command::GetStats => {
                    let r = queues.response_producer.push(Response::Stats(StatsMessage {
                        core: config.core,
                        stats,
                    }));
                    match r {
                        Ok(_) => {}
                        Err(err) => {
                            println!("error: {}", err);
                        }
                    }
                }
            },
        }

        //
        // Receive
        //
        let r = state.rx.try_recv(&mut pending, SOCKET_BATCH_SIZE, bc);
        match r {
            Ok(n) => {
                if n > 0 {
                    stats.rx_packets += n;

                    state.fq_deficit += n;
                } else if state.fq.needs_wakeup() {
                    state.rx.wake();
                }
            }
            Err(err) => {
                panic!("error: {:?}", err);
            }
        }

        //
        // In this example we're not doing anything with the received frames so just put them
        // back into the pool
        //
        for buf in pending.drain(0..) {
            bufs.push(buf);
        }

        if let Ok(n) = r {
            if n > 0 {
                func(&bufs);
            }
        }

        //
        // Fill buffers if required
        //
        if state.fq_deficit >= FILL_THRESHOLD {
            let r = state.fq.fill(&mut bufs, state.fq_deficit);
            match r {
                Ok(n) => {
                    stats.fq_bufs_filled += n;
                    state.fq_deficit -= n;
                }
                Err(err) => panic!("error: {:?}", err),
            }
        }
    }

    println!("Worker for core {:?} exiting", core.id);
}

fn do_controller(exit: Arc<AtomicBool>, mut queues: Vec<ControllerQueues>) {
    let mut map = HashMap::new();
    let mut pending_stats = Vec::new();

    loop {
        thread::sleep(time::Duration::from_secs(1));

        for qs in &mut queues {
            let r = qs.response_consumer.pop();
            match r {
                Ok(msg) => match msg {
                    Response::Stats(msg) => {
                        pending_stats.push(msg);
                    }
                },
                Err(err) => match err {
                    PopError::Empty => {}
                },
            }
        }

        if pending_stats.len() > 0 {
            let mut rows = Vec::new();

            for stat in &pending_stats {
                let r = map.get(&stat.core);
                match r {
                    Some(old) => {
                        let current = StatState {
                            time: time::SystemTime::now(),
                            stats: stat.clone(),
                        };

                        let d = current.diff(&old);

                        rows.push(d);

                        map.insert(stat.core, current);
                    }
                    None => {
                        let d = StatState {
                            time: time::SystemTime::now(),
                            stats: stat.clone(),
                        };
                        map.insert(stat.core, d);
                    }
                }
            }

            let r = cli_table::print_stdout(rows.with_title());
            match r {
                Ok(_) => {}
                Err(err) => {
                    println!("error: {:?}", err);
                }
            }

            pending_stats.clear();
        }

        if exit.load(Ordering::Relaxed) {
            for qs in &mut queues {
                let r = qs.command_producer.push(Command::Exit);
                match r {
                    Ok(_) => {}
                    Err(err) => println!("error: {:?}", err),
                }
            }

            break;
        }

        for qs in &mut queues {
            let r = qs.command_producer.push(Command::GetStats);
            match r {
                Ok(_) => {}
                Err(err) => println!("error: {:?}", err),
            }
        }
    }
}

#[derive(Debug, PartialEq, Deserialize)]
struct YamlWorker {
    core: usize,
    link_name: String,
    link_channel: usize,
}

fn main() {
    let opt = Opt::from_args();

    let exit = Arc::new(AtomicBool::new(false));
    let r = exit.clone();

    ctrlc::set_handler(move || {
        r.store(true, Ordering::Relaxed);
    })
    .expect("Error setting Ctrl-C handler");
    println!("Ctrl-C to exit");

    assert!(setrlimit(Resource::MEMLOCK, rlimit::INFINITY, rlimit::INFINITY).is_ok());

    let r = File::open(&opt.config_file);
    if let Err(err) = r {
        println!(
            "Error opening YAML config file {} error: {:?}",
            &opt.config_file, err
        );
        println!(
            "Example config:
        - core: 1
          link1_name: enp23s0f0
          link1_channel: 2
          link2_name: enp23s0f1
          link2_channel: 2"
        );
        return;
    }

    let mut file = r.unwrap();
    let mut contents = String::new();
    let r = file.read_to_string(&mut contents);
    if let Err(err) = r {
        println!("Error reading file: {:?}", err);
        return;
    };

    let yaml_worker: Vec<YamlWorker> = serde_yaml::from_str(&contents).unwrap();

    let options: MmapAreaOptions;
    if opt.huge_tlb {
        options = MmapAreaOptions { huge_tlb: true };
    } else {
        options = MmapAreaOptions { huge_tlb: false };
    }

    let r: Result<(Arc<MmapArea<BufCustom>>, Vec<BufMmap<BufCustom>>), MmapError> =
        MmapArea::new(opt.bufnum, opt.bufsize, options);
    let (area, mut bufs) = match r {
        Ok((area, bufs)) => (area, bufs),
        Err(err) => panic!("Failed to create MmapArea: {:?}", err),
    };

    println!(
        "Created MmapArea with {} buffers of size {} for a total of {} bytes",
        bufs.len(),
        opt.bufsize,
        bufs.len() * opt.bufsize,
    );

    let mut bp: BufPoolVec<BufMmap<BufCustom>, BufCustom> = BufPoolVec::new(bufs.len());
    let len = bufs.len();
    let r = bp.put(&mut bufs, len);
    assert!(r == len);

    let bp = Arc::new(Mutex::new(bp));

    let mut workers = Vec::new();
    let mut link_names = Vec::new();
    for w in yaml_worker {
        let r = Umem::new(area.clone(), RING_SIZE, RING_SIZE);
        let (umem1, _, umem1fq) = match r {
            Ok(umem) => umem,
            Err(err) => panic!("Failed to create Umem: {:?}", err),
        };

        let mut options = SocketOptions::default();
        options.zero_copy_mode = true;
        options.copy_mode = false;

        let r = Socket::new(
            umem1.clone(),
            &w.link_name,
            w.link_channel,
            RING_SIZE,
            RING_SIZE,
            options,
        );

        let (_skt1, skt1rx, _) = match r {
            Ok(skt) => skt,
            Err(err) => panic!("Failed to create socket: {:?}", err),
        };

        link_names.push(w.link_name);

        let worker = WorkerConfig {
            core: w.core,
            link_rx: skt1rx,
            link_fq: umem1fq,
        };

        workers.push(worker);
    }

    let mut control_queues: Vec<ControllerQueues> = Vec::with_capacity(workers.len());
    let mut worker_queues: Vec<WorkerQueues> = Vec::with_capacity(workers.len());

    for _ in 0..workers.len() {
        let (command_producer, command_consumer) = RingBuffer::new(2);
        let (response_producer, response_consumer) = RingBuffer::new(2);

        let worker = WorkerQueues {
            command_consumer,
            response_producer,
        };

        worker_queues.push(worker);

        let control = ControllerQueues {
            command_producer,
            response_consumer,
        };

        control_queues.push(control);
    }

    let mut thread_handles = Vec::new();
    for worker in workers {
        let bp2 = bp.clone();
        let worker_queues = worker_queues.remove(0);
        let handle = thread::spawn(|| {
            do_worker(worker, worker_queues, bp2, |bufs| {
                for buf in bufs {
                    let data = buf.get_data();
                    let packet = EthernetPacket::new(data).unwrap();
                    println!("receive packet: {:?}", packet);
                }
            });
        });

        thread_handles.push(handle);
    }

    let controller_handle = thread::spawn(move || {
        let exit = exit.clone();

        do_controller(exit, control_queues);
    });

    let r = controller_handle.join();
    match r {
        Ok(_) => {}
        Err(err) => println!("error: {:?}", err),
    }

    for handle in thread_handles {
        let r = handle.join();
        match r {
            Ok(_) => {}
            Err(err) => println!("error: {:?}", err),
        }
    }

    link_names.sort();
    link_names.dedup();

    for link_name in link_names {
        link::link_fd_xdp(link_name);
    }
}
