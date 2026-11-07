use crate::api::models::{DriveFile, FileInfo, ShareEntry};
use crate::util::format_size;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

pub fn tree_line(depth: usize, entry: &ShareEntry) {
    let indent = "  ".repeat(depth);
    if entry.is_folder() {
        println!("{indent}[dir] {}/", entry.name);
    } else {
        println!("{indent}{}  {}", entry.name, format_size(entry.size));
    }
}

pub fn share_file_line(index: usize, info: &FileInfo) {
    println!("{:>3}. {}", index + 1, info.describe());
}

pub fn drive_line(entry: &DriveFile) {
    if entry.is_folder() {
        println!("[dir] {}/", entry.name);
    } else {
        println!("{:>10}  {}", format_size(entry.size), entry.name);
    }
}

pub fn info_line(key: &str, value: impl std::fmt::Display) {
    println!("{key:<14} {value}");
}

pub fn progress_bar(size: u64, label: &str, quiet: bool) -> ProgressBar {
    if quiet {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new(size.max(1));
    let style = ProgressStyle::with_template(
        "{spinner:.green} {msg:30} [{bar:32.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
    )
    .unwrap_or_else(|_| ProgressStyle::default_bar())
    .progress_chars("=>-")
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
    bar.set_style(style);
    bar.set_message(label.to_string());
    bar.enable_steady_tick(Duration::from_millis(120));
    bar
}

pub fn status(quiet: bool, message: impl std::fmt::Display) {
    if !quiet {
        println!("{message}");
    }
}

pub fn warn(message: impl std::fmt::Display) {
    eprintln!("warning: {message}");
}

pub fn success(message: impl std::fmt::Display) {
    println!("{message}");
}
