use clap::{Parser, Subcommand};
use embed_imgs::embed_images;

mod disk_cache;
mod download;
mod embed_imgs;
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

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let cli = Args::parse();
    embed_images(cli.input, cli.output).await;
}
