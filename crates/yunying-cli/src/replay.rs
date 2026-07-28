//! Replay asciinema v2 (.cast) recordings in the terminal.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub struct ReplayOptions {
    pub speed: f64,
    pub idle_limit: Option<f64>,
}

pub fn replay(path: &Path, opts: &ReplayOptions) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let mut lines = reader.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty cast file"))??;
    let header: serde_json::Value = serde_json::from_str(&header_line)?;

    if header["version"] != 2 {
        anyhow::bail!("unsupported cast version: {}", header["version"]);
    }

    let width = header["width"].as_u64().unwrap_or(80);
    let height = header["height"].as_u64().unwrap_or(24);
    eprintln!(
        "\x1b[90m▶ replaying {} ({}x{}, speed={:.1}x)\x1b[0m",
        path.display(),
        width,
        height,
        opts.speed
    );

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let start = Instant::now();
    let mut last_time: f64 = 0.0;
    let mut virtual_elapsed: f64 = 0.0;

    for line in lines {
        let line = line?;
        let event: serde_json::Value = serde_json::from_str(&line)?;
        let arr = match event.as_array() {
            Some(a) => a,
            None => continue,
        };
        if arr.len() < 3 {
            continue;
        }

        let time = arr[0].as_f64().unwrap_or(0.0);
        let kind = arr[1].as_str().unwrap_or("");
        let data = arr[2].as_str().unwrap_or("");

        if kind == "exit" {
            break;
        }

        // Only replay output events
        if kind != "o" {
            continue;
        }

        // Calculate delay with speed and idle cap
        let mut delay = (time - last_time) / opts.speed;
        if let Some(limit) = opts.idle_limit {
            delay = delay.min(limit);
        }
        last_time = time;
        virtual_elapsed += delay;

        if delay > 0.001 {
            let target = start + Duration::from_secs_f64(virtual_elapsed);
            let now = Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
        }

        out.write_all(data.as_bytes())?;
        out.flush()?;
    }

    eprintln!("\r\n\x1b[90m■ replay finished ({:.1}s)\x1b[0m", last_time);
    Ok(())
}
