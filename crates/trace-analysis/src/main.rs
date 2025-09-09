use anyhow::{Context, Result};
use clap::Parser;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::{Path, PathBuf};

mod analyzer;
mod concurrency_analysis;
mod hyperthread_analysis;
mod monotonicity_analysis;

use analyzer::Analyzer;
use concurrency_analysis::ConcurrencyAnalysis;
use hyperthread_analysis::HyperthreadAnalysis;
use monotonicity_analysis::MonotonicityAnalysis;

#[derive(Parser)]
#[command(name = "trace-analysis")]
#[command(about = "Analyze trace data for hyperthread contention and concurrency")]
struct Cli {
    #[arg(short = 'f', long, help = "Input Parquet trace file")]
    filename: PathBuf,

    #[arg(
        long,
        help = "Output file prefix (defaults to base name of input file)"
    )]
    output_prefix: Option<String>,

    #[arg(
        long,
        help = "Analysis type to run: 'concurrency', 'hyperthread', or 'monotonicity'",
        default_value = "hyperthread"
    )]
    analysis_type: String,
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

    // Determine output filename based on analysis type
    let output_filename = determine_output_filename(
        &cli.filename,
        cli.output_prefix.as_deref(),
        &cli.analysis_type,
    )?;

    println!(
        "Processing {} CPUs with {} analysis, output to: {}",
        num_cpus,
        cli.analysis_type,
        output_filename.display()
    );

    // Create analyzer
    let analyzer = Analyzer::new(output_filename);

    match cli.analysis_type.as_str() {
        "concurrency" => {
            // Create concurrency analysis module
            let mut analysis = ConcurrencyAnalysis::new(num_cpus)?;
            
            // Set CSV output paths
            let total_csv_path = determine_csv_output_filename(&cli.filename, cli.output_prefix.as_deref(), "total_concurrency")?;
            let same_process_csv_path = determine_csv_output_filename(&cli.filename, cli.output_prefix.as_deref(), "same_process_concurrency")?;
            analysis.set_csv_paths(total_csv_path.to_string_lossy().to_string(), 
                                  same_process_csv_path.to_string_lossy().to_string());

            // Process the Parquet file
            analyzer.process_parquet_file(builder, analysis)?;
        }
        "hyperthread" => {
            // Create hyperthread analysis module
            let analysis = HyperthreadAnalysis::new(num_cpus)?;

            // Process the Parquet file
            analyzer.process_parquet_file(builder, analysis)?;
        }
        "monotonicity" => {
            // Create CSV output filename for monotonicity analysis
            let csv_output =
                determine_csv_output_filename(&cli.filename, cli.output_prefix.as_deref(), "monotonicity_analysis")?;

            // Create monotonicity analysis module
            let analysis = MonotonicityAnalysis::new(csv_output)?;

            // Process the Parquet file
            analyzer.process_parquet_file(builder, analysis)?;
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid analysis type: {}. Must be 'concurrency', 'hyperthread', or 'monotonicity'",
                cli.analysis_type
            ));
        }
    }

    println!("Analysis complete!");

    Ok(())
}

fn determine_output_filename(
    input_path: &Path,
    output_prefix: Option<&str>,
    analysis_type: &str,
) -> Result<PathBuf> {
    let base_name = input_path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("Invalid input filename"))?
        .to_string_lossy();

    let prefix = output_prefix.unwrap_or(&base_name);
    let output_filename = format!("{}_{}_analysis.parquet", prefix, analysis_type);

    if let Some(parent) = input_path.parent() {
        Ok(parent.join(output_filename))
    } else {
        Ok(PathBuf::from(output_filename))
    }
}

fn determine_csv_output_filename(
    input_path: &Path,
    output_prefix: Option<&str>,
    suffix: &str,
) -> Result<PathBuf> {
    let base_name = input_path
        .file_stem()
        .ok_or_else(|| anyhow::anyhow!("Invalid input filename"))?
        .to_string_lossy();

    let prefix = output_prefix.unwrap_or(&base_name);
    let output_filename = format!("{}_{}.csv", prefix, suffix);

    if let Some(parent) = input_path.parent() {
        Ok(parent.join(output_filename))
    } else {
        Ok(PathBuf::from(output_filename))
    }
}
