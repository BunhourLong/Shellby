//! Windowless font bench + PNG atlas dumps.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use t04_fonts::{Atlas, Glyph, png};

fn main() {
    let command = env::args().nth(1).unwrap_or_else(|| "bench".to_string());
    match command.as_str() {
        "once" | "--once" => once(),
        "bench" => bench(),
        "atlas" => atlas(),
        other => {
            eprintln!("unknown command {other:?}; usage: t04-fonts [bench|once|atlas]");
            std::process::exit(2);
        }
    }
}

fn once() {
    let stack = t04_fonts::stack_name();
    for entry in t04_fonts::bench() {
        println!("{stack}\t{}\t{}", entry.metric, entry.value);
    }
}

fn bench() {
    let exe = env::current_exe().expect("current exe");
    let runs = 20;
    let mut order: Vec<String> = Vec::new();
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack = String::from("unknown");

    for run in 0..runs {
        let output = Command::new(&exe)
            .arg("--once")
            .output()
            .unwrap_or_else(|err| panic!("spawn run {run}: {err}"));
        if !output.status.success() {
            panic!("run {run} failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.splitn(3, '\t');
            let Some(run_stack) = fields.next() else { continue };
            let (Some(metric), Some(value)) = (fields.next(), fields.next()) else { continue };
            stack = run_stack.to_string();
            if !values.contains_key(metric) {
                order.push(metric.to_string());
            }
            values.entry(metric.to_string()).or_default().push(value.to_string());
        }
    }

    println!("stack\tmetric\tmedian");
    for metric in order {
        let Some(run_values) = values.get(&metric) else { continue };
        let parsed: Option<Vec<f64>> = run_values.iter().map(|v| v.parse::<f64>().ok()).collect();
        let median = match parsed {
            Some(nums) if !nums.is_empty() => t04_fonts::fmt_f64(median_of(nums)),
            _ => run_values.first().cloned().unwrap_or_default(),
        };
        println!("{stack}\t{metric}\t{median}");
    }
}

fn median_of(mut nums: Vec<f64>) -> f64 {
    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = nums.len();
    if n % 2 == 1 {
        nums[n / 2]
    } else {
        (nums[n / 2 - 1] + nums[n / 2]) / 2.0
    }
}

const PAD: i32 = 4;
const GAP: i32 = 6;
const GROUP_GAP: i32 = 12;

struct RowLayout {
    x0: i32,
    baseline: i32,
}

fn layout(atlas: &Atlas) -> (Vec<RowLayout>, i32, i32) {
    let mut rows = Vec::with_capacity(atlas.rows.len());
    let mut y = 0;
    let mut width = 0;
    for row in &atlas.rows {
        let mut pen = 0;
        let mut min_left = 0;
        let mut max_right = 0;
        let mut max_top = 0;
        let mut max_bottom = 0;
        for cell in &row.cells {
            for placed in &cell.glyphs {
                let glyph = &placed.glyph;
                if glyph.w == 0 || glyph.h == 0 {
                    continue;
                }
                let left = pen + glyph.left as i32;
                let right = left + glyph.w as i32;
                let top = glyph.top as i32;
                let bottom = glyph.h as i32 - glyph.top as i32;
                min_left = min_left.min(left);
                max_right = max_right.max(right);
                max_top = max_top.max(top);
                max_bottom = max_bottom.max(bottom);
            }
            pen += cell.advance().round().max(0.0) as i32;
            max_right = max_right.max(pen);
        }
        let row_width = max_right - min_left + PAD * 2;
        let row_height =
            (max_top + max_bottom).max(atlas.cell_h.ceil() as i32).max(1) + GAP;
        rows.push(RowLayout { x0: PAD - min_left, baseline: y + PAD + max_top });
        y += row_height;
        width = width.max(row_width);
    }
    (rows, width, y)
}

fn atlas() {
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("out");
    fs::create_dir_all(&out_dir).expect("create out dir");

    let groups = [t04_fonts::atlas(18.0), t04_fonts::atlas(36.0)];
    let layouts: Vec<_> = groups.iter().map(layout).collect();

    let width = layouts.iter().map(|(_, w, _)| *w).max().unwrap_or(1).max(1);
    let height = layouts.iter().map(|(_, _, h)| *h).sum::<i32>()
        + GROUP_GAP * (groups.len() as i32 - 1);
    let (width, height) = (width as u32, height as u32);
    let mut canvas = vec![0u8; width as usize * height as usize * 4];
    for px in canvas.chunks_exact_mut(4) {
        px[3] = 255;
    }

    let mut y_offset = 0;
    for (group, (rows, _, group_height)) in groups.iter().zip(layouts.iter()) {
        draw_group(&mut canvas, width as i32, group, rows, y_offset);
        y_offset += group_height + GROUP_GAP;
    }

    let path = out_dir.join(format!("{}.png", t04_fonts::stack_name()));
    png::write_rgba(&path, width, height, &canvas).expect("write png");
    println!("{}", path.display());
}

fn draw_group(canvas: &mut [u8], canvas_w: i32, atlas: &Atlas, rows: &[RowLayout], y_offset: i32) {
    for (row, layout) in atlas.rows.iter().zip(rows.iter()) {
        let mut pen = 0;
        for cell in &row.cells {
            for placed in &cell.glyphs {
                let x = layout.x0 + pen + placed.glyph.left as i32;
                let y = y_offset + layout.baseline - placed.glyph.top as i32;
                put_glyph(canvas, canvas_w, x, y, &placed.glyph);
            }
            pen += cell.advance().round().max(0.0) as i32;
        }
    }
}

fn put_glyph(canvas: &mut [u8], canvas_w: i32, x: i32, y: i32, glyph: &Glyph) {
    if glyph.w == 0 || glyph.h == 0 {
        return;
    }
    let canvas_h = canvas.len() as i32 / (canvas_w * 4);
    for gy in 0..glyph.h as i32 {
        let dy = y + gy;
        if dy < 0 || dy >= canvas_h {
            continue;
        }
        for gx in 0..glyph.w as i32 {
            let dx = x + gx;
            if dx < 0 || dx >= canvas_w {
                continue;
            }
            let dst = ((dy * canvas_w + dx) * 4) as usize;
            let src = (gy * glyph.w as i32 + gx) as usize;
            if glyph.color {
                let src = src * 4;
                if src + 3 >= glyph.data.len() {
                    continue;
                }
                let alpha = glyph.data[src + 3] as u32;
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    canvas[dst + channel] =
                        (glyph.data[src + channel] as u32 + canvas[dst + channel] as u32 * inverse / 255)
                            as u8;
                }
            } else {
                if src >= glyph.data.len() {
                    continue;
                }
                let alpha = glyph.data[src] as u32;
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    canvas[dst + channel] =
                        ((255 * alpha + canvas[dst + channel] as u32 * inverse) / 255) as u8;
                }
            }
            canvas[dst + 3] = 255;
        }
    }
}

