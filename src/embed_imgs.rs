use crate::disk_cache::DiskCache;
use crate::download::DownloadError;
use crate::download::Downloader;
use crate::image_optimizer::optimize_image;
use crate::image_optimizer::ImageOptimizationSettings;
use crate::image_optimizer::OptimizeImageError;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use pathdiff::diff_paths;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use rayon::iter::ParallelBridge;
use rayon::iter::ParallelIterator;
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::sync::Mutex;
use zip::write::SimpleFileOptions;

fn compute_download_path(url: &Url) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_str());
    format!("_embedded/{}", hex::encode(hasher.finalize()))
}

fn rewrite_src(element: &mut BytesStart, src: &str) {
    let cloned = element.clone();
    let attributes = cloned.attributes();
    element.clear_attributes();

    for attr in attributes {
        if let Ok(attr) = attr {
            if attr.key == QName(b"src") {
                element.push_attribute(Attribute::from(("src".as_bytes(), src.as_bytes())));
            } else if attr.key == QName(b"srcset") {
                continue;
            } else {
                element.push_attribute(attr);
            }
        } else {
            panic!("Error reading attribute");
        }
    }
}

#[derive(Debug)]
struct ImageInfo {
    mimetype: Option<infer::Type>,
    displayed_width: Option<u32>,
}

enum FileToZip {
    NewFile {
        name: String,
        contents: Vec<u8>,
        compressed: bool,
    },
    ExistingFile {
        name: String,
    },
}

pub async fn embed_images(input_path: String, output_path: String) {
    // Progress bars
    let progress_bar_style = ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg} ({eta})",
    )
    .unwrap()
    .progress_chars("##-");

    let progress_bars = MultiProgress::new();
    let html_pb = progress_bars.add(ProgressBar::new(0));
    html_pb.set_style(progress_bar_style.clone());
    html_pb.set_message("🔍 scanning source for <img/> tags");

    let download_pb = progress_bars.add(ProgressBar::new(0));
    download_pb.set_style(progress_bar_style.clone());
    download_pb.set_message("⬇️ downloading images");

    let optimize_pb = progress_bars.add(ProgressBar::new(0));
    optimize_pb.set_style(progress_bar_style.clone());
    optimize_pb.set_message("️⚙️ optimizing images");

    let zip_pb = progress_bars.add(ProgressBar::new(0));
    zip_pb.set_style(progress_bar_style.clone());
    zip_pb.set_message("📦 creating .epub");

    let pool_size = std::thread::available_parallelism()
        .map(|p| (usize::from(p)).min(3) - 2)
        .unwrap_or(1); // Tokio uses 2 threads
    let cpu_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(pool_size)
            .build()
            .unwrap(),
    );

    // State
    let mut tasks = FuturesUnordered::new();

    let (zip_file_tx, zip_file_rx) = std::sync::mpsc::channel::<FileToZip>();
    let image_info = Arc::new(Mutex::new(HashMap::new()));
    let download_manager = Downloader::create().unwrap();
    let optimized_image_cache: DiskCache<ImageOptimizationSettings, Vec<u8>> =
        DiskCache::create("optimized_images").unwrap();
    let total_original_image_size = Arc::new(AtomicUsize::new(0));
    let total_optimized_image_size = Arc::new(AtomicUsize::new(0));

    // Zip creation task
    {
        let zip_pb = zip_pb.clone();
        let input_path = input_path.clone();
        let zip_task = tokio::task::spawn_blocking(move || {
            let mut zip_reader = zip::ZipArchive::new(File::open(input_path).unwrap()).unwrap();
            let mut new_zip = zip::ZipWriter::new(File::create(output_path).unwrap());

            while let Ok(file) = zip_file_rx.recv() {
                match file {
                    FileToZip::NewFile {
                        name,
                        contents,
                        compressed,
                    } => {
                        let mut options = SimpleFileOptions::default();
                        if !compressed {
                            options = options.compression_method(zip::CompressionMethod::Stored);
                        }

                        new_zip.start_file(name, options).unwrap();
                        new_zip.write_all(&contents).unwrap();
                    }
                    FileToZip::ExistingFile { name } => {
                        let file = zip_reader.by_name(&name).unwrap();
                        new_zip.raw_copy_file(file).unwrap();
                    }
                }
                zip_pb.inc(1);
            }

            new_zip.finish().unwrap();
        });
        tasks.push(zip_task);
    }

    // Input read task
    let (html_tx, html_rx) = std::sync::mpsc::sync_channel::<(String, Vec<u8>)>(pool_size * 2);
    {
        let html_pb = html_pb.clone();
        let zip_pb = zip_pb.clone();
        let zip_file_tx = zip_file_tx.clone();

        let input_read_task = tokio::task::spawn_blocking(move || {
            let file = File::open(input_path).unwrap();
            let mut zip = zip::ZipArchive::new(file).unwrap();

            for i in 0..zip.len() {
                let mut file = zip.by_index(i).unwrap();

                if file.name().ends_with(".html") {
                    html_pb.inc_length(1);
                    let mut contents = Vec::new();
                    file.read_to_end(&mut contents).unwrap();
                    html_tx.send((file.name().to_string(), contents)).unwrap();
                } else {
                    zip_pb.inc_length(1);
                    zip_file_tx
                        .send(FileToZip::ExistingFile {
                            name: file.name().to_string(),
                        })
                        .unwrap();
                }
            }
            drop(html_tx);
        });
        tasks.push(input_read_task);
    }

    // HTML parse task
    let urls_to_download = {
        let download_pb = download_pb.clone();
        let zip_file_tx = zip_file_tx.clone();
        let zip_pb = zip_pb.clone();
        let image_info = image_info.clone();
        let cpu_pool = cpu_pool.clone();

        tokio::task::spawn_blocking::<_, Vec<Url>>(move || {
            cpu_pool.install(|| {
                html_rx
                    .into_iter()
                    .par_bridge()
                    .flat_map(|(file_name, html_file)| {
                        let mut reader = Reader::from_reader(Cursor::new(html_file));
                        reader.config_mut().trim_text(true);

                        let mut writer = quick_xml::writer::Writer::new(Cursor::new(Vec::new()));

                        let mut buf = Vec::new();
                        let mut urls_to_download = Vec::new();

                        loop {
                            match reader.read_event_into(&mut buf) {
                                Err(e) => {
                                    panic!("Error at position {}: {:?}", reader.error_position(), e)
                                }
                                // exits the loop when reaching end of file
                                Ok(Event::Eof) => break,
                                Ok(Event::Empty(mut e)) => {
                                    if e.name().as_ref() == b"img" {
                                        if let Ok(Some(srcset)) = e.try_get_attribute(b"srcset") {
                                            let srcset = String::from_utf8_lossy(&srcset.value);
                                            let parsed = srcset_parse::parse(&srcset);
                                            let largest = parsed.into_iter().reduce(|acc, e| {
                                                if let Some(cmp) = acc.partial_cmp(&e) {
                                                    if cmp == std::cmp::Ordering::Less {
                                                        e
                                                    } else {
                                                        acc
                                                    }
                                                } else {
                                                    acc
                                                }
                                            });

                                            if let Some(candidate) = largest {
                                                let width: Option<u32> = e
                                                    .try_get_attribute(b"width")
                                                    .unwrap()
                                                    .map(|w| {
                                                        String::from_utf8_lossy(&w.value)
                                                            .parse()
                                                            .unwrap()
                                                    });

                                                let url: Url = candidate.url.parse().unwrap();
                                                let path = compute_download_path(&url);
                                                if image_info
                                                    .lock()
                                                    .unwrap()
                                                    .insert(
                                                        url.clone(),
                                                        ImageInfo {
                                                            mimetype: None,
                                                            displayed_width: width,
                                                        },
                                                    )
                                                    .is_none()
                                                {
                                                    urls_to_download.push(url);
                                                    let html_dir =
                                                        Path::new(&file_name).parent().unwrap();
                                                    rewrite_src(
                                                        &mut e,
                                                        diff_paths(path, html_dir)
                                                            .unwrap()
                                                            .to_str()
                                                            .unwrap(),
                                                    );
                                                }
                                                writer.write_event(Event::Empty(e)).unwrap();
                                            }
                                        } else if let Ok(Some(src)) = e.try_get_attribute(b"src") {
                                            if src.value.starts_with(b"http") {
                                                let width: Option<u32> = e
                                                    .try_get_attribute(b"width")
                                                    .unwrap()
                                                    .map(|w| {
                                                        String::from_utf8_lossy(&w.value)
                                                            .parse()
                                                            .unwrap()
                                                    });

                                                let url: Url = String::from_utf8_lossy(&src.value)
                                                    .parse()
                                                    .unwrap();
                                                let path = compute_download_path(&url);
                                                if image_info
                                                    .lock()
                                                    .unwrap()
                                                    .insert(
                                                        url.clone(),
                                                        ImageInfo {
                                                            mimetype: None,
                                                            displayed_width: width,
                                                        },
                                                    )
                                                    .is_none()
                                                {
                                                    urls_to_download.push(url);
                                                    let html_dir =
                                                        Path::new(&file_name).parent().unwrap();
                                                    rewrite_src(
                                                        &mut e,
                                                        diff_paths(path, html_dir)
                                                            .unwrap()
                                                            .to_str()
                                                            .unwrap(),
                                                    );
                                                }
                                                writer.write_event(Event::Empty(e)).unwrap();
                                            }
                                        } else {
                                            writer.write_event(Event::Empty(e)).unwrap();
                                        }
                                    }
                                }
                                event => {
                                    writer.write_event(event.unwrap()).unwrap();
                                }
                            }
                            buf.clear();
                        }

                        html_pb.inc(1);
                        download_pb.inc_length(urls_to_download.len() as u64);

                        zip_pb.inc_length(1);
                        zip_file_tx
                            .send(FileToZip::NewFile {
                                name: file_name.to_string(),
                                contents: writer.into_inner().into_inner(),
                                compressed: true,
                            })
                            .unwrap();

                        urls_to_download
                    })
                    .collect()
            })
        })
        .await
    };

    let (optimize_file_tx, mut optimize_file_rx) =
        tokio::sync::mpsc::channel::<(Url, Result<Vec<u8>, DownloadError>)>(pool_size * 2);

    {
        let optimize_pb = optimize_pb.clone();
        let download_task = tokio::spawn(async move {
            let mut download_stream = futures::stream::iter(urls_to_download.unwrap())
                .map(|url| async { (url.clone(), download_manager.download(url).await) })
                .buffer_unordered(10);

            while let Some((url, result)) = download_stream.next().await {
                match result {
                    Ok(downloaded_file) => {
                        download_pb.inc(1);
                        optimize_pb.inc_length(1);
                        optimize_file_tx
                            .send((url, Ok(downloaded_file.contents)))
                            .await
                            .unwrap();
                    }
                    Err(_) => {
                        download_pb.inc(1);
                        optimize_pb.inc_length(1);
                        optimize_file_tx.send((url, Ok(Vec::new()))).await.unwrap();
                    }
                }
            }
        });
        tasks.push(download_task);
    }

    // Optimize task
    {
        let optimize_pb = optimize_pb.clone();
        let total_original_image_size = total_original_image_size.clone();
        let total_optimized_image_size = total_optimized_image_size.clone();
        let zip_file_tx = zip_file_tx.clone();

        let optimize_task = tokio::task::spawn_blocking(move || {
            let iter = std::iter::from_fn(|| optimize_file_rx.blocking_recv());

            cpu_pool.install(move || {
                iter.par_bridge()
                    .map_with(
                        (optimized_image_cache, total_optimized_image_size),
                        |(optimized_image_cache, total_optimized_image_size),
                         result: (Url, Result<Vec<u8>, DownloadError>)| {
                            let (url, result) = result;
                            let file_contents = result.unwrap();
                            total_original_image_size.fetch_add(
                                file_contents.len(),
                                std::sync::atomic::Ordering::Relaxed,
                            );

                            let path = compute_download_path(&url);
                            let width = {
                                let mut image_info = image_info.lock().unwrap();
                                let info = image_info.get_mut(&url).unwrap();
                                info.mimetype = infer::get(&file_contents);
                                info.displayed_width
                            };

                            match optimize_image(
                                &file_contents,
                                ImageOptimizationSettings {
                                    is_grayscale: true,
                                    key: path.clone(),
                                    max_width: Some(width.map(|w| w.min(1200)).unwrap_or(1200)),
                                },
                                optimized_image_cache,
                            ) {
                                Ok(optimized) => {
                                    total_optimized_image_size.fetch_add(
                                        optimized.len(),
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    FileToZip::NewFile {
                                        name: path,
                                        contents: optimized,
                                        compressed: false,
                                    }
                                }
                                Err(OptimizeImageError::UnsupportedImageFormat(_)) => {
                                    FileToZip::NewFile {
                                        name: path,
                                        contents: file_contents,
                                        compressed: false,
                                    }
                                }
                                Err(e) => {
                                    panic!("Error optimizing image: {:?}", e);
                                }
                            }
                        },
                    )
                    .for_each(|file| {
                        zip_pb.inc_length(1);
                        optimize_pb.inc(1);
                        zip_file_tx.send(file).unwrap();
                    });
            });
        });
        tasks.push(optimize_task);
    }

    drop(zip_file_tx);

    while let Some(task) = tasks.next().await {
        task.unwrap();
    }

    let total_original_image_size =
        total_original_image_size.load(std::sync::atomic::Ordering::Relaxed);
    let total_optimized_image_size =
        total_optimized_image_size.load(std::sync::atomic::Ordering::Relaxed);

    progress_bars
        .println(format!(
            "Saved {} ({:.1}%) by optimizing images.",
            human_bytes::human_bytes(
                total_original_image_size as f64 - total_optimized_image_size as f64
            ),
            100.0 - (total_optimized_image_size as f64 / total_original_image_size as f64) * 100.0
        ))
        .unwrap();
}
