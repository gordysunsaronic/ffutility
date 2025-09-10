#![cfg(all(target_os = "linux", feature = "v4l"))]

use anyhow::Result;

use ffutility::{encoders::FfmpegOptions, parsers::AnnexBStreamImport, streams::{V4lH264Stream, V4lH264Config}};

use hang::BroadcastProducer;
use moq_native::client;

use reqwest;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

use url::Url;

#[derive(Deserialize, Debug)]
struct Camera {
    name: String,
    video_dev: String,
}

#[derive(Deserialize, Debug)]
struct CameraGroup {
    cameras: Vec<Camera>,
    default: String,
    name: String,
}

#[derive(Deserialize, Debug)]
struct Config {
    camera_groups: Vec<CameraGroup>,
    relay_server: String,
}

#[derive(Deserialize, Debug)]
struct TokenResponse {
    token: String,
}

async fn fetch_token(path: &str) -> Result<String> {
    let client = reqwest::Client::new();

    let payload = json!({
        "type": "publisher",
        "path": path
    });

    let response = client
        .post("http://gordy-sim:8081/api/v1/boat-token")
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("Token API returned status: {}", response.status()));
    }

    let token_response: TokenResponse = response.json().await?;
    Ok(token_response.token)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Read and parse config file
    let config_content = fs::read_to_string("examples/config.json")?;
    let config: Config = serde_json::from_str(&config_content)?;

    // Get boat name from hostname (e.g., "cr1-crystal" -> "cr1")
    let hostname = gethostname::gethostname();
    let hostname_str = hostname.to_string_lossy();
    let boat_name = hostname_str.split('-').next().unwrap_or("unknown");
    eprintln!("Detected boat name: {} (from hostname: {})", boat_name, hostname_str);

    let mut tls = moq_native::client::ClientTls::default();
    tls.disable_verify = Some(true);

    let quic_client = client::Client::new(client::ClientConfig {
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        tls
    })?;

    // Collect all cameras with their URLs
    let mut handles = Vec::new();

    for group in &config.camera_groups {
        for camera in &group.cameras {
            // Generate the camera path with trailing slash
            let camera_path = format!("{}-{}-{}/", boat_name, group.name, camera.name);

            // Fetch JWT token for this camera path
            let jwt = match fetch_token(&camera_path).await {
                Ok(token) => token,
                Err(e) => {
                    eprintln!("Failed to fetch token for {}: {}", camera_path, e);
                    continue; // Skip this camera if token fetch fails
                }
            };

            let url = format!(
                "{}/{}/?jwt={}",
                config.relay_server.trim_end_matches('/'),
                camera_path.trim_end_matches('/'),
                jwt
            );

            eprintln!("Starting stream for camera: {}-{}", group.name, camera.name);

            // Clone values for the spawned task
            let quic_client = quic_client.clone();
            let video_dev = camera.video_dev.clone();
            let camera_name = format!("{}-{}", group.name, camera.name);

            // Spawn independent streaming task for each camera with retry logic
            let handle: tokio::task::JoinHandle<Result<(), anyhow::Error>> = tokio::spawn(async move {
                let mut retry_count = 0;
                loop {
                    eprintln!("Connecting to: {} (attempt #{})", url, retry_count + 1);

                    // Try to run the stream
                    let result: Result<()> = async {
                        // Create session
                        let session = match quic_client.connect(Url::parse(&url)?).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("Failed to connect for {}: {}", camera_name, e);
                                return Err(e.into());
                            }
                        };

                        let mut session = match moq_lite::Session::connect(session).await {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("Failed to establish session for {}: {}", camera_name, e);
                                return Err(e.into());
                            }
                        };

                        // Create broadcast producer
                        let broadcast = BroadcastProducer::new();
                        session.publish("", broadcast.inner.consume());

                        // Create annexb import
                        let mut annexb_import = AnnexBStreamImport::new(Arc::new(Mutex::new(broadcast)), 736, 414);

                        // Create v4l config
                        let v4l_config = V4lH264Config {
                            output_width: 736,
                            output_height: 414,
                            bitrate: 300000,
                            video_dev: video_dev.clone(),
                        };

                        // Create ffmpeg options
                        let mut ffmpeg_opts = FfmpegOptions::new();
                        ffmpeg_opts.push((String::from("preset"), String::from("superfast")));

                        // Create stream
                        let mut rx_stream = V4lH264Stream::new(v4l_config, ffmpeg_opts)?;

                        // Initialize track
                        let mut track = annexb_import.init_from(&mut rx_stream).await?;
                        eprintln!("Initialized track for camera: {}", camera_name);

                        // Run stream until it ends or session closes
                        tokio::select! {
                            res = annexb_import.read_from(&mut rx_stream, &mut track) => {
                                match &res {
                                    Ok(_) => {
                                        eprintln!("Stream {} completed (device likely disconnected), will retry", camera_name);
                                        // Treat normal completion as an error that needs retry
                                        // Since device disconnection often results in Ok(()) rather than Err
                                        Err(anyhow::anyhow!("Stream ended unexpectedly"))
                                    },
                                    Err(e) => {
                                        eprintln!("Stream {} error: {}", camera_name, e);
                                        Err(anyhow::anyhow!("Stream error: {}", e))
                                    }
                                }
                            },
                            res = session.closed() => {
                                eprintln!("Session {} closed: {:?}", camera_name, res);
                                Err(res.into())
                            }
                        }
                    }.await;

                    // Always retry since we want streams to run indefinitely
                    match result {
                        Ok(_) => {
                            // This should never happen now since we always return Err above
                            eprintln!("Stream {} ended unexpectedly without error, retrying...", camera_name);
                        }
                        Err(e) => {
                            retry_count += 1;
                            eprintln!("Stream {} failed (attempt #{}): {}, retrying in 5 seconds...",
                                     camera_name, retry_count, e);
                        }
                    }

                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    // Continue loop to retry
                }
            });

            handles.push(handle);
        }
    }

    eprintln!("Started {} camera streams", handles.len());

    // Wait for all tasks to complete
    // Each stream runs independently - if one fails, others continue
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(_)) => eprintln!("Stream {} completed successfully", i),
            Ok(Err(e)) => eprintln!("Stream {} failed: {}", i, e),
            Err(e) => eprintln!("Stream {} task panicked: {}", i, e),
        }
    }

    Ok(())
}