mod app;
mod config;
mod debug;
mod metrics;
mod sources;

use app::App;
use clap::{CommandFactory, Parser, Subcommand, parser::ValueSource};
use metrics::Sampler;
use std::error::Error;

#[derive(Debug, Subcommand)]
enum Commands {
  /// Output metrics in JSON format (suitable for piping)
  #[command(alias = "raw")]
  Pipe {
    /// Number of samples to run for. Set to 0 to run indefinitely
    #[arg(short, long, default_value_t = 0)]
    samples: u32,

    /// Include SoC information in the output
    #[arg(long, default_value_t = false)]
    soc_info: bool,
  },

  /// Print debug information
  Debug,
}

const INTERACTIVE_KEYS: &str = "\
Interactive keys:
  q, Ctrl-C  quit
  c          cycle through colors
  v          toggle view (sparkline / gauge)
  -, +       decrease / increase the update interval by 250ms";

/// Sudoless performance monitoring CLI tool for Apple Silicon processors
/// https://github.com/vladkens/macmon
#[derive(Debug, Parser)]
#[command(version, verbatim_doc_comment, after_help = INTERACTIVE_KEYS)]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// Update interval in milliseconds
  #[arg(short, long, global = true, default_value_t = 1000)]
  interval: u32,

  /// Hide CPU graphs
  #[arg(long, default_value_t = false)]
  no_cpu: bool,

  /// Hide GPU graph
  #[arg(long, default_value_t = false)]
  no_gpu: bool,

  /// Hide memory graph
  #[arg(long, default_value_t = false)]
  no_mem: bool,

  /// Hide network graphs
  #[arg(long, default_value_t = false)]
  no_net: bool,

  /// Hide power graphs
  #[arg(long, default_value_t = false)]
  no_power: bool,

  /// Hide the top title line with the machine specs
  #[arg(long, default_value_t = false)]
  no_title: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
  let args = Cli::parse();

  match &args.command {
    Some(Commands::Pipe { samples, soc_info }) => {
      let mut sampler = Sampler::new()?;
      let mut counter = 0u32;

      let soc_info_val = if *soc_info { Some(sampler.get_soc_info().clone()) } else { None };

      loop {
        let doc = sampler.get_metrics(args.interval.max(100))?;

        let mut doc = serde_json::to_value(&doc)?;
        if let Some(ref soc) = soc_info_val {
          doc["soc"] = serde_json::to_value(soc)?;
        }
        doc["timestamp"] = serde_json::to_value(chrono::Utc::now().to_rfc3339())?;
        let doc = serde_json::to_string(&doc)?;

        println!("{}", doc);

        counter += 1;
        if *samples > 0 && counter >= *samples {
          break;
        }
      }
    }
    Some(Commands::Debug) => debug::print_debug()?,
    _ => {
      let mut app = App::new()?;

      let matches = Cli::command().get_matches();
      let msec = match matches.value_source("interval") {
        Some(ValueSource::CommandLine) => Some(args.interval),
        _ => None,
      };

      // CLI flags override config (--no-X hides, absence preserves config)
      if args.no_cpu { app.cfg_mut().show_cpu = false; }
      if args.no_gpu { app.cfg_mut().show_gpu = false; }
      if args.no_mem { app.cfg_mut().show_mem = false; }
      if args.no_net { app.cfg_mut().show_net = false; }
      if args.no_power { app.cfg_mut().show_power = false; }
      if args.no_title { app.cfg_mut().show_title = false; }

      app.run_loop(msec)?;
    }
  }

  Ok(())
}
