use clap::{Parser, Subcommand};
use device_profiles::DeviceProfile;
use disk_cache::clear_all_caches;
use embed_imgs::embed_images;

mod device_profiles;
mod disk_cache;
mod download;
mod embed_imgs;
mod image_optimizer;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Embeds images
    EmbedImages {
        /// Path to input .epub file
        #[arg(short, long)]
        input: String,
        /// Path to output .epub file
        #[arg(short, long)]
        output: String,
        #[arg(short, long)]
        #[clap(default_value("10"))]
        download_concurrency: usize,
        #[arg(long)]
        #[clap(default_value("kindle-paperwhite2024"))]
        device_profile: DeviceProfile,
    },
    /// Clears the cache
    ClearCache,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let cli = Args::parse();

    match cli.command {
        Commands::EmbedImages {
            input,
            output,
            download_concurrency,
            device_profile,
        } => {
            embed_images(input, output, download_concurrency, device_profile).await;
        }
        Commands::ClearCache => {
            clear_all_caches().unwrap();
        }
    }
}
