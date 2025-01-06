use clap::{Parser, Subcommand};
use embed_images::embed_images;

mod disk_cache;
mod downloader;
mod embed_images;
mod image_optimizer;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to input .epub file
    #[arg(short, long)]
    input: String,
    /// Path to output .epub file
    #[arg(short, long)]
    output: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Embeds images
    EmbedImages,
}

#[tokio::main]
async fn main() {
    let cli = Args::parse();
    embed_images(cli.input, cli.output).await;
}
