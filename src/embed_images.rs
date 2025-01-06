use crate::disk_cache::DiskCache;
use crate::downloader::DownloadManager;
use crate::image_optimizer::optimize_image;
use crate::image_optimizer::ImageOptimizationSettings;
use crate::image_optimizer::OptimizeImageError;
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
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
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

struct FileToZip {
    name: String,
    contents: Vec<u8>,
    compressed: bool,
}

#[derive(Debug)]
struct ImageInfo {
    mimetype: Option<infer::Type>,
    displayed_width: Option<u32>,
}

pub async fn embed_images(input_path: String, output_path: String) {
    let sty = ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg} ({eta})",
    )
    .unwrap()
    .progress_chars("##-");

    let file = File::open(input_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();

    let mut new_zip = zip::ZipWriter::new(File::create(output_path).unwrap());
    let (tx, rx) = std::sync::mpsc::channel::<FileToZip>();
    let zip_task = tokio::task::spawn_blocking(move || {
        while let Ok(file) = rx.recv() {
            let mut options = SimpleFileOptions::default();
            if !file.compressed {
                options = options.compression_method(zip::CompressionMethod::Stored);
            }

            new_zip.start_file(file.name, options).unwrap();
            new_zip.write_all(&file.contents).unwrap();
        }

        new_zip.finish().unwrap();
    });

    let mut download_manager = DownloadManager::build(20).unwrap();

    let html_files = zip
        .file_names()
        .filter(|name| name.ends_with(".html"))
        .collect::<Vec<_>>();

    let progress_bars = MultiProgress::new();
    let html_pb = progress_bars.add(ProgressBar::new(html_files.len() as u64));
    html_pb.set_style(sty.clone());
    html_pb.set_message("🔍 scanning source for <img/> tags");

    let download_pb = progress_bars.add(ProgressBar::new(0));
    download_pb.set_style(sty.clone());
    download_pb.set_message("⬇️ downloading images");

    let optimize_pb = progress_bars.add(ProgressBar::new(0));
    optimize_pb.set_style(sty.clone());
    optimize_pb.set_message("️⚙️ optimizing images");

    let mut image_info = HashMap::new();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(10)
        .build()
        .unwrap();

    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();

        if file.name().ends_with(".opf") {
            continue;
        }

        if !file.name().ends_with(".html") {
            // todo: should use cheap copy?
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).unwrap();
            tx.send(FileToZip {
                name: file.name().to_string(),
                contents: buffer,
                compressed: file.compression() != zip::CompressionMethod::Stored,
            })
            .unwrap();
            continue;
        }

        let file_name = file.name().to_string();

        let buffered = BufReader::new(file);
        let mut reader = Reader::from_reader(buffered);
        reader.config_mut().trim_text(true);

        let mut writer = quick_xml::writer::Writer::new(Cursor::new(Vec::new()));

        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Err(e) => panic!("Error at position {}: {:?}", reader.error_position(), e),
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
                                    .map(|w| String::from_utf8_lossy(&w.value).parse().unwrap());

                                let url: Url = candidate.url.parse().unwrap();
                                let path = compute_download_path(&url);
                                if image_info
                                    .insert(
                                        url.clone(),
                                        ImageInfo {
                                            mimetype: None,
                                            displayed_width: width,
                                        },
                                    )
                                    .is_none()
                                {
                                    download_pb.inc_length(1);
                                    optimize_pb.inc_length(1);
                                    download_manager.queue_download(url);
                                    let html_dir = Path::new(&file_name).parent().unwrap();
                                    rewrite_src(
                                        &mut e,
                                        diff_paths(path, html_dir).unwrap().to_str().unwrap(),
                                    );
                                }
                                writer.write_event(Event::Empty(e)).unwrap();
                            }
                        } else if let Ok(Some(src)) = e.try_get_attribute(b"src") {
                            if src.value.starts_with(b"http") {
                                let width: Option<u32> = e
                                    .try_get_attribute(b"width")
                                    .unwrap()
                                    .map(|w| String::from_utf8_lossy(&w.value).parse().unwrap());

                                let url: Url = String::from_utf8_lossy(&src.value).parse().unwrap();
                                let path = compute_download_path(&url);
                                if image_info
                                    .insert(
                                        url.clone(),
                                        ImageInfo {
                                            mimetype: None,
                                            displayed_width: width,
                                        },
                                    )
                                    .is_none()
                                {
                                    download_pb.inc_length(1);
                                    optimize_pb.inc_length(1);
                                    download_manager.queue_download(url);
                                    let html_dir = Path::new(&file_name).parent().unwrap();
                                    rewrite_src(
                                        &mut e,
                                        diff_paths(path, html_dir).unwrap().to_str().unwrap(),
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

        tx.send(FileToZip {
            name: file_name,
            contents: writer.into_inner().into_inner(),
            compressed: true,
        })
        .unwrap();

        html_pb.inc(1);
    }

    let optimized_image_cache = DiskCache::create("optimized_images").unwrap();

    let total_original_image_size = Arc::new(AtomicUsize::new(0));
    let total_optimized_image_size = Arc::new(AtomicUsize::new(0));

    while let Some(item) = download_manager.next().await {
        match item {
            Ok((url, file_contents)) => {
                total_original_image_size
                    .fetch_add(file_contents.len(), std::sync::atomic::Ordering::Relaxed);

                download_pb.inc(1);

                let path = compute_download_path(&url);
                let width = {
                    let image_info = image_info.get_mut(&url).unwrap();
                    image_info.mimetype = infer::get(&file_contents);
                    image_info.displayed_width
                };

                let tx = tx.clone();
                let optimized_image_cache = optimized_image_cache.clone();
                let total_optimized_image_size = total_optimized_image_size.clone();
                let optimize_pb = optimize_pb.clone();
                pool.spawn(move || {
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
                            total_optimized_image_size
                                .fetch_add(optimized.len(), std::sync::atomic::Ordering::Relaxed);
                            tx.send(FileToZip {
                                name: path,
                                contents: optimized,
                                compressed: false,
                            })
                            .unwrap();
                        }
                        Err(OptimizeImageError::UnsupportedImageFormat(_)) => {
                            tx.send(FileToZip {
                                name: path,
                                contents: file_contents,
                                compressed: false,
                            })
                            .unwrap();
                        }
                        Err(e) => {
                            panic!("Error optimizing image: {:?}", e);
                        }
                    }

                    optimize_pb.inc(1);
                });
            }
            Err(e) => {
                // panic!("Error downloading image: {:?}", e);
            }
        }
    }

    // Rewrite package file
    let opf_name = zip
        .file_names()
        .find(|name| name.ends_with(".opf"))
        .expect("No .opf file found")
        .to_string();
    let opf_file = zip.by_name(&opf_name).unwrap();

    let buffered = BufReader::new(opf_file);
    let mut reader = Reader::from_reader(buffered);
    reader.config_mut().trim_text(true);

    let mut writer = quick_xml::writer::Writer::new(Cursor::new(Vec::new()));

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => panic!("Error at position {}: {:?}", reader.error_position(), e),
            // exits the loop when reaching end of file
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if e.name() == QName(b"manifest") {
                    writer.write_event(Event::Start(e.clone())).unwrap();

                    for (url, info) in image_info.iter() {
                        let path = compute_download_path(url);

                        writer
                            .write_event(Event::Empty(
                                BytesStart::new("item")
                                    .with_attributes(
                                        vec![
                                            Attribute::from(("id".as_bytes(), path.as_bytes())),
                                            Attribute::from(("href".as_bytes(), path.as_bytes())),
                                            Attribute::from((
                                                "media-type".as_bytes(),
                                                info.mimetype
                                                    .map(|m| m.mime_type())
                                                    .unwrap_or("application/octet-stream")
                                                    .as_bytes(),
                                            )),
                                        ]
                                        .into_iter(),
                                    )
                                    .into_owned(),
                            ))
                            .unwrap();
                    }
                } else {
                    writer.write_event(Event::Start(e)).unwrap();
                }
            }
            Ok(Event::End(e)) => {
                writer.write_event(Event::End(e)).unwrap();
            }
            Ok(Event::Empty(e)) => {
                writer.write_event(Event::Empty(e)).unwrap();
            }
            Ok(Event::Text(e)) => {
                writer.write_event(Event::Text(e)).unwrap();
            }
            event => {
                writer.write_event(event.unwrap()).unwrap();
            }
        }
        buf.clear();
    }

    tx.send(FileToZip {
        name: opf_name,
        contents: writer.into_inner().into_inner(),
        compressed: true,
    })
    .unwrap();

    drop(tx);
    zip_task.await.unwrap();

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
