use std::sync::{Arc, RwLock};
use std::{io::stdout, time::Instant};
use std::{sync::mpsc, time::Duration};

use ratatui::crossterm::{
  ExecutableCommand,
  event::{self, KeyCode, KeyModifiers},
  terminal,
};
use ratatui::{prelude::*, widgets::*};

use crate::config::{Config, ViewType};
use crate::metrics::{Metrics, NetMetrics, Sampler, zero_div};
use crate::{metrics::MemMetrics, sources::SocInfo};

type WithError<T> = Result<T, Box<dyn std::error::Error>>;

const GB: u64 = 1024 * 1024 * 1024;
const MAX_SPARKLINE: usize = 128;
const MAX_TEMPS: usize = 8;

// MARK: Term utils

fn enter_term() -> Terminal<impl Backend> {
  std::panic::set_hook(Box::new(|info| {
    leave_term();
    eprintln!("{}", info);
  }));

  terminal::enable_raw_mode().unwrap();
  stdout().execute(terminal::EnterAlternateScreen).unwrap();

  let term = CrosstermBackend::new(std::io::stdout());
  Terminal::new(term).unwrap()
}

fn leave_term() {
  terminal::disable_raw_mode().unwrap();
  stdout().execute(terminal::LeaveAlternateScreen).unwrap();
}

// MARK: Storage

#[derive(Debug, Default)]
struct FreqStore {
  items: Vec<u64>, // from 0 to 100
  top_value: u64,
  usage: f64, // from 0.0 to 1.0
}

impl FreqStore {
  fn push(&mut self, value: u64, usage: f64) {
    self.items.insert(0, (usage * 100.0) as u64);
    self.items.truncate(MAX_SPARKLINE);

    self.top_value = value;
    self.usage = usage;
  }
}

#[derive(Debug, Default)]
struct PowerStore {
  items: Vec<u64>,
  top_value: f64,
  max_value: f64,
  avg_value: f64,
}

impl PowerStore {
  fn push(&mut self, value: f64) {
    let was_top = if !self.items.is_empty() { self.items[0] as f64 / 1000.0 } else { 0.0 };

    self.items.insert(0, (value * 1000.0) as u64);
    self.items.truncate(MAX_SPARKLINE);

    self.top_value = avg2(was_top, value);
    self.avg_value = self.items.iter().sum::<u64>() as f64 / self.items.len() as f64 / 1000.0;
    self.max_value = self.items.iter().max().map_or(0, |v| *v) as f64 / 1000.0;
  }
}

#[derive(Debug, Default)]
struct MemoryStore {
  items: Vec<u64>,
  ram_usage: u64,
  ram_total: u64,
  swap_usage: u64,
  swap_total: u64,
  max_ram: u64,
}

impl MemoryStore {
  fn push(&mut self, value: MemMetrics) {
    self.items.insert(0, value.ram_usage);
    self.items.truncate(MAX_SPARKLINE);

    self.ram_usage = value.ram_usage;
    self.ram_total = value.ram_total;
    self.swap_usage = value.swap_usage;
    self.swap_total = value.swap_total;
    self.max_ram = self.items.iter().max().map_or(0, |v| *v);
  }
}

#[derive(Debug, Default)]
struct TempStore {
  items: Vec<f32>,
}

impl TempStore {
  fn last(&self) -> f32 {
    *self.items.first().unwrap_or(&0.0)
  }

  fn push(&mut self, value: f32) {
    // https://www.tunabellysoftware.com/blog/files/tg-pro-apple-silicon-m3-series-support.html
    // https://github.com/vladkens/macmon/issues/12
    let value = if value == 0.0 { self.trend_ema(0.8) } else { value };
    if value == 0.0 {
      return; // skip if not sensor available
    }

    self.items.insert(0, value);
    self.items.truncate(MAX_TEMPS);
  }

  // https://en.wikipedia.org/wiki/Exponential_smoothing
  fn trend_ema(&self, alpha: f32) -> f32 {
    if self.items.len() < 2 {
      return 0.0;
    }

    // starts from most recent value, so need to be reversed
    let mut iter = self.items.iter().rev();
    let mut ema = *iter.next().unwrap_or(&0.0);

    for &item in iter {
      ema = alpha * item + (1.0 - alpha) * ema;
    }

    ema
  }
}

#[derive(Debug, Default)]
struct NetworkStore {
  in_items: Vec<u64>,  // bytes/sec history for sparkline
  out_items: Vec<u64>, // bytes/sec history for sparkline
  prev_in: u64,        // previous cumulative bytes in
  prev_out: u64,       // previous cumulative bytes out
  in_rate: f64,        // current bytes/sec in
  out_rate: f64,       // current bytes/sec out
  max_rate: f64,       // peak rate for sparkline scaling
  initialized: bool,   // first sample just sets prev, shows 0
}

impl NetworkStore {
  fn push(&mut self, value: NetMetrics) {
    if !self.initialized {
      self.prev_in = value.bytes_in;
      self.prev_out = value.bytes_out;
      self.initialized = true;
      return;
    }

    let delta_in = value.bytes_in.saturating_sub(self.prev_in);
    let delta_out = value.bytes_out.saturating_sub(self.prev_out);

    self.prev_in = value.bytes_in;
    self.prev_out = value.bytes_out;

    self.in_rate = avg2(self.in_rate, delta_in as f64);
    self.out_rate = avg2(self.out_rate, delta_out as f64);

    self.in_items.insert(0, delta_in);
    self.in_items.truncate(MAX_SPARKLINE);

    self.out_items.insert(0, delta_out);
    self.out_items.truncate(MAX_SPARKLINE);

    let local_max = self
      .in_items
      .iter()
      .chain(self.out_items.iter())
      .max()
      .map_or(0, |v| *v) as f64;
    self.max_rate = local_max;
  }
}

fn format_rate(bytes_per_sec: f64) -> String {
  const KB: f64 = 1024.0;
  const MB: f64 = 1024.0 * 1024.0;
  const GB: f64 = 1024.0 * 1024.0 * 1024.0;

  if bytes_per_sec >= GB {
    format!("{:.1} GB/s", bytes_per_sec / GB)
  } else if bytes_per_sec >= MB {
    format!("{:.1} MB/s", bytes_per_sec / MB)
  } else if bytes_per_sec >= KB {
    format!("{:.1} KB/s", bytes_per_sec / KB)
  } else {
    format!("{:.0} B/s", bytes_per_sec)
  }
}

// MARK: Components

fn h_stack(area: Rect) -> (Rect, Rect) {
  let ha = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Fill(1), Constraint::Fill(1)].as_ref())
    .split(area);

  (ha[0], ha[1])
}

// MARK: Threads

enum Event {
  Update(Metrics),
  ChangeColor,
  ChangeView,
  IncInterval,
  DecInterval,
  Tick,
  Quit,
}

fn handle_key_event(key: &event::KeyEvent, tx: &mpsc::Sender<Event>) -> WithError<()> {
  match key.code {
    KeyCode::Char('q') => Ok(tx.send(Event::Quit)?),
    KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => Ok(tx.send(Event::Quit)?),
    KeyCode::Char('c') => Ok(tx.send(Event::ChangeColor)?),
    KeyCode::Char('v') => Ok(tx.send(Event::ChangeView)?),
    KeyCode::Char('+') => Ok(tx.send(Event::IncInterval)?),
    KeyCode::Char('=') => Ok(tx.send(Event::IncInterval)?), // fallback to press without shift
    KeyCode::Char('-') => Ok(tx.send(Event::DecInterval)?),
    _ => Ok(()),
  }
}

fn run_inputs_thread(tx: mpsc::Sender<Event>, tick: u64) {
  let tick_rate = Duration::from_millis(tick);

  std::thread::spawn(move || {
    let mut last_tick = Instant::now();

    loop {
      if event::poll(Duration::from_millis(tick)).unwrap() {
        match event::read().unwrap() {
          event::Event::Key(key) => handle_key_event(&key, &tx).unwrap(),
          _ => {}
        };
      }

      if last_tick.elapsed() >= tick_rate {
        tx.send(Event::Tick).unwrap();
        last_tick = Instant::now();
      }
    }
  });
}

fn run_sampler_thread(tx: mpsc::Sender<Event>, msec: Arc<RwLock<u32>>) {
  std::thread::spawn(move || {
    let mut sampler = Sampler::new().unwrap();

    // Send initial metrics
    tx.send(Event::Update(sampler.get_metrics(100).unwrap())).unwrap();

    loop {
      let msec = *msec.read().unwrap();
      tx.send(Event::Update(sampler.get_metrics(msec).unwrap())).unwrap();
    }
  });
}

// get average of two values, used to smooth out metrics
// see: https://github.com/vladkens/macmon/issues/10
fn avg2<T: num_traits::Float>(a: T, b: T) -> T {
  if a == T::zero() { b } else { (a + b) / T::from(2.0).unwrap() }
}

// MARK: App

#[derive(Debug, Default)]
pub struct App {
  cfg: Config,

  soc: SocInfo,
  mem: MemoryStore,

  cpu_power: PowerStore,
  gpu_power: PowerStore,
  ane_power: PowerStore,
  all_power: PowerStore,
  sys_power: PowerStore,

  cpu_temp: TempStore,
  gpu_temp: TempStore,

  ecpu_freq: FreqStore,
  pcpu_freq: FreqStore,
  igpu_freq: FreqStore,

  net: NetworkStore,
}

impl App {
  pub fn new() -> WithError<Self> {
    let soc = SocInfo::new()?;
    let cfg = Config::load();
    Ok(Self { cfg, soc, ..Default::default() })
  }

  pub fn cfg_mut(&mut self) -> &mut Config {
    &mut self.cfg
  }

  fn update_metrics(&mut self, data: Metrics) {
    self.cpu_power.push(data.cpu_power as f64);
    self.gpu_power.push(data.gpu_power as f64);
    self.ane_power.push(data.ane_power as f64);
    self.all_power.push(data.all_power as f64);
    self.sys_power.push(data.sys_power as f64);
    self.ecpu_freq.push(data.ecpu_usage.0 as u64, data.ecpu_usage.1 as f64);
    self.pcpu_freq.push(data.pcpu_usage.0 as u64, data.pcpu_usage.1 as f64);
    self.igpu_freq.push(data.gpu_usage.0 as u64, data.gpu_usage.1 as f64);

    self.cpu_temp.push(data.temp.cpu_temp_avg);
    self.gpu_temp.push(data.temp.gpu_temp_avg);

    self.mem.push(data.memory);
    self.net.push(data.network);
  }

  // Individual metric boxes: full rounded border, labels on the top edge.
  fn title_block<'a>(&self, label_l: &str, label_r: &str) -> Block<'a> {
    let mut block = Block::new()
      .borders(Borders::ALL)
      .border_type(BorderType::Rounded)
      .border_style(self.cfg.color)
      // .title_style(Style::default().gray())
      .padding(Padding::ZERO);

    if !label_l.is_empty() {
      block = block.title_top(Line::from(format!(" {label_l} ")));
    }

    if !label_r.is_empty() {
      block = block.title_top(Line::from(format!(" {label_r} ")).alignment(Alignment::Right));
    }

    block
  }

  // Section headers (machine specs, power summary) are a bare label line with no box
  // around the children. Titles don't reserve space in ratatui, so carve off one row
  // explicitly and return the remainder for the child blocks.
  fn render_section_title(&self, f: &mut Frame, area: Rect, label_l: &str, label_r: &str) -> Rect {
    if area.height == 0 {
      return area;
    }

    let rows = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Length(1), Constraint::Fill(1)].as_ref())
      .split(area);

    let mut block = Block::new().borders(Borders::NONE).style(self.cfg.color);

    if !label_l.is_empty() {
      block = block.title_top(Line::from(format!(" {label_l} ")));
    }

    if !label_r.is_empty() {
      block = block.title_top(Line::from(format!(" {label_r} ")).alignment(Alignment::Right));
    }

    f.render_widget(block, rows[0]);
    rows[1]
  }

  fn get_power_block<'a>(&self, label: &str, val: &'a PowerStore, temp: f32) -> Sparkline<'a> {
    let label_l = format!(
      "{} {:.2}W ({:.2}, {:.2})",
      // "{} {:.2}W (avg: {:.2}W, max: {:.2}W)",
      // "{} {:.2}W (~{:.2}W ^{:.2}W)",
      label,
      val.top_value,
      val.avg_value,
      val.max_value
    );

    let label_r = if temp > 0.0 { format!("{:.1}°C", temp) } else { "".to_string() };

    Sparkline::default()
      .block(self.title_block(label_l.as_str(), label_r.as_str()))
      .direction(RenderDirection::RightToLeft)
      .data(&val.items)
      .style(self.cfg.color)
  }

  fn render_freq_block(&self, f: &mut Frame, r: Rect, label: &str, val: &FreqStore) {
    let label = format!("{} {:3.0}% @ {:4.0} MHz", label, val.usage * 100.0, val.top_value);
    let block = self.title_block(label.as_str(), "");

    match self.cfg.view_type {
      ViewType::Sparkline => {
        let w = Sparkline::default()
          .block(block)
          .direction(RenderDirection::RightToLeft)
          .data(&val.items)
          .max(100)
          .style(self.cfg.color);
        f.render_widget(w, r);
      }
      ViewType::Gauge => {
        let w = Gauge::default()
          .block(block)
          .gauge_style(self.cfg.color)
          .style(self.cfg.color)
          .label("")
          .ratio(val.usage);
        f.render_widget(w, r);
      }
    }
  }

  fn render_mem_block(&self, f: &mut Frame, r: Rect, val: &MemoryStore) {
    let ram_usage_gb = val.ram_usage as f64 / GB as f64;
    let ram_total_gb = val.ram_total as f64 / GB as f64;

    let swap_usage_gb = val.swap_usage as f64 / GB as f64;
    let swap_total_gb = val.swap_total as f64 / GB as f64;

    let label_l = format!("RAM {:4.2} / {:4.1} GB", ram_usage_gb, ram_total_gb);
    let label_r = format!("SWAP {:.2} / {:.1} GB", swap_usage_gb, swap_total_gb);

    let block = self.title_block(label_l.as_str(), label_r.as_str());
    match self.cfg.view_type {
      ViewType::Sparkline => {
        let w = Sparkline::default()
          .block(block)
          .direction(RenderDirection::RightToLeft)
          .data(&val.items)
          .max(val.ram_total)
          .style(self.cfg.color);
        f.render_widget(w, r);
      }
      ViewType::Gauge => {
        let w = Gauge::default()
          .block(block)
          .gauge_style(self.cfg.color)
          .style(self.cfg.color)
          .label("")
          .ratio(zero_div(ram_usage_gb, ram_total_gb));
        f.render_widget(w, r);
      }
    }
  }

  fn render_net_block(&self, f: &mut Frame, r: Rect, val: &NetworkStore) {
    let (c1, c2) = h_stack(r);
    let max = val.max_rate.max(1.0) as u64;

    let label_in = format!("Net ▼ {}", format_rate(val.in_rate));
    let w_in = Sparkline::default()
      .block(self.title_block(label_in.as_str(), ""))
      .direction(RenderDirection::RightToLeft)
      .data(&val.in_items)
      .max(max)
      .style(self.cfg.color);
    f.render_widget(w_in, c1);

    let label_out = format!("Net ▲ {}", format_rate(val.out_rate));
    let w_out = Sparkline::default()
      .block(self.title_block(label_out.as_str(), ""))
      .direction(RenderDirection::RightToLeft)
      .data(&val.out_items)
      .max(max)
      .style(self.cfg.color);
    f.render_widget(w_out, c2);
  }

  fn render(&mut self, f: &mut Frame) {
    let show_top = self.cfg.show_cpu || self.cfg.show_gpu || self.cfg.show_mem;
    let show_net = self.cfg.show_net;
    let show_power = self.cfg.show_power;

    // Build dynamic row constraints
    let mut constraints: Vec<Constraint> = Vec::new();
    if show_top { constraints.push(Constraint::Fill(2)); }
    if show_net { constraints.push(Constraint::Fill(1)); }
    if show_power { constraints.push(Constraint::Fill(1)); }
    if constraints.is_empty() { constraints.push(Constraint::Fill(1)); }

    let rows = Layout::default()
      .direction(Direction::Vertical)
      .constraints(constraints)
      .split(f.area());

    let mut row_idx = 0;

    // Top block: CPU/GPU/Mem
    if show_top {
      let iarea = if self.cfg.show_title {
        let label_l = format!(
          "{} ({}E+{}P+{}GPU {}GB)",
          self.soc.chip_name,
          self.soc.ecpu_cores,
          self.soc.pcpu_cores,
          self.soc.gpu_cores,
          self.soc.memory_gb,
        );

        let brand = format!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        self.render_section_title(f, rows[row_idx], &label_l, &brand)
      } else {
        rows[row_idx]
      };

      // Determine sub-rows based on what's visible
      let show_row1 = self.cfg.show_cpu;
      let show_row2 = self.cfg.show_mem || self.cfg.show_gpu;

      let sub_constraints: Vec<Constraint> = [show_row1, show_row2]
        .iter()
        .filter(|&&v| v)
        .map(|_| Constraint::Fill(1))
        .collect();

      let sub_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(sub_constraints)
        .split(iarea);

      let mut sub_idx = 0;

      if show_row1 {
        let (c1, c2) = h_stack(sub_rows[sub_idx]);
        self.render_freq_block(f, c1, "E-CPU", &self.ecpu_freq);
        self.render_freq_block(f, c2, "P-CPU", &self.pcpu_freq);
        sub_idx += 1;
      }

      if show_row2 {
        let (c1, c2) = h_stack(sub_rows[sub_idx]);
        if self.cfg.show_mem {
          self.render_mem_block(f, c1, &self.mem);
        }
        if self.cfg.show_gpu {
          let area = if self.cfg.show_mem { c2 } else { sub_rows[sub_idx] };
          self.render_freq_block(f, area, "GPU", &self.igpu_freq);
        }
      }

      row_idx += 1;
    }

    // Network row
    if show_net {
      self.render_net_block(f, rows[row_idx], &self.net);
      row_idx += 1;
    }

    // Power row
    if show_power {
      let label_l = format!(
        "Power: {:.2}W (avg {:.2}W, max {:.2}W)",
        self.all_power.top_value, self.all_power.avg_value, self.all_power.max_value,
      );

      let label_r = if self.sys_power.top_value > 0.0 {
        format!(
          "Total {:.2}W ({:.2}, {:.2})",
          self.sys_power.top_value, self.sys_power.avg_value, self.sys_power.max_value
        )
      } else {
        "".to_string()
      };

      let iarea = self.render_section_title(f, rows[row_idx], &label_l, &label_r);

      let ha = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)].as_ref())
        .split(iarea);

      f.render_widget(self.get_power_block("CPU", &self.cpu_power, self.cpu_temp.last()), ha[0]);
      f.render_widget(self.get_power_block("GPU", &self.gpu_power, self.gpu_temp.last()), ha[1]);
      f.render_widget(self.get_power_block("ANE", &self.ane_power, 0.0), ha[2]);
    }
  }

  pub fn run_loop(&mut self, interval: Option<u32>) -> WithError<()> {
    // use from arg if provided, otherwise use config restored value
    self.cfg.interval = interval.unwrap_or(self.cfg.interval).clamp(100, 10_000);
    let msec = Arc::new(RwLock::new(self.cfg.interval));

    let (tx, rx) = mpsc::channel::<Event>();
    run_inputs_thread(tx.clone(), 250);
    run_sampler_thread(tx.clone(), msec.clone());

    let mut term = enter_term();

    loop {
      term.draw(|f| self.render(f)).unwrap();

      match rx.recv()? {
        Event::Quit => break,
        Event::Update(data) => self.update_metrics(data),
        Event::ChangeColor => self.cfg.next_color(),
        Event::ChangeView => self.cfg.next_view_type(),
        Event::IncInterval => {
          self.cfg.inc_interval();
          *msec.write().unwrap() = self.cfg.interval;
        }
        Event::DecInterval => {
          self.cfg.dec_interval();
          *msec.write().unwrap() = self.cfg.interval;
        }
        _ => {}
      }
    }

    leave_term();
    Ok(())
  }
}
