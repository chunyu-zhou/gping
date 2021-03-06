mod ringbuffer;

use anyhow::{Result, anyhow};
use crossterm::event::{KeyEvent, KeyModifiers};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use dns_lookup::lookup_host;
use histogram::Histogram;
use pinger::{ping, PingResult};
use std::collections::HashMap;
use std::io;
use std::io::Write;
use std::iter;
use std::net::IpAddr;
use std::ops::Add;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use structopt::StructOpt;
use tui::backend::CrosstermBackend;
use tui::layout::{Constraint, Direction, Layout};
use tui::style::{Color, Style};
use tui::text::Span;
use tui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use tui::{symbols, Terminal};

#[derive(Debug, StructOpt)]
#[structopt(name = "gping", about = "Ping, but with a graph.")]
struct Args {
    #[structopt(
        long,
        help = "Command to run, graphing the executing time",
        conflicts_with("hosts")
    )]
    watch: Option<String>,
    #[structopt(
        short = "n",
        long,
        help = "Watch interval seconds (provide partial seconds like '0.5')",
        default_value = "2"
    )]
    watch_interval: f32,
    #[structopt(help = "Hosts or IPs to ping", required_if("watch", ""))]
    hosts: Vec<String>,
    #[structopt(
        short,
        long,
        default_value = "100",
        help = "Determines the number pings to display."
    )]
    buffer: usize,
}

struct App {
    styles: Vec<Style>,
    data: Vec<ringbuffer::FixedRingBuffer<(f64, f64)>>,
    capacity: usize,
    idx: Vec<i64>,
    window_min: Vec<f64>,
    window_max: Vec<f64>,
    map_host_ip: HashMap<String, String>,
}

impl App {
    fn new(thread_count: usize, capacity: usize) -> Self {
        App {
            styles: (0..thread_count)
                .map(|i| Style::default().fg(Color::Indexed(i as u8 + 1)))
                .collect(),
            data: (0..thread_count)
                .map(|_| ringbuffer::FixedRingBuffer::new(capacity))
                .collect(),
            capacity,
            idx: vec![0; thread_count],
            window_min: vec![0.0; thread_count],
            window_max: vec![capacity as f64; thread_count],
            map_host_ip: HashMap::new(),
        }
    }
    fn update(&mut self, host_id: usize, item: Option<Duration>) {
        self.idx[host_id] += 1;
        let data = &mut self.data[host_id];
        if data.len() >= self.capacity {
            self.window_min[host_id] += 1_f64;
            self.window_max[host_id] += 1_f64;
        }
        match item {
            Some(dur) => data.push((self.idx[host_id] as f64, dur.as_micros() as f64)),
            None => data.push((self.idx[host_id] as f64, 0_f64)),
        }
    }
    fn stats(&self) -> Vec<Histogram> {
        self.data
            .iter()
            .map(|data| {
                let mut hist = Histogram::new();

                for (_, val) in data.iter().filter(|v| v.1 != 0f64) {
                    hist.increment(*val as u64).unwrap_or(());
                }

                hist
            })
            .collect()
    }
    fn x_axis_bounds(&self) -> [f64; 2] {
        [
            self.window_min.iter().fold(f64::INFINITY, |a, &b| a.min(b)),
            self.window_max.iter().fold(0f64, |a, &b| a.max(b)),
        ]
    }
    fn y_axis_bounds(&self) -> [f64; 2] {
        let iter = self
            .data
            .iter()
            .map(|b| b.as_slice())
            .flatten()
            .map(|v| v.1);
        let min = iter.clone().fold(f64::INFINITY, |a, b| a.min(b));
        let max = iter.fold(0f64, |a, b| a.max(b));
        // Add a 10% buffer to the top and bottom
        let max_10_percent = (max * 10_f64) / 100_f64;
        let min_10_percent = (min * 10_f64) / 100_f64;
        [min - min_10_percent, max + max_10_percent]
    }
    fn y_axis_labels(&self, bounds: [f64; 2]) -> Vec<Span> {
        // Split into 5 sections
        let min = bounds[0];
        let max = bounds[1];

        let difference = max - min;
        let increment = Duration::from_micros((difference / 3f64) as u64);
        let duration = Duration::from_micros(min as u64);

        (0..7)
            .map(|i| Span::raw(format!("{:?}", duration.add(increment * i))))
            .collect()
    }
    fn get_hosts_ipaddr(&mut self, hosts: &Vec<String>) -> Result<()> {
        for (_host_id, host) in hosts.iter().cloned().enumerate() {
            let ipaddr: Vec<IpAddr> = match lookup_host(&host) {
                Ok(ip) => ip,
                Err(_) => return Err(anyhow!("Could not resolve hostname {}", host)),
            };
            let _ipaddr = ipaddr.first();
            self.map_host_ip.insert(host, _ipaddr.unwrap().to_string());
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Update {
    Result(Duration),
    Timeout,
}

impl From<PingResult> for Update {
    fn from(result: PingResult) -> Self {
        match result {
            PingResult::Pong(duration) => Update::Result(duration),
            PingResult::Timeout => Update::Timeout,
        }
    }
}

#[derive(Debug)]
enum Event {
    Update(usize, Update),
    Input(KeyEvent),
}

fn main() -> Result<()> {
    let args = Args::from_args();
    let num_threads = std::cmp::max(1, args.hosts.len());
    let mut app = App::new(num_threads, args.buffer);
    app.get_hosts_ipaddr(&args.hosts)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;

    terminal.clear()?;

    let (key_tx, rx) = mpsc::channel();

    let mut threads = vec![];

    let killed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    if let Some(ref watch_cmd) = args.watch {
        let cmd_tx = key_tx.clone();
        let killed_cmd = std::sync::Arc::clone(&killed);
        let mut words = watch_cmd.split_ascii_whitespace();
        let cmd = words
            .next()
            .expect("Must specify a command to watch")
            .to_string();
        let cmd_args = words
            .into_iter()
            .map(|w| w.to_string())
            .collect::<Vec<String>>();

        let interval = Duration::from_millis((args.watch_interval * 1000.0) as u64);

        // Pump cmd watches into the queue
        let cmd_thread = thread::spawn(move || -> Result<()> {
            while !killed_cmd.load(Ordering::Acquire) {
                let start = Instant::now();
                let output = Command::new(&cmd).args(&cmd_args).output()?;
                let duration = start.elapsed();
                let update = if output.status.success() {
                    Update::Result(duration)
                } else {
                    Update::Timeout
                };
                cmd_tx.send(Event::Update(0, update))?;
                thread::sleep(interval);
            }
            Ok(())
        });
        threads.push(cmd_thread);
    } else {
        for (host_id, host) in args.hosts.iter().cloned().enumerate() {
            let ping_tx = key_tx.clone();

            let killed_ping = std::sync::Arc::clone(&killed);
            // Pump ping messages into the queue
            let ping_thread = thread::spawn(move || -> Result<()> {
                let stream = ping(host)?;
                while !killed_ping.load(Ordering::Acquire) {
                    ping_tx.send(Event::Update(host_id, stream.recv()?.into()))?;
                }
                Ok(())
            });
            threads.push(ping_thread);
        }
    }

    // Pump keyboard messages into the queue
    let killed_thread = std::sync::Arc::clone(&killed);
    let key_thread = thread::spawn(move || -> Result<()> {
        while !killed_thread.load(Ordering::Acquire) {
            if event::poll(Duration::from_millis(100))? {
                if let CEvent::Key(key) = event::read()? {
                    key_tx.send(Event::Input(key))?;
                }
            }
        }
        Ok(())
    });
    threads.push(key_thread);

    loop {
        match rx.recv()? {
            Event::Update(host_id, update) => {
                match update {
                    Update::Result(duration) => app.update(host_id, Some(duration)),
                    Update::Timeout => app.update(host_id, None),
                };
                terminal.draw(|f| {
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .margin(2)
                        .constraints(
                            iter::repeat(Constraint::Length(1))
                                .take(num_threads)
                                .chain(iter::once(Constraint::Percentage(10)))
                                .collect::<Vec<_>>()
                                .as_ref(),
                        )
                        .split(f.size());
                    let (hosts, action) = if let Some(ref watch_cmd) = args.watch {
                        (vec![watch_cmd.to_string()], "Running")
                    } else {
                        (args.hosts.clone(), "Pinging")
                    };

                    for (((host_id, host), stats), &style) in
                        hosts.iter().enumerate().zip(app.stats()).zip(&app.styles)
                    {
                        let header_layout = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints(
                                [
                                    Constraint::Percentage(25),
                                    Constraint::Percentage(25),
                                    Constraint::Percentage(25),
                                    Constraint::Percentage(25),
                                ]
                                .as_ref(),
                            )
                            .split(chunks[host_id]);

                        let mut ping_text = format!("{} {}", action, host);
                        let real_host = match app.map_host_ip.get::<String>(&host) {
                            Some(ip) => ip,
                            _ => host,
                        }
                        .to_owned();
                        let s = format!(" ({})", real_host);
                        ping_text.push_str(&s.to_string());

                        f.render_widget(Paragraph::new(ping_text).style(style), header_layout[0]);

                        f.render_widget(
                            Paragraph::new(format!(
                                "min {:?}",
                                Duration::from_micros(stats.minimum().unwrap_or(0))
                            ))
                            .style(style),
                            header_layout[1],
                        );
                        f.render_widget(
                            Paragraph::new(format!(
                                "max {:?}",
                                Duration::from_micros(stats.maximum().unwrap_or(0))
                            ))
                            .style(style),
                            header_layout[2],
                        );
                        f.render_widget(
                            Paragraph::new(format!(
                                "p95 {:?}",
                                Duration::from_micros(stats.percentile(95.0).unwrap_or(0))
                            ))
                            .style(style),
                            header_layout[3],
                        );
                    }

                    let datasets: Vec<_> = app
                        .data
                        .iter()
                        .zip(&app.styles)
                        .map(|(data, &style)| {
                            Dataset::default()
                                .marker(symbols::Marker::Braille)
                                .style(style)
                                .graph_type(GraphType::Line)
                                .data(data.as_slice())
                        })
                        .collect();

                    let y_axis_bounds = app.y_axis_bounds();

                    let chart = Chart::new(datasets)
                        .block(Block::default().borders(Borders::NONE))
                        .x_axis(
                            Axis::default()
                                .style(Style::default().fg(Color::Gray))
                                .bounds(app.x_axis_bounds()),
                        )
                        .y_axis(
                            Axis::default()
                                .style(Style::default().fg(Color::Gray))
                                .bounds(y_axis_bounds)
                                .labels(app.y_axis_labels(y_axis_bounds)),
                        );
                    f.render_widget(chart, chunks[num_threads]);
                })?;
            }
            Event::Input(input) => match input.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    killed.store(true, Ordering::Release);
                    break;
                }
                KeyCode::Char('c') if input.modifiers == KeyModifiers::CONTROL => {
                    killed.store(true, Ordering::Release);
                    break;
                }
                _ => {}
            },
        }
    }

    for thread in threads {
        thread.join().unwrap()?;
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
