use anyhow::{Context, Result};
use clap::Parser;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::{Path, PathBuf};

mod hyperthread_analysis;
use hyperthread_analysis::HyperthreadAnalysis;

#[derive(Parser)]
#[command(name = "trace-analysis")]
#[command(about = "Analyze trace data for hyperthread contention")]
struct Cli {
    #[arg(short = 'f', long, help = "Input Parquet trace file")]
    filename: PathBuf,

    #[arg(
        long,
        help = "Output file prefix (defaults to base name of input file)"
    )]
    output_prefix: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Open the input Parquet file
    let file = File::open(&cli.filename)
        .with_context(|| format!("Failed to open input file: {}", cli.filename.display()))?;

    // Create ParquetRecordBatchReaderBuilder to access metadata
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| "Failed to create Parquet reader builder")?;

    // Extract num_cpus from metadata
    let metadata = builder.metadata();
    let file_metadata = metadata.file_metadata();
    let key_value_metadata = file_metadata
        .key_value_metadata()
        .ok_or_else(|| anyhow::anyhow!("No key-value metadata found in Parquet file"))?;

    let num_cpus = key_value_metadata
        .iter()
        .find(|kv| kv.key == "num_cpus")
        .ok_or_else(|| anyhow::anyhow!("num_cpus not found in metadata"))?
        .value
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("num_cpus value is empty"))?
        .parse::<usize>()
        .with_context(|| "Failed to parse num_cpus as integer")?;

    // Determine output filename
    let output_filename = determine_output_filename(&cli.filename, cli.output_prefix.as_deref())?;

    println!(
        "Processing {} CPUs, output to: {}",
        num_cpus,
        output_filename.display()
    );

    // Create hyperthread analysis module
    let mut analysis = HyperthreadAnalysis::new(num_cpus, output_filename)?;

    // Process the Parquet file
    analysis.process_parquet_file(builder)?;

    println!("Analysis complete!");

    Ok(())
}

fn determine_output_filename(input_path: &Path, output_prefix: Option<&str>) -> Result<PathBuf> {
    let base_name = input_path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("Invalid input filename"))?
        .to_string_lossy();

    let prefix = output_prefix.unwrap_or(&base_name);
    let output_filename = format!("{}_hyperthread_analysis.parquet", prefix);

    if let Some(parent) = input_path.parent() {
        Ok(parent.join(output_filename))
    } else {
        Ok(PathBuf::from(output_filename))
    }
}
