use crate::downloader::DownloadManager;
use futures::StreamExt;
use human_bytes::human_bytes;
use pathdiff::diff_paths;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use zip::write::SimpleFileOptions;

fn compute_download_path(url: &Url) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_str());
    format!("_embedded/{}.jpeg", hex::encode(hasher.finalize()))
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

pub async fn embed_images(input_path: String, output_path: String) {
    let file = File::open(input_path).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();

    let mut new_zip = zip::ZipWriter::new(File::create(output_path).unwrap());
    let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<u8>)>();
    let zip_task = tokio::task::spawn_blocking(move || {
        while let Ok((file_name, contents)) = rx.recv() {
            new_zip
                .start_file(file_name, SimpleFileOptions::default())
                .unwrap();
            new_zip.write_all(&contents).unwrap();
        }

        new_zip.finish().unwrap();
    });

    let mut download_manager = DownloadManager::build(20).unwrap();

    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();

        if file.name().ends_with(".opf") {
            continue;
        }

        if !file.name().ends_with(".html") {
            // todo: should use cheap copy?
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).unwrap();
            tx.send((file.name().to_string(), buffer)).unwrap();
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
                                let url: Url = candidate.url.parse().unwrap();
                                let path = compute_download_path(&url);
                                download_manager.queue_download(url);
                                let html_dir = Path::new(&file_name).parent().unwrap();
                                rewrite_src(
                                    &mut e,
                                    diff_paths(path, html_dir).unwrap().to_str().unwrap(),
                                );
                                writer.write_event(Event::Empty(e)).unwrap();
                            }
                        } else if let Ok(Some(src)) = e.try_get_attribute(b"src") {
                            if src.value.starts_with(b"http") {
                                let url: Url = String::from_utf8_lossy(&src.value).parse().unwrap();
                                let path = compute_download_path(&url);
                                download_manager.queue_download(url);
                                let html_dir = Path::new(&file_name).parent().unwrap();
                                rewrite_src(
                                    &mut e,
                                    diff_paths(path, html_dir).unwrap().to_str().unwrap(),
                                );
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

        tx.send((file_name, writer.into_inner().into_inner()))
            .unwrap();
    }

    let mut recorded_stats = Vec::new();

    let mut added_images = HashMap::new();
    while let Some(Ok((url, file_contents))) = download_manager.next().await {
        let path = compute_download_path(&url);
        if added_images.contains_key(&path) {
            continue;
        }

        added_images.insert(path.clone(), infer::get(&file_contents));
        tx.send((path, file_contents)).unwrap();

        let stats = download_manager.stats();

        recorded_stats.push(stats);

        // Average speed from the last 5 seconds:
        let last_5_seconds = recorded_stats.iter().rev().take(5).collect::<Vec<_>>();
        let speed = last_5_seconds
            .windows(2)
            .map(|pair| pair[0].speed(*pair[1]))
            .sum::<f64>()
            / last_5_seconds.len() as f64;

        println!(
            "Speed: {}/s, Downloaded: {}, Cached: {}, HTTP errors: {}",
            human_bytes(speed),
            stats.downloaded_files,
            stats.cached_files,
            stats.http_errors
        );
    }

    // Rewrite package file
    let opf_name = zip
        .file_names()
        .find(|name| name.ends_with(".opf"))
        .expect("No .opf file found")
        .to_string();
    let mut opf_file = zip.by_name(&opf_name).unwrap();

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

                    for (path, mime) in added_images.iter() {
                        writer
                            .write_event(Event::Empty(
                                BytesStart::new("item")
                                    .with_attributes(
                                        vec![
                                            Attribute::from(("id".as_bytes(), path.as_bytes())),
                                            Attribute::from(("href".as_bytes(), path.as_bytes())),
                                            Attribute::from((
                                                "media-type".as_bytes(),
                                                mime.map(|m| m.mime_type())
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

    tx.send((opf_name, writer.into_inner().into_inner()))
        .unwrap();

    drop(tx);
    zip_task.await.unwrap();
}
